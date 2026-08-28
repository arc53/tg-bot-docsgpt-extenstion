//! Guest mode: one reply when mentioned in a chat the bot is not a member of.

use anyhow::Result;
use frankenstein::types::Message;
use std::sync::Arc;

use crate::app::BotContext;
use crate::handlers::{chat, groups, message};
use crate::storage::ChatScope;
use crate::telegram::api::Target;

pub async fn handle(ctx: &Arc<BotContext>, msg: Message) -> Result<()> {
    if !ctx.cfg.guest {
        return Ok(());
    }
    let Some(guest_query_id) = msg.guest_query_id.clone() else {
        return Ok(());
    };
    let Some(user) = msg.from.clone() else {
        return Ok(());
    };
    let text = groups::strip_mention(groups::text_of(&msg).unwrap_or(""), ctx.username());
    if text.is_empty() {
        ctx.tg
            .answer_guest_query(
                &guest_query_id,
                "Mention me with a question and I'll answer it.",
                None,
            )
            .await?;
        return Ok(());
    }
    let scope = ChatScope::guest(&ctx.cfg.name, msg.chat.id, user.id);
    let (agent, question) = match chat::resolve_agent(ctx, &scope, &text).await {
        Ok(v) => v,
        Err(reply) => {
            ctx.tg
                .answer_guest_query(&guest_query_id, &reply, None)
                .await?;
            return Ok(());
        }
    };
    if question.is_empty() {
        return Ok(());
    }
    chat::ask(
        ctx,
        chat::Ask {
            scope,
            target: Target::chat(msg.chat.id),
            user: message::user_info(&msg),
            question,
            attachments: vec![],
            agent,
            delivery: chat::Delivery::Guest { guest_query_id },
            source_message_id: None,
            voice_reply: false,
        },
    )
    .await
}
