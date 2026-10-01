//! How a turn looks in Telegram: typing and 👀, live drafts in private chats,
//! then the final rich message (falling back to MarkdownV2, then plain text),
//! images and files. `docsgpt_bot::run_turn` drives it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use docsgpt_bot::docsgpt::{Download, Source};
use docsgpt_bot::markdown::strip_images;
use docsgpt_bot::{CancelGuard, Final, Progress, Surface, Turn};
use frankenstein::ParseMode;
use frankenstein::types::ChatAction;
use tokio_util::sync::CancellationToken;

use crate::app::BotContext;
use crate::telegram::api::{Target, is_bad_request};
use crate::telegram::render;
use crate::util;

/// How the answer is delivered.
#[derive(Debug, Clone)]
pub enum Delivery {
    /// Private chat: live draft, then the final message.
    Draft,
    /// Typing indicator, then the final message.
    Final,
    /// One reply through `answerGuestQuery`.
    Guest { guest_query_id: String },
}

/// One question being answered in Telegram.
pub struct TelegramSurface {
    pub ctx: Arc<BotContext>,
    pub target: Target,
    pub delivery: Delivery,
    /// The question's message (for 👀); `None` for guests.
    pub source_message_id: Option<i32>,
    guest_answered: AtomicBool,
    reacted: AtomicBool,
    /// The answer as plain text, for a voice reply after the turn.
    spoken: Mutex<Option<String>>,
}

/// State of one answer while it is being written.
pub struct TgDraft {
    draft_id: i64,
    rich_ok: bool,
    shown: bool,
    typing: CancellationToken,
    _cancel: Option<CancelGuard>,
}

const DRAFT_INTERVAL: Duration = Duration::from_millis(500);

impl TelegramSurface {
    pub fn new(
        ctx: Arc<BotContext>,
        target: Target,
        delivery: Delivery,
        source_message_id: Option<i32>,
    ) -> Self {
        Self {
            ctx,
            target,
            delivery,
            source_message_id,
            guest_answered: AtomicBool::new(false),
            reacted: AtomicBool::new(false),
            spoken: Mutex::new(None),
        }
    }

    fn is_guest(&self) -> bool {
        matches!(self.delivery, Delivery::Guest { .. })
    }

    /// Reactions aren't available in guest replies or business chats.
    fn can_react(&self) -> bool {
        self.ctx.cfg.reactions && !self.is_guest() && self.target.business_connection_id.is_none()
    }

    /// The answer as plain text, once the turn has finished with one.
    pub fn spoken_text(&self) -> Option<String> {
        self.spoken.lock().unwrap().clone()
    }

    /// Remove the 👀 reaction, if this surface added one.
    pub async fn clear_reaction(&self) {
        if self.reacted.swap(false, Ordering::SeqCst)
            && let Some(mid) = self.source_message_id
        {
            let _ = self.ctx.tg.react(self.target.chat_id, mid, None).await;
        }
    }

    fn spawn_typing(&self) -> CancellationToken {
        let token = CancellationToken::new();
        if self.is_guest() {
            return token;
        }
        let (ctx, target, t) = (self.ctx.clone(), self.target.clone(), token.clone());
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

    async fn send_plain(&self, text: &str) {
        match &self.delivery {
            Delivery::Guest { guest_query_id } => {
                if !self.guest_answered.swap(true, Ordering::SeqCst)
                    && let Err(e) = self
                        .ctx
                        .tg
                        .answer_guest_query(guest_query_id, text, None)
                        .await
                {
                    tracing::warn!(error = %e, "guest reply failed");
                }
            }
            _ => {
                if let Err(e) = self.ctx.tg.send_text(&self.target, text, None, None).await {
                    tracing::warn!(error = %e, "sending message failed");
                }
            }
        }
    }

    /// A draft: rich first, plain after Telegram rejects rich (400).
    async fn flush(&self, d: &mut TgDraft, answer: &str, status: Option<&str>) {
        let ctx = &self.ctx;
        let (chat_id, thread) = (self.target.chat_id, self.target.thread_id);
        let mut text = answer.to_string();
        if let Some(s) = status {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!("_⚙️ {s}…_"));
        }
        if text.trim().is_empty() {
            if !d.shown
                && ctx
                    .tg
                    .send_draft(chat_id, thread, d.draft_id, "", true)
                    .await
                    .is_ok()
            {
                d.shown = true;
                d.typing.cancel();
            }
            return;
        }
        if d.rich_ok {
            match ctx
                .tg
                .send_rich_draft(
                    chat_id,
                    thread,
                    d.draft_id,
                    &render::clamp_rich(&text, render::RICH_TEXT_LIMIT),
                    true,
                )
                .await
            {
                Ok(()) => {
                    d.shown = true;
                    d.typing.cancel();
                    return;
                }
                Err(e) if is_bad_request(&e) => {
                    tracing::debug!(error = %format!("{e:#}"), "rich draft rejected; using plain drafts");
                    d.rich_ok = false;
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "rich draft failed");
                    return;
                }
            }
        }
        let plain = util::truncate_chars(&render::markdown_to_plain(&text), render::TG_TEXT_LIMIT);
        match ctx
            .tg
            .send_draft(chat_id, thread, d.draft_id, &plain, true)
            .await
        {
            Ok(()) => {
                d.shown = true;
                d.typing.cancel();
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "plain draft failed"),
        }
    }

    /// Final message: rich → rich without images → MarkdownV2 → plain, then
    /// images not embedded. Returns the id of the message holding the answer.
    async fn deliver_final(
        &self,
        answer: &str,
        sources: &[Source],
        images: &[String],
    ) -> Option<i32> {
        let ctx = &self.ctx;
        let target = &self.target;
        let (text_no_images, inline_images) = strip_images(answer);
        let mut images_to_send: Vec<String> = images
            .iter()
            .filter(|u| !inline_images.contains(u))
            .cloned()
            .collect();
        let mut message_id = None;

        let rich_full = render::clamp_rich(
            &format!("{answer}{}", render::rich_sources(sources)),
            render::RICH_TEXT_LIMIT,
        );
        match ctx.tg.send_rich_markdown(target, &rich_full, None).await {
            Ok(m) => message_id = Some(m.message_id),
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "rich message rejected; trying without inline media");
                if !inline_images.is_empty() {
                    let rich_plain = render::clamp_rich(
                        &format!("{text_no_images}{}", render::rich_sources(sources)),
                        render::RICH_TEXT_LIMIT,
                    );
                    if let Ok(m) = ctx.tg.send_rich_markdown(target, &rich_plain, None).await {
                        message_id = Some(m.message_id);
                        images_to_send.extend(inline_images.iter().cloned());
                    }
                }
            }
        }

        if message_id.is_none() {
            let mut messages = render::markdown_v2_messages(&text_no_images, render::TG_TEXT_LIMIT);
            if let Some(s) = render::v2_sources(sources) {
                match messages.last_mut() {
                    Some(last)
                        if last.chars().count() + s.chars().count() + 2
                            <= render::TG_TEXT_LIMIT =>
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
                    Ok(sent) => message_id = Some(sent.message_id),
                    Err(e) if is_bad_request(&e) => {
                        tracing::debug!(error = %format!("{e:#}"), "MarkdownV2 rejected; sending plain text");
                        v2_ok = false;
                        break;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "sending answer failed");
                        return None;
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
                    match ctx.tg.send_text(target, &m, None, None).await {
                        Ok(sent) => message_id = Some(sent.message_id),
                        Err(e) => {
                            tracing::error!(error = %e, "sending plain answer failed");
                            return None;
                        }
                    }
                }
            }
            for u in &inline_images {
                if !images_to_send.contains(u) {
                    images_to_send.push(u.clone());
                }
            }
        }

        for url in images_to_send {
            if ctx.tg.send_photo_url(target, &url, None).await.is_ok() {
                continue;
            }
            match ctx.core.client.fetch_url(&url).await {
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
        message_id
    }

    async fn deliver_guest(&self, guest_query_id: &str, answer: &str, sources: &[Source]) {
        if self.guest_answered.swap(true, Ordering::SeqCst) {
            return;
        }
        let (text, _) = strip_images(answer);
        let mut messages = render::markdown_v2_messages(&text, render::TG_TEXT_LIMIT);
        if let Some(s) = render::v2_sources(sources)
            && let Some(last) = messages.last_mut()
            && last.chars().count() + s.chars().count() + 2 <= render::TG_TEXT_LIMIT
        {
            last.push_str("\n\n");
            last.push_str(&s);
        }
        let first = messages.into_iter().next().unwrap_or_default();
        if self
            .ctx
            .tg
            .answer_guest_query(guest_query_id, &first, Some(ParseMode::MarkdownV2))
            .await
            .is_ok()
        {
            return;
        }
        let plain = util::truncate_chars(&render::markdown_to_plain(&text), render::TG_TEXT_LIMIT);
        if let Err(e) = self
            .ctx
            .tg
            .answer_guest_query(guest_query_id, &plain, None)
            .await
        {
            tracing::error!(error = %e, "guest reply failed");
        }
    }
}

#[async_trait]
impl Surface for TelegramSurface {
    type Draft = TgDraft;

    async fn begin(&self, turn: &Turn) -> docsgpt_bot::Result<TgDraft> {
        let draft_id = util::new_draft_id();
        // Telegram's Stop button exists only on drafts.
        let cancel = matches!(self.delivery, Delivery::Draft).then(|| {
            let key = BotContext::cancel_key(
                self.target.chat_id,
                self.target.thread_id.unwrap_or(0),
                draft_id,
            );
            self.ctx.cancels.insert(key, turn.cancel.clone())
        });
        if self.can_react()
            && let Some(mid) = self.source_message_id
        {
            match self
                .ctx
                .tg
                .react(self.target.chat_id, mid, Some("👀"))
                .await
            {
                Ok(()) => self.reacted.store(true, Ordering::SeqCst),
                Err(e) => tracing::debug!(error = %e, "reaction failed"),
            }
        }
        Ok(TgDraft {
            draft_id,
            rich_ok: true,
            shown: false,
            typing: self.spawn_typing(),
            _cancel: cancel,
        })
    }

    async fn update(
        &self,
        _turn: &Turn,
        d: &mut TgDraft,
        p: Progress<'_>,
    ) -> docsgpt_bot::Result<()> {
        if !matches!(self.delivery, Delivery::Draft) {
            return Ok(());
        }
        if p.thinking && p.answer.is_empty() && p.status.is_none() {
            if !d.shown {
                self.flush(d, "", None).await;
            }
            return Ok(());
        }
        self.flush(d, p.answer, p.status).await;
        Ok(())
    }

    async fn finish(
        &self,
        _turn: &Turn,
        d: TgDraft,
        f: &Final,
    ) -> docsgpt_bot::Result<Option<String>> {
        d.typing.cancel();
        if f.answer.is_empty() {
            self.send_plain(&f.display_text()).await;
            return Ok(None);
        }
        // The answer as written (images embedded), plus a note if it was cut short.
        let answer = match f.note() {
            Some(n) => format!("{}\n\n{n}", f.raw.trim()),
            None => f.raw.trim().to_string(),
        };
        *self.spoken.lock().unwrap() = Some(render::markdown_to_plain(&strip_images(&answer).0));
        let message_id = match &self.delivery {
            Delivery::Guest { guest_query_id } => {
                self.deliver_guest(guest_query_id, &answer, &f.sources)
                    .await;
                None
            }
            _ => self.deliver_final(&answer, &f.sources, &f.images).await,
        };
        // Reactions (feedback) don't reach the bot from business chats.
        if self.target.business_connection_id.is_some() {
            return Ok(None);
        }
        Ok(message_id.map(|m| format!("{}:{m}", self.target.chat_id)))
    }

    async fn send_file(&self, _turn: &Turn, file: Download) -> docsgpt_bot::Result<()> {
        if self.is_guest() {
            return Ok(());
        }
        let name = util::safe_filename(&file.filename, "file");
        let tg = &self.ctx.tg;
        if file
            .mime
            .as_deref()
            .is_some_and(|m| m.starts_with("image/"))
            && tg
                .send_photo_bytes(&self.target, &name, file.bytes.clone(), Some(&name))
                .await
                .is_ok()
        {
            return Ok(());
        }
        tg.send_document(&self.target, &name, file.bytes, None)
            .await
            .map(drop)
            .map_err(|e| docsgpt_bot::Error::platform(format!("{e:#}")))
    }

    async fn notice(&self, text: &str) -> docsgpt_bot::Result<()> {
        self.send_plain(text).await;
        Ok(())
    }

    fn update_interval(&self) -> Duration {
        DRAFT_INTERVAL
    }
}
