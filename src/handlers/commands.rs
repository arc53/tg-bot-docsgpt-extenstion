//! Slash commands.

use anyhow::Result;
use frankenstein::ParseMode;
use frankenstein::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, Message, MessageEntityType, ReplyMarkup,
};

use crate::app::BotContext;
use crate::handlers::{chat, groups, message};
use crate::storage::{ChatScope, ChatStatePatch};
use crate::telegram::raw::EphemeralMessageParameters;
use crate::telegram::render::escape_v2;

/// Extract `(command, args)` when the message starts with a command for this bot.
pub fn parse(msg: &Message, username: &str) -> Option<(String, String)> {
    let text = msg.text.as_deref()?;
    let is_cmd = msg.entities.as_ref().is_some_and(|es| {
        es.iter()
            .any(|e| e.offset == 0 && matches!(e.type_field, MessageEntityType::BotCommand))
    }) || text.starts_with('/');
    if !is_cmd || !text.starts_with('/') {
        return None;
    }
    let (head, rest) = match text.find(char::is_whitespace) {
        Some(i) => (&text[..i], text[i..].trim()),
        None => (text, ""),
    };
    let head = &head[1..];
    let (cmd, target) = match head.split_once('@') {
        Some((c, t)) => (c, Some(t)),
        None => (head, None),
    };
    if let Some(t) = target
        && !t.eq_ignore_ascii_case(username)
    {
        return None;
    }
    Some((cmd.to_ascii_lowercase(), rest.to_string()))
}

fn agents_list(ctx: &BotContext, active: &str) -> String {
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

pub fn agents_keyboard(ctx: &BotContext, active: &str) -> ReplyMarkup {
    let rows: Vec<Vec<InlineKeyboardButton>> = ctx
        .cfg
        .agents
        .iter()
        .map(|a| {
            let label = if a.name == active {
                format!("✓ {}", a.name)
            } else {
                a.name.clone()
            };
            vec![
                InlineKeyboardButton::builder()
                    .text(label)
                    .callback_data(format!("agent:{}", a.name))
                    .build(),
            ]
        })
        .collect();
    ReplyMarkup::InlineKeyboardMarkup(
        InlineKeyboardMarkup::builder()
            .inline_keyboard(rows)
            .build(),
    )
}

pub async fn active_agent_name(ctx: &BotContext, scope: &ChatScope) -> String {
    let st = ctx
        .app
        .storage
        .get_chat_state(scope)
        .await
        .unwrap_or_default();
    st.active_agent
        .filter(|a| ctx.cfg.agent(a).is_some())
        .unwrap_or_else(|| ctx.cfg.default_agent().name.clone())
}

/// Reply privately in groups when possible, otherwise normally.
async fn reply(
    ctx: &BotContext,
    msg: &Message,
    ephemeral_id: Option<i32>,
    text: &str,
    markup: Option<ReplyMarkup>,
) -> Result<()> {
    let mut target = message::target_for(msg);
    if groups::is_group(&msg.chat)
        && let Some(user) = &msg.from
    {
        let _ = ephemeral_id;
        target.ephemeral = Some(EphemeralMessageParameters {
            receiver_user_id: user.id,
            callback_query_id: None,
            replace_callback_query_message: None,
        });
        if ctx
            .tg
            .send_text(&target, text, Some(ParseMode::MarkdownV2), markup.clone())
            .await
            .is_ok()
        {
            return Ok(());
        }
        target.ephemeral = None;
    }
    if let Err(e) = ctx
        .tg
        .send_text(&target, text, Some(ParseMode::MarkdownV2), markup.clone())
        .await
    {
        tracing::debug!(error = %e, "MarkdownV2 command reply failed; sending plain");
        let plain = crate::telegram::render::markdown_to_plain(&text.replace('\\', ""));
        ctx.tg.send_text(&target, &plain, None, markup).await?;
    }
    Ok(())
}

pub fn help_text(ctx: &BotContext) -> String {
    let mut lines = vec![
        format!("*{}*", escape_v2(ctx.title())),
        String::new(),
        escape_v2("Send a question and I'll answer from the connected knowledge base."),
        escape_v2(
            "You can also send photos, documents or voice notes — I'll read or listen to them.",
        ),
        String::new(),
        "*Commands*".to_string(),
        escape_v2("/new — start a new conversation"),
        escape_v2("/help — this message"),
    ];
    if ctx.cfg.agents.len() > 1 {
        lines.push(escape_v2("/agents — choose which agent answers"));
        lines.push(escape_v2("/agent <name> — switch agent"));
        lines.push(escape_v2("#name question — ask one agent just once"));
    }
    if !ctx.username().is_empty() {
        lines.push(String::new());
        lines.push(escape_v2(&format!(
            "In groups, mention @{} or reply to one of my messages.",
            ctx.username()
        )));
    }
    lines.join("\n")
}

pub async fn handle(
    ctx: &std::sync::Arc<BotContext>,
    msg: &Message,
    cmd: &str,
    args: &str,
    ephemeral_id: Option<i32>,
) -> Result<()> {
    let scope = message::scope_for(ctx, msg);
    match cmd {
        "start" => {
            if let Some(name) = args
                .strip_prefix("agent_")
                .or_else(|| args.strip_prefix("agent-"))
                && ctx.cfg.agent(name).is_some()
            {
                ctx.app
                    .storage
                    .update_chat_state(
                        &scope,
                        ChatStatePatch::active_agent(Some(&name.to_ascii_lowercase())),
                    )
                    .await?;
            }
            let active = active_agent_name(ctx, &scope).await;
            let first = msg
                .from
                .as_ref()
                .map(|u| u.first_name.clone())
                .unwrap_or_else(|| "there".into());
            let text = match &ctx.cfg.welcome {
                Some(w) => escape_v2(
                    &w.replace("{name}", &first)
                        .replace("{agents}", &agents_list(ctx, &active)),
                ),
                None => {
                    let mut t = format!("Hi {}\\! Ask me anything\\.", escape_v2(&first));
                    if ctx.cfg.agents.len() > 1 {
                        t.push_str(&format!(
                            "\n\n*Agents*\n{}\n\nUse /agents to switch\\.",
                            escape_v2(&agents_list(ctx, &active))
                        ));
                    }
                    t
                }
            };
            reply(ctx, msg, ephemeral_id, &text, None).await
        }
        "help" => reply(ctx, msg, ephemeral_id, &help_text(ctx), None).await,
        "agents" => {
            let active = active_agent_name(ctx, &scope).await;
            if ctx.cfg.agents.len() <= 1 {
                return reply(
                    ctx,
                    msg,
                    ephemeral_id,
                    &escape_v2("Only one agent is configured here."),
                    None,
                )
                .await;
            }
            let text = format!(
                "*Agents*\n{}\n\nTap to switch\\.",
                escape_v2(&agents_list(ctx, &active))
            );
            reply(
                ctx,
                msg,
                ephemeral_id,
                &text,
                Some(agents_keyboard(ctx, &active)),
            )
            .await
        }
        "agent" => {
            let name = args.trim().trim_start_matches('#').to_ascii_lowercase();
            if name.is_empty() {
                let active = active_agent_name(ctx, &scope).await;
                return reply(
                    ctx,
                    msg,
                    ephemeral_id,
                    &escape_v2(&format!(
                        "Current agent: {active}. Use /agent <name> to switch."
                    )),
                    None,
                )
                .await;
            }
            match ctx.cfg.agent(&name) {
                Some(a) => {
                    ctx.app
                        .storage
                        .update_chat_state(&scope, ChatStatePatch::active_agent(Some(&a.name)))
                        .await?;
                    reply(
                        ctx,
                        msg,
                        ephemeral_id,
                        &escape_v2(&format!("Switched to {}.", a.name)),
                        None,
                    )
                    .await
                }
                None => {
                    let names = ctx
                        .cfg
                        .agents
                        .iter()
                        .map(|a| a.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    reply(
                        ctx,
                        msg,
                        ephemeral_id,
                        &escape_v2(&format!("Unknown agent {name:?}. Available: {names}")),
                        None,
                    )
                    .await
                }
            }
        }
        "new" | "reset" | "clear" => {
            let active = active_agent_name(ctx, &scope).await;
            ctx.app.storage.clear_conversation(&scope, &active).await?;
            let text = if ctx.cfg.agents.len() > 1 {
                format!("Started a new conversation with {active}.")
            } else {
                "Started a new conversation.".to_string()
            };
            reply(ctx, msg, ephemeral_id, &escape_v2(&text), None).await
        }
        "regen" | "regenerate" => {
            let st = ctx
                .app
                .storage
                .get_chat_state(&scope)
                .await
                .unwrap_or_default();
            match st.last_question {
                Some(q) => chat::ask_from_message(ctx, msg, q, vec![], None).await,
                None => {
                    reply(
                        ctx,
                        msg,
                        ephemeral_id,
                        &escape_v2("Nothing to regenerate yet."),
                        None,
                    )
                    .await
                }
            }
        }
        _ => Ok(()),
    }
}
