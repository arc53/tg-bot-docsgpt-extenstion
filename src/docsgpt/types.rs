use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One server-sent event from `/stream`.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// First event: ids reserved for this turn.
    MessageId {
        message_id: String,
        conversation_id: Option<String>,
    },
    /// Answer text delta.
    Answer(String),
    /// Reasoning text delta (models with visible thinking).
    Thought(String),
    /// Retrieved sources (sent once, before the end).
    Source(Vec<Source>),
    /// Incremental tool call state (`pending` → `completed`).
    ToolCall(ToolCall),
    /// Final summary of all tool calls made during the turn.
    ToolCalls(Vec<ToolCall>),
    /// Conversation id to reuse for the next turn.
    ConversationId(String),
    /// Full structured (JSON-schema) answer, when the agent is configured for it.
    StructuredAnswer(String),
    Notice(String),
    Error(String),
    End,
    /// Anything we don't model (guardrail, workflow_run, …).
    Other(String, Value),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Source {
    pub title: String,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ArtifactRef {
    pub id: String,
    pub filename: String,
    #[serde(default)]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct ToolCall {
    #[serde(default)]
    pub tool_name: String,
    #[serde(default)]
    pub call_id: String,
    #[serde(default)]
    pub action_name: String,
    #[serde(default)]
    pub arguments: Value,
    /// Python `repr` of the tool result, or JSON.
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub artifact_id: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<RawArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
pub struct RawArtifact {
    #[serde(default, alias = "artifact_id")]
    pub id: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub mime_type: Option<String>,
}

/// Files and images a tool produced that the bot should deliver.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolOutputs {
    pub artifacts: Vec<ArtifactRef>,
    pub image_urls: Vec<String>,
}

impl ToolOutputs {
    pub fn merge(&mut self, other: ToolOutputs) {
        for a in other.artifacts {
            if !self.artifacts.iter().any(|x| x.id == a.id) {
                self.artifacts.push(a);
            }
        }
        for u in other.image_urls {
            if !self.image_urls.contains(&u) {
                self.image_urls.push(u);
            }
        }
    }
    pub fn is_empty(&self) -> bool {
        self.artifacts.is_empty() && self.image_urls.is_empty()
    }
}

impl ToolCall {
    pub fn is_completed(&self) -> bool {
        self.status == "completed"
    }

    /// Short label for progress indicators ("Running code", "Generating image").
    pub fn label(&self) -> String {
        match (self.tool_name.as_str(), self.action_name.as_str()) {
            ("code_executor", _) => "Running code".into(),
            ("imagegen", _) | (_, "imagegen_generate") => "Generating image".into(),
            (t, a) if !a.is_empty() => format!("Using {} ({})", pretty(t), pretty(a)),
            (t, _) => format!("Using {}", pretty(t)),
        }
    }

    /// Extract artifacts and image URLs from the explicit fields and the result payload.
    pub fn outputs(&self) -> ToolOutputs {
        let mut out = ToolOutputs::default();
        for a in &self.artifacts {
            if !a.id.is_empty() {
                out.artifacts.push(ArtifactRef {
                    id: a.id.clone(),
                    filename: if a.filename.is_empty() {
                        a.id.clone()
                    } else {
                        a.filename.clone()
                    },
                    mime_type: a.mime_type.clone(),
                });
            }
        }
        if let Some(res) = &self.result {
            if let Some(v) = crate::util::python_repr_to_json(res) {
                if let Some(list) = v.get("artifacts").and_then(Value::as_array) {
                    for a in list {
                        let id = a
                            .get("artifact_id")
                            .or_else(|| a.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if id.is_empty() || out.artifacts.iter().any(|x| x.id == id) {
                            continue;
                        }
                        out.artifacts.push(ArtifactRef {
                            id: id.to_string(),
                            filename: a
                                .get("filename")
                                .and_then(Value::as_str)
                                .unwrap_or(id)
                                .to_string(),
                            mime_type: a
                                .get("mime_type")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        });
                    }
                }
                for key in ["image_urls", "urls"] {
                    if let Some(list) = v.get(key).and_then(Value::as_array) {
                        for u in list.iter().filter_map(Value::as_str) {
                            if u.starts_with("http") && !out.image_urls.iter().any(|x| x == u) {
                                out.image_urls.push(u.to_string());
                            }
                        }
                    }
                }
                if let Some(u) = v.get("image_url").and_then(Value::as_str)
                    && u.starts_with("http")
                    && !out.image_urls.iter().any(|x| x == u)
                {
                    out.image_urls.push(u.to_string());
                }
            }
            // Fill in mime types the explicit list lacked.
            if let Some(v) = crate::util::python_repr_to_json(res)
                && let Some(list) = v.get("artifacts").and_then(Value::as_array)
            {
                for a in list {
                    let id = a
                        .get("artifact_id")
                        .or_else(|| a.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if let Some(target) = out.artifacts.iter_mut().find(|x| x.id == id)
                        && target.mime_type.is_none()
                    {
                        target.mime_type = a
                            .get("mime_type")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                }
            }
        }
        if let Some(id) = &self.artifact_id
            && !id.is_empty()
            && !out.artifacts.iter().any(|x| &x.id == id)
        {
            out.artifacts.push(ArtifactRef {
                id: id.clone(),
                filename: id.clone(),
                mime_type: None,
            });
        }
        out
    }
}

fn pretty(s: &str) -> String {
    s.replace('_', " ")
}

impl Event {
    /// Parse the JSON payload of one `data:` line.
    pub fn parse(data: &str) -> Option<Event> {
        let v: Value = serde_json::from_str(data).ok()?;
        let ty = v
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Some(match ty.as_str() {
            "answer" => Event::Answer(str_field(&v, "answer")),
            "thought" => Event::Thought(str_field(&v, "thought")),
            "message_id" => Event::MessageId {
                message_id: str_field(&v, "message_id"),
                conversation_id: v
                    .get("conversation_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            "id" => Event::ConversationId(str_field(&v, "id")),
            "end" => Event::End,
            "error" => Event::Error(match v.get("error") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => "unknown error".into(),
            }),
            "notice" => Event::Notice(str_field(&v, "notice")),
            "structured_answer" => Event::StructuredAnswer(match v.get("answer") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            }),
            "source" | "sources" => {
                let list = v
                    .get("source")
                    .or_else(|| v.get("sources"))
                    .and_then(Value::as_array);
                Event::Source(
                    list.map(|l| l.iter().filter_map(parse_source).collect())
                        .unwrap_or_default(),
                )
            }
            "tool_call" => {
                let data = v.get("data").cloned().unwrap_or(Value::Null);
                Event::ToolCall(serde_json::from_value(data).unwrap_or_default())
            }
            "tool_calls" => {
                let list = v.get("tool_calls").cloned().unwrap_or(Value::Array(vec![]));
                Event::ToolCalls(serde_json::from_value(list).unwrap_or_default())
            }
            _ => Event::Other(ty, v),
        })
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn parse_source(v: &Value) -> Option<Source> {
    let title = v
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| v.get("source").and_then(Value::as_str))
        .unwrap_or("Source")
        .to_string();
    let url = ["link", "url", "source"]
        .iter()
        .filter_map(|k| v.get(*k).and_then(Value::as_str))
        .find(|s| s.starts_with("http://") || s.starts_with("https://"))
        .map(str::to_string);
    Some(Source { title, url })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_core_events() {
        assert_eq!(
            Event::parse(r#"{"type": "answer", "answer": "Hi"}"#),
            Some(Event::Answer("Hi".into()))
        );
        assert_eq!(Event::parse(r#"{"type": "end"}"#), Some(Event::End));
        assert_eq!(
            Event::parse(r#"{"type": "id", "id": "c1"}"#),
            Some(Event::ConversationId("c1".into()))
        );
        assert_eq!(
            Event::parse(r#"{"type": "message_id", "message_id": "m", "conversation_id": "c"}"#),
            Some(Event::MessageId {
                message_id: "m".into(),
                conversation_id: Some("c".into())
            })
        );
        match Event::parse(
            r#"{"type": "source", "source": [{"title": "Doc", "link": "https://x"}, {"title": ""}]}"#,
        ) {
            Some(Event::Source(s)) => {
                assert_eq!(
                    s[0],
                    Source {
                        title: "Doc".into(),
                        url: Some("https://x".into())
                    }
                );
                assert_eq!(s[1].title, "Source");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn extracts_artifacts_from_tool_call() {
        let raw = r#"{"type": "tool_call", "data": {"tool_name": "code_executor", "call_id": "c", "action_name": "run_code", "arguments": {"code": "x"}, "artifact_id": "dfb9", "artifacts": [{"id": "dfb9", "filename": "hello.txt", "ref": "A1"}], "result": "{'status': 'ok', 'stdout_tail': '', 'artifacts': [{'artifact_id': 'dfb9', 'version': 1, 'filename': 'hello.txt', 'mime_type': 'text/plain', 'size': 18, 'ref': 'A1'}]}", "status": "completed"}}"#;
        let Some(Event::ToolCall(tc)) = Event::parse(raw) else {
            panic!()
        };
        assert!(tc.is_completed());
        let out = tc.outputs();
        assert_eq!(out.artifacts.len(), 1);
        assert_eq!(out.artifacts[0].filename, "hello.txt");
        assert_eq!(out.artifacts[0].mime_type.as_deref(), Some("text/plain"));
        assert_eq!(tc.label(), "Running code");
    }

    #[test]
    fn extracts_image_urls() {
        let raw = r#"{"type": "tool_call", "data": {"tool_name": "imagegen", "call_id": "c", "action_name": "imagegen_generate", "arguments": {"prompt": "cat"}, "result": "{'status_code': 200, 'image_urls': ['https://artefacts.docsgpt.cloud/images/a.png'], 'message': 'respond like this: ![image]({image_url})'}", "status": "completed"}}"#;
        let Some(Event::ToolCall(tc)) = Event::parse(raw) else {
            panic!()
        };
        let out = tc.outputs();
        assert_eq!(
            out.image_urls,
            vec!["https://artefacts.docsgpt.cloud/images/a.png".to_string()]
        );
        assert_eq!(tc.label(), "Generating image");
    }

    #[test]
    fn unknown_event_is_other() {
        match Event::parse(r#"{"type": "guardrail", "guardrail": {"x": 1}}"#) {
            Some(Event::Other(t, _)) => assert_eq!(t, "guardrail"),
            other => panic!("{other:?}"),
        }
    }
}
