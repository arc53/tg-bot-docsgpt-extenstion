//! Configuration: a TOML file with `${ENV}` references, or a legacy
//! environment-variable layout (TELEGRAM_BOT_TOKEN / API_KEY / API_KEY_*).

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;

pub use docsgpt_bot::config::{AgentConfig, Backend, StorageConfig};
use docsgpt_bot::config::{
    DEFAULT_API_BASE, agents_from_env, expand_env, find_config, normalize_name,
};

/// SQLite file used when the config names none.
pub const DEFAULT_SQLITE_PATH: &str = "data/docsgpt-telegram.db";

/// Shown when a config still asks for MongoDB.
const MONGODB_REMOVED: &str = "MongoDB storage was removed in version 3. Conversations now live in \
SQLite (the default; keep data/ on a volume) or in memory. Remove STORAGE_TYPE=mongodb and the MONGODB_* \
variables, or `backend = \"mongodb\"` and the uri/db_name/collection keys under [storage]. Chats continue in \
new DocsGPT conversations. See \"Upgrading to version 3\" in the README.";

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

fn default_api_base() -> String {
    DEFAULT_API_BASE.into()
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

/// Load configuration. Order: explicit path → `DOCSGPT_TG_CONFIG` → `docsgpt-tg.toml`
/// in the working directory → legacy environment variables.
pub fn load(explicit: Option<&Path>) -> Result<Config> {
    let mut cfg = match find_config(explicit, "DOCSGPT_TG_CONFIG", "docsgpt-tg.toml") {
        Some(path) => {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading config {}", path.display()))?;
            let cfg = parse(&raw).with_context(|| format!("parsing config {}", path.display()))?;
            tracing::info!(path = %path.display(), bots = cfg.bots.len(), "loaded config file");
            cfg
        }
        None => {
            let cfg = from_env()?;
            tracing::info!("no config file found; using environment variables");
            cfg
        }
    };
    normalize(&mut cfg)?;
    Ok(cfg)
}

/// Expand `${VAR}` references and parse a TOML config.
pub fn parse(raw: &str) -> Result<Config> {
    // Before expanding ${VAR}: a leftover `${MONGODB_URI}` whose variable is
    // already unset would otherwise fail with "missing environment variables".
    static MONGO: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?m)^\s*backend\s*=\s*["']mongo(db)?["']|\$\{MONGODB_"#)
            .expect("valid regex")
    });
    if MONGO.is_match(raw) {
        bail!(MONGODB_REMOVED);
    }
    let expanded = expand_env(raw)?;
    toml::from_str(&expanded).map_err(|e| {
        let msg = e.to_string();
        if msg.contains("mongodb")
            || (raw.contains("[storage]") && (msg.contains("`uri`") || msg.contains("`db_name`")))
        {
            anyhow!(MONGODB_REMOVED)
        } else {
            anyhow!(msg)
        }
    })
}

/// Build a single-bot config from the v1 environment layout.
pub fn from_env() -> Result<Config> {
    let token = std::env::var("TELEGRAM_BOT_TOKEN")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("TELEGRAM_BOT_TOKEN is not set and no config file was found"))?;
    let agents = agents_from_env(std::env::vars());
    if agents.is_empty() {
        bail!("API_KEY (or API_KEY_<NAME>) is not set");
    }

    let storage = match std::env::var("STORAGE_TYPE")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "mongodb" | "mongo" => bail!(MONGODB_REMOVED),
        "memory" => StorageConfig {
            backend: Backend::Memory,
            path: None,
        },
        _ => StorageConfig {
            backend: Backend::Sqlite,
            path: std::env::var("SQLITE_PATH").ok(),
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
        bot.name = normalize_name("bot", &bot.name)?;
        if !names.insert(bot.name.clone()) {
            bail!("duplicate bot name {:?}", bot.name);
        }
        if bot.token.trim().is_empty() {
            bail!("bot {:?}: token is empty", bot.name);
        }
        // Names lowercased and unique, keys present, exactly one default.
        bot.agents = docsgpt_bot::Agents::new(std::mem::take(&mut bot.agents))
            .map_err(|e| anyhow!("bot {:?}: {e}", bot.name))?
            .iter()
            .cloned()
            .collect();
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
    cfg.api_base = cfg.api_base.trim_end_matches('/').to_string();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mongodb_settings_get_the_upgrade_message() {
        let err = parse("[storage]\nbackend = \"mongodb\"\nuri = \"mongodb://x\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("MongoDB storage was removed"), "{err}");
        // Even when the Mongo variables are already gone from the environment.
        let err = parse(
            "[storage]\nbackend = \"mongodb\"\nuri = \"${DOCSGPT_TG_TEST_UNSET_MONGO_URI}\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("MongoDB storage was removed"), "{err}");
        let ok = parse("[storage]\nbackend = \"memory\"\n").unwrap();
        assert_eq!(ok.storage.backend, Backend::Memory);
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

#[cfg(test)]
mod example_tests {
    #[test]
    fn example_config_parses() {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/docsgpt-tg.example.toml"
        ))
        .unwrap();
        let raw = regex::Regex::new(r"\$\{[A-Z_]+\}")
            .unwrap()
            .replace_all(&raw, "placeholder");
        let mut cfg: super::Config = toml::from_str(&raw).unwrap();
        super::normalize(&mut cfg).unwrap();
        assert!(!cfg.bots.is_empty());
    }
}
