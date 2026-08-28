//! End-to-end: private chats, multi-agent routing, groups, delivery fallbacks,
//! stop button, commands and error paths. The real bot runs against the mocks
//! in `common` through its own long-polling loop.

mod common;

use common::*;
use docsgpt_telegram::config::GroupMode;
use serde_json::json;
use std::time::{Duration, Instant};

#[tokio::test]
async fn private_text_streams_drafts_then_final_with_sources_and_multi_turn() {
    let docs = start_docsgpt().await;
    docs.on_stream(|_| {
        sse(answer_steps(
            "The answer is **42**.",
            "conv-1",
            &[("Doc A", "https://example.com/a")],
        ))
    });
    let mut cfg = bot_config("private", &docs);
    cfg.description = Some("Answers from the docs".into());
    cfg.short_description = Some("DocsGPT".into());
    cfg.menu_button_url = Some("https://example.com/app".into());
    let bot = start_bot(cfg, &docs).await;
    let tg = bot.tg.clone();

    // Startup chores pushed during init / at the start of polling.
    tg.rec.wait_any("deleteWebhook").await;
    assert!(tg.rec.count("getMe") >= 1);
    assert_eq!(tg.rec.count("setMyCommands"), 2);
    assert_eq!(tg.rec.count("setMyDescription"), 1);
    assert_eq!(tg.rec.count("setMyShortDescription"), 1);
    assert_eq!(tg.rec.count("setChatMenuButton"), 1);

    bot.push_private(10, "What is the answer?");

    let draft = tg
        .rec
        .wait_for(
            "sendRichMessageDraft",
            |c| c.body["can_stop"] == json!(true),
            WAIT,
        )
        .await;
    assert_eq!(draft.body["chat_id"], json!(CHAT));
    assert!(draft.body["draft_id"].as_i64().unwrap_or(0) > 0);
    assert_eq!(draft.body["keep_on_stop"], json!(true));

    let final_msg = tg.rec.wait_any("sendRichMessage").await;
    let md = final_msg.markdown();
    assert!(md.contains("The answer is **42**."), "markdown: {md}");
    assert!(
        md.contains("<details><summary>Sources (1)</summary>"),
        "markdown: {md}"
    );
    assert!(
        md.contains("[Doc A](https://example.com/a)"),
        "markdown: {md}"
    );
    assert_eq!(final_msg.body["chat_id"], json!(CHAT));
    assert!(
        final_msg.body.get("reply_parameters").is_none(),
        "private replies are not threaded"
    );

    // 👀 while working, cleared when done.
    tg.rec
        .wait_for(
            "setMessageReaction",
            |c| c.body["message_id"] == json!(10) && c.body["reaction"] == json!([]),
            WAIT,
        )
        .await;
    let reactions = tg.rec.calls("setMessageReaction");
    assert_eq!(reactions[0].body["message_id"], json!(10));
    assert_eq!(reactions[0].body["reaction"][0]["type"], json!("emoji"));
    assert_eq!(reactions[0].body["reaction"][0]["emoji"], json!("👀"));

    let first = docs.rec.calls("/stream");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].body["question"], json!("What is the answer?"));
    assert_eq!(first[0].body["api_key"], json!("key-default"));
    assert_eq!(first[0].body["history"], json!("[]"));
    assert!(first[0].body["conversation_id"].is_null());
    assert!(first[0].body.get("attachments").is_none());

    // Second turn in the same chat continues the conversation DocsGPT returned.
    bot.push_private(11, "And why?");
    let second = docs
        .rec
        .wait_for("/stream", |c| c.body["question"] == json!("And why?"), WAIT)
        .await;
    assert_eq!(second.body["conversation_id"], json!("conv-1"));
    tg.rec.wait_count("sendRichMessage", 2, WAIT).await;
}

#[tokio::test]
async fn multi_agent_routing_switching_and_reset() {
    let docs = start_docsgpt().await;
    docs.on_stream(|body| {
        let key = body["api_key"].as_str().unwrap_or("");
        let agent = key.strip_prefix("key-").unwrap_or("unknown").to_string();
        sse(answer_steps(
            &format!("answer from {agent}"),
            &format!("conv-{agent}"),
            &[],
        ))
    });
    let mut cfg = bot_config("multi", &docs);
    cfg.agents = vec![
        agent("support", "key-support", true),
        agent("sales", "key-sales", false),
    ];
    let bot = start_bot(cfg, &docs).await;
    let tg = bot.tg.clone();

    // #sales prefix routes one question to the sales agent (fresh conversation).
    bot.push_private(101, "#sales hello");
    let s = docs
        .rec
        .wait_for("/stream", |c| c.body["question"] == json!("hello"), WAIT)
        .await;
    assert_eq!(s.body["api_key"], json!("key-sales"));
    assert!(s.body["conversation_id"].is_null());
    tg.rec
        .wait_for(
            "sendRichMessage",
            |c| c.markdown().contains("answer from sales"),
            WAIT,
        )
        .await;

    // A plain question goes to the default agent, with its own (new) conversation.
    bot.push_private(102, "hi support");
    let s = docs
        .rec
        .wait_for(
            "/stream",
            |c| c.body["question"] == json!("hi support"),
            WAIT,
        )
        .await;
    assert_eq!(s.body["api_key"], json!("key-support"));
    assert!(s.body["conversation_id"].is_null());
    tg.rec
        .wait_for(
            "sendRichMessage",
            |c| c.markdown().contains("answer from support"),
            WAIT,
        )
        .await;

    // The sales conversation is tracked separately from support's in the same chat.
    bot.push_private(103, "#sales again");
    let s = docs
        .rec
        .wait_for("/stream", |c| c.body["question"] == json!("again"), WAIT)
        .await;
    assert_eq!(s.body["api_key"], json!("key-sales"));
    assert_eq!(s.body["conversation_id"], json!("conv-sales"));
    tg.rec.wait_count("sendRichMessage", 3, WAIT).await;

    // /agent sales makes sales the active agent for plain questions.
    bot.push_private_command(104, "/agent sales");
    tg.rec
        .wait_for(
            "sendMessage",
            |c| c.text().contains("Switched to sales"),
            WAIT,
        )
        .await;
    bot.push_private(105, "second sales question");
    let s = docs
        .rec
        .wait_for(
            "/stream",
            |c| c.body["question"] == json!("second sales question"),
            WAIT,
        )
        .await;
    assert_eq!(s.body["api_key"], json!("key-sales"));
    assert_eq!(s.body["conversation_id"], json!("conv-sales"));
    tg.rec.wait_count("sendRichMessage", 4, WAIT).await;

    // /agents shows an inline keyboard with one button per agent.
    bot.push_private_command(106, "/agents");
    let menu = tg
        .rec
        .wait_for(
            "sendMessage",
            |c| c.body.get("reply_markup").is_some(),
            WAIT,
        )
        .await;
    let buttons: Vec<String> = menu.body["reply_markup"]["inline_keyboard"]
        .as_array()
        .expect("inline_keyboard rows")
        .iter()
        .flat_map(|row| row.as_array().unwrap().iter())
        .map(|b| b["callback_data"].as_str().unwrap().to_string())
        .collect();
    assert!(
        buttons.contains(&"agent:support".to_string()),
        "{buttons:?}"
    );
    assert!(buttons.contains(&"agent:sales".to_string()), "{buttons:?}");
    assert!(
        menu.text().contains("✓ sales"),
        "active agent is marked: {}",
        menu.text()
    );

    // Tapping a button switches back to support.
    tg.push(
        "callback_query",
        json!({
            "id": "cb1",
            "from": user(USER, "Alice"),
            "chat_instance": "ci-1",
            "data": "agent:support",
            "message": message(500, chat(CHAT, "private"), bot_user(), "Agents"),
        }),
    );
    let cb = tg
        .rec
        .wait_for(
            "answerCallbackQuery",
            |c| c.body["callback_query_id"] == json!("cb1"),
            WAIT,
        )
        .await;
    assert!(cb.text().contains("Switched to support"), "{}", cb.text());
    tg.rec
        .wait_for(
            "editMessageText",
            |c| c.body["message_id"] == json!(500),
            WAIT,
        )
        .await;
    bot.push_private(107, "back to support");
    let s = docs
        .rec
        .wait_for(
            "/stream",
            |c| c.body["question"] == json!("back to support"),
            WAIT,
        )
        .await;
    assert_eq!(s.body["api_key"], json!("key-support"));
    assert_eq!(s.body["conversation_id"], json!("conv-support"));
    tg.rec.wait_count("sendRichMessage", 5, WAIT).await;

    // /new forgets the active agent's conversation.
    bot.push_private_command(108, "/new");
    tg.rec
        .wait_for(
            "sendMessage",
            |c| c.text().contains("Started a new conversation with support"),
            WAIT,
        )
        .await;
    bot.push_private(109, "fresh start");
    let s = docs
        .rec
        .wait_for(
            "/stream",
            |c| c.body["question"] == json!("fresh start"),
            WAIT,
        )
        .await;
    assert_eq!(s.body["api_key"], json!("key-support"));
    assert!(
        s.body["conversation_id"].is_null(),
        "conversation should be cleared after /new"
    );
    tg.rec.wait_count("sendRichMessage", 6, WAIT).await;

    // Unknown #agent is reported, not sent to DocsGPT.
    bot.push_private(110, "#foo hello");
    let r = tg
        .rec
        .wait_for("sendMessage", |c| c.text().contains("Unknown agent"), WAIT)
        .await;
    assert!(r.text().contains("#foo"), "{}", r.text());
    assert!(
        r.text().contains("#support") && r.text().contains("#sales"),
        "{}",
        r.text()
    );
    assert_eq!(docs.rec.count("/stream"), 6);
}

#[tokio::test]
async fn groups_answer_only_when_mentioned_or_replied_to() {
    let docs = start_docsgpt().await;
    let bot = start_bot(bot_config("group", &docs), &docs).await;
    let tg = bot.tg.clone();
    let group = chat(-100500, "supergroup");
    let alice = user(7, "Alice");

    // Plain chatter is ignored.
    tg.push(
        "message",
        message(1, group.clone(), alice.clone(), "just chatting"),
    );
    docs.rec
        .assert_none("/stream", Duration::from_millis(1500))
        .await;
    assert_eq!(
        tg.rec.count("sendMessage") + tg.rec.count("sendRichMessage"),
        0
    );

    // A mention (UTF-16 offsets: the emoji is two units) is answered with the mention stripped.
    let text = "😀 @mockbot what is up?";
    let mut m = message(2, group.clone(), alice.clone(), text);
    m["entities"] = json!([mention_entity(text, "@mockbot")]);
    tg.push("message", m);
    let s = docs.rec.wait_any("/stream").await;
    assert_eq!(s.body["question"], json!("😀 what is up?"));
    let reply = tg.rec.wait_any("sendRichMessage").await;
    assert_eq!(reply.body["chat_id"], json!(-100500));
    assert_eq!(reply.body["reply_parameters"]["message_id"], json!(2));
    assert!(reply.markdown().contains("Hello from the mock assistant."));

    // A reply to one of the bot's messages is answered too.
    let mut m = message(3, group.clone(), alice.clone(), "and this?");
    m["reply_to_message"] = message(99, group.clone(), bot_user(), "earlier answer");
    tg.push("message", m);
    docs.rec
        .wait_for(
            "/stream",
            |c| c.body["question"] == json!("and this?"),
            WAIT,
        )
        .await;
    let replies = tg.rec.wait_count("sendRichMessage", 2, WAIT).await;
    assert_eq!(replies[1].body["reply_parameters"]["message_id"], json!(3));

    // Groups never get drafts, and typing was shown instead.
    assert_eq!(tg.drafts(), 0);
    assert!(tg.rec.count("sendChatAction") >= 1);
}

#[tokio::test]
async fn groups_all_mode_answers_every_message() {
    let docs = start_docsgpt().await;
    let mut cfg = bot_config("groupall", &docs);
    cfg.groups = GroupMode::All;
    let bot = start_bot(cfg, &docs).await;
    let tg = bot.tg.clone();

    tg.push(
        "message",
        message(
            1,
            chat(-100600, "supergroup"),
            user(8, "Bob"),
            "no mention here",
        ),
    );
    let s = docs.rec.wait_any("/stream").await;
    assert_eq!(s.body["question"], json!("no mention here"));
    let reply = tg.rec.wait_any("sendRichMessage").await;
    assert_eq!(reply.body["reply_parameters"]["message_id"], json!(1));
    assert_eq!(tg.drafts(), 0);
}

#[tokio::test]
async fn groups_off_mode_ignores_even_mentions() {
    let docs = start_docsgpt().await;
    let mut cfg = bot_config("groupoff", &docs);
    cfg.groups = GroupMode::Off;
    let bot = start_bot(cfg, &docs).await;
    let tg = bot.tg.clone();

    let text = "@mockbot are you there?";
    let mut m = message(1, chat(-100700, "supergroup"), user(9, "Cat"), text);
    m["entities"] = json!([mention_entity(text, "@mockbot")]);
    tg.push("message", m);
    docs.rec
        .assert_none("/stream", Duration::from_millis(1500))
        .await;
    assert_eq!(
        tg.rec.count("sendMessage")
            + tg.rec.count("sendRichMessage")
            + tg.rec.count("setMessageReaction"),
        0
    );
}

#[tokio::test]
async fn fallback_chain_rich_to_markdown_v2_to_plain_with_splitting() {
    let docs = start_docsgpt().await;
    let long: String = (1..=40)
        .map(|i| {
            format!(
                "Paragraph {i}: {}",
                "lorem ipsum dolor sit amet ".repeat(10)
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    assert!(long.chars().count() > 4096 * 2);
    let script = long.clone();
    docs.on_stream(move |_| {
        sse(answer_steps(
            &script,
            "conv-1",
            &[("Doc", "https://example.com/d")],
        ))
    });
    let bot = start_bot(bot_config("fallback", &docs), &docs).await;
    let tg = bot.tg.clone();

    // Rich rejected → MarkdownV2 messages, split under the 4096 limit.
    tg.reject("sendRichMessage");
    bot.push_private(1, "long please");
    tg.rec
        .wait_for(
            "setMessageReaction",
            |c| c.body["message_id"] == json!(1) && c.body["reaction"] == json!([]),
            WAIT,
        )
        .await;
    assert!(
        tg.rec.count("sendRichMessage") >= 1,
        "rich delivery was attempted first"
    );
    let v2 = tg.rec.calls("sendMessage");
    assert!(
        v2.len() >= 3,
        "expected the long answer split into several messages, got {}",
        v2.len()
    );
    for m in &v2 {
        assert_eq!(m.body["parse_mode"], json!("MarkdownV2"));
        assert!(
            m.text().chars().count() <= 4096,
            "message over limit: {} chars",
            m.text().chars().count()
        );
        assert_eq!(m.body["chat_id"], json!(CHAT));
    }
    let joined = v2.iter().map(|m| m.text()).collect::<Vec<_>>().join("\n");
    assert!(
        joined.contains("Paragraph 1:") && joined.contains("Paragraph 40:"),
        "all paragraphs delivered"
    );
    assert!(
        joined.contains("*Sources*"),
        "sources appended in MarkdownV2"
    );

    // MarkdownV2 rejected as well → plain text, again split.
    let before = tg.rec.count("sendMessage");
    tg.reject_if("sendMessage", |b| b["parse_mode"] == json!("MarkdownV2"));
    bot.push_private(2, "long again");
    tg.rec
        .wait_for(
            "setMessageReaction",
            |c| c.body["message_id"] == json!(2) && c.body["reaction"] == json!([]),
            WAIT,
        )
        .await;
    let sent: Vec<Call> = tg
        .rec
        .calls("sendMessage")
        .into_iter()
        .skip(before)
        .collect();
    let rejected_v2: Vec<&Call> = sent
        .iter()
        .filter(|m| m.body["parse_mode"] == json!("MarkdownV2"))
        .collect();
    assert_eq!(
        rejected_v2.len(),
        1,
        "stops retrying MarkdownV2 after the first 400"
    );
    let plain: Vec<&Call> = sent
        .iter()
        .filter(|m| m.body.get("parse_mode").is_none())
        .collect();
    assert!(
        plain.len() >= 3,
        "expected several plain messages, got {}",
        plain.len()
    );
    for m in &plain {
        assert!(m.text().chars().count() <= 4096);
    }
    let joined = plain
        .iter()
        .map(|m| m.text())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("Paragraph 1:") && joined.contains("Paragraph 40:"));
    assert!(joined.contains("Sources:") && joined.contains("https://example.com/d"));
}

#[tokio::test]
async fn stop_button_cancels_generation_promptly() {
    let docs = start_docsgpt().await;
    docs.on_stream(|_| {
        sse(vec![
            ev(ev_message_id("m1", "conv-1")),
            ev(ev_answer("Partial ")),
            ev(ev_answer("answer")),
            Step::Sleep(Duration::from_secs(20)),
            ev(ev_answer(" that never arrives")),
            ev(ev_end()),
        ])
    });
    let bot = start_bot(bot_config("stop", &docs), &docs).await;
    let tg = bot.tg.clone();

    bot.push_private(1, "tell me a long story");
    let draft = tg
        .rec
        .wait_for(
            "sendRichMessageDraft",
            |c| c.markdown().contains("Partial"),
            WAIT,
        )
        .await;
    assert_eq!(draft.body["can_stop"], json!(true));
    let draft_id = draft.body["draft_id"].as_i64().expect("draft id");

    // Let the bot park on the stalled stream, then press Stop.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let pressed = Instant::now();
    tg.push(
        "stopped_message_generation",
        json!({"chat": chat(CHAT, "private"), "draft_id": draft_id}),
    );

    let final_msg = tg
        .rec
        .wait_for("sendRichMessage", |_| true, Duration::from_secs(6))
        .await;
    let took = pressed.elapsed();
    assert!(
        took < Duration::from_secs(3),
        "final message arrived {took:?} after stop"
    );
    assert_eq!(final_msg.markdown().trim(), "Partial answer");
    assert!(!final_msg.markdown().contains("never arrives"));
    tg.rec
        .wait_for(
            "setMessageReaction",
            |c| c.body["reaction"] == json!([]),
            WAIT,
        )
        .await;
}

#[tokio::test]
async fn start_and_help_reply_in_markdown_v2() {
    let docs = start_docsgpt().await;
    let bot = start_bot(bot_config("cmds", &docs), &docs).await;
    let tg = bot.tg.clone();

    bot.push_private_command(1, "/start");
    let start = tg
        .rec
        .wait_for("sendMessage", |c| c.text().contains("Hi Alice"), WAIT)
        .await;
    assert_eq!(start.body["parse_mode"], json!("MarkdownV2"));
    assert_eq!(start.body["chat_id"], json!(CHAT));

    bot.push_private_command(2, "/help");
    let help = tg
        .rec
        .wait_for("sendMessage", |c| c.text().contains("Commands"), WAIT)
        .await;
    assert_eq!(help.body["parse_mode"], json!("MarkdownV2"));
    assert!(help.text().contains("/new"), "{}", help.text());
    assert!(help.text().contains("@mockbot"), "{}", help.text());

    // Commands addressed to another bot are not handled as commands (no help
    // text). Note: the bot currently forwards such text to DocsGPT as a plain
    // question in private chats; that is not asserted here.
    bot.push_private_command(3, "/help@otherbot");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(tg.rec.count("sendMessage"), 2, "no third command reply");
}

#[tokio::test]
async fn allowed_chats_restriction() {
    let docs = start_docsgpt().await;
    let mut cfg = bot_config("allowed", &docs);
    cfg.allowed_chats = vec![CHAT];
    let bot = start_bot(cfg, &docs).await;
    let tg = bot.tg.clone();

    tg.push(
        "message",
        message(1, chat(2002, "private"), user(2002, "Eve"), "let me in"),
    );
    let refusal = tg
        .rec
        .wait_for("sendMessage", |c| c.body["chat_id"] == json!(2002), WAIT)
        .await;
    assert!(refusal.text().contains("restricted"), "{}", refusal.text());
    docs.rec
        .assert_none("/stream", Duration::from_millis(800))
        .await;

    bot.push_private(2, "hello from an allowed chat");
    let s = docs.rec.wait_any("/stream").await;
    assert_eq!(s.body["question"], json!("hello from an allowed chat"));
    tg.rec
        .wait_for(
            "sendRichMessage",
            |c| c.body["chat_id"] == json!(CHAT),
            WAIT,
        )
        .await;
}

#[tokio::test]
async fn docsgpt_http_error_sends_apology() {
    let docs = start_docsgpt().await;
    docs.on_stream(|_| StreamReply::Http(500, "internal error".into()));
    let bot = start_bot(bot_config("err500", &docs), &docs).await;
    let tg = bot.tg.clone();

    bot.push_private(1, "anything");
    let m = tg
        .rec
        .wait_for("sendMessage", |c| c.text().contains("Sorry"), WAIT)
        .await;
    assert!(m.text().contains("could not be reached"), "{}", m.text());
    assert!(m.body.get("parse_mode").is_none());
    tg.rec
        .wait_for(
            "setMessageReaction",
            |c| c.body["reaction"] == json!([]),
            WAIT,
        )
        .await;
    assert_eq!(tg.rec.count("sendRichMessage"), 0);
}

#[tokio::test]
async fn docsgpt_error_event_sends_apology() {
    let docs = start_docsgpt().await;
    docs.on_stream(|_| {
        sse(vec![
            ev(ev_message_id("m1", "conv-1")),
            ev(ev_error("boom")),
        ])
    });
    let bot = start_bot(bot_config("errevent", &docs), &docs).await;
    let tg = bot.tg.clone();

    bot.push_private(1, "anything");
    let m = tg
        .rec
        .wait_for("sendMessage", |c| c.text().contains("Sorry"), WAIT)
        .await;
    assert!(m.text().contains("boom"), "{}", m.text());
    tg.rec
        .wait_for(
            "setMessageReaction",
            |c| c.body["reaction"] == json!([]),
            WAIT,
        )
        .await;
    assert_eq!(tg.rec.count("sendRichMessage"), 0);
}
