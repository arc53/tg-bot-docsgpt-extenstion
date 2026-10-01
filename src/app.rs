//! Shared state for the process and for each bot.

use anyhow::{Context, Result};
use docsgpt_bot::{Agents, BotCore, CancelRegistry, Shutdown, Storage};
use frankenstein::types::{BotCommand, BotCommandScope, User};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::{BotConfig, Config};
use crate::telegram::Tg;

pub struct AppState {
    pub cfg: Config,
    pub storage: Arc<dyn Storage>,
    pub http: reqwest::Client,
    /// Process lifetime: polling loops watch its token; handlers are spawned on
    /// it so shutdown can let answers in progress finish.
    pub shutdown: Shutdown,
}

pub struct BotContext {
    pub app: Arc<AppState>,
    pub cfg: BotConfig,
    pub tg: Tg,
    pub me: User,
    /// DocsGPT client, agents, storage and per-chat locks.
    pub core: BotCore,
    /// Running drafts the user can stop, by `"{chat}:{thread}:{draft}"`.
    pub cancels: CancelRegistry,
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
        let client = docsgpt_bot::docsgpt::Client::builder(cfg.api_base(&app.cfg.api_base))
            .http_client(app.http.clone())
            .build()
            .context("DocsGPT client")?;
        tracing::info!(
            bot = %cfg.name,
            username = %me.username.clone().unwrap_or_default(),
            agents = ?cfg.agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            docsgpt = client.base_url(),
            "bot ready"
        );
        let core = BotCore::new(
            cfg.name.clone(),
            client,
            Agents::new(cfg.agents.clone())?,
            app.storage.clone(),
        );
        let ctx = Arc::new(Self {
            app,
            cfg,
            tg,
            me,
            core,
            cancels: CancelRegistry::default(),
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

    /// Key under which a draft's Stop is registered.
    pub fn cancel_key(chat_id: i64, thread_id: i32, draft_id: i64) -> String {
        format!("{chat_id}:{thread_id}:{draft_id}")
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
