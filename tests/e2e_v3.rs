//! Behaviour new in version 3 (shared docsgpt-bot crates).

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use serde_json::json;

#[tokio::test]
async fn thumbs_reactions_become_feedback_on_that_answer() {
    let docs = start_docsgpt().await;
    let bot = start_bot(bot_config("fb", &docs), &docs).await;
    bot.push_private(1, "first question");
    let answer = bot.sent_message_id("Hello from the mock assistant").await;

    bot.push_reaction(answer, &[], &["👍"]);
    let fb = docs.rec.wait_any("/api/feedback").await;
    assert_eq!(
        fb.body,
        json!({"feedback": "like", "conversation_id": "conv-1", "question_index": 0, "api_key": "key-default"})
    );

    // Taking the 👍 back clears it; other emoji are not feedback.
    bot.push_reaction(answer, &["👍"], &[]);
    let calls = docs.rec.wait_count("/api/feedback", 2, WAIT).await;
    assert!(calls[1].body["feedback"].is_null());
    bot.push_reaction(answer, &[], &["🔥"]);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(docs.rec.count("/api/feedback"), 2);

    // The second answer in the conversation is position 1; 👎 is a dislike.
    bot.push_private(2, "second question");
    docs.rec.wait_count("/stream", 2, WAIT).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let sent: Vec<i64> = bot
        .tg
        .rec
        .calls("sent")
        .iter()
        .filter(|c| {
            c.body["text"]
                .as_str()
                .is_some_and(|t| t.contains("Hello from the mock assistant"))
        })
        .map(|c| c.body["message_id"].as_i64().unwrap())
        .collect();
    bot.push_reaction(*sent.last().unwrap(), &[], &["👎"]);
    let calls = docs.rec.wait_count("/api/feedback", 3, WAIT).await;
    assert_eq!(
        (
            calls[2].body["feedback"].as_str(),
            calls[2].body["question_index"].as_u64()
        ),
        (Some("dislike"), Some(1))
    );
}

#[tokio::test]
async fn a_version_2_sqlite_file_keeps_working() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("docsgpt-telegram.db");
    {
        // What version 2 left on disk: a conversation and the chat's chosen agent.
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(
            "CREATE TABLE conversations (scope TEXT NOT NULL, agent TEXT NOT NULL, conversation_id TEXT NOT NULL,
               updated_at INTEGER NOT NULL, PRIMARY KEY (scope, agent));
             CREATE TABLE chat_state (scope TEXT PRIMARY KEY, state TEXT NOT NULL, updated_at INTEGER NOT NULL);
             CREATE TABLE business_links (bot TEXT NOT NULL, id TEXT NOT NULL, link TEXT NOT NULL,
               updated_at INTEGER NOT NULL, PRIMARY KEY (bot, id));
             INSERT INTO conversations VALUES ('upg:1001:0', 'sales', 'conv-old', 1);
             INSERT INTO chat_state VALUES ('upg:1001:0',
               '{\"active_agent\":\"sales\",\"last_question\":\"old q\",\"user\":{\"id\":1001,\"first_name\":\"Alice\"}}', 1);",
        )
        .unwrap();
    }
    let storage = docsgpt_bot::storage::sqlite::SqliteStorage::open(path.to_str().unwrap())
        .await
        .unwrap();
    let docs = start_docsgpt().await;
    docs.on_stream(|body| {
        reply_text(
            "Sure.",
            body["conversation_id"].as_str().unwrap_or("conv-new"),
        )
    });
    let mut cfg = bot_config("upg", &docs);
    cfg.agents = vec![
        agent("support", "key-support", true),
        agent("sales", "key-sales", false),
    ];
    let bot = start_bot_with_storage(cfg, &docs, Arc::new(storage)).await;

    bot.push_private(1, "next question");
    let call = docs.rec.wait_any("/stream").await;
    assert_eq!(
        call.body["api_key"], "key-sales",
        "the chosen agent is kept"
    );
    assert_eq!(
        call.body["conversation_id"], "conv-old",
        "the conversation continues"
    );

    // Earlier answers in that conversation weren't counted, so no feedback position is known.
    let answer = bot.sent_message_id("Sure.").await;
    bot.push_reaction(answer, &[], &["👍"]);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(docs.rec.count("/api/feedback"), 0);

    // After /new, answers are counted again.
    bot.push_private_command(2, "/new");
    bot.tg
        .rec
        .wait_for(
            "sendMessage",
            |c| c.text().contains("new conversation"),
            WAIT,
        )
        .await;
    bot.push_private(3, "fresh start");
    let calls = docs.rec.wait_count("/stream", 2, WAIT).await;
    assert!(calls[1].body.get("conversation_id").is_none());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let sent = bot.tg.rec.calls("sent");
    let last = sent
        .iter()
        .rev()
        .find(|c| c.body["text"].as_str().is_some_and(|t| t.contains("Sure.")))
        .unwrap();
    bot.push_reaction(last.body["message_id"].as_i64().unwrap(), &[], &["👍"]);
    let fb = docs.rec.wait_any("/api/feedback").await;
    assert_eq!(
        (
            fb.body["conversation_id"].as_str(),
            fb.body["question_index"].as_u64()
        ),
        (Some("conv-new"), Some(0))
    );
}

#[tokio::test]
async fn a_partial_answer_that_fails_says_so() {
    let docs = start_docsgpt().await;
    docs.on_stream(|_| {
        sse(vec![
            ev(ev_message_id("m", "c")),
            ev(ev_answer("Half an answer")),
            ev(ev_error("LLM overloaded")),
        ])
    });
    let bot = start_bot(bot_config("partial", &docs), &docs).await;
    bot.push_private(1, "q");
    let msg = bot.tg.rec.wait_any("sendRichMessage").await;
    assert_eq!(
        msg.markdown().trim(),
        "Half an answer\n\n_The answer was cut short: LLM overloaded_"
    );
}

#[tokio::test]
async fn a_bare_agent_tag_switches_the_chat() {
    let docs = start_docsgpt().await;
    let mut cfg = bot_config("switch", &docs);
    cfg.agents = vec![
        agent("support", "key-support", true),
        agent("sales", "key-sales", false),
    ];
    let bot = start_bot(cfg, &docs).await;
    bot.push_private(1, "#sales");
    bot.tg
        .rec
        .wait_for(
            "sendMessage",
            |c| c.text() == "Now answering with #sales.",
            WAIT,
        )
        .await;
    assert_eq!(docs.rec.count("/stream"), 0);
    bot.push_private(2, "what does it cost?");
    let call = docs.rec.wait_any("/stream").await;
    assert_eq!(call.body["api_key"], "key-sales");
}
