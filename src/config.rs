//! Configuration: a TOML file with `${ENV}` references, or a legacy
//! environment-variable layout (TELEGRAM_BOT_TOKEN / API_KEY / API_KEY_*).

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// DocsGPT base URL (cloud by default).
    #[serde(default = "default_api_base")]
    pub api_base: String,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub bots: Vec<BotConfig>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Memory,
    Sqlite,
    Mongodb,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    #[serde(default = "default_backend")]
    pub backend: Backend,
    /// SQLite file path.
    #[serde(default = "default_sqlite_path")]
    pub path: String,
    /// MongoDB connection string.
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default = "default_db_name")]
    pub db_name: String,
    /// Collection used by v2 for chat state and conversation ids.
    #[serde(default = "default_collection")]
    pub collection: String,
    /// Collection written by the v1 Python bot; read once for migration.
    #[serde(default = "default_legacy_collection")]
    pub legacy_collection: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            path: default_sqlite_path(),
            uri: None,
            db_name: default_db_name(),
            collection: default_collection(),
            legacy_collection: default_legacy_collection(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Start the HTTP server (`/healthz`, webhooks). Auto-enabled when a bot uses webhook mode.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Public HTTPS base URL Telegram can reach, e.g. `https://bots.example.com`.
    #[serde(default)]
    pub public_url: Option<String>,
    /// Secret sent by Telegram in `X-Telegram-Bot-Api-Secret-Token`. Generated if absent.
    #[serde(default)]
    pub webhook_secret: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_bind(),
            public_url: None,
            webhook_secret: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Polling,
    Webhook,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GroupMode {
    /// Answer only when mentioned or replied to.
    Mention,
    /// Answer every message (requires privacy mode off in BotFather).
    All,
    /// Ignore groups entirely.
    Off,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotConfig {
    /// Short identifier used in storage keys and webhook paths.
    pub name: String,
    pub token: String,
    #[serde(default = "default_mode")]
    pub mode: Mode,
    #[serde(default = "default_group_mode")]
    pub groups: GroupMode,
    /// Stream answers as live drafts in private chats.
    #[serde(default = "default_true")]
    pub streaming: bool,
    /// React 👀 while working.
    #[serde(default = "default_true")]
    pub reactions: bool,
    /// Accept photos, documents, voice notes.
    #[serde(default = "default_true")]
    pub attachments: bool,
    /// Reply to voice notes with a generated voice note as well.
    #[serde(default)]
    pub voice_replies: bool,
    /// Answer customers in connected Telegram Business chats.
    #[serde(default = "default_true")]
    pub business: bool,
    /// Answer guest mentions in chats the bot is not a member of.
    #[serde(default = "default_true")]
    pub guest: bool,
    /// Answer inline queries (`@bot question?`).
    #[serde(default = "default_true")]
    pub inline: bool,
    /// Restrict the bot to these chat ids (empty = everyone).
    #[serde(default)]
    pub allowed_chats: Vec<i64>,
    /// Text for /start. `{agents}` expands to the agent list.
    #[serde(default)]
    pub welcome: Option<String>,
    /// Pushed with setMyDescription at startup.
    #[serde(default)]
    pub description: Option<String>,
    /// Pushed with setMyShortDescription at startup.
    #[serde(default)]
    pub short_description: Option<String>,
    /// Menu button opening a Mini App (for example the DocsGPT web widget).
    #[serde(default)]
    pub menu_button_url: Option<String>,
    #[serde(default = "default_max_file_mb")]
    pub max_file_mb: u64,
    /// Override the global DocsGPT base URL for this bot.
    #[serde(default)]
    pub api_base: Option<String>,
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub name: String,
    pub api_key: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub default: bool,
}

fn default_api_base() -> String {
    "https://gptcloud.arc53.com".into()
}
fn default_backend() -> Backend {
    Backend::Sqlite
}
fn default_sqlite_path() -> String {
    "data/docsgpt-telegram.db".into()
}
fn default_db_name() -> String {
    "telegram_bot_memory".into()
}
fn default_collection() -> String {
    "telegram_bot_state".into()
}
fn default_legacy_collection() -> String {
    "chat_histories".into()
}
fn default_bind() -> String {
    "0.0.0.0:8080".into()
}
fn default_mode() -> Mode {
    Mode::Polling
}
fn default_group_mode() -> GroupMode {
    GroupMode::Mention
}
fn default_true() -> bool {
    true
}
fn default_max_file_mb() -> u64 {
    20
}

impl BotConfig {
    pub fn default_agent(&self) -> &AgentConfig {
        self.agents
            .iter()
            .find(|a| a.default)
            .unwrap_or(&self.agents[0])
    }
    pub fn agent(&self, name: &str) -> Option<&AgentConfig> {
        let name = name.to_ascii_lowercase();
        self.agents.iter().find(|a| a.name == name)
    }
    pub fn api_base<'a>(&'a self, global: &'a str) -> &'a str {
        self.api_base.as_deref().unwrap_or(global)
    }
}

/// Expand `${VAR}` and `${VAR:-default}` references from the process environment.
pub fn expand_env(input: &str) -> Result<String> {
    let re = regex::Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}").unwrap();
    let mut missing = Vec::new();
    let out = re.replace_all(input, |caps: &regex::Captures| {
        let var = &caps[1];
        match std::env::var(var) {
            Ok(v) => v,
            Err(_) => match caps.get(2) {
                Some(d) => d.as_str().to_string(),
                None => {
                    missing.push(var.to_string());
                    String::new()
                }
            },
        }
    });
    if !missing.is_empty() {
        bail!(
            "missing environment variables referenced in config: {}",
            missing.join(", ")
        );
    }
    Ok(out.into_owned())
}

/// Load configuration. Order: explicit path → `DOCSGPT_TG_CONFIG` → `docsgpt-tg.toml`
/// in the working directory → legacy environment variables.
pub fn load(explicit: Option<&Path>) -> Result<Config> {
    let candidate = explicit
        .map(|p| p.to_path_buf())
        .or_else(|| std::env::var_os("DOCSGPT_TG_CONFIG").map(Into::into))
        .or_else(|| {
            let p = Path::new("docsgpt-tg.toml");
            p.exists().then(|| p.to_path_buf())
        });

    let mut cfg = match candidate {
        Some(path) => {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading config {}", path.display()))?;
            let expanded = expand_env(&raw)?;
            let cfg: Config = toml::from_str(&expanded)
                .with_context(|| format!("parsing config {}", path.display()))?;
            tracing::info!(path = %path.display(), bots = cfg.bots.len(), "loaded config file");
            cfg
        }
        None => {
            let cfg = from_env()?;
            tracing::info!("no config file found; using legacy environment variables");
            cfg
        }
    };
    normalize(&mut cfg)?;
    Ok(cfg)
}

/// Build a single-bot config from the v1 environment layout.
pub fn from_env() -> Result<Config> {
    let token = std::env::var("TELEGRAM_BOT_TOKEN")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("TELEGRAM_BOT_TOKEN is not set and no config file was found"))?;
    let mut agents = Vec::new();
    if let Ok(key) = std::env::var("API_KEY")
        && !key.trim().is_empty()
    {
        agents.push(AgentConfig {
            name: "default".into(),
            api_key: key,
            description: None,
            default: true,
        });
    }
    let mut extra: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| k.starts_with("API_KEY_") && k.len() > 8)
        .map(|(k, v)| (k[8..].to_ascii_lowercase(), v))
        .collect();
    extra.sort();
    for (name, key) in extra {
        agents.push(AgentConfig {
            name,
            api_key: key,
            description: None,
            default: false,
        });
    }
    if agents.is_empty() {
        bail!("API_KEY (or API_KEY_<NAME>) is not set");
    }
    if !agents.iter().any(|a| a.default) {
        agents[0].default = true;
    }

    let storage = match std::env::var("STORAGE_TYPE")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "mongodb" | "mongo" => StorageConfig {
            backend: Backend::Mongodb,
            uri: std::env::var("MONGODB_URI").ok(),
            db_name: std::env::var("MONGODB_DB_NAME").unwrap_or_else(|_| default_db_name()),
            legacy_collection: std::env::var("MONGODB_COLLECTION_NAME")
                .unwrap_or_else(|_| default_legacy_collection()),
            ..Default::default()
        },
        "memory" => StorageConfig {
            backend: Backend::Memory,
            ..Default::default()
        },
        _ => StorageConfig {
            backend: Backend::Sqlite,
            path: std::env::var("SQLITE_PATH").unwrap_or_else(|_| default_sqlite_path()),
            ..Default::default()
        },
    };

    let groups = match std::env::var("GROUPS_MODE")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "all" => GroupMode::All,
        "off" => GroupMode::Off,
        _ => GroupMode::Mention,
    };
    let streaming = std::env::var("STREAMING")
        .map(|v| v != "0" && v != "false")
        .unwrap_or(true);
    let mode = if std::env::var("WEBHOOK_PUBLIC_URL").is_ok() {
        Mode::Webhook
    } else {
        Mode::Polling
    };

    Ok(Config {
        api_base: std::env::var("API_BASE").unwrap_or_else(|_| default_api_base()),
        storage,
        server: ServerConfig {
            enabled: std::env::var("HTTP_BIND").is_ok() || mode == Mode::Webhook,
            bind: std::env::var("HTTP_BIND").unwrap_or_else(|_| default_bind()),
            public_url: std::env::var("WEBHOOK_PUBLIC_URL").ok(),
            webhook_secret: std::env::var("WEBHOOK_SECRET").ok(),
        },
        bots: vec![BotConfig {
            name: std::env::var("BOT_NAME").unwrap_or_else(|_| "bot".into()),
            token,
            mode,
            groups,
            streaming,
            reactions: true,
            attachments: true,
            voice_replies: std::env::var("VOICE_REPLIES")
                .map(|v| v == "1" || v == "true")
                .unwrap_or(false),
            business: true,
            guest: true,
            inline: true,
            allowed_chats: vec![],
            welcome: std::env::var("WELCOME_TEXT").ok(),
            description: None,
            short_description: None,
            menu_button_url: std::env::var("MENU_BUTTON_URL").ok(),
            max_file_mb: default_max_file_mb(),
            api_base: None,
            agents,
        }],
    })
}

fn normalize(cfg: &mut Config) -> Result<()> {
    if cfg.bots.is_empty() {
        bail!("no bots configured");
    }
    let mut names = HashSet::new();
    for bot in &mut cfg.bots {
        bot.name = bot.name.trim().to_ascii_lowercase();
        if bot.name.is_empty()
            || !bot
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!(
                "bot name {:?} must be alphanumeric with '-' or '_'",
                bot.name
            );
        }
        if !names.insert(bot.name.clone()) {
            bail!("duplicate bot name {:?}", bot.name);
        }
        if bot.token.trim().is_empty() {
            bail!("bot {:?}: token is empty", bot.name);
        }
        if bot.agents.is_empty() {
            bail!("bot {:?}: at least one agent is required", bot.name);
        }
        let mut agent_names = HashSet::new();
        for a in &mut bot.agents {
            a.name = a.name.trim().to_ascii_lowercase();
            if a.name.is_empty()
                || !a
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                bail!(
                    "bot {:?}: agent name {:?} must be alphanumeric with '-' or '_'",
                    bot.name,
                    a.name
                );
            }
            if !agent_names.insert(a.name.clone()) {
                bail!("bot {:?}: duplicate agent {:?}", bot.name, a.name);
            }
            if a.api_key.trim().is_empty() {
                bail!(
                    "bot {:?}: agent {:?} has an empty api_key",
                    bot.name,
                    a.name
                );
            }
        }
        let defaults = bot.agents.iter().filter(|a| a.default).count();
        if defaults > 1 {
            bail!("bot {:?}: more than one default agent", bot.name);
        }
        if defaults == 0 {
            bot.agents[0].default = true;
        }
        if bot.mode == Mode::Webhook {
            cfg.server.enabled = true;
            if cfg.server.public_url.is_none() {
                bail!(
                    "bot {:?} uses webhook mode but server.public_url is not set",
                    bot.name
                );
            }
        }
    }
    if cfg.storage.backend == Backend::Mongodb
        && cfg
            .storage
            .uri
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
    {
        bail!("storage.backend = mongodb requires storage.uri (or MONGODB_URI)");
    }
    cfg.api_base = cfg.api_base.trim_end_matches('/').to_string();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_env_with_defaults() {
        unsafe { std::env::set_var("DOCSGPT_TG_TEST_VAR", "abc") };
        assert_eq!(expand_env("x ${DOCSGPT_TG_TEST_VAR} y").unwrap(), "x abc y");
        assert_eq!(
            expand_env("${DOCSGPT_TG_MISSING:-fallback}").unwrap(),
            "fallback"
        );
        assert!(expand_env("${DOCSGPT_TG_MISSING_NO_DEFAULT}").is_err());
    }

    #[test]
    fn parses_multi_bot_toml() {
        let toml = r#"
            api_base = "https://example.com/"
            [storage]
            backend = "memory"
            [[bots]]
            name = "Support"
            token = "1:a"
            [[bots.agents]]
            name = "support"
            api_key = "k1"
            default = true
            [[bots.agents]]
            name = "Sales"
            api_key = "k2"
            [[bots]]
            name = "docs"
            token = "2:b"
            groups = "all"
            [[bots.agents]]
            name = "docs"
            api_key = "k3"
        "#;
        let mut cfg: Config = toml::from_str(toml).unwrap();
        normalize(&mut cfg).unwrap();
        assert_eq!(cfg.api_base, "https://example.com");
        assert_eq!(cfg.bots.len(), 2);
        assert_eq!(cfg.bots[0].name, "support");
        assert_eq!(cfg.bots[0].agents[1].name, "sales");
        assert_eq!(cfg.bots[0].default_agent().name, "support");
        assert_eq!(cfg.bots[1].default_agent().name, "docs");
        assert_eq!(cfg.bots[1].groups, GroupMode::All);
        assert!(cfg.bots[1].streaming);
    }

    #[test]
    fn rejects_duplicate_agents() {
        let toml = r#"
            [[bots]]
            name = "a"
            token = "t"
            [[bots.agents]]
            name = "x"
            api_key = "k"
            [[bots.agents]]
            name = "X"
            api_key = "k2"
        "#;
        let mut cfg: Config = toml::from_str(toml).unwrap();
        assert!(normalize(&mut cfg).is_err());
    }
}
