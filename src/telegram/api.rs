//! Thin wrapper around frankenstein with retries, rate limiting, uploads from
//! memory, and raw calls for fields newer than the pinned crate.

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use frankenstein::AsyncTelegramApi;
use frankenstein::ParseMode;
use frankenstein::client_reqwest::Bot;
use frankenstein::inline_mode::{
    InlineQueryResult, InlineQueryResultArticle, InputMessageContent, InputTextMessageContent,
};
use frankenstein::input_file::{FileUpload, InputFile};
use frankenstein::methods::*;
use frankenstein::response::{ErrorResponse, MethodResponse};
use frankenstein::types::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::future::Future;
use std::time::Duration;

use super::ratelimit::{Kind, RateLimiter};
use super::raw;

pub struct Tg {
    pub bot: Bot,
    pub http: reqwest::Client,
    token: String,
    pub name: String,
    pub limiter: RateLimiter,
    /// Bot API root, e.g. `https://api.telegram.org` (override with `TELEGRAM_API_URL`).
    api_root: String,
}

/// Bot API root: `TELEGRAM_API_URL` lets you point at a local Bot API server or a mock.
pub fn api_root() -> String {
    std::env::var("TELEGRAM_API_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| "https://api.telegram.org".to_string())
}

/// Where a reply goes.
#[derive(Debug, Clone, Default)]
pub struct Target {
    pub chat_id: i64,
    pub thread_id: Option<i32>,
    pub business_connection_id: Option<String>,
    pub reply_to: Option<i32>,
    /// Deliver only to this user (groups), optionally tied to a callback query.
    pub ephemeral: Option<raw::EphemeralMessageParameters>,
}

impl Target {
    pub fn chat(chat_id: i64) -> Self {
        Self {
            chat_id,
            ..Default::default()
        }
    }
    fn reply_parameters(&self) -> Option<ReplyParameters> {
        self.reply_to.map(|id| {
            ReplyParameters::builder()
                .message_id(id)
                .allow_sending_without_reply(true)
                .build()
        })
    }
}

/// Find the Telegram API error inside an `anyhow` chain.
pub fn api_error(e: &anyhow::Error) -> Option<&ErrorResponse> {
    e.chain()
        .find_map(|c| match c.downcast_ref::<frankenstein::Error>() {
            Some(frankenstein::Error::Api(r)) => Some(r),
            _ => None,
        })
}

pub fn is_bad_request(e: &anyhow::Error) -> bool {
    api_error(e).is_some_and(|r| r.error_code == 400)
}

impl Tg {
    pub fn new(http: reqwest::Client, token: &str, name: &str) -> Self {
        let api_root = api_root();
        let bot = Bot::builder()
            .api_url(format!("{api_root}/bot{token}"))
            .client(http.clone())
            .build();
        Self {
            bot,
            http,
            token: token.to_string(),
            name: name.to_string(),
            limiter: RateLimiter::new(),
            api_root,
        }
    }

    /// Run a Telegram call with retries for flood limits and transient errors.
    async fn retrying<T, F, Fut>(&self, what: &str, mut f: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = std::result::Result<MethodResponse<T>, frankenstein::Error>>,
    {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match f().await {
                Ok(resp) => return Ok(resp.result),
                Err(frankenstein::Error::Api(r)) if r.error_code == 429 && attempt <= 3 => {
                    let wait = r
                        .parameters
                        .as_ref()
                        .and_then(|p| p.retry_after)
                        .unwrap_or(2) as u64;
                    tracing::warn!(bot = %self.name, what, wait, "rate limited by Telegram; retrying");
                    tokio::time::sleep(Duration::from_secs(wait.min(30))).await;
                }
                Err(frankenstein::Error::Api(r)) if r.error_code >= 500 && attempt <= 3 => {
                    tracing::warn!(bot = %self.name, what, code = r.error_code, "Telegram server error; retrying");
                    tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
                }
                Err(frankenstein::Error::HttpReqwest(e))
                    if attempt <= 3 && (e.is_connect() || e.is_timeout() || e.is_request()) =>
                {
                    tracing::warn!(bot = %self.name, what, error = %e, "network error talking to Telegram; retrying");
                    tokio::time::sleep(Duration::from_millis(700 * attempt as u64)).await;
                }
                Err(e) => return Err(anyhow::Error::from(e).context(format!("Telegram {what}"))),
            }
        }
    }

    /// Raw method call with JSON params.
    pub async fn call<P: Serialize + std::fmt::Debug + Send + Sync + Clone, T: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<T> {
        self.retrying(method, || {
            let p = params.clone();
            async move {
                self.bot
                    .request::<P, MethodResponse<T>>(method, Some(p))
                    .await
            }
        })
        .await
    }

    pub async fn get_me(&self) -> Result<User> {
        self.retrying("getMe", || self.bot.get_me()).await
    }

    pub async fn get_updates_raw(
        &self,
        offset: Option<i64>,
        timeout_secs: u32,
    ) -> Result<Vec<Value>> {
        let params = json!({
            "offset": offset,
            "timeout": timeout_secs,
            "limit": 100,
            "allowed_updates": raw::ALLOWED_UPDATES,
        });
        let resp = self
            .bot
            .request::<Value, MethodResponse<Vec<Value>>>("getUpdates", Some(params))
            .await
            .map_err(|e| anyhow::Error::from(e).context("Telegram getUpdates"))?;
        Ok(resp.result)
    }

    pub async fn set_webhook(&self, url: &str, secret: &str) -> Result<()> {
        let params = json!({
            "url": url,
            "secret_token": secret,
            "allowed_updates": raw::ALLOWED_UPDATES,
            "drop_pending_updates": false,
        });
        let _: bool = self.call("setWebhook", params).await?;
        Ok(())
    }

    pub async fn delete_webhook(&self) -> Result<()> {
        let params = DeleteWebhookParams::builder()
            .drop_pending_updates(false)
            .build();
        self.retrying("deleteWebhook", || self.bot.delete_webhook(&params))
            .await?;
        Ok(())
    }

    // ---- messages -------------------------------------------------------

    pub async fn send_text(
        &self,
        target: &Target,
        text: &str,
        parse_mode: Option<ParseMode>,
        markup: Option<ReplyMarkup>,
    ) -> Result<Message> {
        self.limiter.acquire(target.chat_id, Kind::Message).await;
        let params = raw::SendMessageRaw {
            chat_id: target.chat_id,
            message_thread_id: target.thread_id,
            business_connection_id: target.business_connection_id.clone(),
            text: text.to_string(),
            parse_mode,
            entities: None,
            link_preview_options: Some(raw::LinkPreviewOptions {
                is_disabled: Some(true),
            }),
            reply_parameters: target.reply_parameters(),
            reply_markup: markup,
            ephemeral_message_parameters: target.ephemeral.clone(),
        };
        self.call("sendMessage", params).await
    }

    pub async fn send_rich_markdown(
        &self,
        target: &Target,
        markdown: &str,
        markup: Option<ReplyMarkup>,
    ) -> Result<Message> {
        self.limiter.acquire(target.chat_id, Kind::Message).await;
        let params = raw::SendRichMessageRaw {
            chat_id: target.chat_id,
            message_thread_id: target.thread_id,
            business_connection_id: target.business_connection_id.clone(),
            rich_message: raw::InputRichMessageRaw {
                markdown: Some(markdown.to_string()),
                html: None,
            },
            reply_parameters: target.reply_parameters(),
            reply_markup: markup,
            ephemeral_message_parameters: target.ephemeral.clone(),
        };
        self.call("sendRichMessage", params).await
    }

    /// Plain-text draft; empty text shows "Thinking…".
    pub async fn send_draft(
        &self,
        chat_id: i64,
        thread_id: Option<i32>,
        draft_id: i64,
        text: &str,
        can_stop: bool,
    ) -> Result<()> {
        self.limiter.acquire(chat_id, Kind::Draft).await;
        let params = raw::SendMessageDraftRaw {
            chat_id,
            message_thread_id: thread_id,
            draft_id,
            text: text.to_string(),
            parse_mode: None,
            can_stop: Some(can_stop),
            keep_on_stop: Some(true),
        };
        let _: bool = self.call("sendMessageDraft", params).await?;
        Ok(())
    }

    pub async fn send_rich_draft(
        &self,
        chat_id: i64,
        thread_id: Option<i32>,
        draft_id: i64,
        markdown: &str,
        can_stop: bool,
    ) -> Result<()> {
        self.limiter.acquire(chat_id, Kind::Draft).await;
        let params = raw::SendRichMessageDraftRaw {
            chat_id,
            message_thread_id: thread_id,
            draft_id,
            rich_message: raw::InputRichMessageRaw {
                markdown: Some(markdown.to_string()),
                html: None,
            },
            can_stop: Some(can_stop),
            keep_on_stop: Some(true),
        };
        let _: bool = self.call("sendRichMessageDraft", params).await?;
        Ok(())
    }

    pub async fn chat_action(&self, target: &Target, action: ChatAction) -> Result<()> {
        self.limiter.acquire(target.chat_id, Kind::Light).await;
        let params = SendChatActionParams::builder()
            .chat_id(target.chat_id)
            .action(action)
            .maybe_message_thread_id(target.thread_id)
            .maybe_business_connection_id(target.business_connection_id.clone())
            .build();
        self.retrying("sendChatAction", || self.bot.send_chat_action(&params))
            .await?;
        Ok(())
    }

    /// Set (Some) or clear (None) the bot's reaction on a message.
    pub async fn react(&self, chat_id: i64, message_id: i32, emoji: Option<&str>) -> Result<()> {
        self.limiter.acquire(chat_id, Kind::Light).await;
        let reaction = emoji
            .map(|e| {
                vec![ReactionType::Emoji(
                    ReactionTypeEmoji::builder().emoji(e).build(),
                )]
            })
            .unwrap_or_default();
        let params = SetMessageReactionParams::builder()
            .chat_id(chat_id)
            .message_id(message_id)
            .reaction(reaction)
            .build();
        self.retrying("setMessageReaction", || {
            self.bot.set_message_reaction(&params)
        })
        .await?;
        Ok(())
    }

    pub async fn edit_text(
        &self,
        chat_id: i64,
        message_id: i32,
        text: &str,
        parse_mode: Option<ParseMode>,
        markup: Option<InlineKeyboardMarkup>,
    ) -> Result<()> {
        self.limiter.acquire(chat_id, Kind::Light).await;
        let mut params = json!({ "chat_id": chat_id, "message_id": message_id, "text": text });
        if let Some(pm) = parse_mode {
            params["parse_mode"] = json!(pm);
        }
        if let Some(m) = markup {
            params["reply_markup"] = serde_json::to_value(m)?;
        }
        let _: Value = self.call("editMessageText", params).await?;
        Ok(())
    }

    pub async fn answer_callback(&self, id: &str, text: Option<&str>) -> Result<()> {
        let params = AnswerCallbackQueryParams::builder()
            .callback_query_id(id)
            .maybe_text(text.map(str::to_string))
            .build();
        self.retrying("answerCallbackQuery", || {
            self.bot.answer_callback_query(&params)
        })
        .await?;
        Ok(())
    }

    pub async fn answer_guest_query(
        &self,
        guest_query_id: &str,
        text: &str,
        parse_mode: Option<ParseMode>,
    ) -> Result<()> {
        let content = InputTextMessageContent::builder()
            .message_text(text)
            .maybe_parse_mode(parse_mode)
            .build();
        let article = InlineQueryResultArticle::builder()
            .id("answer")
            .title("Answer")
            .input_message_content(InputMessageContent::Text(content))
            .build();
        let params = AnswerGuestQueryParams::builder()
            .guest_query_id(guest_query_id)
            .result(InlineQueryResult::Article(article))
            .build();
        self.retrying("answerGuestQuery", || self.bot.answer_guest_query(&params))
            .await?;
        Ok(())
    }

    pub async fn answer_inline_query(
        &self,
        id: &str,
        results: Vec<InlineQueryResult>,
    ) -> Result<()> {
        let params = AnswerInlineQueryParams::builder()
            .inline_query_id(id)
            .results(results)
            .cache_time(0u32)
            .is_personal(true)
            .build();
        self.retrying("answerInlineQuery", || {
            self.bot.answer_inline_query(&params)
        })
        .await?;
        Ok(())
    }

    // ---- files ----------------------------------------------------------

    /// Download a file by id. Returns bytes and the server-side path (has the extension).
    pub async fn download_file(&self, file_id: &str, max_bytes: u64) -> Result<(Bytes, String)> {
        let params = GetFileParams::builder().file_id(file_id).build();
        let file = self
            .retrying("getFile", || self.bot.get_file(&params))
            .await?;
        if let Some(size) = file.file_size
            && size > max_bytes
        {
            bail!(
                "file is {} MB; the limit is {} MB",
                size / 1024 / 1024,
                max_bytes / 1024 / 1024
            );
        }
        let path = file
            .file_path
            .ok_or_else(|| anyhow!("Telegram returned no file path"))?;
        let url = format!("{}/file/bot{}/{}", self.api_root, self.token, path);
        let resp = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .context("downloading file from Telegram")?;
        if !resp.status().is_success() {
            bail!("Telegram file download returned {}", resp.status());
        }
        let bytes = resp.bytes().await.context("reading file from Telegram")?;
        if bytes.len() as u64 > max_bytes {
            bail!("file exceeds {} MB", max_bytes / 1024 / 1024);
        }
        Ok((bytes, path))
    }

    async fn temp_file(
        bytes: &Bytes,
        filename: &str,
    ) -> Result<(tempfile::TempDir, std::path::PathBuf)> {
        let dir = tempfile::tempdir().context("creating temp dir")?;
        let path = dir
            .path()
            .join(crate::util::safe_filename(filename, "file"));
        tokio::fs::write(&path, bytes)
            .await
            .context("writing temp file")?;
        Ok((dir, path))
    }

    pub async fn send_document(
        &self,
        target: &Target,
        filename: &str,
        bytes: Bytes,
        caption: Option<&str>,
    ) -> Result<Message> {
        self.limiter.acquire(target.chat_id, Kind::Message).await;
        let (_dir, path) = Self::temp_file(&bytes, filename).await?;
        let params = SendDocumentParams::builder()
            .chat_id(target.chat_id)
            .document(FileUpload::InputFile(InputFile { path }))
            .maybe_message_thread_id(target.thread_id)
            .maybe_business_connection_id(target.business_connection_id.clone())
            .maybe_caption(caption.map(|c| crate::util::truncate_chars(c, 1000)))
            .maybe_reply_parameters(target.reply_parameters())
            .build();
        self.retrying("sendDocument", || self.bot.send_document(&params))
            .await
    }

    pub async fn send_photo_bytes(
        &self,
        target: &Target,
        filename: &str,
        bytes: Bytes,
        caption: Option<&str>,
    ) -> Result<Message> {
        self.limiter.acquire(target.chat_id, Kind::Message).await;
        let (_dir, path) = Self::temp_file(&bytes, filename).await?;
        let params = SendPhotoParams::builder()
            .chat_id(target.chat_id)
            .photo(FileUpload::InputFile(InputFile { path }))
            .maybe_message_thread_id(target.thread_id)
            .maybe_business_connection_id(target.business_connection_id.clone())
            .maybe_caption(caption.map(|c| crate::util::truncate_chars(c, 1000)))
            .maybe_reply_parameters(target.reply_parameters())
            .build();
        self.retrying("sendPhoto", || self.bot.send_photo(&params))
            .await
    }

    pub async fn send_photo_url(
        &self,
        target: &Target,
        url: &str,
        caption: Option<&str>,
    ) -> Result<Message> {
        self.limiter.acquire(target.chat_id, Kind::Message).await;
        let params = SendPhotoParams::builder()
            .chat_id(target.chat_id)
            .photo(FileUpload::String(url.to_string()))
            .maybe_message_thread_id(target.thread_id)
            .maybe_business_connection_id(target.business_connection_id.clone())
            .maybe_caption(caption.map(|c| crate::util::truncate_chars(c, 1000)))
            .maybe_reply_parameters(target.reply_parameters())
            .build();
        self.retrying("sendPhoto", || self.bot.send_photo(&params))
            .await
    }

    pub async fn send_voice_bytes(
        &self,
        target: &Target,
        filename: &str,
        bytes: Bytes,
    ) -> Result<Message> {
        self.limiter.acquire(target.chat_id, Kind::Message).await;
        let (_dir, path) = Self::temp_file(&bytes, filename).await?;
        let params = SendVoiceParams::builder()
            .chat_id(target.chat_id)
            .voice(FileUpload::InputFile(InputFile { path }))
            .maybe_message_thread_id(target.thread_id)
            .maybe_business_connection_id(target.business_connection_id.clone())
            .maybe_reply_parameters(target.reply_parameters())
            .build();
        self.retrying("sendVoice", || self.bot.send_voice(&params))
            .await
    }

    // ---- bot profile ----------------------------------------------------

    pub async fn set_my_commands(
        &self,
        commands: Vec<BotCommand>,
        scope: Option<BotCommandScope>,
    ) -> Result<()> {
        let params = SetMyCommandsParams::builder()
            .commands(commands)
            .maybe_scope(scope)
            .build();
        self.retrying("setMyCommands", || self.bot.set_my_commands(&params))
            .await?;
        Ok(())
    }

    pub async fn set_my_description(&self, description: &str) -> Result<()> {
        let params = SetMyDescriptionParams::builder()
            .description(description)
            .build();
        self.retrying("setMyDescription", || self.bot.set_my_description(&params))
            .await?;
        Ok(())
    }

    pub async fn set_my_short_description(&self, description: &str) -> Result<()> {
        let params = SetMyShortDescriptionParams::builder()
            .short_description(description)
            .build();
        self.retrying("setMyShortDescription", || {
            self.bot.set_my_short_description(&params)
        })
        .await?;
        Ok(())
    }

    pub async fn set_menu_button_web_app(&self, text: &str, url: &str) -> Result<()> {
        let button = MenuButton::WebApp(
            MenuButtonWebApp::builder()
                .text(text)
                .web_app(WebAppInfo::builder().url(url).build())
                .build(),
        );
        let params = SetChatMenuButtonParams::builder()
            .menu_button(button)
            .build();
        self.retrying("setChatMenuButton", || {
            self.bot.set_chat_menu_button(&params)
        })
        .await?;
        Ok(())
    }
}
