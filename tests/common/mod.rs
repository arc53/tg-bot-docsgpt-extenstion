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
// Recorder and request parsing: shared with the docsgpt crate's mock
// ---------------------------------------------------------------------------

pub use docsgpt::mock::{Call, Recorder, UploadedFile, loose_json, parse_body, read_multipart};

/// Telegram-flavoured accessors for recorded calls.
pub trait CallExt {
    /// `text` field (sendMessage, drafts, callbacks).
    fn text(&self) -> String;
    /// `rich_message.markdown` field (sendRichMessage, sendRichMessageDraft).
    fn markdown(&self) -> String;
}

impl CallExt for Call {
    fn text(&self) -> String {
        self.body["text"].as_str().unwrap_or("").to_string()
    }
    fn markdown(&self) -> String {
        self.body["rich_message"]["markdown"].as_str().unwrap_or("").to_string()
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
// Mock DocsGPT: the docsgpt crate's mock, under this harness's helper names
// ---------------------------------------------------------------------------

pub use docsgpt::mock::{Step, StreamReply, answer_steps, reply_text, sse};
pub type DocsMock = docsgpt::mock::MockDocsGpt;

pub async fn start_docsgpt() -> Arc<DocsMock> {
    docsgpt::mock::MockDocsGpt::start().await
}
pub fn ev(v: Value) -> Step {
    Step::Event(v)
}
pub fn ev_message_id(message_id: &str, conversation_id: &str) -> Value {
    docsgpt::mock::ev::message_id(message_id, conversation_id)
}
pub fn ev_answer(delta: &str) -> Value {
    docsgpt::mock::ev::answer(delta)
}
pub fn ev_thought(t: &str) -> Value {
    docsgpt::mock::ev::thought(t)
}
pub fn ev_source(list: &[(&str, &str)]) -> Value {
    docsgpt::mock::ev::source(list)
}
pub fn ev_tool_call(data: Value) -> Value {
    docsgpt::mock::ev::tool_call(data)
}
pub fn ev_id(conversation_id: &str) -> Value {
    docsgpt::mock::ev::id(conversation_id)
}
pub fn ev_end() -> Value {
    docsgpt::mock::ev::end()
}
pub fn ev_error(message: &str) -> Value {
    docsgpt::mock::ev::error(message)
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
