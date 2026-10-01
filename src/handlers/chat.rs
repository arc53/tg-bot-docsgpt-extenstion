//! The question → DocsGPT → Telegram flow. `docsgpt_bot::run_turn` does the
//! work (routing, streaming, tool outputs, files, feedback bookkeeping);
//! [`TelegramSurface`] shows it; this module is the glue around one turn.

use anyhow::Result;
use docsgpt_bot::{AgentConfig, Routed, Scope, StatePatch, TurnReport, run_turn};
use frankenstein::types::Message;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::app::BotContext;
use crate::handlers::{groups, message};
use crate::surface::TelegramSurface;
use crate::telegram::api::Target;
use crate::util;

pub use crate::surface::Delivery;

/// Who asked, kept in the chat state.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserInfo {
    pub id: u64,
    pub first_name: String,
    #[serde(default)]
    pub last_name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub language_code: Option<String>,
}

/// One question to answer.
pub struct Ask {
    pub scope: Scope,
    pub target: Target,
    pub user: Option<UserInfo>,
    /// The question. A leading `#agent` picks the agent unless `agent` is set.
    pub text: String,
    pub attachments: Vec<String>,
    /// Agent chosen already (attachments are uploaded with its key).
    pub agent: Option<String>,
    pub delivery: Delivery,
    pub source_message_id: Option<i32>,
    pub voice_reply: bool,
}

/// Pick the agent for `text` now (needed before uploading attachments):
/// `#name` prefix, then the chat's active agent, then the default. Returns the
/// agent and the text without the prefix, or the reply for an unknown `#tag`.
pub async fn resolve_agent(
    ctx: &BotContext,
    scope: &Scope,
    text: &str,
) -> std::result::Result<(AgentConfig, String), String> {
    let active = ctx
        .core
        .storage
        .chat_state(scope)
        .await
        .ok()
        .and_then(|s| s.active_agent);
    match ctx.core.agents.route(text, active.as_deref()) {
        Routed::Agent {
            agent, question, ..
        } => Ok((agent.clone(), question)),
        Routed::UnknownTag { tag, available } => {
            let names = available
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!("Unknown agent #{tag}. Available: {names}"))
        }
    }
}

/// Entry point for a question that arrived as a message.
pub async fn ask_from_message(
    ctx: &Arc<BotContext>,
    msg: &Message,
    text: String,
    attachments: Vec<String>,
    user: Option<UserInfo>,
) -> Result<()> {
    let private = groups::is_private(&msg.chat) && msg.business_connection_id.is_none();
    let delivery = if private && ctx.cfg.streaming {
        Delivery::Draft
    } else {
        Delivery::Final
    };
    ask(
        ctx,
        Ask {
            scope: message::scope_for(ctx, msg),
            target: message::target_for(msg),
            user: user.or_else(|| message::user_info(msg)),
            text,
            attachments,
            agent: None,
            delivery,
            source_message_id: Some(msg.message_id),
            voice_reply: false,
        },
    )
    .await
}

/// Run one turn end to end.
pub async fn ask(ctx: &Arc<BotContext>, a: Ask) -> Result<()> {
    // Remember the question (for "regenerate") and who asked.
    let mut patch = StatePatch::default().set("last_question", a.text.clone());
    if let Some(u) = &a.user {
        patch = patch.set("user", serde_json::to_value(u)?);
    }
    if let Err(e) = ctx.core.storage.update_chat_state(&a.scope, patch).await {
        tracing::warn!(error = %e, "could not save chat state");
    }

    let is_guest = matches!(a.delivery, Delivery::Guest { .. });
    let surface = TelegramSurface::new(
        ctx.clone(),
        a.target.clone(),
        a.delivery,
        a.source_message_id,
    );
    let mut ask = docsgpt_bot::Ask::new(a.scope, a.text).attachments(a.attachments);
    ask.agent = a.agent.clone();
    let report = run_turn(&ctx.core, &surface, ask).await;
    match &report {
        Ok(r) => tracing::debug!(bot = %ctx.cfg.name, report = ?r, "turn done"),
        Err(e) => tracing::warn!(bot = %ctx.cfg.name, error = %e, "turn failed"),
    }

    if a.voice_reply
        && !is_guest
        && matches!(report, Ok(TurnReport::Answered { .. }))
        && let Some(text) = surface.spoken_text()
    {
        let agent = a
            .agent
            .as_deref()
            .and_then(|n| ctx.core.agents.get(n))
            .unwrap_or_else(|| ctx.core.agents.default_agent());
        let spoken = util::truncate_chars(&text, 1500);
        match ctx.core.client.tts(&agent.api_key, &spoken).await {
            Ok(audio) => {
                if let Err(e) = ctx
                    .tg
                    .send_voice_bytes(&a.target, "answer.mp3", audio)
                    .await
                {
                    tracing::warn!(error = %e, "voice reply failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "text-to-speech failed"),
        }
    }
    surface.clear_reaction().await;
    Ok(())
}
