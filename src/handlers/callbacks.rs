//! Inline keyboard callbacks.

use anyhow::Result;
use frankenstein::ParseMode;
use frankenstein::types::{CallbackQuery, MaybeInaccessibleMessage};

use crate::app::BotContext;
use crate::handlers::{chat, commands, groups, message};
use crate::storage::{ChatScope, ChatStatePatch};
use crate::telegram::render::escape_v2;

pub async fn handle(ctx: &std::sync::Arc<BotContext>, q: CallbackQuery) -> Result<()> {
    let data = q.data.clone().unwrap_or_default();
    let msg = match &q.message {
        Some(MaybeInaccessibleMessage::Message(m)) => Some(m.as_ref().clone()),
        _ => None,
    };
    let Some(msg) = msg else {
        ctx.tg.answer_callback(&q.id, None).await?;
        return Ok(());
    };
    let scope = match &msg.business_connection_id {
        Some(bc) => ChatScope::business(&ctx.cfg.name, msg.chat.id, bc),
        None => ChatScope::new(&ctx.cfg.name, msg.chat.id, groups::thread_id(&msg)),
    };

    if let Some(name) = data.strip_prefix("agent:") {
        match ctx.cfg.agent(name) {
            Some(a) => {
                ctx.app
                    .storage
                    .update_chat_state(&scope, ChatStatePatch::active_agent(Some(&a.name)))
                    .await?;
                ctx.tg
                    .answer_callback(&q.id, Some(&format!("Switched to {}", a.name)))
                    .await?;
                let text = format!(
                    "*Agents*\n{}\n\nNow answering as *{}*\\.",
                    escape_v2(&agents_list_for(ctx, &a.name)),
                    escape_v2(&a.name)
                );
                let markup = match commands::agents_keyboard(ctx, &a.name) {
                    frankenstein::types::ReplyMarkup::InlineKeyboardMarkup(m) => Some(m),
                    _ => None,
                };
                if let Err(e) = ctx
                    .tg
                    .edit_text(
                        msg.chat.id,
                        msg.message_id,
                        &text,
                        Some(ParseMode::MarkdownV2),
                        markup,
                    )
                    .await
                {
                    tracing::debug!(error = %e, "could not edit agent picker");
                }
            }
            None => {
                ctx.tg
                    .answer_callback(&q.id, Some("That agent is no longer configured"))
                    .await?
            }
        }
        return Ok(());
    }
    match data.as_str() {
        "new" => {
            let active = commands::active_agent_name(ctx, &scope).await;
            ctx.app.storage.clear_conversation(&scope, &active).await?;
            ctx.tg
                .answer_callback(&q.id, Some("Started a new conversation"))
                .await?;
        }
        "regen" => {
            ctx.tg.answer_callback(&q.id, None).await?;
            let st = ctx
                .app
                .storage
                .get_chat_state(&scope)
                .await
                .unwrap_or_default();
            if let Some(question) = st.last_question {
                // Re-ask on behalf of the user who pressed the button.
                let mut m = msg.clone();
                m.from = Some(Box::new(q.from.clone()));
                m.text = Some(question.clone());
                let user = message::user_info(&m);
                chat::ask_from_message(ctx, &m, question, vec![], user).await?;
            }
        }
        _ => ctx.tg.answer_callback(&q.id, None).await?,
    }
    Ok(())
}

fn agents_list_for(ctx: &BotContext, active: &str) -> String {
    ctx.cfg
        .agents
        .iter()
        .map(|a| {
            let mark = if a.name == active { "✓ " } else { "" };
            match &a.description {
                Some(d) => format!("{mark}{} — {d}", a.name),
                None => format!("{mark}{}", a.name),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
