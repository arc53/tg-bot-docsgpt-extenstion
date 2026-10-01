use anyhow::{Context, Result};
use clap::Parser;
use docsgpt_bot::storage::MESSAGE_REF_KIND;
use docsgpt_bot::{MESSAGE_REF_TTL, Shutdown};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use docsgpt_telegram::app::{AppState, BotContext};
use docsgpt_telegram::config::{self, DEFAULT_SQLITE_PATH, Mode};
use docsgpt_telegram::telegram::{runtime, webhook};
use docsgpt_telegram::util;

/// Telegram bots for DocsGPT agents.
#[derive(Parser, Debug)]
#[command(name = "docsgpt-telegram", version)]
struct Cli {
    /// Path to a TOML config (default: docsgpt-tg.toml, else environment variables).
    #[arg(short, long, env = "DOCSGPT_TG_CONFIG")]
    config: Option<PathBuf>,
    /// Validate the config and bot tokens, then exit.
    #[arg(long)]
    check: bool,
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hyper_util=warn,reqwest=warn"));
    if std::env::var("LOG_FORMAT")
        .map(|v| v == "json")
        .unwrap_or(false)
    {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .init();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    init_tracing();
    let cli = Cli::parse();

    let cfg = config::load(cli.config.as_deref())?;
    let storage = docsgpt_bot::storage::open(&cfg.storage, DEFAULT_SQLITE_PATH).await?;
    let http = reqwest::Client::builder()
        .user_agent(format!("docsgpt-telegram/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .context("building http client")?;
    let shutdown = Shutdown::new();
    let app = Arc::new(AppState {
        cfg: cfg.clone(),
        storage,
        http,
        shutdown: shutdown.clone(),
    });

    let mut bots = Vec::new();
    for bot_cfg in cfg.bots.clone() {
        bots.push(BotContext::init(app.clone(), bot_cfg).await?);
    }
    if cli.check {
        for b in &bots {
            println!(
                "ok: {} → @{} ({} agent(s))",
                b.cfg.name,
                b.username(),
                b.cfg.agents.len()
            );
        }
        return Ok(());
    }

    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move { shutdown.wait_for_signal().await });
    }
    {
        // Message refs map an answer to its conversation for 👍/👎; drop old ones daily.
        let (storage, token) = (app.storage.clone(), shutdown.token().clone());
        tokio::spawn(async move {
            loop {
                match storage.prune_json(MESSAGE_REF_KIND, MESSAGE_REF_TTL).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(pruned = n, "pruned old message refs"),
                    Err(e) => tracing::warn!(error = %e, "pruning message refs failed"),
                }
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(24 * 3600)) => {}
                }
            }
        });
    }

    let mut tasks = Vec::new();
    let mut webhook_bots: HashMap<String, Arc<BotContext>> = HashMap::new();
    for ctx in bots {
        match ctx.cfg.mode {
            Mode::Polling => tasks.push(tokio::spawn(runtime::run_polling(ctx))),
            Mode::Webhook => {
                webhook_bots.insert(ctx.cfg.name.clone(), ctx);
            }
        }
    }

    if cfg.server.enabled || !webhook_bots.is_empty() {
        let secret = cfg
            .server
            .webhook_secret
            .clone()
            .unwrap_or_else(|| util::random_token(48));
        if let Some(public) = cfg.server.public_url.as_deref() {
            for (name, ctx) in &webhook_bots {
                let url = format!("{}/webhook/{}", public.trim_end_matches('/'), name);
                ctx.tg
                    .set_webhook(&url, &secret)
                    .await
                    .with_context(|| format!("setting webhook for bot {name}"))?;
                tracing::info!(bot = %name, url, "webhook registered");
            }
        }
        let state = webhook::ServerState {
            bots: Arc::new(webhook_bots),
            secret,
        };
        let bind = cfg.server.bind.clone();
        let sd = shutdown.token().clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = webhook::serve(&bind, state, sd).await {
                tracing::error!(error = %e, "http server failed");
            }
        }));
    }

    for t in tasks {
        let _ = t.await;
    }
    // Polling and the server have stopped; let answers in progress finish.
    if !shutdown.drain(Duration::from_secs(30)).await {
        tracing::warn!("some answers were cut off by shutdown");
    }
    tracing::info!("bye");
    Ok(())
}
