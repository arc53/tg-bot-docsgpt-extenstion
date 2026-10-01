//! Guest mode: one reply when mentioned in a chat the bot is not a member of.

use anyhow::Result;
use frankenstein::types::Message;
use std::sync::Arc;

use crate::app::BotContext;
use crate::handlers::{chat, groups, message};
use crate::telegram::api::Target;
use docsgpt_bot::Scope;

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
    let scope = Scope::new(&ctx.cfg.name, msg.chat.id.to_string(), "0")
        .with_namespace(format!("guest:{}", user.id));
    chat::ask(
        ctx,
        chat::Ask {
            scope,
            target: Target::chat(msg.chat.id),
            user: message::user_info(&msg),
            text,
            attachments: vec![],
            agent: None,
            delivery: chat::Delivery::Guest { guest_query_id },
            source_message_id: None,
            voice_reply: false,
        },
    )
    .await
}
