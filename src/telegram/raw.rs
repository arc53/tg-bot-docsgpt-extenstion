//! Parameters for Bot API fields newer than the frankenstein release we pin
//! (Bot API 10.3: draft stop button, ephemeral messages, stop updates).

use frankenstein::ParseMode;
use frankenstein::types::{Chat, MessageEntity, ReplyMarkup, ReplyParameters};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Default)]
pub struct EphemeralMessageParameters {
    pub receiver_user_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_query_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replace_callback_query_message: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct LinkPreviewOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_disabled: Option<bool>,
}

/// `sendMessage` with Bot API 10.2 ephemeral parameters.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SendMessageRaw {
    pub chat_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_thread_id: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub business_connection_id: Option<String>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_mode: Option<ParseMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entities: Option<Vec<MessageEntity>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_preview_options: Option<LinkPreviewOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_parameters: Option<ReplyParameters>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_markup: Option<ReplyMarkup>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ephemeral_message_parameters: Option<EphemeralMessageParameters>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct InputRichMessageRaw {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
}

/// `sendRichMessage` with ephemeral parameters.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SendRichMessageRaw {
    pub chat_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_thread_id: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub business_connection_id: Option<String>,
    pub rich_message: InputRichMessageRaw,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_parameters: Option<ReplyParameters>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_markup: Option<ReplyMarkup>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ephemeral_message_parameters: Option<EphemeralMessageParameters>,
}

/// `sendMessageDraft` with Bot API 10.3 `can_stop` / `keep_on_stop`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SendMessageDraftRaw {
    pub chat_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_thread_id: Option<i32>,
    pub draft_id: i64,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_mode: Option<ParseMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_stop: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_on_stop: Option<bool>,
}

/// `sendRichMessageDraft` with Bot API 10.3 `can_stop` / `keep_on_stop`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SendRichMessageDraftRaw {
    pub chat_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_thread_id: Option<i32>,
    pub draft_id: i64,
    pub rich_message: InputRichMessageRaw,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_stop: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_on_stop: Option<bool>,
}

/// Update payload for `stopped_message_generation` (Bot API 10.3).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct MessageGenerationStopped {
    pub chat: Chat,
    #[serde(default)]
    pub message_thread_id: Option<i32>,
    pub draft_id: i64,
}

/// Every update type we ask Telegram for, including ones the pinned crate
/// cannot deserialize (handled by [`parse_update`]).
pub const ALLOWED_UPDATES: &[&str] = &[
    "message",
    "edited_message",
    "business_connection",
    "business_message",
    "guest_message",
    "message_reaction",
    "inline_query",
    "callback_query",
    "my_chat_member",
    "stopped_message_generation",
];

#[derive(Debug)]
pub enum Incoming {
    Update(Box<frankenstein::updates::Update>, Value),
    StoppedGeneration(MessageGenerationStopped),
    Unknown(Value),
}

/// Parse one raw update object, tolerating kinds the crate doesn't model.
pub fn parse_update(v: Value) -> (i64, Incoming) {
    let update_id = v.get("update_id").and_then(Value::as_i64).unwrap_or(0);
    if let Some(s) = v.get("stopped_message_generation")
        && let Ok(parsed) = serde_json::from_value::<MessageGenerationStopped>(s.clone())
    {
        return (update_id, Incoming::StoppedGeneration(parsed));
    }
    match serde_json::from_value::<frankenstein::updates::Update>(v.clone()) {
        Ok(u) => (update_id, Incoming::Update(Box::new(u), v)),
        Err(e) => {
            let kind = v
                .as_object()
                .map(|o| {
                    o.keys()
                        .filter(|k| *k != "update_id")
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            tracing::debug!(update_id, kind, error = %e, "unparsed update");
            (update_id, Incoming::Unknown(v))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_stop_update() {
        let v = json!({"update_id": 7, "stopped_message_generation": {"chat": {"id": 5, "type": "private"}, "draft_id": 99}});
        let (id, inc) = parse_update(v);
        assert_eq!(id, 7);
        match inc {
            Incoming::StoppedGeneration(s) => {
                assert_eq!(s.draft_id, 99);
                assert_eq!(s.chat.id, 5);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_message_update() {
        let v = json!({"update_id": 1, "message": {"message_id": 3, "date": 1, "chat": {"id": 5, "type": "private"}, "text": "hi"}});
        let (_, inc) = parse_update(v);
        assert!(matches!(inc, Incoming::Update(_, _)));
    }

    #[test]
    fn unknown_update_is_tolerated() {
        let v = json!({"update_id": 2, "some_future_update": {"x": 1}});
        let (id, inc) = parse_update(v);
        assert_eq!(id, 2);
        assert!(matches!(inc, Incoming::Unknown(_)));
    }

    #[test]
    fn draft_params_serialize_new_fields() {
        let p = SendMessageDraftRaw {
            chat_id: 1,
            draft_id: 2,
            text: "".into(),
            can_stop: Some(true),
            ..Default::default()
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"can_stop\":true"));
        assert!(!s.contains("keep_on_stop"));
    }
}
