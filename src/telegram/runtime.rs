//! Per-bot update loop (long polling) and shared startup chores.

use std::sync::Arc;
use std::time::Duration;

use crate::app::BotContext;
use crate::handlers;
use crate::telegram::raw;

pub async fn run_polling(ctx: Arc<BotContext>) {
    if let Err(e) = ctx.tg.delete_webhook().await {
        tracing::warn!(bot = %ctx.cfg.name, error = %e, "could not delete webhook before polling");
    }
    let mut offset: Option<i64> = None;
    let mut backoff = Duration::from_secs(1);
    tracing::info!(bot = %ctx.cfg.name, username = %ctx.me.username.clone().unwrap_or_default(), "polling for updates");
    loop {
        let updates = tokio::select! {
            _ = ctx.app.shutdown.cancelled() => break,
            r = ctx.tg.get_updates_raw(offset, 30) => r,
        };
        match updates {
            Ok(list) => {
                backoff = Duration::from_secs(1);
                for v in list {
                    let (update_id, incoming) = raw::parse_update(v);
                    offset = Some(update_id + 1);
                    let ctx = ctx.clone();
                    tokio::spawn(async move {
                        handlers::dispatch(ctx, incoming).await;
                    });
                }
            }
            Err(e) => {
                tracing::warn!(bot = %ctx.cfg.name, error = %format!("{e:#}"), "getUpdates failed; retrying in {:?}", backoff);
                tokio::select! {
                    _ = ctx.app.shutdown.cancelled() => break,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
    tracing::info!(bot = %ctx.cfg.name, "polling stopped");
}
