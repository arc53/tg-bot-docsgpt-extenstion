//! The question → DocsGPT → Telegram flow: streaming drafts, final delivery,
//! sources, tool outputs (files, images), voice replies.

use anyhow::Result;
use frankenstein::ParseMode;
use frankenstein::types::{ChatAction, Message};
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use crate::app::BotContext;
use crate::config::AgentConfig;
use crate::docsgpt::{Event, Source, StreamRequest, ToolOutputs};
use crate::handlers::{groups, message};
use crate::storage::{ChatScope, ChatStatePatch, UserInfo};
use crate::telegram::api::{Target, is_bad_request};
use crate::telegram::render;
use crate::util;

#[derive(Debug, Clone)]
pub enum Delivery {
    /// Private chat: live draft, then the final message.
    Draft,
    /// Typing indicator, then the final message.
    Final,
    /// One reply through `answerGuestQuery`.
    Guest { guest_query_id: String },
}

pub struct Ask {
    pub scope: ChatScope,
    pub target: Target,
    pub user: Option<UserInfo>,
    pub question: String,
    pub attachments: Vec<String>,
    pub agent: AgentConfig,
    pub delivery: Delivery,
    pub source_message_id: Option<i32>,
    pub voice_reply: bool,
}

const DRAFT_INTERVAL: Duration = Duration::from_millis(500);
const IDLE_TIMEOUT: Duration = Duration::from_secs(150);

/// Pick the agent: `#name` prefix, then the chat's active agent, then the default.
pub async fn resolve_agent(
    ctx: &BotContext,
    scope: &ChatScope,
    text: &str,
) -> std::result::Result<(AgentConfig, String), String> {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix('#') {
        let (tag, remainder) = match rest.find(char::is_whitespace) {
            Some(i) => (&rest[..i], rest[i..].trim()),
            None => (rest, ""),
        };
        if let Some(a) = ctx.cfg.agent(tag) {
            return Ok((a.clone(), remainder.to_string()));
        }
        if ctx.cfg.agents.len() > 1 && !tag.is_empty() {
            let names = ctx
                .cfg
                .agents
                .iter()
                .map(|a| format!("#{}", a.name))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!("Unknown agent #{tag}. Available: {names}"));
        }
    }
    let st = ctx
        .app
        .storage
        .get_chat_state(scope)
        .await
        .unwrap_or_default();
    let agent = st
        .active_agent
        .as_deref()
        .and_then(|n| ctx.cfg.agent(n))
        .unwrap_or_else(|| ctx.cfg.default_agent())
        .clone();
    Ok((agent, trimmed.to_string()))
}

/// Entry point for a question that arrived as a message.
pub async fn ask_from_message(
    ctx: &Arc<BotContext>,
    msg: &Message,
    text: String,
    attachments: Vec<String>,
    user: Option<UserInfo>,
) -> Result<()> {
    let scope = message::scope_for(ctx, msg);
    let target = message::target_for(msg);
    let (agent, question) = match resolve_agent(ctx, &scope, &text).await {
        Ok(v) => v,
        Err(reply) => {
            ctx.tg.send_text(&target, &reply, None, None).await?;
            return Ok(());
        }
    };
    if question.is_empty() && attachments.is_empty() {
        return Ok(());
    }
    let private = groups::is_private(&msg.chat) && msg.business_connection_id.is_none();
    let delivery = if private && ctx.cfg.streaming {
        Delivery::Draft
    } else {
        Delivery::Final
    };
    ask(
        ctx,
        Ask {
            scope,
            target,
            user: user.or_else(|| message::user_info(msg)),
            question,
            attachments,
            agent,
            delivery,
            source_message_id: Some(msg.message_id),
            voice_reply: false,
        },
    )
    .await
}

fn spawn_typing(ctx: Arc<BotContext>, target: Target) -> CancellationToken {
    let token = CancellationToken::new();
    let t = token.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = ctx.tg.chat_action(&target, ChatAction::Typing).await {
                tracing::debug!(error = %e, "typing action failed");
                break;
            }
            tokio::select! {
                _ = t.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(4)) => {}
            }
        }
    });
    token
}

async fn wait_cancel(token: &Option<CancellationToken>) {
    match token {
        Some(t) => t.cancelled().await,
        None => std::future::pending().await,
    }
}

struct DraftState {
    draft_id: i64,
    thread: Option<i32>,
    rich_ok: bool,
    last_flush: Instant,
    shown: bool,
}

impl DraftState {
    async fn flush(
        &mut self,
        ctx: &BotContext,
        chat_id: i64,
        answer: &str,
        status: Option<&str>,
        typing: &CancellationToken,
    ) {
        let (visible, _) = render::strip_images(answer);
        let mut text = render::close_open_fence(&visible);
        if let Some(s) = status {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!("_⚙️ {s}…_"));
        }
        if text.trim().is_empty() {
            if !self.shown
                && ctx
                    .tg
                    .send_draft(chat_id, self.thread, self.draft_id, "", true)
                    .await
                    .is_ok()
            {
                self.shown = true;
                typing.cancel();
            }
            return;
        }
        if self.rich_ok {
            match ctx
                .tg
                .send_rich_draft(
                    chat_id,
                    self.thread,
                    self.draft_id,
                    &render::clamp_rich(&text, render::RICH_TEXT_LIMIT),
                    true,
                )
                .await
            {
                Ok(()) => {
                    self.shown = true;
                    typing.cancel();
                    self.last_flush = Instant::now();
                    return;
                }
                Err(e) if is_bad_request(&e) => {
                    tracing::debug!(error = %format!("{e:#}"), "rich draft rejected; using plain drafts");
                    self.rich_ok = false;
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "rich draft failed");
                    self.last_flush = Instant::now();
                    return;
                }
            }
        }
        let plain = util::truncate_chars(&render::markdown_to_plain(&text), render::TG_TEXT_LIMIT);
        match ctx
            .tg
            .send_draft(chat_id, self.thread, self.draft_id, &plain, true)
            .await
        {
            Ok(()) => {
                self.shown = true;
                typing.cancel();
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "plain draft failed"),
        }
        self.last_flush = Instant::now();
    }
}

/// Run one turn end to end.
pub async fn ask(ctx: &Arc<BotContext>, a: Ask) -> Result<()> {
    let _guard = ctx.lock_scope(&a.scope.key()).await;
    let is_guest = matches!(a.delivery, Delivery::Guest { .. });
    let chat_id = a.target.chat_id;

    let _ = ctx
        .app
        .storage
        .update_chat_state(
            &a.scope,
            ChatStatePatch {
                last_question: Some(a.question.clone()),
                user: a.user.clone(),
                ..Default::default()
            },
        )
        .await;
    // Reactions are not available for guest replies or business chats.
    let can_react = ctx.cfg.reactions && !is_guest && a.target.business_connection_id.is_none();
    if can_react
        && let Some(mid) = a.source_message_id
        && let Err(e) = ctx.tg.react(chat_id, mid, Some("👀")).await
    {
        tracing::debug!(error = %e, "reaction failed");
    }

    let conversation_id = ctx
        .app
        .storage
        .get_conversation(&a.scope, &a.agent.name)
        .await
        .unwrap_or(None);
    let typing = if is_guest {
        CancellationToken::new()
    } else {
        spawn_typing(ctx.clone(), a.target.clone())
    };

    let req = StreamRequest {
        question: a.question.clone(),
        api_key: a.agent.api_key.clone(),
        conversation_id: conversation_id.clone(),
        attachments: a.attachments.clone(),
    };
    tracing::info!(bot = %ctx.cfg.name, chat = chat_id, agent = %a.agent.name, conversation = ?conversation_id, attachments = a.attachments.len(), "asking DocsGPT");
    let mut stream = match ctx.docsgpt.stream(&req).await {
        Ok(s) => s,
        Err(e) => {
            typing.cancel();
            tracing::error!(error = %e, "DocsGPT stream failed to open");
            deliver_error(ctx, &a, "The assistant could not be reached.").await;
            clear_reaction(ctx, &a).await;
            return Ok(());
        }
    };

    let mut answer = String::new();
    let mut sources: Vec<Source> = Vec::new();
    let mut outputs = ToolOutputs::default();
    let mut new_conversation = conversation_id.clone();
    let mut error: Option<String> = None;
    let mut status: Option<String> = None;
    let mut stopped = false;
    let mut dirty = false;

    let thread = a.target.thread_id;
    let mut draft = if matches!(a.delivery, Delivery::Draft) {
        Some(DraftState {
            draft_id: util::new_draft_id(),
            thread,
            rich_ok: true,
            last_flush: Instant::now() - DRAFT_INTERVAL,
            shown: false,
        })
    } else {
        None
    };
    let cancel = match &draft {
        Some(d) => Some(
            ctx.register_cancel(chat_id, thread.unwrap_or(0), d.draft_id)
                .await,
        ),
        None => None,
    };

    loop {
        let next = tokio::time::timeout(IDLE_TIMEOUT, async {
            tokio::select! {
                ev = stream.next() => Some(ev),
                _ = wait_cancel(&cancel) => None,
            }
        })
        .await;
        let mut force_flush = false;
        match next {
            Err(_) => {
                error = Some("Timed out waiting for the assistant.".into());
                break;
            }
            Ok(None) => {
                stopped = true;
                break;
            }
            Ok(Some(None)) => break,
            Ok(Some(Some(Err(e)))) => {
                tracing::warn!(error = %e, "stream error");
                error = Some("The connection to the assistant dropped.".into());
                break;
            }
            Ok(Some(Some(Ok(ev)))) => match ev {
                Event::Answer(d) => {
                    answer.push_str(&d);
                    dirty = true;
                }
                Event::Thought(_) => {
                    if let Some(d) = &mut draft
                        && answer.is_empty()
                        && !d.shown
                    {
                        d.flush(ctx, chat_id, "", None, &typing).await;
                    }
                }
                Event::ToolCall(tc) => {
                    if tc.is_completed() || tc.status == "error" || tc.status == "denied" {
                        outputs.merge(tc.outputs());
                        status = None;
                    } else {
                        status = Some(tc.label());
                    }
                    dirty = true;
                    force_flush = true;
                }
                Event::ToolCalls(list) => {
                    for tc in list {
                        outputs.merge(tc.outputs());
                    }
                    status = None;
                }
                Event::Source(s) => sources = s,
                Event::MessageId {
                    conversation_id: Some(c),
                    ..
                } => new_conversation = Some(c),
                Event::MessageId { .. } => {}
                Event::ConversationId(c) => new_conversation = Some(c),
                Event::StructuredAnswer(s) => {
                    if answer.trim().is_empty() {
                        answer = s;
                    }
                }
                Event::Notice(n) => tracing::info!(notice = %n, "DocsGPT notice"),
                Event::Error(e) => {
                    tracing::warn!(error = %e, "DocsGPT error event");
                    error = Some(e);
                    break;
                }
                Event::End => break,
                Event::Other(t, _) => tracing::debug!(kind = %t, "unhandled DocsGPT event"),
            },
        }
        if let Some(d) = &mut draft
            && dirty
            && (force_flush || d.last_flush.elapsed() >= DRAFT_INTERVAL)
        {
            d.flush(ctx, chat_id, &answer, status.as_deref(), &typing)
                .await;
            dirty = false;
        }
    }
    drop(stream);
    typing.cancel();
    if let Some(d) = &draft {
        ctx.unregister_cancel(chat_id, thread.unwrap_or(0), d.draft_id)
            .await;
    }

    if let Some(c) = &new_conversation
        && Some(c) != conversation_id.as_ref()
        && let Err(e) = ctx
            .app
            .storage
            .set_conversation(&a.scope, &a.agent.name, c)
            .await
    {
        tracing::warn!(error = %e, "could not persist conversation id");
    }

    let answer = answer.trim().to_string();
    if answer.is_empty() {
        if stopped {
            deliver_text(ctx, &a, "Stopped.").await;
        } else {
            let detail = error
                .clone()
                .unwrap_or_else(|| "No answer was produced.".into());
            tracing::warn!(bot = %ctx.cfg.name, chat = chat_id, %detail, "empty answer");
            deliver_error(ctx, &a, &detail).await;
        }
        clear_reaction(ctx, &a).await;
        return Ok(());
    }
    tracing::info!(bot = %ctx.cfg.name, chat = chat_id, chars = answer.chars().count(), sources = sources.len(), artifacts = outputs.artifacts.len(), images = outputs.image_urls.len(), stopped, "answer ready");

    match &a.delivery {
        Delivery::Guest { guest_query_id } => {
            deliver_guest(ctx, guest_query_id, &answer, &sources).await
        }
        _ => {
            deliver_final(
                ctx,
                &a,
                &answer,
                &sources,
                &outputs,
                new_conversation.as_deref(),
            )
            .await
        }
    }

    if a.voice_reply && !is_guest {
        let spoken = util::truncate_chars(&render::markdown_to_plain(&answer), 1500);
        match ctx.docsgpt.tts(&spoken).await {
            Ok(audio) => {
                if let Err(e) = ctx
                    .tg
                    .send_voice_bytes(&a.target, "answer.mp3", audio)
                    .await
                {
                    tracing::warn!(error = %e, "voice reply failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "text-to-speech failed"),
        }
    }
    clear_reaction(ctx, &a).await;
    Ok(())
}

async fn clear_reaction(ctx: &BotContext, a: &Ask) {
    if ctx.cfg.reactions
        && !matches!(a.delivery, Delivery::Guest { .. })
        && a.target.business_connection_id.is_none()
        && let Some(mid) = a.source_message_id
    {
        let _ = ctx.tg.react(a.target.chat_id, mid, None).await;
    }
}

async fn deliver_text(ctx: &BotContext, a: &Ask, text: &str) {
    match &a.delivery {
        Delivery::Guest { guest_query_id } => {
            let _ = ctx.tg.answer_guest_query(guest_query_id, text, None).await;
        }
        _ => {
            let _ = ctx.tg.send_text(&a.target, text, None, None).await;
        }
    }
}

async fn deliver_error(ctx: &BotContext, a: &Ask, detail: &str) {
    let detail = util::truncate_chars(detail, 300);
    deliver_text(
        ctx,
        a,
        &format!("Sorry, I couldn't get an answer right now.\n\n{detail}"),
    )
    .await;
}

/// Final message: rich → MarkdownV2 → plain, then images and files.
async fn deliver_final(
    ctx: &BotContext,
    a: &Ask,
    answer: &str,
    sources: &[Source],
    outputs: &ToolOutputs,
    conversation_id: Option<&str>,
) {
    let target = &a.target;
    let (text_no_images, inline_images) = render::strip_images(answer);
    let mut images_to_send: Vec<String> = outputs
        .image_urls
        .iter()
        .filter(|u| !inline_images.contains(u))
        .cloned()
        .collect();

    let mut sent_rich = false;
    let rich_full = render::clamp_rich(
        &format!("{answer}{}", render::rich_sources(sources)),
        render::RICH_TEXT_LIMIT,
    );
    match ctx.tg.send_rich_markdown(target, &rich_full, None).await {
        Ok(_) => sent_rich = true,
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "rich message rejected; trying without inline media");
            if !inline_images.is_empty() {
                let rich_plain = render::clamp_rich(
                    &format!("{text_no_images}{}", render::rich_sources(sources)),
                    render::RICH_TEXT_LIMIT,
                );
                if ctx
                    .tg
                    .send_rich_markdown(target, &rich_plain, None)
                    .await
                    .is_ok()
                {
                    sent_rich = true;
                    images_to_send.extend(inline_images.iter().cloned());
                }
            }
        }
    }

    if !sent_rich {
        let mut messages = render::markdown_v2_messages(&text_no_images, render::TG_TEXT_LIMIT);
        if let Some(s) = render::v2_sources(sources) {
            match messages.last_mut() {
                Some(last)
                    if last.chars().count() + s.chars().count() + 2 <= render::TG_TEXT_LIMIT =>
                {
                    last.push_str("\n\n");
                    last.push_str(&s);
                }
                _ => messages.push(s),
            }
        }
        let mut v2_ok = true;
        for m in &messages {
            match ctx
                .tg
                .send_text(target, m, Some(ParseMode::MarkdownV2), None)
                .await
            {
                Ok(_) => {}
                Err(e) if is_bad_request(&e) => {
                    tracing::debug!(error = %format!("{e:#}"), "MarkdownV2 rejected; sending plain text");
                    v2_ok = false;
                    break;
                }
                Err(e) => {
                    tracing::error!(error = %e, "sending answer failed");
                    return;
                }
            }
        }
        if !v2_ok {
            let mut plain = render::markdown_to_plain(&text_no_images);
            if let Some(s) = render::plain_sources(sources) {
                plain.push_str("\n\n");
                plain.push_str(&s);
            }
            for m in render::plain_messages(&plain, render::TG_TEXT_LIMIT) {
                if let Err(e) = ctx.tg.send_text(target, &m, None, None).await {
                    tracing::error!(error = %e, "sending plain answer failed");
                    return;
                }
            }
        }
        let extra: Vec<String> = inline_images
            .iter()
            .filter(|u| !images_to_send.contains(u))
            .cloned()
            .collect();
        images_to_send.extend(extra);
    }

    for url in images_to_send {
        if ctx.tg.send_photo_url(target, &url, None).await.is_ok() {
            continue;
        }
        match ctx.docsgpt.fetch_url(&url).await {
            Ok(d) => {
                let name = util::safe_filename(&d.filename, "image.png");
                if ctx
                    .tg
                    .send_photo_bytes(target, &name, d.bytes.clone(), None)
                    .await
                    .is_err()
                    && let Err(e) = ctx.tg.send_document(target, &name, d.bytes, None).await
                {
                    tracing::warn!(error = %e, url, "could not deliver image");
                }
            }
            Err(e) => tracing::warn!(error = %e, url, "could not fetch image"),
        }
    }

    for art in &outputs.artifacts {
        let Some(conv) = conversation_id else {
            tracing::warn!(artifact = %art.id, "no conversation id; cannot download artifact");
            continue;
        };
        match ctx
            .docsgpt
            .download_artifact(&a.agent.api_key, conv, &art.id, &art.filename)
            .await
        {
            Ok(d) => {
                let name = util::safe_filename(
                    if d.filename.is_empty() {
                        &art.filename
                    } else {
                        &d.filename
                    },
                    "file",
                );
                let is_image = d
                    .mime
                    .as_deref()
                    .or(art.mime_type.as_deref())
                    .is_some_and(|m| m.starts_with("image/"));
                let sent = if is_image {
                    ctx.tg
                        .send_photo_bytes(target, &name, d.bytes.clone(), Some(&name))
                        .await
                        .is_ok()
                } else {
                    false
                };
                if !sent && let Err(e) = ctx.tg.send_document(target, &name, d.bytes, None).await {
                    tracing::warn!(error = %e, artifact = %art.id, "could not send artifact");
                    let _ = ctx
                        .tg
                        .send_text(
                            target,
                            &format!("I created {name} but couldn't send it here."),
                            None,
                            None,
                        )
                        .await;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, artifact = %art.id, "artifact download failed");
                let _ = ctx
                    .tg
                    .send_text(
                        target,
                        &format!(
                            "I created {} but couldn't fetch it: {}",
                            art.filename,
                            util::truncate_chars(&e.to_string(), 120)
                        ),
                        None,
                        None,
                    )
                    .await;
            }
        }
    }
}

async fn deliver_guest(ctx: &BotContext, guest_query_id: &str, answer: &str, sources: &[Source]) {
    let (text, _) = render::strip_images(answer);
    let mut messages = render::markdown_v2_messages(&text, render::TG_TEXT_LIMIT);
    if let Some(s) = render::v2_sources(sources)
        && let Some(last) = messages.last_mut()
        && last.chars().count() + s.chars().count() + 2 <= render::TG_TEXT_LIMIT
    {
        last.push_str("\n\n");
        last.push_str(&s);
    }
    let first = messages.into_iter().next().unwrap_or_default();
    if ctx
        .tg
        .answer_guest_query(guest_query_id, &first, Some(ParseMode::MarkdownV2))
        .await
        .is_ok()
    {
        return;
    }
    let plain = util::truncate_chars(&render::markdown_to_plain(&text), render::TG_TEXT_LIMIT);
    if let Err(e) = ctx
        .tg
        .answer_guest_query(guest_query_id, &plain, None)
        .await
    {
        tracing::error!(error = %e, "guest reply failed");
    }
}
