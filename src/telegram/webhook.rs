//! HTTP server: `/healthz` and one webhook route per bot.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::app::BotContext;
use crate::handlers;
use crate::telegram::raw;

#[derive(Clone)]
pub struct ServerState {
    pub bots: Arc<HashMap<String, Arc<BotContext>>>,
    pub secret: String,
}

pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/webhook/{bot}", post(webhook))
        .with_state(state)
}

async fn healthz(State(state): State<ServerState>) -> String {
    format!("ok bots={}\n", state.bots.len())
}

async fn webhook(
    State(state): State<ServerState>,
    Path(bot): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> StatusCode {
    let provided = headers
        .get("x-telegram-bot-api-secret-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if provided != state.secret {
        return StatusCode::UNAUTHORIZED;
    }
    let Some(ctx) = state.bots.get(&bot) else {
        return StatusCode::NOT_FOUND;
    };
    let (_, incoming) = raw::parse_update(body);
    let ctx = ctx.clone();
    tokio::spawn(async move { handlers::dispatch(ctx, incoming).await });
    StatusCode::OK
}

pub async fn serve(
    bind: &str,
    state: ServerState,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(bind, "http server listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await?;
    Ok(())
}
