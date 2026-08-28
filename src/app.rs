//! Shared state for the process and for each bot.

use anyhow::{Context, Result};
use frankenstein::types::{BotCommand, BotCommandScope, User};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;

use crate::config::{BotConfig, Config};
use crate::docsgpt::DocsGpt;
use crate::storage::Storage;
use crate::telegram::Tg;

pub struct AppState {
    pub cfg: Config,
    pub storage: Arc<dyn Storage>,
    pub http: reqwest::Client,
    pub shutdown: CancellationToken,
}

pub struct BotContext {
    pub app: Arc<AppState>,
    pub cfg: BotConfig,
    pub tg: Tg,
    pub me: User,
    pub docsgpt: DocsGpt,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    cancels: Mutex<HashMap<(i64, i32, i64), CancellationToken>>,
    pub media_groups: Mutex<HashMap<String, Vec<frankenstein::types::Message>>>,
}

impl BotContext {
    /// Connect to Telegram, verify the token, and push profile settings.
    pub async fn init(app: Arc<AppState>, cfg: BotConfig) -> Result<Arc<Self>> {
        let tg = Tg::new(app.http.clone(), &cfg.token, &cfg.name);
        let me = tg
            .get_me()
            .await
            .with_context(|| format!("bot {:?}: token rejected by Telegram", cfg.name))?;
        let docsgpt = DocsGpt::new(app.http.clone(), cfg.api_base(&app.cfg.api_base));
        tracing::info!(
            bot = %cfg.name,
            username = %me.username.clone().unwrap_or_default(),
            agents = ?cfg.agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            docsgpt = docsgpt.base(),
            "bot ready"
        );
        let ctx = Arc::new(Self {
            app,
            cfg,
            tg,
            me,
            docsgpt,
            locks: Mutex::new(HashMap::new()),
            cancels: Mutex::new(HashMap::new()),
            media_groups: Mutex::new(HashMap::new()),
        });
        ctx.push_profile().await;
        Ok(ctx)
    }

    pub fn username(&self) -> &str {
        self.me.username.as_deref().unwrap_or("")
    }

    pub fn title(&self) -> &str {
        &self.me.first_name
    }

    /// Serialize work per conversation scope so turns don't interleave.
    pub async fn lock_scope(&self, key: &str) -> OwnedMutexGuard<()> {
        let m = {
            let mut map = self.locks.lock().await;
            if map.len() > 5000 {
                map.retain(|_, v| Arc::strong_count(v) > 1);
            }
            map.entry(key.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        m.lock_owned().await
    }

    pub async fn register_cancel(
        &self,
        chat_id: i64,
        thread_id: i32,
        draft_id: i64,
    ) -> CancellationToken {
        let token = CancellationToken::new();
        self.cancels
            .lock()
            .await
            .insert((chat_id, thread_id, draft_id), token.clone());
        token
    }

    pub async fn unregister_cancel(&self, chat_id: i64, thread_id: i32, draft_id: i64) {
        self.cancels
            .lock()
            .await
            .remove(&(chat_id, thread_id, draft_id));
    }

    /// Called from a `stopped_message_generation` update.
    pub async fn cancel_generation(&self, chat_id: i64, thread_id: i32, draft_id: i64) -> bool {
        let map = self.cancels.lock().await;
        match map.get(&(chat_id, thread_id, draft_id)) {
            Some(t) => {
                t.cancel();
                true
            }
            None => false,
        }
    }

    /// Commands, descriptions and menu button — best effort, logged on failure.
    async fn push_profile(&self) {
        let multi = self.cfg.agents.len() > 1;
        let mut private = vec![
            cmd("start", "Start a conversation", false),
            cmd("new", "Start a new conversation", false),
            cmd("help", "How to use this bot", false),
        ];
        let mut groups = vec![
            cmd("new", "Start a new conversation here", false),
            cmd("help", "How to use this bot", true),
        ];
        if multi {
            private.push(cmd("agents", "Choose which agent answers", false));
            private.push(cmd("agent", "Switch agent: /agent <name>", false));
            groups.push(cmd("agents", "Choose which agent answers", true));
            groups.push(cmd("agent", "Switch agent: /agent <name>", false));
        }
        if let Err(e) = self
            .tg
            .set_my_commands(private, Some(BotCommandScope::AllPrivateChats))
            .await
        {
            tracing::warn!(bot = %self.cfg.name, error = %e, "setMyCommands (private) failed");
        }
        if let Err(e) = self
            .tg
            .set_my_commands(groups, Some(BotCommandScope::AllGroupChats))
            .await
        {
            tracing::warn!(bot = %self.cfg.name, error = %e, "setMyCommands (groups) failed");
        }
        if let Some(d) = &self.cfg.description
            && let Err(e) = self.tg.set_my_description(d).await
        {
            tracing::warn!(bot = %self.cfg.name, error = %e, "setMyDescription failed");
        }
        if let Some(d) = &self.cfg.short_description
            && let Err(e) = self.tg.set_my_short_description(d).await
        {
            tracing::warn!(bot = %self.cfg.name, error = %e, "setMyShortDescription failed");
        }
        if let Some(url) = &self.cfg.menu_button_url
            && let Err(e) = self.tg.set_menu_button_web_app("Open", url).await
        {
            tracing::warn!(bot = %self.cfg.name, error = %e, "setChatMenuButton failed");
        }
    }
}

fn cmd(command: &str, description: &str, ephemeral: bool) -> BotCommand {
    BotCommand::builder()
        .command(command)
        .description(description)
        .maybe_is_ephemeral(ephemeral.then_some(true))
        .build()
}
