//! Live checks against a real DocsGPT deployment. They are ignored by default:
//!
//! ```text
//! DOCSGPT_LIVE_KEY=<agent api key> cargo test --test docsgpt_live -- --ignored --nocapture
//! ```
//! Optional: `DOCSGPT_LIVE_BASE` (default https://gptcloud.arc53.com).

use bytes::Bytes;
use docsgpt_telegram::docsgpt::{DocsGpt, Event, StreamRequest, ToolOutputs};
use futures_util::StreamExt;
use std::time::Duration;

fn live() -> Option<(DocsGpt, String)> {
    let key = std::env::var("DOCSGPT_LIVE_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())?;
    let base =
        std::env::var("DOCSGPT_LIVE_BASE").unwrap_or_else(|_| "https://gptcloud.arc53.com".into());
    Some((DocsGpt::new(reqwest::Client::new(), &base), key))
}

struct Turn {
    answer: String,
    conversation_id: Option<String>,
    outputs: ToolOutputs,
    error: Option<String>,
}

async fn run_turn(client: &DocsGpt, req: &StreamRequest) -> Turn {
    let mut stream = client.stream(req).await.expect("open stream");
    let mut turn = Turn {
        answer: String::new(),
        conversation_id: None,
        outputs: ToolOutputs::default(),
        error: None,
    };
    while let Some(ev) = stream.next().await {
        match ev.expect("stream item") {
            Event::Answer(d) => turn.answer.push_str(&d),
            Event::MessageId {
                conversation_id: Some(c),
                ..
            }
            | Event::ConversationId(c) => turn.conversation_id = Some(c),
            Event::ToolCall(tc) if tc.is_completed() => turn.outputs.merge(tc.outputs()),
            Event::ToolCalls(list) => list
                .into_iter()
                .for_each(|tc| turn.outputs.merge(tc.outputs())),
            Event::Error(e) => turn.error = Some(e),
            Event::End => break,
            _ => {}
        }
    }
    turn
}

#[tokio::test]
#[ignore]
async fn stream_multi_turn() {
    let Some((client, key)) = live() else { return };
    let t1 = run_turn(
        &client,
        &StreamRequest {
            question: "My favourite colour is teal. Reply with just OK.".into(),
            api_key: key.clone(),
            ..Default::default()
        },
    )
    .await;
    assert!(t1.error.is_none(), "{:?}", t1.error);
    let conv = t1.conversation_id.expect("conversation id");
    let t2 = run_turn(
        &client,
        &StreamRequest {
            question: "What is my favourite colour? One word.".into(),
            api_key: key,
            conversation_id: Some(conv),
            ..Default::default()
        },
    )
    .await;
    println!("turn 2: {}", t2.answer);
    assert!(t2.answer.to_lowercase().contains("teal"));
}

#[tokio::test]
#[ignore]
async fn attachment_roundtrip() {
    let Some((client, key)) = live() else { return };
    let content = "Internal memo.\nThe secret launch code is 4471.\nDo not share.\n";
    let stored = client
        .store_attachment(
            &key,
            "memo.txt",
            Bytes::from_static(content.as_bytes()),
            Some("text/plain"),
        )
        .await
        .expect("upload");
    println!(
        "attachment {} task {:?}",
        stored.attachment_id, stored.task_id
    );
    if let Some(task) = &stored.task_id {
        client
            .wait_for_task(task, Duration::from_secs(120))
            .await
            .expect("task");
    }
    let turn = run_turn(
        &client,
        &StreamRequest {
            question:
                "What is the secret launch code in the attached memo? Reply with just the number."
                    .into(),
            api_key: key,
            conversation_id: None,
            attachments: vec![stored.attachment_id],
        },
    )
    .await;
    println!("answer: {}", turn.answer);
    assert!(
        turn.answer.contains("4471"),
        "answer did not use the attachment: {}",
        turn.answer
    );
}

#[tokio::test]
#[ignore]
async fn artifact_download() {
    let Some((client, key)) = live() else { return };
    let turn = run_turn(
        &client,
        &StreamRequest {
            question: "Use your code execution tool to write a file named numbers.txt containing the numbers 1 to 5, one per line (pass outputs=['numbers.txt']). Then reply with just: done".into(),
            api_key: key.clone(),
            ..Default::default()
        },
    )
    .await;
    println!(
        "answer: {} | artifacts: {:?}",
        turn.answer, turn.outputs.artifacts
    );
    let conv = turn.conversation_id.expect("conversation id");
    let art = turn
        .outputs
        .artifacts
        .first()
        .expect("an artifact was produced");
    let d = client
        .download_artifact(&key, &conv, &art.id, &art.filename)
        .await
        .expect("download");
    println!(
        "downloaded {} ({} bytes, mime {:?})",
        d.filename,
        d.bytes.len(),
        d.mime
    );
    let text = String::from_utf8_lossy(&d.bytes);
    assert!(
        text.contains('1') && text.contains('5'),
        "unexpected artifact content: {text}"
    );
    assert!(d.filename.ends_with(".txt"));
}

#[tokio::test]
#[ignore]
async fn tts_then_stt() {
    let Some((client, key)) = live() else { return };
    let audio = client
        .tts("Hello from the Telegram integration test. The magic word is pineapple.")
        .await
        .expect("tts");
    println!("tts bytes: {}", audio.len());
    assert!(audio.len() > 1000);
    let text = client
        .stt(&key, "speech.mp3", audio, Some("audio/mpeg"))
        .await
        .expect("stt");
    println!("stt: {text}");
    assert!(text.to_lowercase().contains("pineapple"));
}

/// Voice notes fall back to the attachment pipeline when `/api/stt` is unavailable;
/// this checks that an OGG/Opus upload is understood by the agent.
#[tokio::test]
#[ignore]
async fn audio_attachment_roundtrip() {
    let Some((client, key)) = live() else { return };
    let path = std::env::var("DOCSGPT_LIVE_AUDIO").unwrap_or_default();
    if path.is_empty() {
        eprintln!("DOCSGPT_LIVE_AUDIO not set (path to an .ogg/.mp3 speech sample); skipping");
        return;
    }
    let bytes = Bytes::from(std::fs::read(&path).expect("read audio sample"));
    let name = std::path::Path::new(&path)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let mime = if name.ends_with(".ogg") {
        "audio/ogg"
    } else {
        "audio/mpeg"
    };
    let stored = client
        .store_attachment(&key, &name, bytes, Some(mime))
        .await
        .expect("upload audio");
    if let Some(task) = &stored.task_id {
        client
            .wait_for_task(task, Duration::from_secs(60))
            .await
            .expect("task");
    }
    let turn = run_turn(
        &client,
        &StreamRequest {
            question: "What does the attached voice message say? Quote it.".into(),
            api_key: key,
            conversation_id: None,
            attachments: vec![stored.attachment_id],
        },
    )
    .await;
    println!("answer: {}", turn.answer);
    assert!(!turn.answer.is_empty());
}
