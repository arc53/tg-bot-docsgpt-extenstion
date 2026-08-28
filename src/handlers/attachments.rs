//! Photos, documents and voice notes → DocsGPT attachments / speech-to-text.

use anyhow::{Context, Result};
use frankenstein::types::{ChatAction, Message};
use std::sync::Arc;
use std::time::Duration;

use crate::app::BotContext;
use crate::handlers::{chat, groups, message};
use crate::util;

pub fn has_media(msg: &Message) -> bool {
    msg.photo.is_some()
        || msg.document.is_some()
        || msg.voice.is_some()
        || msg.audio.is_some()
        || msg.video_note.is_some()
        || msg.video.is_some()
        || msg.animation.is_some()
}

fn is_audio(msg: &Message) -> bool {
    msg.voice.is_some() || msg.audio.is_some()
}

pub async fn handle(ctx: &Arc<BotContext>, msg: Message, text: String) -> Result<()> {
    if !ctx.cfg.attachments {
        if groups::is_private(&msg.chat) {
            ctx.tg
                .send_text(
                    &message::target_for(&msg),
                    "Attachments are disabled for this bot.",
                    None,
                    None,
                )
                .await?;
        }
        return Ok(());
    }
    if let Some(gid) = msg.media_group_id.clone() {
        let first = {
            let mut map = ctx.media_groups.lock().await;
            let entry = map.entry(gid.clone()).or_default();
            entry.push(msg);
            entry.len() == 1
        };
        if !first {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let batch = ctx
            .media_groups
            .lock()
            .await
            .remove(&gid)
            .unwrap_or_default();
        let caption = batch
            .iter()
            .find_map(|m| m.caption.clone())
            .map(|c| groups::strip_mention(&c, ctx.username()))
            .unwrap_or(text);
        return process(ctx, batch, caption).await;
    }
    process(ctx, vec![msg], text).await
}

async fn process(ctx: &Arc<BotContext>, batch: Vec<Message>, caption: String) -> Result<()> {
    let first = batch.first().context("empty media batch")?.clone();
    let target = message::target_for(&first);
    let scope = message::scope_for(ctx, &first);
    let max_bytes = ctx.cfg.max_file_mb * 1024 * 1024;

    let (agent, caption) = match chat::resolve_agent(ctx, &scope, &caption).await {
        Ok(v) => v,
        Err(reply) => {
            ctx.tg.send_text(&target, &reply, None, None).await?;
            return Ok(());
        }
    };

    // Voice / audio → transcript → question.
    if batch.len() == 1 && is_audio(&first) {
        let (file_id, filename, mime) = if let Some(v) = &first.voice {
            (
                v.file_id.clone(),
                "voice.ogg".to_string(),
                Some("audio/ogg".to_string()),
            )
        } else {
            let a = first.audio.as_ref().unwrap();
            let name = a.file_name.clone().unwrap_or_else(|| "audio.mp3".into());
            (a.file_id.clone(), name, a.mime_type.clone())
        };
        let (bytes, _) = match ctx.tg.download_file(&file_id, max_bytes).await {
            Ok(v) => v,
            Err(e) => {
                ctx.tg
                    .send_text(
                        &target,
                        &format!("I couldn't download that audio: {e}"),
                        None,
                        None,
                    )
                    .await?;
                return Ok(());
            }
        };
        let filename = normalize_audio_name(&filename, mime.as_deref());
        let private = groups::is_private(&first.chat) && first.business_connection_id.is_none();
        let delivery = if private && ctx.cfg.streaming {
            chat::Delivery::Draft
        } else {
            chat::Delivery::Final
        };
        // Fast path: transcribe, then ask the transcript as a normal question.
        // Fallback: DocsGPT's attachment pipeline also understands audio.
        let (question, attachments) = match ctx
            .docsgpt
            .stt(&agent.api_key, &filename, bytes.clone(), mime.as_deref())
            .await
        {
            Ok(transcript) => (
                if caption.is_empty() {
                    transcript
                } else {
                    format!("{caption}\n\n{transcript}")
                },
                vec![],
            ),
            Err(e) => {
                tracing::warn!(error = %e, "speech-to-text failed; sending the audio as an attachment instead");
                match ctx
                    .docsgpt
                    .store_attachment(&agent.api_key, &filename, bytes, mime.as_deref())
                    .await
                {
                    Ok(stored) => {
                        if let Some(task) = &stored.task_id {
                            ctx.docsgpt
                                .wait_for_task(task, Duration::from_secs(60))
                                .await
                                .unwrap_or_else(|e| tracing::warn!(error = %e, "attachment task"));
                        }
                        let q = if caption.is_empty() {
                            "Please answer the question in the attached voice message.".to_string()
                        } else {
                            format!("{caption}\n\n(See the attached voice message.)")
                        };
                        (q, vec![stored.attachment_id])
                    }
                    Err(e2) => {
                        tracing::warn!(error = %e2, "audio attachment upload failed");
                        ctx.tg
                            .send_text(
                                &target,
                                "I couldn't understand that audio. Could you type it instead?",
                                None,
                                None,
                            )
                            .await?;
                        return Ok(());
                    }
                }
            }
        };
        return chat::ask(
            ctx,
            chat::Ask {
                scope,
                target,
                user: message::user_info(&first),
                question,
                attachments,
                agent,
                delivery,
                source_message_id: Some(first.message_id),
                voice_reply: ctx.cfg.voice_replies,
            },
        )
        .await;
    }

    let _ = ctx
        .tg
        .chat_action(&target, ChatAction::UploadDocument)
        .await;
    let mut ids = Vec::new();
    let mut names = Vec::new();
    let mut skipped = Vec::new();
    for m in &batch {
        let Some((file_id, filename, mime)) = pick_file(m) else {
            skipped.push(describe(m));
            continue;
        };
        let (bytes, path) = match ctx.tg.download_file(&file_id, max_bytes).await {
            Ok(v) => v,
            Err(e) => {
                skipped.push(format!("{filename} ({e})"));
                continue;
            }
        };
        let filename = ensure_extension(&filename, &path);
        let mime = mime.or_else(|| {
            mime_guess::from_path(&filename)
                .first_raw()
                .map(str::to_string)
        });
        match ctx
            .docsgpt
            .store_attachment(&agent.api_key, &filename, bytes, mime.as_deref())
            .await
        {
            Ok(stored) => {
                if let Some(task) = &stored.task_id {
                    ctx.docsgpt
                        .wait_for_task(task, Duration::from_secs(120))
                        .await
                        .unwrap_or_else(|e| tracing::warn!(error = %e, "attachment task"));
                }
                ids.push(stored.attachment_id);
                names.push(filename);
            }
            Err(e) => {
                tracing::warn!(error = %e, "attachment upload failed");
                skipped.push(format!("{filename} (upload failed)"));
            }
        }
    }
    if ids.is_empty() {
        let why = if skipped.is_empty() {
            "no supported file found".to_string()
        } else {
            skipped.join(", ")
        };
        ctx.tg
            .send_text(
                &target,
                &format!("I couldn't use that attachment: {why}"),
                None,
                None,
            )
            .await?;
        return Ok(());
    }
    if !skipped.is_empty() {
        let _ = ctx
            .tg
            .send_text(
                &target,
                &format!("Skipped: {}", skipped.join(", ")),
                None,
                None,
            )
            .await;
    }
    let question = if caption.is_empty() {
        if names.len() == 1 {
            format!(
                "I've attached {}. Please review it and summarize the key points.",
                names[0]
            )
        } else {
            format!(
                "I've attached {} files: {}. Please review them and summarize the key points.",
                names.len(),
                names.join(", ")
            )
        }
    } else {
        caption
    };
    let private = groups::is_private(&first.chat) && first.business_connection_id.is_none();
    let delivery = if private && ctx.cfg.streaming {
        chat::Delivery::Draft
    } else {
        chat::Delivery::Final
    };
    chat::ask(
        ctx,
        chat::Ask {
            scope,
            target,
            user: message::user_info(&first),
            question,
            attachments: ids,
            agent,
            delivery,
            source_message_id: Some(first.message_id),
            voice_reply: false,
        },
    )
    .await
}

fn describe(m: &Message) -> String {
    if m.video.is_some() {
        "video (not supported)".into()
    } else if m.video_note.is_some() {
        "video note (not supported)".into()
    } else if m.animation.is_some() {
        "animation (not supported)".into()
    } else {
        "unsupported media".into()
    }
}

/// `(file_id, filename, mime)` for photos and documents.
fn pick_file(m: &Message) -> Option<(String, String, Option<String>)> {
    if let Some(sizes) = &m.photo {
        let best = sizes
            .iter()
            .max_by_key(|p| (p.file_size.unwrap_or(0), p.width))?;
        return Some((
            best.file_id.clone(),
            format!("photo_{}.jpg", &best.file_unique_id),
            Some("image/jpeg".into()),
        ));
    }
    if let Some(d) = &m.document {
        let name = d
            .file_name
            .clone()
            .unwrap_or_else(|| format!("document_{}", d.file_unique_id));
        return Some((
            d.file_id.clone(),
            util::safe_filename(&name, "document"),
            d.mime_type.clone(),
        ));
    }
    None
}

fn ensure_extension(filename: &str, tg_path: &str) -> String {
    if filename.contains('.') {
        return filename.to_string();
    }
    match tg_path.rsplit_once('.') {
        Some((_, ext))
            if !ext.is_empty()
                && ext.len() <= 5
                && ext.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            format!("{filename}.{ext}")
        }
        _ => filename.to_string(),
    }
}

/// DocsGPT accepts .wav .mp3 .m4a .ogg .webm; Telegram voice notes are Opus in OGG.
fn normalize_audio_name(name: &str, mime: Option<&str>) -> String {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".oga") || lower.ends_with(".opus") {
        return format!(
            "{}.ogg",
            name.rsplit_once('.').map(|(a, _)| a).unwrap_or(name)
        );
    }
    if !lower.contains('.') {
        let ext = match mime {
            Some("audio/mpeg") | Some("audio/mp3") => "mp3",
            Some("audio/mp4") | Some("audio/x-m4a") => "m4a",
            Some("audio/wav") | Some("audio/x-wav") => "wav",
            Some("audio/webm") | Some("video/webm") => "webm",
            _ => "ogg",
        };
        return format!("{name}.{ext}");
    }
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_names() {
        assert_eq!(normalize_audio_name("voice.oga", None), "voice.ogg");
        assert_eq!(normalize_audio_name("clip", Some("audio/mpeg")), "clip.mp3");
        assert_eq!(normalize_audio_name("a.m4a", None), "a.m4a");
    }

    #[test]
    fn extension_from_path() {
        assert_eq!(
            ensure_extension("photo_x", "photos/file_1.jpg"),
            "photo_x.jpg"
        );
        assert_eq!(
            ensure_extension("report.pdf", "documents/file_2.pdf"),
            "report.pdf"
        );
    }
}
