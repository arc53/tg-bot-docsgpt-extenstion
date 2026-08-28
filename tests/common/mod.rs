//! Shared harness for the end-to-end tests: an in-process mock Telegram Bot
//! API, an in-process mock DocsGPT, and helpers that boot the real bot
//! (`BotContext::init` + `run_polling`) against both.
//!
//! The Telegram mock is one server per test binary (the bot reads
//! `TELEGRAM_API_URL` from the process environment); its state is keyed by bot
//! token so tests can run in parallel. The DocsGPT mock is one server per test,
//! wired in through `BotConfig.api_base`.
#![allow(dead_code, unused_imports)]

use axum::Router;
use axum::body::Body;
use axum::extract::{FromRequest, Multipart, Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

use docsgpt_telegram::app::{AppState, BotContext};
use docsgpt_telegram::config::{
    AgentConfig, BotConfig, Config, GroupMode, Mode, ServerConfig, StorageConfig,
};
use docsgpt_telegram::storage::Storage;
use docsgpt_telegram::storage::memory::MemoryStorage;
use docsgpt_telegram::telegram::runtime;

/// Generous default for waiting on the recorders.
pub const WAIT: Duration = Duration::from_secs(15);
pub const BOT_ID: u64 = 424242;
pub const BOT_USERNAME: &str = "mockbot";
/// Default private chat / user id used by most tests.
pub const CHAT: i64 = 1001;
pub const USER: u64 = 1001;

/// A valid 1x1 transparent PNG.
pub const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

// ---------------------------------------------------------------------------
// Recorder
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct UploadedFile {
    pub filename: String,
    pub content_type: Option<String>,
    pub bytes: Bytes,
}

/// One request seen by a mock.
#[derive(Clone, Debug)]
pub struct Call {
    /// Telegram method name (`sendMessage`) or DocsGPT path (`/stream`).
    pub method: String,
    /// JSON body, or the text fields of a multipart form (JSON-ish values parsed).
    pub body: Value,
    pub query: HashMap<String, String>,
    /// Multipart file parts by field name.
    pub files: HashMap<String, UploadedFile>,
    pub at: Instant,
}

impl Call {
    pub fn new(method: &str, body: Value) -> Self {
        Self {
            method: method.to_string(),
            body,
            query: HashMap::new(),
            files: HashMap::new(),
            at: Instant::now(),
        }
    }
    /// `text` field (sendMessage, drafts, callbacks).
    pub fn text(&self) -> String {
        self.body["text"].as_str().unwrap_or("").to_string()
    }
    /// `rich_message.markdown` field (sendRichMessage, sendRichMessageDraft).
    pub fn markdown(&self) -> String {
        self.body["rich_message"]["markdown"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }
    pub fn file(&self, name: &str) -> &UploadedFile {
        self.files.get(name).unwrap_or_else(|| {
            panic!(
                "no multipart file {name:?} in {} call; file fields: {:?}",
                self.method,
                self.files.keys().collect::<Vec<_>>()
            )
        })
    }
}

#[derive(Default)]
pub struct Recorder {
    calls: Mutex<Vec<Call>>,
}

impl Recorder {
    pub fn record(&self, call: Call) {
        self.calls.lock().unwrap().push(call);
    }
    pub fn all(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
    pub fn calls(&self, method: &str) -> Vec<Call> {
        self.all()
            .into_iter()
            .filter(|c| c.method == method)
            .collect()
    }
    pub fn count(&self, method: &str) -> usize {
        self.calls(method).len()
    }
    pub fn last(&self, method: &str) -> Option<Call> {
        self.calls(method).pop()
    }
    pub fn summary(&self) -> String {
        self.all()
            .iter()
            .map(|c| format!("  {} {}", c.method, truncate(&c.body.to_string(), 200)))
            .collect::<Vec<_>>()
            .join("\n")
    }
    /// Wait until a call to `method` satisfying `pred` has been recorded; returns the first such call.
    pub async fn wait_for(
        &self,
        method: &str,
        pred: impl Fn(&Call) -> bool,
        timeout: Duration,
    ) -> Call {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(c) = self.calls(method).into_iter().find(|c| pred(c)) {
                return c;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out after {timeout:?} waiting for {method}; recorded calls:\n{}",
                    self.summary()
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    pub async fn wait_any(&self, method: &str) -> Call {
        self.wait_for(method, |_| true, WAIT).await
    }
    /// Wait until at least `n` calls to `method` exist; returns all of them.
    pub async fn wait_count(&self, method: &str, n: usize, timeout: Duration) -> Vec<Call> {
        let deadline = Instant::now() + timeout;
        loop {
            let calls = self.calls(method);
            if calls.len() >= n {
                return calls;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out after {timeout:?} waiting for {n} {method} calls (have {}); recorded calls:\n{}",
                    calls.len(),
                    self.summary()
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    /// Sleep for `within`, then assert `method` was never called.
    pub async fn assert_none(&self, method: &str, within: Duration) {
        tokio::time::sleep(within).await;
        let n = self.count(method);
        assert!(
            n == 0,
            "expected no {method} calls, got {n}; recorded calls:\n{}",
            self.summary()
        );
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

// ---------------------------------------------------------------------------
// Body parsing shared by both mocks
// ---------------------------------------------------------------------------

/// Multipart text fields arrive as strings; frankenstein serialises non-string
/// params as JSON text, so parse the ones that look like JSON.
fn loose_json(s: &str) -> Value {
    let t = s.trim();
    let looks_json = t.starts_with('{')
        || t.starts_with('[')
        || t == "true"
        || t == "false"
        || t == "null"
        || t.parse::<i64>().is_ok();
    if looks_json {
        serde_json::from_str(t).unwrap_or_else(|_| Value::String(s.to_string()))
    } else {
        Value::String(s.to_string())
    }
}

async fn read_multipart(mut mp: Multipart) -> (Value, HashMap<String, UploadedFile>) {
    let mut body = serde_json::Map::new();
    let mut files = HashMap::new();
    while let Some(field) = mp.next_field().await.expect("multipart field") {
        let name = field.name().unwrap_or("").to_string();
        let filename = field.file_name().map(str::to_string);
        let content_type = field.content_type().map(str::to_string);
        let bytes = field.bytes().await.expect("multipart field bytes");
        match filename {
            Some(filename) => {
                files.insert(
                    name,
                    UploadedFile {
                        filename,
                        content_type,
                        bytes,
                    },
                );
            }
            None => {
                body.insert(name, loose_json(&String::from_utf8_lossy(&bytes)));
            }
        }
    }
    (Value::Object(body), files)
}

async fn parse_body(req: Request) -> (Value, HashMap<String, UploadedFile>) {
    let ct = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ct.starts_with("multipart/form-data") {
        let mp = Multipart::from_request(req, &())
            .await
            .expect("multipart request");
        read_multipart(mp).await
    } else {
        let bytes = axum::body::to_bytes(req.into_body(), 64 << 20)
            .await
            .unwrap_or_default();
        (
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            HashMap::new(),
        )
    }
}

// ---------------------------------------------------------------------------
// Mock Telegram Bot API
// ---------------------------------------------------------------------------

type RejectRule = Arc<dyn Fn(&Value) -> bool + Send + Sync>;

pub struct MockFile {
    pub path: String,
    pub bytes: Bytes,
}

/// Per-token state of the mock Telegram server.
pub struct TgBot {
    pub token: String,
    /// Every method call except `getUpdates`.
    pub rec: Recorder,
    pending: Mutex<VecDeque<Value>>,
    next_update: AtomicI64,
    next_message: AtomicI32,
    rejects: Mutex<Vec<(String, RejectRule)>>,
    files: Mutex<HashMap<String, MockFile>>,
    business: Mutex<HashMap<String, Value>>,
    wake: Notify,
}

impl TgBot {
    fn new(token: &str) -> Self {
        Self {
            token: token.to_string(),
            rec: Recorder::default(),
            pending: Mutex::new(VecDeque::new()),
            next_update: AtomicI64::new(1),
            next_message: AtomicI32::new(5000),
            rejects: Mutex::new(Vec::new()),
            files: Mutex::new(HashMap::new()),
            business: Mutex::new(HashMap::new()),
            wake: Notify::new(),
        }
    }

    /// Queue an update `{"update_id": n, <kind>: content}` for the next getUpdates.
    pub fn push(&self, kind: &str, content: Value) -> i64 {
        let id = self.next_update.fetch_add(1, Ordering::SeqCst);
        let mut u = serde_json::Map::new();
        u.insert("update_id".into(), json!(id));
        u.insert(kind.to_string(), content);
        self.pending.lock().unwrap().push_back(Value::Object(u));
        self.wake.notify_one();
        id
    }

    /// Answer `method` with HTTP 400 `{"ok":false,...}` from now on.
    pub fn reject(&self, method: &str) {
        self.reject_if(method, |_| true);
    }
    /// Reject `method` only when the request body matches `pred`.
    pub fn reject_if(&self, method: &str, pred: impl Fn(&Value) -> bool + Send + Sync + 'static) {
        self.rejects
            .lock()
            .unwrap()
            .push((method.to_string(), Arc::new(pred)));
    }
    pub fn clear_rejects(&self) {
        self.rejects.lock().unwrap().clear();
    }
    fn rejected(&self, method: &str, body: &Value) -> bool {
        self.rejects
            .lock()
            .unwrap()
            .iter()
            .any(|(m, rule)| m == method && rule(body))
    }

    /// Register a downloadable file: getFile returns `path`, and
    /// `GET /file/bot<token>/<path>` serves `bytes`.
    pub fn add_file(&self, file_id: &str, path: &str, bytes: Bytes) {
        self.files.lock().unwrap().insert(
            file_id.to_string(),
            MockFile {
                path: path.to_string(),
                bytes,
            },
        );
    }
    /// Make `getBusinessConnection` answer with this object.
    pub fn add_business_connection(&self, conn: Value) {
        let id = conn["id"].as_str().unwrap_or("").to_string();
        self.business.lock().unwrap().insert(id, conn);
    }
    pub fn drafts(&self) -> usize {
        self.rec.count("sendMessageDraft") + self.rec.count("sendRichMessageDraft")
    }
}

pub struct TgServer {
    pub port: u16,
    bots: Mutex<HashMap<String, Arc<TgBot>>>,
}

impl TgServer {
    /// State for a token, created on first use.
    pub fn bot(&self, token: &str) -> Arc<TgBot> {
        self.bots
            .lock()
            .unwrap()
            .entry(token.to_string())
            .or_insert_with(|| Arc::new(TgBot::new(token)))
            .clone()
    }
    fn lookup(&self, token: &str) -> Option<Arc<TgBot>> {
        self.bots.lock().unwrap().get(token).cloned()
    }
}

static TG: OnceLock<Arc<TgServer>> = OnceLock::new();

/// The process-wide mock Telegram server. Starting it also sets
/// `TELEGRAM_API_URL` and `TG_RATE_LIMITS=off`, before any bot is constructed.
pub fn telegram() -> Arc<TgServer> {
    TG.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Arc<TgServer>>();
        std::thread::Builder::new()
            .name("mock-telegram".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("mock telegram runtime");
                rt.block_on(async move {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                        .await
                        .expect("bind mock telegram");
                    let port = listener.local_addr().unwrap().port();
                    let server = Arc::new(TgServer {
                        port,
                        bots: Mutex::new(HashMap::new()),
                    });
                    tx.send(server.clone()).unwrap();
                    let app = Router::new().fallback(tg_handler).with_state(server);
                    axum::serve(listener, app)
                        .await
                        .expect("mock telegram server");
                });
            })
            .expect("spawn mock telegram thread");
        let server = rx.recv().expect("mock telegram started");
        // SAFETY: called once, before any bot (and therefore any reader of these
        // variables) exists in this process; guarded by the OnceLock.
        unsafe {
            std::env::set_var(
                "TELEGRAM_API_URL",
                format!("http://127.0.0.1:{}", server.port),
            );
            std::env::set_var("TG_RATE_LIMITS", "off");
        }
        server
    })
    .clone()
}

pub fn bot_user() -> Value {
    json!({"id": BOT_ID, "is_bot": true, "first_name": "Mock Bot", "username": BOT_USERNAME})
}

fn api_ok(result: Value) -> Response {
    axum::Json(json!({"ok": true, "result": result})).into_response()
}

fn api_error(code: u16, description: &str) -> Response {
    let status = StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST);
    (
        status,
        axum::Json(json!({"ok": false, "error_code": code, "description": description})),
    )
        .into_response()
}

fn message_result(bot: &TgBot, body: &Value) -> Value {
    let id = bot.next_message.fetch_add(1, Ordering::SeqCst);
    let chat_id = body.get("chat_id").and_then(Value::as_i64).unwrap_or(0);
    let mut m = json!({"message_id": id, "date": 1, "from": bot_user(), "chat": {"id": chat_id, "type": "private"}});
    if let Some(t) = body.get("text").and_then(Value::as_str) {
        m["text"] = json!(t);
    } else if let Some(md) = body.pointer("/rich_message/markdown") {
        m["text"] = md.clone();
    }
    m
}

/// Long polling: hand out queued updates at or after `offset`, otherwise wait
/// briefly for a push and finally return `[]`.
async fn get_updates(bot: &TgBot, body: &Value) -> Response {
    let offset = body.get("offset").and_then(Value::as_i64);
    let limit = body.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
    let deadline = Instant::now() + Duration::from_millis(300);
    loop {
        let ready: Vec<Value> = {
            let mut q = bot.pending.lock().unwrap();
            if let Some(o) = offset {
                q.retain(|u| u["update_id"].as_i64().unwrap_or(0) >= o);
            }
            q.iter().take(limit).cloned().collect()
        };
        if !ready.is_empty() {
            return api_ok(Value::Array(ready));
        }
        let now = Instant::now();
        if now >= deadline {
            return api_ok(json!([]));
        }
        let _ = tokio::time::timeout(deadline - now, bot.wake.notified()).await;
    }
}

async fn tg_handler(State(srv): State<Arc<TgServer>>, req: Request) -> Response {
    let path = req.uri().path().to_string();

    // File downloads: GET /file/bot<token>/<path>
    if let Some(rest) = path.strip_prefix("/file/bot") {
        let Some((token, fpath)) = rest.split_once('/') else {
            return (StatusCode::NOT_FOUND, "bad file path").into_response();
        };
        let bytes = srv.lookup(token).and_then(|b| {
            b.files
                .lock()
                .unwrap()
                .values()
                .find(|f| f.path == fpath)
                .map(|f| f.bytes.clone())
        });
        return match bytes {
            Some(b) => (StatusCode::OK, b).into_response(),
            None => (StatusCode::NOT_FOUND, "file not found").into_response(),
        };
    }

    // Methods: POST /bot<token>/<method>
    let Some(rest) = path.strip_prefix("/bot") else {
        return api_error(404, "Not Found");
    };
    let Some((token, method)) = rest.split_once('/') else {
        return api_error(404, "Not Found");
    };
    let (token, method) = (token.to_string(), method.to_string());
    let Some(bot) = srv.lookup(&token) else {
        return api_error(401, "Unauthorized");
    };
    let (body, files) = parse_body(req).await;

    if method == "getUpdates" {
        return get_updates(&bot, &body).await;
    }
    let mut call = Call::new(&method, body.clone());
    call.files = files;
    bot.rec.record(call);

    if bot.rejected(&method, &body) {
        return api_error(400, "Bad Request: can't parse entities");
    }
    let result = match method.as_str() {
        "getMe" => bot_user(),
        "sendMessage" | "sendRichMessage" | "sendDocument" | "sendPhoto" | "sendVoice"
        | "sendAudio" | "sendVideo" | "sendAnimation" => message_result(&bot, &body),
        "getFile" => {
            let id = body["file_id"].as_str().unwrap_or("").to_string();
            let found = bot
                .files
                .lock()
                .unwrap()
                .get(&id)
                .map(|f| (f.path.clone(), f.bytes.len()));
            match found {
                Some((path, size)) => {
                    json!({"file_id": id, "file_unique_id": format!("u-{id}"), "file_size": size, "file_path": path})
                }
                None => return api_error(400, "Bad Request: invalid file_id"),
            }
        }
        "getBusinessConnection" => {
            let id = body["business_connection_id"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let found = bot.business.lock().unwrap().get(&id).cloned();
            match found {
                Some(c) => c,
                None => return api_error(400, "Bad Request: business connection not found"),
            }
        }
        "answerGuestQuery" => json!({"inline_message_id": "guest-msg-1"}),
        _ => Value::Bool(true),
    };
    api_ok(result)
}

// ---------------------------------------------------------------------------
// Mock DocsGPT
// ---------------------------------------------------------------------------

/// One piece of a scripted `/stream` response.
pub enum Step {
    /// `data: <json>\n\n`
    Event(Value),
    /// Raw bytes (e.g. `: keepalive\n\n`).
    Raw(String),
    /// Stall the stream (a keepalive comment is sent afterwards).
    Sleep(Duration),
}

pub enum StreamReply {
    Sse(Vec<Step>),
    /// Plain HTTP status + body instead of a stream.
    Http(u16, String),
}

pub fn sse(steps: Vec<Step>) -> StreamReply {
    StreamReply::Sse(steps)
}
pub fn ev(v: Value) -> Step {
    Step::Event(v)
}
pub fn ev_message_id(message_id: &str, conversation_id: &str) -> Value {
    json!({"type": "message_id", "message_id": message_id, "conversation_id": conversation_id, "request_id": "req-1"})
}
pub fn ev_answer(delta: &str) -> Value {
    json!({"type": "answer", "answer": delta})
}
pub fn ev_thought(t: &str) -> Value {
    json!({"type": "thought", "thought": t})
}
pub fn ev_source(list: &[(&str, &str)]) -> Value {
    json!({"type": "source", "source": list.iter().map(|(title, link)| json!({"title": title, "link": link})).collect::<Vec<_>>()})
}
pub fn ev_tool_call(data: Value) -> Value {
    json!({"type": "tool_call", "data": data})
}
pub fn ev_id(conversation_id: &str) -> Value {
    json!({"type": "id", "id": conversation_id})
}
pub fn ev_end() -> Value {
    json!({"type": "end"})
}
pub fn ev_error(message: &str) -> Value {
    json!({"type": "error", "error": message})
}

/// A realistic answer stream: message_id, word-sized deltas, optional sources,
/// a keepalive comment, the conversation id, end.
pub fn answer_steps(text: &str, conversation_id: &str, sources: &[(&str, &str)]) -> Vec<Step> {
    let mut steps = vec![ev(ev_message_id("m1", conversation_id))];
    let mut cur = String::new();
    for ch in text.chars() {
        cur.push(ch);
        if ch == ' ' || ch == '\n' {
            steps.push(ev(ev_answer(&cur)));
            cur.clear();
        }
    }
    if !cur.is_empty() {
        steps.push(ev(ev_answer(&cur)));
    }
    if !sources.is_empty() {
        steps.push(ev(ev_source(sources)));
    }
    steps.push(Step::Raw(": keepalive\n\n".into()));
    steps.push(ev(ev_id(conversation_id)));
    steps.push(ev(ev_end()));
    steps
}

pub fn reply_text(text: &str, conversation_id: &str) -> StreamReply {
    sse(answer_steps(text, conversation_id, &[]))
}

type StreamFn = Box<dyn FnMut(&Value) -> StreamReply + Send + 'static>;
type AnswerFn = Box<dyn FnMut(&Value) -> (u16, Value) + Send + 'static>;

pub struct Artifact {
    pub filename: String,
    pub mime: String,
    pub bytes: Bytes,
}

pub struct DocsMock {
    pub url: String,
    /// Methods: `/stream`, `/api/answer`, `/api/store_attachment`, `/api/task_status`,
    /// `/api/stt`, `/api/artifacts/download`, `/images`.
    pub rec: Recorder,
    stream: Mutex<StreamFn>,
    answer: Mutex<AnswerFn>,
    store: Mutex<(u16, Value)>,
    task_status: Mutex<VecDeque<String>>,
    stt: Mutex<(u16, Value)>,
    artifacts: Mutex<HashMap<String, Artifact>>,
    images: Mutex<HashMap<String, Bytes>>,
}

impl DocsMock {
    /// Script `/stream`: the closure sees the request body and returns the reply.
    pub fn on_stream(&self, f: impl FnMut(&Value) -> StreamReply + Send + 'static) {
        *self.stream.lock().unwrap() = Box::new(f);
    }
    /// Script `/api/answer`.
    pub fn on_answer(&self, f: impl FnMut(&Value) -> (u16, Value) + Send + 'static) {
        *self.answer.lock().unwrap() = Box::new(f);
    }
    pub fn set_store_attachment(&self, status: u16, body: Value) {
        *self.store.lock().unwrap() = (status, body);
    }
    /// Statuses returned by successive `/api/task_status` calls; `SUCCESS` once exhausted.
    pub fn queue_task_status(&self, statuses: &[&str]) {
        self.task_status
            .lock()
            .unwrap()
            .extend(statuses.iter().map(|s| s.to_string()));
    }
    pub fn set_stt(&self, text: &str) {
        *self.stt.lock().unwrap() = (200, json!({"success": true, "text": text}));
    }
    pub fn add_artifact(&self, id: &str, filename: &str, mime: &str, bytes: Bytes) {
        self.artifacts.lock().unwrap().insert(
            id.to_string(),
            Artifact {
                filename: filename.to_string(),
                mime: mime.to_string(),
                bytes,
            },
        );
    }
    pub fn add_image(&self, name: &str, bytes: Bytes) {
        self.images.lock().unwrap().insert(name.to_string(), bytes);
    }
    pub fn image_url(&self, name: &str) -> String {
        format!("{}/images/{name}", self.url)
    }
}

fn sse_response(steps: Vec<Step>) -> Response {
    let stream = futures_util::stream::unfold(steps.into_iter(), |mut it| async move {
        let step = it.next()?;
        let chunk = match step {
            Step::Event(v) => format!("data: {v}\n\n"),
            Step::Raw(s) => s,
            Step::Sleep(d) => {
                tokio::time::sleep(d).await;
                ": keepalive\n\n".to_string()
            }
        };
        Some((
            Ok::<Bytes, std::convert::Infallible>(Bytes::from(chunk)),
            it,
        ))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn docs_stream(
    State(m): State<Arc<DocsMock>>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    m.rec.record(Call::new("/stream", body.clone()));
    let reply = {
        let mut f = m.stream.lock().unwrap();
        (*f)(&body)
    };
    match reply {
        StreamReply::Http(code, text) => (
            StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            text,
        )
            .into_response(),
        StreamReply::Sse(steps) => sse_response(steps),
    }
}

async fn docs_answer(
    State(m): State<Arc<DocsMock>>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    m.rec.record(Call::new("/api/answer", body.clone()));
    let (code, v) = {
        let mut f = m.answer.lock().unwrap();
        (*f)(&body)
    };
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        axum::Json(v),
    )
        .into_response()
}

async fn docs_store(State(m): State<Arc<DocsMock>>, mp: Multipart) -> Response {
    let (body, files) = read_multipart(mp).await;
    let mut call = Call::new("/api/store_attachment", body);
    call.files = files;
    m.rec.record(call);
    let (code, v) = m.store.lock().unwrap().clone();
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        axum::Json(v),
    )
        .into_response()
}

async fn docs_task(
    State(m): State<Arc<DocsMock>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let mut call = Call::new("/api/task_status", Value::Null);
    call.query = q;
    m.rec.record(call);
    let status = m
        .task_status
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| "SUCCESS".to_string());
    axum::Json(json!({"status": status})).into_response()
}

async fn docs_stt(
    State(m): State<Arc<DocsMock>>,
    Query(q): Query<HashMap<String, String>>,
    mp: Multipart,
) -> Response {
    let (body, files) = read_multipart(mp).await;
    let mut call = Call::new("/api/stt", body);
    call.query = q;
    call.files = files;
    m.rec.record(call);
    let (code, v) = m.stt.lock().unwrap().clone();
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        axum::Json(v),
    )
        .into_response()
}

async fn docs_artifact(
    State(m): State<Arc<DocsMock>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let mut call = Call::new("/api/artifacts/download", json!({"id": id}));
    call.query = q;
    m.rec.record(call);
    let found = m
        .artifacts
        .lock()
        .unwrap()
        .get(&id)
        .map(|a| (a.filename.clone(), a.mime.clone(), a.bytes.clone()));
    match found {
        Some((filename, mime, bytes)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime)
            .header(
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            )
            .body(Body::from(bytes))
            .unwrap(),
        None => (StatusCode::NOT_FOUND, "no such artifact").into_response(),
    }
}

async fn docs_image(State(m): State<Arc<DocsMock>>, Path(name): Path<String>) -> Response {
    m.rec.record(Call::new("/images", json!({"name": name})));
    let found = m.images.lock().unwrap().get(&name).cloned();
    match found {
        Some(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/png")
            .body(Body::from(bytes))
            .unwrap(),
        None => (StatusCode::NOT_FOUND, "no such image").into_response(),
    }
}

/// Start a mock DocsGPT on a random port, on the current runtime.
pub async fn start_docsgpt() -> Arc<DocsMock> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock docsgpt");
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let m = Arc::new(DocsMock {
        url,
        rec: Recorder::default(),
        stream: Mutex::new(Box::new(|_: &Value| {
            reply_text("Hello from the mock assistant.", "conv-1")
        })),
        answer: Mutex::new(Box::new(|_: &Value| {
            (200, json!({"answer": "X is y", "conversation_id": "c"}))
        })),
        store: Mutex::new((
            200,
            json!({"success": true, "attachment_id": "att-1", "task_id": "task-1"}),
        )),
        task_status: Mutex::new(VecDeque::new()),
        stt: Mutex::new((200, json!({"success": true, "text": "transcribed speech"}))),
        artifacts: Mutex::new(HashMap::new()),
        images: Mutex::new(HashMap::new()),
    });
    let app = Router::new()
        .route("/stream", post(docs_stream))
        .route("/api/answer", post(docs_answer))
        .route("/api/store_attachment", post(docs_store))
        .route("/api/task_status", get(docs_task))
        .route("/api/stt", post(docs_stt))
        .route("/api/artifacts/{id}/download", get(docs_artifact))
        .route("/images/{name}", get(docs_image))
        .with_state(m.clone());
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock docsgpt server");
    });
    m
}

// ---------------------------------------------------------------------------
// Running the real bot
// ---------------------------------------------------------------------------

static TOKEN_SEQ: AtomicU64 = AtomicU64::new(1);

pub fn agent(name: &str, api_key: &str, default: bool) -> AgentConfig {
    AgentConfig {
        name: name.to_string(),
        api_key: api_key.to_string(),
        description: None,
        default,
    }
}

/// A single-agent bot config with a unique token, pointed at `docs`.
pub fn bot_config(name: &str, docs: &DocsMock) -> BotConfig {
    let n = TOKEN_SEQ.fetch_add(1, Ordering::SeqCst);
    BotConfig {
        name: name.to_string(),
        token: format!("{}:{name}-{n}", 1000 + n),
        mode: Mode::Polling,
        groups: GroupMode::Mention,
        streaming: true,
        reactions: true,
        attachments: true,
        voice_replies: false,
        business: true,
        guest: true,
        inline: true,
        allowed_chats: vec![],
        welcome: None,
        description: None,
        short_description: None,
        menu_button_url: None,
        max_file_mb: 20,
        api_base: Some(docs.url.clone()),
        agents: vec![agent("default", "key-default", true)],
    }
}

pub struct TestBot {
    pub ctx: Arc<BotContext>,
    pub app: Arc<AppState>,
    pub tg: Arc<TgBot>,
    pub docs: Arc<DocsMock>,
}

impl TestBot {
    pub fn name(&self) -> &str {
        &self.ctx.cfg.name
    }
    /// Push a text message from the default user in the default private chat.
    pub fn push_private(&self, message_id: i32, text: &str) -> i64 {
        self.tg.push(
            "message",
            message(message_id, chat(CHAT, "private"), user(USER, "Alice"), text),
        )
    }
    /// Push a `/command` from the default user in the default private chat.
    pub fn push_private_command(&self, message_id: i32, text: &str) -> i64 {
        self.tg.push(
            "message",
            command(message_id, chat(CHAT, "private"), user(USER, "Alice"), text),
        )
    }
}

impl Drop for TestBot {
    fn drop(&mut self) {
        self.app.shutdown.cancel();
    }
}

/// Boot the real bot: `BotContext::init` (getMe, setMyCommands, …) then the
/// long-polling loop, exactly as `main.rs` does.
pub async fn start_bot(cfg: BotConfig, docs: &Arc<DocsMock>) -> TestBot {
    let server = telegram();
    let tg = server.bot(&cfg.token);
    let http = reqwest::Client::builder()
        .no_proxy()
        .user_agent("docsgpt-telegram-e2e")
        .build()
        .expect("http client");
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
    let shutdown = tokio_util::sync::CancellationToken::new();
    let app = Arc::new(AppState {
        cfg: Config {
            api_base: docs.url.clone(),
            storage: StorageConfig::default(),
            server: ServerConfig::default(),
            bots: vec![],
        },
        storage,
        http,
        shutdown,
    });
    let ctx = BotContext::init(app.clone(), cfg)
        .await
        .expect("bot init against mock telegram");
    tokio::spawn(runtime::run_polling(ctx.clone()));
    TestBot {
        ctx,
        app,
        tg,
        docs: docs.clone(),
    }
}

// ---------------------------------------------------------------------------
// Update builders
// ---------------------------------------------------------------------------

pub fn user(id: u64, first_name: &str) -> Value {
    json!({"id": id, "is_bot": false, "first_name": first_name, "username": format!("user{id}")})
}

pub fn chat(id: i64, kind: &str) -> Value {
    let mut c = json!({"id": id, "type": kind});
    if kind == "private" {
        c["first_name"] = json!("Alice");
    } else {
        c["title"] = json!("Test Group");
    }
    c
}

pub fn bare_message(message_id: i32, chat: Value, from: Value) -> Value {
    json!({"message_id": message_id, "date": 1, "chat": chat, "from": from})
}

pub fn message(message_id: i32, chat: Value, from: Value, text: &str) -> Value {
    let mut m = bare_message(message_id, chat, from);
    m["text"] = json!(text);
    m
}

/// A message whose first token is a bot command entity.
pub fn command(message_id: i32, chat: Value, from: Value, text: &str) -> Value {
    let mut m = message(message_id, chat, from, text);
    m["entities"] = json!([command_entity(text)]);
    m
}

pub fn command_entity(text: &str) -> Value {
    let length = text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .encode_utf16()
        .count();
    json!({"type": "bot_command", "offset": 0, "length": length})
}

/// A `mention` entity for `needle` inside `text`, with UTF-16 offset/length as Telegram sends them.
pub fn mention_entity(text: &str, needle: &str) -> Value {
    let idx = text
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} not in {text:?}"));
    let offset = text[..idx].encode_utf16().count();
    let length = needle.encode_utf16().count();
    json!({"type": "mention", "offset": offset, "length": length})
}
