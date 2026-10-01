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

use docsgpt_bot::docsgpt::Feedback;
use docsgpt_bot::submit_feedback;
use frankenstein::types::{MessageReactionUpdated, ReactionType};
use frankenstein::updates::UpdateContent;
use std::sync::Arc;

use crate::app::BotContext;
use crate::telegram::raw::Incoming;

pub async fn dispatch(ctx: Arc<BotContext>, incoming: Incoming) {
    let result = match incoming {
        Incoming::StoppedGeneration(s) => {
            let key =
                BotContext::cancel_key(s.chat.id, s.message_thread_id.unwrap_or(0), s.draft_id);
            let hit = ctx.cancels.cancel(&key);
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
            UpdateContent::MessageReaction(r) => reaction_feedback(&ctx, &r).await,
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

fn thumbs(list: &[ReactionType]) -> Option<Feedback> {
    list.iter().find_map(|x| match x {
        ReactionType::Emoji(e) if e.emoji == "👍" => Some(Feedback::Like),
        ReactionType::Emoji(e) if e.emoji == "👎" => Some(Feedback::Dislike),
        _ => None,
    })
}

/// 👍/👎 on an answer becomes DocsGPT feedback on that answer; taking the
/// reaction back clears it. Other reactions are ignored.
async fn reaction_feedback(
    ctx: &Arc<BotContext>,
    r: &MessageReactionUpdated,
) -> anyhow::Result<()> {
    let feedback = match (thumbs(&r.new_reaction), thumbs(&r.old_reaction)) {
        (Some(f), _) => f,
        (None, Some(_)) => Feedback::Clear,
        (None, None) => return Ok(()),
    };
    let message = format!("{}:{}", r.chat.id, r.message_id);
    match submit_feedback(&ctx.core, &message, feedback).await {
        Ok(true) => tracing::info!(bot = %ctx.cfg.name, %message, ?feedback, "feedback sent"),
        Ok(false) => {
            tracing::debug!(%message, "reaction on a message that holds no rateable answer")
        }
        Err(e) => tracing::warn!(error = %e, %message, "feedback failed"),
    }
    Ok(())
}
