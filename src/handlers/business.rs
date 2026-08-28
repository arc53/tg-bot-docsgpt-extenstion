//! Telegram Business: answer customers on behalf of a connected account.

use anyhow::Result;
use frankenstein::types::{BusinessConnection, Message};
use serde_json::json;
use std::sync::Arc;

use crate::app::BotContext;
use crate::handlers::{attachments, chat, groups, message};
use crate::storage::BusinessLink;

fn link_from(c: &BusinessConnection) -> BusinessLink {
    BusinessLink {
        id: c.id.clone(),
        user_id: c.user.id,
        user_chat_id: c.user_chat_id as i64,
        is_enabled: c.is_enabled,
        can_reply: c.rights.as_ref().and_then(|r| r.can_reply).unwrap_or(false),
    }
}

pub async fn connection(ctx: &Arc<BotContext>, c: BusinessConnection) -> Result<()> {
    let link = link_from(&c);
    tracing::info!(bot = %ctx.cfg.name, connection = %link.id, owner = link.user_id, enabled = link.is_enabled, can_reply = link.can_reply, "business connection updated");
    ctx.app
        .storage
        .set_business_link(&ctx.cfg.name, &link)
        .await
}

async fn link_for(ctx: &Arc<BotContext>, id: &str) -> Option<BusinessLink> {
    if let Ok(Some(l)) = ctx.app.storage.get_business_link(&ctx.cfg.name, id).await {
        return Some(l);
    }
    match ctx
        .tg
        .call::<_, BusinessConnection>(
            "getBusinessConnection",
            json!({ "business_connection_id": id }),
        )
        .await
    {
        Ok(c) => {
            let link = link_from(&c);
            let _ = ctx
                .app
                .storage
                .set_business_link(&ctx.cfg.name, &link)
                .await;
            Some(link)
        }
        Err(e) => {
            tracing::warn!(error = %e, "getBusinessConnection failed");
            None
        }
    }
}

pub async fn message(ctx: &Arc<BotContext>, msg: Message) -> Result<()> {
    if !ctx.cfg.business {
        return Ok(());
    }
    let Some(bc) = msg.business_connection_id.clone() else {
        return Ok(());
    };
    let Some(link) = link_for(ctx, &bc).await else {
        return Ok(());
    };
    if !link.is_enabled || !link.can_reply {
        return Ok(());
    }
    // Messages typed by the business owner (or by us) are not questions.
    if msg
        .from
        .as_ref()
        .is_none_or(|u| u.id == link.user_id || u.is_bot)
    {
        return Ok(());
    }
    if !message::allowed(ctx, msg.chat.id) {
        return Ok(());
    }
    let text = groups::text_of(&msg).unwrap_or("").trim().to_string();
    if attachments::has_media(&msg) {
        return attachments::handle(ctx, msg, text).await;
    }
    if text.is_empty() || text.starts_with('/') {
        return Ok(());
    }
    chat::ask_from_message(ctx, &msg, text, vec![], None).await
}
