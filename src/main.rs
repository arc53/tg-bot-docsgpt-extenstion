use anyhow::{Context, Result};
use clap::Parser;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use docsgpt_telegram::app::{AppState, BotContext};
use docsgpt_telegram::config::{self, Mode};
use docsgpt_telegram::storage;
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
    let storage = storage::open(&cfg.storage).await?;
    let http = reqwest::Client::builder()
        .user_agent(format!("docsgpt-telegram/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .context("building http client")?;
    let shutdown = CancellationToken::new();
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
        tokio::spawn(async move {
            wait_for_signal().await;
            tracing::info!("shutdown requested");
            shutdown.cancel();
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
        let sd = shutdown.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = webhook::serve(&bind, state, sd).await {
                tracing::error!(error = %e, "http server failed");
            }
        }));
    }

    for t in tasks {
        let _ = t.await;
    }
    tracing::info!("bye");
    Ok(())
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("sigterm handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
