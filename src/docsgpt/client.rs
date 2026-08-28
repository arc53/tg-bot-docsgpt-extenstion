use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::pin::Pin;
use std::time::Duration;

use super::sse::SseParser;
use super::types::Event;

pub type EventStream = Pin<Box<dyn Stream<Item = Result<Event>> + Send>>;

#[derive(Clone)]
pub struct DocsGpt {
    http: reqwest::Client,
    base: String,
}

#[derive(Debug, Clone, Default)]
pub struct StreamRequest {
    pub question: String,
    pub api_key: String,
    pub conversation_id: Option<String>,
    pub attachments: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StoredAttachment {
    #[serde(default)]
    pub attachment_id: String,
    #[serde(default)]
    pub task_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Downloaded {
    pub filename: String,
    pub bytes: Bytes,
    pub mime: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnswerResponse {
    #[serde(default)]
    pub answer: String,
    #[serde(default)]
    pub conversation_id: Option<String>,
}

const MAX_DOWNLOAD: usize = 50 * 1024 * 1024;

impl DocsGpt {
    pub fn new(http: reqwest::Client, base: &str) -> Self {
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn body(req: &StreamRequest) -> Value {
        let mut body = json!({
            "question": req.question,
            "api_key": req.api_key,
            "history": "[]",
            "conversation_id": req.conversation_id,
        });
        if !req.attachments.is_empty() {
            body["attachments"] = json!(req.attachments);
        }
        body
    }

    /// Open `/stream` and return the parsed event stream.
    pub async fn stream(&self, req: &StreamRequest) -> Result<EventStream> {
        let resp = self
            .http
            .post(format!("{}/stream", self.base))
            .header("Accept", "text/event-stream")
            .json(&Self::body(req))
            .timeout(Duration::from_secs(600))
            .send()
            .await
            .context("connecting to DocsGPT /stream")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "DocsGPT /stream returned {status}: {}",
                crate::util::truncate_chars(&text, 300)
            );
        }
        let bytes = resp.bytes_stream();
        let state = (
            bytes,
            SseParser::default(),
            std::collections::VecDeque::<Event>::new(),
            false,
        );
        let stream = futures_util::stream::unfold(
            state,
            |(mut bytes, mut parser, mut queue, mut done)| async move {
                loop {
                    if let Some(ev) = queue.pop_front() {
                        return Some((Ok(ev), (bytes, parser, queue, done)));
                    }
                    if done {
                        return None;
                    }
                    match bytes.next().await {
                        Some(Ok(chunk)) => {
                            for data in parser.push(&chunk) {
                                if let Some(ev) = Event::parse(&data) {
                                    queue.push_back(ev);
                                }
                            }
                        }
                        Some(Err(e)) => {
                            done = true;
                            return Some((
                                Err(anyhow!("reading DocsGPT stream: {e}")),
                                (bytes, parser, queue, done),
                            ));
                        }
                        None => {
                            done = true;
                            if let Some(data) = parser.finish()
                                && let Some(ev) = Event::parse(&data)
                            {
                                queue.push_back(ev);
                            }
                        }
                    }
                }
            },
        );
        Ok(Box::pin(stream))
    }

    /// Non-streaming `/api/answer` (used for guest and inline replies).
    pub async fn answer(&self, req: &StreamRequest, timeout: Duration) -> Result<AnswerResponse> {
        let resp = self
            .http
            .post(format!("{}/api/answer", self.base))
            .json(&Self::body(req))
            .timeout(timeout)
            .send()
            .await
            .context("calling DocsGPT /api/answer")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!(
                "DocsGPT /api/answer returned {status}: {}",
                crate::util::truncate_chars(&text, 300)
            );
        }
        let parsed: AnswerResponse =
            serde_json::from_str(&text).context("decoding /api/answer response")?;
        Ok(parsed)
    }

    /// Upload a file so it can be referenced by id in `attachments`.
    pub async fn store_attachment(
        &self,
        api_key: &str,
        filename: &str,
        bytes: Bytes,
        mime: Option<&str>,
    ) -> Result<StoredAttachment> {
        let mut part =
            reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(filename.to_string());
        if let Some(m) = mime {
            part = part.mime_str(m).unwrap_or_else(|_| {
                reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(filename.to_string())
            });
        }
        let form = reqwest::multipart::Form::new()
            .text("api_key", api_key.to_string())
            .part("file", part);
        let resp = self
            .http
            .post(format!("{}/api/store_attachment", self.base))
            .multipart(form)
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .context("uploading attachment to DocsGPT")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!(
                "DocsGPT /api/store_attachment returned {status}: {}",
                crate::util::truncate_chars(&text, 300)
            );
        }
        let v: Value = serde_json::from_str(&text).context("decoding store_attachment response")?;
        let mut stored: StoredAttachment =
            serde_json::from_value(v.clone()).unwrap_or(StoredAttachment {
                attachment_id: String::new(),
                task_id: None,
            });
        if stored.attachment_id.is_empty() {
            // Multi-file shape: {"tasks": [{"task_id", "attachment_id"}]}
            if let Some(first) = v
                .get("tasks")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
            {
                stored.attachment_id = first
                    .get("attachment_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                stored.task_id = first
                    .get("task_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
        }
        if stored.attachment_id.is_empty() {
            bail!(
                "DocsGPT did not return an attachment id: {}",
                crate::util::truncate_chars(&text, 300)
            );
        }
        Ok(stored)
    }

    /// Wait until an attachment's processing task finishes (best effort).
    pub async fn wait_for_task(&self, task_id: &str, max_wait: Duration) -> Result<()> {
        let started = std::time::Instant::now();
        let mut delay = Duration::from_millis(700);
        loop {
            let resp = self
                .http
                .get(format!("{}/api/task_status", self.base))
                .query(&[("task_id", task_id)])
                .timeout(Duration::from_secs(20))
                .send()
                .await;
            match resp {
                Ok(r) if r.status().is_success() => {
                    let v: Value = r.json().await.unwrap_or(Value::Null);
                    let status = v
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_ascii_uppercase();
                    match status.as_str() {
                        "SUCCESS" => return Ok(()),
                        "FAILURE" | "REVOKED" => bail!(
                            "attachment processing failed: {}",
                            crate::util::truncate_chars(&v.to_string(), 200)
                        ),
                        _ => {}
                    }
                }
                Ok(r) => {
                    tracing::debug!(status = %r.status(), "task_status not available; continuing without waiting");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    return Ok(());
                }
                Err(e) => {
                    tracing::debug!(error = %e, "task_status request failed; continuing");
                    return Ok(());
                }
            }
            if started.elapsed() > max_wait {
                tracing::warn!(
                    task_id,
                    "attachment task still pending after {:?}; proceeding",
                    max_wait
                );
                return Ok(());
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 3 / 2).min(Duration::from_secs(3));
        }
    }

    /// Speech to text.
    pub async fn stt(
        &self,
        api_key: &str,
        filename: &str,
        bytes: Bytes,
        mime: Option<&str>,
    ) -> Result<String> {
        let mut part =
            reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(filename.to_string());
        if let Some(m) = mime
            && let Ok(p) = reqwest::multipart::Part::bytes(bytes.to_vec())
                .file_name(filename.to_string())
                .mime_str(m)
        {
            part = p;
        }
        let form = reqwest::multipart::Form::new()
            .text("api_key", api_key.to_string())
            .part("file", part);
        let resp = self
            .http
            .post(format!("{}/api/stt", self.base))
            .query(&[("api_key", api_key)])
            .multipart(form)
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .context("calling DocsGPT /api/stt")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!(
                "DocsGPT /api/stt returned {status}: {}",
                crate::util::truncate_chars(&text, 300)
            );
        }
        let v: Value = serde_json::from_str(&text).context("decoding /api/stt response")?;
        let transcript = v
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if transcript.is_empty() {
            bail!("speech recognition returned no text");
        }
        Ok(transcript)
    }

    /// Text to speech; returns audio bytes (typically MP3).
    pub async fn tts(&self, text: &str) -> Result<Bytes> {
        let resp = self
            .http
            .post(format!("{}/api/tts", self.base))
            .json(&json!({ "text": text }))
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .context("calling DocsGPT /api/tts")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("DocsGPT /api/tts returned {status}");
        }
        let v: Value = serde_json::from_str(&body).context("decoding /api/tts response")?;
        let b64 = v
            .get("audio_base64")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("tts response has no audio"))?;
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .context("decoding tts audio")?;
        Ok(Bytes::from(bytes))
    }

    /// Download an artifact produced by a tool.
    /// Requires the conversation the artifact belongs to (agent-key auth is conversation-scoped).
    pub async fn download_artifact(
        &self,
        api_key: &str,
        conversation_id: &str,
        artifact_id: &str,
        filename_hint: &str,
    ) -> Result<Downloaded> {
        let url = format!("{}/api/artifacts/{}/download", self.base, artifact_id);
        let resp = self
            .http
            .get(&url)
            .query(&[("api_key", api_key), ("conversation_id", conversation_id)])
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .context("downloading artifact from DocsGPT")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "artifact download returned {status}: {}",
                crate::util::truncate_chars(&text, 200)
            );
        }
        let mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).trim().to_string());
        let filename = resp
            .headers()
            .get(reqwest::header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_disposition_filename)
            .unwrap_or_else(|| filename_hint.to_string());
        let bytes = read_limited(resp).await?;
        Ok(Downloaded {
            filename,
            bytes,
            mime,
        })
    }

    /// Fetch any public URL (image-generation results live on a public bucket).
    pub async fn fetch_url(&self, url: &str) -> Result<Downloaded> {
        let resp = self
            .http
            .get(url)
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .with_context(|| format!("fetching {url}"))?;
        if !resp.status().is_success() {
            bail!("fetching {url} returned {}", resp.status());
        }
        let mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).trim().to_string());
        let filename = url
            .rsplit('/')
            .next()
            .unwrap_or("file")
            .split('?')
            .next()
            .unwrap_or("file")
            .to_string();
        let bytes = read_limited(resp).await?;
        Ok(Downloaded {
            filename,
            bytes,
            mime,
        })
    }
}

async fn read_limited(resp: reqwest::Response) -> Result<Bytes> {
    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading body")?;
        if buf.len() + chunk.len() > MAX_DOWNLOAD {
            bail!("download exceeds {} MB", MAX_DOWNLOAD / 1024 / 1024);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buf))
}

fn parse_content_disposition_filename(header: &str) -> Option<String> {
    for part in header.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("filename*=") {
            let v = v.trim_matches('"');
            let v = v.rsplit("''").next().unwrap_or(v);
            return Some(percent_decode(v));
        }
        if let Some(v) = part.strip_prefix("filename=") {
            return Some(v.trim_matches('"').to_string());
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition() {
        assert_eq!(
            parse_content_disposition_filename("attachment; filename=\"a b.txt\"").as_deref(),
            Some("a b.txt")
        );
        assert_eq!(
            parse_content_disposition_filename("attachment; filename*=UTF-8''r%C3%A9sum%C3%A9.pdf")
                .as_deref(),
            Some("résumé.pdf")
        );
        assert_eq!(parse_content_disposition_filename("inline"), None);
    }

    #[test]
    fn body_shape() {
        let req = StreamRequest {
            question: "q".into(),
            api_key: "k".into(),
            conversation_id: None,
            attachments: vec!["a".into()],
        };
        let b = DocsGpt::body(&req);
        assert_eq!(b["question"], "q");
        assert!(b["conversation_id"].is_null());
        assert_eq!(b["attachments"][0], "a");
        let req2 = StreamRequest {
            attachments: vec![],
            ..req
        };
        assert!(DocsGpt::body(&req2).get("attachments").is_none());
    }
}
