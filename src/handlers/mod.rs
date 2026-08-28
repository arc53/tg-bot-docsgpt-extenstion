//! Update routing.

pub mod attachments;
pub mod business;
pub mod callbacks;
pub mod chat;
pub mod commands;
pub mod groups;
pub mod guest;
pub mod inline;
pub mod message;

use frankenstein::updates::UpdateContent;
use std::sync::Arc;

use crate::app::BotContext;
use crate::telegram::raw::Incoming;

pub async fn dispatch(ctx: Arc<BotContext>, incoming: Incoming) {
    let result = match incoming {
        Incoming::StoppedGeneration(s) => {
            let hit = ctx
                .cancel_generation(s.chat.id, s.message_thread_id.unwrap_or(0), s.draft_id)
                .await;
            tracing::info!(bot = %ctx.cfg.name, chat = s.chat.id, draft = s.draft_id, hit, "user stopped generation");
            Ok(())
        }
        Incoming::Update(u, raw) => match u.content {
            UpdateContent::Message(m) => message::handle(&ctx, *m, &raw).await,
            UpdateContent::BusinessConnection(c) => business::connection(&ctx, c).await,
            UpdateContent::BusinessMessage(m) => business::message(&ctx, *m).await,
            UpdateContent::GuestMessage(m) => guest::handle(&ctx, *m).await,
            UpdateContent::CallbackQuery(q) => callbacks::handle(&ctx, *q).await,
            UpdateContent::InlineQuery(q) => inline::handle(&ctx, q).await,
            UpdateContent::MessageReaction(r) => {
                let emoji: Vec<String> = r
                    .new_reaction
                    .iter()
                    .filter_map(|x| match x {
                        frankenstein::types::ReactionType::Emoji(e) => Some(e.emoji.clone()),
                        _ => None,
                    })
                    .collect();
                tracing::info!(bot = %ctx.cfg.name, chat = r.chat.id, message = r.message_id, ?emoji, "reaction feedback");
                Ok(())
            }
            UpdateContent::MyChatMember(m) => {
                tracing::info!(bot = %ctx.cfg.name, chat = m.chat.id, status = ?m.new_chat_member, "membership changed");
                Ok(())
            }
            _ => Ok(()),
        },
        Incoming::Unknown(_) => Ok(()),
    };
    if let Err(e) = result {
        tracing::error!(bot = %ctx.cfg.name, error = ?e, "update handling failed");
    }
}
