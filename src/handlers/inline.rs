//! Inline mode: `@bot question?` from any chat.

use anyhow::Result;
use frankenstein::ParseMode;
use frankenstein::inline_mode::{
    InlineQuery, InlineQueryResult, InlineQueryResultArticle, InputMessageContent,
    InputTextMessageContent,
};
use std::sync::Arc;
use std::time::Duration;

use crate::app::BotContext;
use crate::docsgpt::StreamRequest;
use crate::telegram::render;
use crate::util;

fn article(
    id: &str,
    title: &str,
    description: Option<&str>,
    text: &str,
    parse_mode: Option<ParseMode>,
) -> InlineQueryResult {
    let content = InputTextMessageContent::builder()
        .message_text(text)
        .maybe_parse_mode(parse_mode)
        .build();
    let article = InlineQueryResultArticle::builder()
        .id(id)
        .title(title)
        .input_message_content(InputMessageContent::Text(content))
        .maybe_description(description.map(str::to_string))
        .build();
    InlineQueryResult::Article(article)
}

pub async fn handle(ctx: &Arc<BotContext>, q: InlineQuery) -> Result<()> {
    if !ctx.cfg.inline {
        return Ok(());
    }
    let query = q.query.trim().to_string();
    if query.chars().count() < 3 {
        return ctx.tg.answer_inline_query(&q.id, vec![]).await;
    }
    if !query.ends_with('?') {
        let hint = article(
            "hint",
            "Finish with ? to get an answer",
            Some(&query),
            &query,
            None,
        );
        return ctx.tg.answer_inline_query(&q.id, vec![hint]).await;
    }
    let (agent, question) = match query.strip_prefix('#') {
        Some(rest) => match rest.split_once(char::is_whitespace) {
            Some((tag, remainder)) if ctx.cfg.agent(tag).is_some() => (
                ctx.cfg.agent(tag).unwrap().clone(),
                remainder.trim().to_string(),
            ),
            _ => (ctx.cfg.default_agent().clone(), query.clone()),
        },
        None => (ctx.cfg.default_agent().clone(), query.clone()),
    };
    let req = StreamRequest {
        question: question.clone(),
        api_key: agent.api_key.clone(),
        conversation_id: None,
        attachments: vec![],
    };
    let result = match ctx.docsgpt.answer(&req, Duration::from_secs(25)).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "inline answer failed");
            let err = article(
                "error",
                "Couldn't get an answer right now",
                Some("Try again in a moment"),
                &query,
                None,
            );
            return ctx.tg.answer_inline_query(&q.id, vec![err]).await;
        }
    };
    let (text, _) = render::strip_images(&result.answer);
    let plain = render::markdown_to_plain(&text);
    let preview = util::truncate_chars(&plain.replace('\n', " "), 100);
    let v2 = render::markdown_v2_messages(
        &format!("*Q:* {}\n\n{}", question, text),
        render::TG_TEXT_LIMIT,
    )
    .into_iter()
    .next()
    .unwrap_or_default();
    let mut results = vec![article(
        "answer",
        "Answer",
        Some(&preview),
        &v2,
        Some(ParseMode::MarkdownV2),
    )];
    results.push(article(
        "answer_plain",
        "Answer (plain text)",
        Some(&preview),
        &util::truncate_chars(&format!("Q: {question}\n\n{plain}"), render::TG_TEXT_LIMIT),
        None,
    ));
    ctx.tg.answer_inline_query(&q.id, results).await
}
