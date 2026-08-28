//! End-to-end: attachments (documents, photos, voice), tool outputs (artifacts,
//! generated images), Telegram Business, guest queries and inline mode.

mod common;

use bytes::Bytes;
use common::*;
use serde_json::json;
use std::time::{Duration, Instant};

#[tokio::test]
async fn document_with_caption_is_uploaded_and_attached() {
    let docs = start_docsgpt().await;
    docs.queue_task_status(&["PENDING", "SUCCESS"]);
    let pdf = Bytes::from_static(b"%PDF-1.4\n% fake report bytes for the e2e test\n%%EOF\n");
    let bot = start_bot(bot_config("docupload", &docs), &docs).await;
    let tg = bot.tg.clone();
    tg.add_file("file-doc-1", "documents/file_1.pdf", pdf.clone());

    let mut m = bare_message(1, chat(CHAT, "private"), user(USER, "Alice"));
    m["document"] = json!({"file_id": "file-doc-1", "file_unique_id": "u1", "file_name": "report.pdf", "mime_type": "application/pdf", "file_size": pdf.len()});
    m["caption"] = json!("Summarize this");
    tg.push("message", m);

    let gf = tg.rec.wait_any("getFile").await;
    assert_eq!(gf.body["file_id"], json!("file-doc-1"));

    let up = docs.rec.wait_any("/api/store_attachment").await;
    let file = up.file("file");
    assert_eq!(
        file.bytes, pdf,
        "uploaded bytes must equal what Telegram served"
    );
    assert_eq!(file.filename, "report.pdf");
    assert_eq!(file.content_type.as_deref(), Some("application/pdf"));
    assert_eq!(up.body["api_key"], json!("key-default"));

    let s = docs.rec.wait_any("/stream").await;
    assert_eq!(s.body["attachments"], json!(["att-1"]));
    assert_eq!(s.body["question"], json!("Summarize this"));
    let polls = docs.rec.calls("/api/task_status");
    assert!(
        polls.len() >= 2,
        "polled until SUCCESS, got {} polls",
        polls.len()
    );
    assert_eq!(polls[0].query["task_id"], "task-1");
    assert!(
        polls[0].at < s.at,
        "attachment processing finished before asking"
    );

    tg.rec.wait_any("sendRichMessage").await;
    assert!(
        tg.rec
            .calls("sendChatAction")
            .iter()
            .any(|c| c.body["action"] == json!("upload_document"))
    );
}

#[tokio::test]
async fn photo_without_caption_uses_default_question() {
    let docs = start_docsgpt().await;
    let jpg = Bytes::from_static(b"\xFF\xD8\xFF\xE0 fake jpeg \xFF\xD9");
    let bot = start_bot(bot_config("photo", &docs), &docs).await;
    let tg = bot.tg.clone();
    tg.add_file("file-photo-big", "photos/file_2.jpg", jpg.clone());
    tg.add_file(
        "file-photo-small",
        "photos/file_3.jpg",
        Bytes::from_static(b"small"),
    );

    let mut m = bare_message(1, chat(CHAT, "private"), user(USER, "Alice"));
    m["photo"] = json!([
        {"file_id": "file-photo-small", "file_unique_id": "ps", "width": 90, "height": 90, "file_size": 1000},
        {"file_id": "file-photo-big", "file_unique_id": "pb", "width": 800, "height": 800, "file_size": 50000},
    ]);
    tg.push("message", m);

    let gf = tg.rec.wait_any("getFile").await;
    assert_eq!(
        gf.body["file_id"],
        json!("file-photo-big"),
        "largest size is picked"
    );
    let up = docs.rec.wait_any("/api/store_attachment").await;
    let file = up.file("file");
    assert_eq!(file.bytes, jpg);
    assert_eq!(file.filename, "photo_pb.jpg");
    assert_eq!(file.content_type.as_deref(), Some("image/jpeg"));

    let s = docs.rec.wait_any("/stream").await;
    let q = s.body["question"].as_str().unwrap_or("");
    assert!(q.starts_with("I've attached photo_pb.jpg"), "{q}");
    assert!(q.contains("Please review"), "{q}");
    assert_eq!(s.body["attachments"], json!(["att-1"]));
    tg.rec.wait_any("sendRichMessage").await;
    assert_eq!(tg.rec.count("getFile"), 1);
}

#[tokio::test]
async fn voice_note_is_transcribed_before_asking() {
    let docs = start_docsgpt().await;
    docs.set_stt("what is the weather like");
    let ogg = Bytes::from_static(b"OggS fake opus voice note");
    let bot = start_bot(bot_config("voice", &docs), &docs).await;
    let tg = bot.tg.clone();
    tg.add_file("file-voice-1", "voice/file_4.oga", ogg.clone());

    let mut m = bare_message(1, chat(CHAT, "private"), user(USER, "Alice"));
    m["voice"] = json!({"file_id": "file-voice-1", "file_unique_id": "v1", "duration": 3, "mime_type": "audio/ogg", "file_size": ogg.len()});
    tg.push("message", m);

    let stt = docs.rec.wait_any("/api/stt").await;
    let file = stt.file("file");
    assert!(
        file.filename.ends_with(".ogg"),
        "filename: {}",
        file.filename
    );
    assert_eq!(file.bytes, ogg);
    assert_eq!(
        stt.query.get("api_key").map(String::as_str),
        Some("key-default")
    );
    assert_eq!(stt.body["api_key"], json!("key-default"));

    let s = docs.rec.wait_any("/stream").await;
    assert_eq!(s.body["question"], json!("what is the weather like"));
    assert!(s.body.get("attachments").is_none());
    assert_eq!(docs.rec.count("/api/store_attachment"), 0);
    tg.rec.wait_any("sendRichMessage").await;
}

#[tokio::test]
async fn artifacts_are_downloaded_and_sent_as_documents() {
    let docs = start_docsgpt().await;
    let content = Bytes::from_static(b"hello from artifact\n");
    docs.add_artifact("art-1", "hello.txt", "text/plain", content.clone());
    docs.on_stream(|_| {
        sse(vec![
            ev(ev_message_id("m1", "conv-1")),
            ev(ev_tool_call(json!({"tool_name": "code_executor", "call_id": "c1", "action_name": "run_code", "arguments": {"code": "x"}, "status": "pending"}))),
            ev(ev_answer("I wrote the file.")),
            ev(ev_tool_call(json!({
                "tool_name": "code_executor", "call_id": "c1", "action_name": "run_code", "arguments": {"code": "x"},
                "artifact_id": "art-1",
                "artifacts": [{"id": "art-1", "filename": "hello.txt", "ref": "A1"}],
                "result": "{'status': 'ok', 'stdout_tail': '', 'artifacts': [{'artifact_id': 'art-1', 'version': 1, 'filename': 'hello.txt', 'mime_type': 'text/plain', 'size': 20, 'ref': 'A1'}]}",
                "status": "completed"
            }))),
            ev(json!({"type": "tool_calls", "tool_calls": []})),
            ev(ev_id("conv-1")),
            ev(ev_end()),
        ])
    });
    let bot = start_bot(bot_config("artifacts", &docs), &docs).await;
    let tg = bot.tg.clone();

    bot.push_private(1, "make a file");
    let dl = docs
        .rec
        .wait_for(
            "/api/artifacts/download",
            |c| c.body["id"] == json!("art-1"),
            WAIT,
        )
        .await;
    assert_eq!(
        dl.query.get("api_key").map(String::as_str),
        Some("key-default")
    );
    assert_eq!(
        dl.query.get("conversation_id").map(String::as_str),
        Some("conv-1")
    );

    let doc = tg.rec.wait_any("sendDocument").await;
    let file = doc.file("document");
    assert_eq!(file.bytes, content);
    assert_eq!(file.filename, "hello.txt");
    assert_eq!(doc.body["chat_id"], json!(CHAT));

    let rich = tg.rec.calls("sendRichMessage");
    assert_eq!(rich.len(), 1);
    assert!(rich[0].markdown().contains("I wrote the file."));
    assert!(rich[0].at <= doc.at, "text answer goes out before the file");
    assert!(
        tg.rec
            .calls("sendRichMessageDraft")
            .iter()
            .any(|c| c.markdown().contains("Running code")),
        "tool progress shown in a draft"
    );
}

#[tokio::test]
async fn generated_image_is_embedded_in_rich_message() {
    let docs = start_docsgpt().await;
    docs.add_image("cat.png", Bytes::from_static(TINY_PNG));
    let url = docs.image_url("cat.png");
    let script_url = url.clone();
    docs.on_stream(move |_| image_stream(&script_url));
    let bot = start_bot(bot_config("imagerich", &docs), &docs).await;
    let tg = bot.tg.clone();

    bot.push_private(1, "draw a cat");
    let rich = tg.rec.wait_any("sendRichMessage").await;
    assert!(
        rich.markdown().contains(&format!("![cat]({url})")),
        "{}",
        rich.markdown()
    );
    tg.rec
        .wait_for(
            "setMessageReaction",
            |c| c.body["reaction"] == json!([]),
            WAIT,
        )
        .await;
    assert_eq!(
        tg.rec.count("sendPhoto"),
        0,
        "the rich message already embeds the image"
    );
    assert_eq!(tg.rec.count("sendMessage"), 0);
}

#[tokio::test]
async fn generated_image_falls_back_to_send_photo_when_rich_rejected() {
    let docs = start_docsgpt().await;
    docs.add_image("cat.png", Bytes::from_static(TINY_PNG));
    let url = docs.image_url("cat.png");
    let script_url = url.clone();
    docs.on_stream(move |_| image_stream(&script_url));
    let bot = start_bot(bot_config("imagefallback", &docs), &docs).await;
    let tg = bot.tg.clone();
    tg.reject("sendRichMessage");

    bot.push_private(1, "draw a cat");
    let photo = tg.rec.wait_any("sendPhoto").await;
    assert_eq!(photo.body["photo"], json!(url));
    assert_eq!(photo.body["chat_id"], json!(CHAT));

    let v2 = tg.rec.calls("sendMessage");
    assert_eq!(v2.len(), 1);
    assert_eq!(v2[0].body["parse_mode"], json!("MarkdownV2"));
    assert!(
        v2[0].text().contains("Here is your cat"),
        "{}",
        v2[0].text()
    );
    assert!(
        !v2[0].text().contains("![cat]"),
        "image markdown stripped from the text: {}",
        v2[0].text()
    );
    assert!(v2[0].at < photo.at, "text before photo");
    assert_eq!(
        tg.rec.count("sendRichMessage"),
        2,
        "rich tried with and without the inline image"
    );
}

/// An image-generation turn: pending tool call, completed tool call with
/// `image_urls`, and an answer embedding the image.
fn image_stream(url: &str) -> StreamReply {
    let result = format!("{{'status_code': 200, 'image_urls': ['{url}'], 'message': 'done'}}");
    sse(vec![
        ev(ev_message_id("m1", "conv-1")),
        ev(ev_tool_call(
            json!({"tool_name": "imagegen", "call_id": "c1", "action_name": "imagegen_generate", "arguments": {"prompt": "cat"}, "status": "pending"}),
        )),
        ev(ev_tool_call(
            json!({"tool_name": "imagegen", "call_id": "c1", "action_name": "imagegen_generate", "arguments": {"prompt": "cat"}, "result": result, "status": "completed"}),
        )),
        ev(ev_answer(&format!(
            "Here is your cat:\n\n![cat]({url})\n\nEnjoy!"
        ))),
        ev(ev_id("conv-1")),
        ev(ev_end()),
    ])
}

#[tokio::test]
async fn business_messages_are_answered_without_drafts() {
    let docs = start_docsgpt().await;
    let bot = start_bot(bot_config("biz", &docs), &docs).await;
    let tg = bot.tg.clone();

    let conn = json!({"id": "bc1", "user": user(777, "Owner"), "user_chat_id": 777, "date": 1, "is_enabled": true, "rights": {"can_reply": true}});
    tg.add_business_connection(conn.clone());
    tg.push("business_connection", conn);
    let deadline = Instant::now() + WAIT;
    while bot
        .app
        .storage
        .get_business_link(bot.name(), "bc1")
        .await
        .unwrap()
        .is_none()
    {
        assert!(
            Instant::now() < deadline,
            "business connection was not stored"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The owner's own messages are not questions.
    let mut owner = message(1, chat(555, "private"), user(777, "Owner"), "note to self");
    owner["business_connection_id"] = json!("bc1");
    tg.push("business_message", owner);
    docs.rec
        .assert_none("/stream", Duration::from_millis(1000))
        .await;

    // A customer's message is answered through the connection, without drafts.
    let mut customer = message(
        2,
        chat(555, "private"),
        user(999, "Customer"),
        "Do you ship abroad?",
    );
    customer["business_connection_id"] = json!("bc1");
    tg.push("business_message", customer);
    let s = docs.rec.wait_any("/stream").await;
    assert_eq!(s.body["question"], json!("Do you ship abroad?"));
    let reply = tg.rec.wait_any("sendRichMessage").await;
    assert_eq!(reply.body["business_connection_id"], json!("bc1"));
    assert_eq!(reply.body["chat_id"], json!(555));
    assert_eq!(tg.drafts(), 0);
    assert!(
        tg.rec
            .calls("sendChatAction")
            .iter()
            .any(|c| c.body["business_connection_id"] == json!("bc1")),
        "typing goes through the connection"
    );
    assert_eq!(docs.rec.count("/stream"), 1);
}

#[tokio::test]
async fn guest_query_is_answered_with_answer_guest_query() {
    let docs = start_docsgpt().await;
    docs.on_stream(|_| sse(answer_steps("X is the unknown.", "conv-g", &[])));
    let bot = start_bot(bot_config("guest", &docs), &docs).await;
    let tg = bot.tg.clone();

    let text = "@mockbot what is X?";
    let mut m = message(1, chat(-100123, "supergroup"), user(42, "Guest"), text);
    m["guest_query_id"] = json!("gq1");
    m["entities"] = json!([mention_entity(text, "@mockbot")]);
    tg.push("guest_message", m);

    let a = tg.rec.wait_any("answerGuestQuery").await;
    assert_eq!(a.body["guest_query_id"], json!("gq1"));
    assert_eq!(a.body["result"]["type"], json!("article"));
    let msg_text = a.body["result"]["input_message_content"]["message_text"]
        .as_str()
        .unwrap_or("");
    assert!(msg_text.contains("X is the unknown"), "{msg_text}");

    let s = docs.rec.calls("/stream");
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].body["question"], json!("what is X?"));
    assert_eq!(
        tg.rec.count("sendMessage") + tg.rec.count("sendRichMessage"),
        0,
        "nothing is posted to the chat directly"
    );
    assert_eq!(tg.drafts(), 0);
    assert_eq!(tg.rec.count("setMessageReaction"), 0);
}

#[tokio::test]
async fn inline_query_answers_and_hints() {
    let docs = start_docsgpt().await;
    docs.on_answer(|_| (200, json!({"answer": "X is y", "conversation_id": "c"})));
    let bot = start_bot(bot_config("inline", &docs), &docs).await;
    let tg = bot.tg.clone();

    tg.push(
        "inline_query",
        json!({"id": "iq1", "from": user(5, "Ann"), "query": "what is x?", "offset": ""}),
    );
    let a = tg
        .rec
        .wait_for(
            "answerInlineQuery",
            |c| c.body["inline_query_id"] == json!("iq1"),
            WAIT,
        )
        .await;
    let results = a.body["results"].as_array().expect("results");
    assert!(!results.is_empty());
    assert_eq!(results[0]["type"], json!("article"));
    assert!(
        results[0]["input_message_content"]["message_text"]
            .as_str()
            .unwrap_or("")
            .contains("X is y"),
        "{results:?}"
    );
    let answers = docs.rec.calls("/api/answer");
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].body["question"], json!("what is x?"));
    assert_eq!(answers[0].body["api_key"], json!("key-default"));
    assert!(answers[0].body["conversation_id"].is_null());

    // No trailing "?" → a hint, and DocsGPT is not called.
    tg.push(
        "inline_query",
        json!({"id": "iq2", "from": user(5, "Ann"), "query": "what is x", "offset": ""}),
    );
    let h = tg
        .rec
        .wait_for(
            "answerInlineQuery",
            |c| c.body["inline_query_id"] == json!("iq2"),
            WAIT,
        )
        .await;
    assert_eq!(h.body["results"][0]["id"], json!("hint"));
    assert_eq!(docs.rec.count("/api/answer"), 1);
    assert_eq!(docs.rec.count("/stream"), 0);
}
