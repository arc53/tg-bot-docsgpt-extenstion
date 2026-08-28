//! Regular messages: commands, group gating, media, questions.

use anyhow::Result;
use frankenstein::types::{ChatType, Message};
use serde_json::Value;

use crate::app::BotContext;
use crate::config::GroupMode;
use crate::handlers::{attachments, chat, commands, groups};
use crate::storage::{ChatScope, UserInfo};
use crate::telegram::api::Target;

pub fn user_info(msg: &Message) -> Option<UserInfo> {
    msg.from.as_ref().map(|u| UserInfo {
        id: u.id,
        first_name: u.first_name.clone(),
        last_name: u.last_name.clone(),
        username: u.username.clone(),
        language_code: u.language_code.clone(),
    })
}

pub fn allowed(ctx: &BotContext, chat_id: i64) -> bool {
    ctx.cfg.allowed_chats.is_empty() || ctx.cfg.allowed_chats.contains(&chat_id)
}

/// Build the reply target for a message: threads in topics, replies in groups.
pub fn target_for(msg: &Message) -> Target {
    let group = groups::is_group(&msg.chat);
    Target {
        chat_id: msg.chat.id,
        thread_id: groups::thread_id(msg),
        business_connection_id: msg.business_connection_id.clone(),
        reply_to: if group { Some(msg.message_id) } else { None },
        ephemeral: None,
    }
}

pub fn scope_for(ctx: &BotContext, msg: &Message) -> ChatScope {
    match &msg.business_connection_id {
        Some(bc) => ChatScope::business(&ctx.cfg.name, msg.chat.id, bc),
        None => ChatScope::new(&ctx.cfg.name, msg.chat.id, groups::thread_id(msg)),
    }
}

pub async fn handle(ctx: &std::sync::Arc<BotContext>, msg: Message, raw: &Value) -> Result<()> {
    if msg.from.as_ref().is_some_and(|u| u.is_bot)
        || matches!(msg.chat.type_field, ChatType::Channel)
    {
        return Ok(());
    }
    let group = groups::is_group(&msg.chat);
    if !allowed(ctx, msg.chat.id) {
        if !group {
            let _ = ctx
                .tg
                .send_text(
                    &Target::chat(msg.chat.id),
                    "This bot is restricted to specific chats.",
                    None,
                    None,
                )
                .await;
        }
        return Ok(());
    }
    if group && ctx.cfg.groups == GroupMode::Off {
        return Ok(());
    }

    if let Some((cmd, args)) = commands::parse(&msg, ctx.username()) {
        let ephemeral_id = raw
            .pointer("/message/ephemeral_message_id")
            .and_then(Value::as_i64)
            .map(|v| v as i32);
        return commands::handle(ctx, &msg, &cmd, &args, ephemeral_id).await;
    }
    // A command for another bot (or a bare "/") is not a question.
    if msg.text.as_deref().is_some_and(|t| t.starts_with('/')) {
        return Ok(());
    }

    // Decide whether a group message is for us, and clean it.
    let text = if group {
        match groups::gate(ctx, &msg) {
            Some(t) => t,
            None => return Ok(()),
        }
    } else {
        groups::text_of(&msg).unwrap_or("").trim().to_string()
    };

    if attachments::has_media(&msg) {
        return attachments::handle(ctx, msg, text).await;
    }
    if text.is_empty() {
        return Ok(());
    }
    chat::ask_from_message(ctx, &msg, text, vec![], None).await
}
