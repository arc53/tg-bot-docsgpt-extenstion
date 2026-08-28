//! Group behaviour: mention gating and topic resolution.

use frankenstein::types::{Chat, ChatType, Message, MessageEntity, MessageEntityType};

use crate::app::BotContext;
use crate::config::GroupMode;

pub fn is_group(chat: &Chat) -> bool {
    matches!(chat.type_field, ChatType::Group | ChatType::Supergroup)
}

pub fn is_private(chat: &Chat) -> bool {
    matches!(chat.type_field, ChatType::Private)
}

/// The topic a message belongs to, when topics are in play.
pub fn thread_id(msg: &Message) -> Option<i32> {
    if msg.is_topic_message == Some(true) || is_private(&msg.chat) {
        msg.message_thread_id
    } else {
        None
    }
}

pub fn text_of(msg: &Message) -> Option<&str> {
    msg.text.as_deref().or(msg.caption.as_deref())
}

fn entities_of(msg: &Message) -> &[MessageEntity] {
    if msg.text.is_some() {
        msg.entities.as_deref().unwrap_or(&[])
    } else {
        msg.caption_entities.as_deref().unwrap_or(&[])
    }
}

/// Remove `@username` mentions of this bot from the text.
pub fn strip_mention(text: &str, username: &str) -> String {
    if username.is_empty() {
        return text.trim().to_string();
    }
    let needle = format!("@{}", username.to_ascii_lowercase());
    let mut out = String::with_capacity(text.len());
    let lower = text.to_ascii_lowercase();
    let mut i = 0;
    while i < text.len() {
        if lower[i..].starts_with(&needle) {
            let end = i + needle.len();
            let boundary = text[end..]
                .chars()
                .next()
                .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
            if boundary {
                i = end;
                continue;
            }
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Does this message mention the bot or reply to it?
pub fn addressed_to_bot(ctx: &BotContext, msg: &Message) -> bool {
    let me = ctx.me.id;
    if let Some(reply) = &msg.reply_to_message
        && reply.from.as_ref().is_some_and(|u| u.id == me)
    {
        return true;
    }
    let text = text_of(msg).unwrap_or("");
    let username = ctx.username().to_ascii_lowercase();
    let utf16: Vec<u16> = text.encode_utf16().collect();
    for e in entities_of(msg) {
        let start = e.offset as usize;
        let end = (e.offset as usize + e.length as usize).min(utf16.len());
        if start >= end {
            continue;
        }
        match e.type_field {
            MessageEntityType::Mention => {
                let m = String::from_utf16_lossy(&utf16[start..end]).to_ascii_lowercase();
                if !username.is_empty() && m == format!("@{username}") {
                    return true;
                }
            }
            MessageEntityType::TextMention if e.user.as_ref().is_some_and(|u| u.id == me) => {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// In a group, decide whether to answer and return the cleaned text.
pub fn gate(ctx: &BotContext, msg: &Message) -> Option<String> {
    let raw = text_of(msg).unwrap_or("");
    match ctx.cfg.groups {
        GroupMode::Off => None,
        GroupMode::All => Some(strip_mention(raw, ctx.username())),
        GroupMode::Mention => {
            if addressed_to_bot(ctx, msg) {
                Some(strip_mention(raw, ctx.username()))
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_mentions() {
        assert_eq!(
            strip_mention("@MyBot hello  there @mybot", "MyBot"),
            "hello there"
        );
        assert_eq!(strip_mention("@mybotx stays", "mybot"), "@mybotx stays");
        assert_eq!(strip_mention("plain", ""), "plain");
    }
}
