//! MongoDB backend.
//!
//! Everything lives in one collection (`storage.collection`) of small documents
//! keyed by a string `_id`:
//!
//! | `_id`                          | body                                     |
//! |--------------------------------|------------------------------------------|
//! | `conv:<scope.key()>:<agent>`   | `{ conversation_id, updated_at }`        |
//! | `chat:<scope.key()>`           | `{ state: ChatState, updated_at }`       |
//! | `biz:<bot>:<id>`               | `{ link: BusinessLink, updated_at }`     |
//!
//! The v1 Python bot kept one document per chat in `storage.legacy_collection`
//! (`_id` = chat id as a string, with `conversation_id` and `user_info`). The
//! first `get_conversation` for a plain chat that has no v2 document adopts
//! that conversation for the requesting agent, copies `user_info` into the chat
//! state, and stamps the legacy document with `migrated_to` so it is never
//! adopted twice. Legacy documents are left in place.
use super::*;
use crate::config::StorageConfig;
use anyhow::{Context, anyhow};
use mongodb::bson::{self, Bson, DateTime, Document, doc};
use mongodb::options::ClientOptions;
use mongodb::{Client, Collection};
use std::time::Duration;

const SERVER_SELECTION_TIMEOUT: Duration = Duration::from_secs(5);

pub struct MongoStorage {
    coll: Collection<Document>,
    /// v1 collection, only consulted when its name differs from `coll`.
    legacy: Option<Collection<Document>>,
}

fn conv_key(scope: &ChatScope, agent: &str) -> String {
    format!("conv:{}:{}", scope.key(), agent)
}
fn chat_key(scope: &ChatScope) -> String {
    format!("chat:{}", scope.key())
}
fn biz_key(bot: &str, id: &str) -> String {
    format!("biz:{bot}:{id}")
}

/// Non-empty trimmed string field, or `None`.
fn str_field(doc: &Document, key: &str) -> Option<String> {
    doc.get_str(key)
        .ok()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Decode a v1 `user_info` sub-document. Python stored the id as whichever
/// integer width fit, so accept both, plus a double for good measure.
fn legacy_user(info: &Document) -> Option<UserInfo> {
    let id = match info.get("id")? {
        Bson::Int32(v) => u64::try_from(*v).ok()?,
        Bson::Int64(v) => u64::try_from(*v).ok()?,
        Bson::Double(v) if v.fract() == 0.0 && *v >= 0.0 => *v as u64,
        _ => return None,
    };
    Some(UserInfo {
        id,
        first_name: str_field(info, "first_name").unwrap_or_default(),
        last_name: str_field(info, "last_name"),
        username: str_field(info, "username"),
        language_code: str_field(info, "language_code"),
    })
}

impl MongoStorage {
    pub async fn connect(cfg: &StorageConfig) -> Result<Self> {
        let uri = cfg
            .uri
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!("storage.uri (MONGODB_URI) is required for the mongodb backend")
            })?;
        let mut options = ClientOptions::parse(uri)
            .await
            .context("parsing MongoDB connection string")?;
        options.server_selection_timeout = Some(SERVER_SELECTION_TIMEOUT);
        options
            .connect_timeout
            .get_or_insert(SERVER_SELECTION_TIMEOUT);
        options
            .app_name
            .get_or_insert_with(|| "docsgpt-telegram".into());
        let client = Client::with_options(options).context("creating MongoDB client")?;
        let db = client.database(&cfg.db_name);
        db.run_command(doc! { "ping": 1 })
            .await
            .with_context(|| format!("MongoDB is not reachable (database {:?})", cfg.db_name))?;

        let coll = db.collection::<Document>(&cfg.collection);
        let legacy_name = cfg.legacy_collection.trim();
        let legacy = (!legacy_name.is_empty() && legacy_name != cfg.collection)
            .then(|| db.collection::<Document>(legacy_name));
        tracing::info!(
            db = %cfg.db_name,
            collection = %cfg.collection,
            legacy_collection = legacy.as_ref().map(|c| c.name()),
            "connected to MongoDB"
        );
        Ok(Self { coll, legacy })
    }

    /// Upsert `fields` (plus `updated_at`) into the document with the given `_id`.
    async fn upsert(&self, id: &str, mut fields: Document) -> Result<()> {
        fields.insert("updated_at", DateTime::now());
        self.coll
            .update_one(doc! { "_id": id }, doc! { "$set": fields })
            .upsert(true)
            .await
            .with_context(|| format!("writing {id}"))?;
        Ok(())
    }

    async fn read_chat_state(&self, key: &str) -> Result<ChatState> {
        let found = self
            .coll
            .find_one(doc! { "_id": key })
            .await
            .with_context(|| format!("reading {key}"))?;
        Ok(found
            .and_then(|d| d.get("state").cloned())
            .and_then(|b| bson::from_bson::<ChatState>(b).ok())
            .unwrap_or_default())
    }

    /// Adopt the v1 conversation for `scope` under `new_id`, if there is one
    /// and it has not been claimed already. Returns the adopted conversation id.
    async fn adopt_legacy(&self, scope: &ChatScope, new_id: &str) -> Result<Option<String>> {
        let Some(legacy) = &self.legacy else {
            return Ok(None);
        };
        let legacy_id = scope.chat_id.to_string();
        let Some(old) = legacy
            .find_one(doc! { "_id": &legacy_id })
            .await
            .context("reading legacy chat document")?
        else {
            return Ok(None);
        };
        if old
            .get("migrated_to")
            .is_some_and(|v| !matches!(v, Bson::Null))
        {
            return Ok(None);
        }
        let Some(conversation_id) = str_field(&old, "conversation_id") else {
            return Ok(None);
        };

        self.upsert(new_id, doc! { "conversation_id": &conversation_id })
            .await
            .context("writing migrated conversation")?;

        // Best effort from here on: the conversation is already reachable under
        // the new key, so a failure below must not hide it from the caller.
        if let Some(user) = old.get_document("user_info").ok().and_then(legacy_user) {
            let patch = ChatStatePatch {
                user: Some(user),
                ..Default::default()
            };
            if let Err(e) = self.update_chat_state(scope, patch).await {
                tracing::warn!(error = %e, chat_id = scope.chat_id, "could not copy legacy user info");
            }
        }
        let mark = doc! { "$set": { "migrated_to": new_id, "migrated_at": DateTime::now() } };
        if let Err(e) = legacy.update_one(doc! { "_id": &legacy_id }, mark).await {
            tracing::warn!(error = %e, chat_id = scope.chat_id, "could not mark legacy document as migrated");
        }
        tracing::info!(chat_id = scope.chat_id, bot = %scope.bot, key = new_id, "migrated legacy conversation");
        Ok(Some(conversation_id))
    }
}

#[async_trait]
impl Storage for MongoStorage {
    async fn get_conversation(&self, scope: &ChatScope, agent: &str) -> Result<Option<String>> {
        let key = conv_key(scope, agent);
        let found = self
            .coll
            .find_one(doc! { "_id": &key })
            .await
            .with_context(|| format!("reading {key}"))?;
        if let Some(d) = found {
            return Ok(str_field(&d, "conversation_id"));
        }
        if scope.is_legacy_shape() {
            match self.adopt_legacy(scope, &key).await {
                Ok(adopted) => return Ok(adopted),
                Err(e) => tracing::warn!(
                    error = %e, chat_id = scope.chat_id, bot = %scope.bot, agent,
                    "legacy conversation migration failed"
                ),
            }
        }
        Ok(None)
    }
    async fn set_conversation(
        &self,
        scope: &ChatScope,
        agent: &str,
        conversation_id: &str,
    ) -> Result<()> {
        self.upsert(
            &conv_key(scope, agent),
            doc! { "conversation_id": conversation_id },
        )
        .await
    }
    async fn clear_conversation(&self, scope: &ChatScope, agent: &str) -> Result<()> {
        let key = conv_key(scope, agent);
        self.coll
            .delete_one(doc! { "_id": &key })
            .await
            .with_context(|| format!("deleting {key}"))?;
        Ok(())
    }
    async fn get_chat_state(&self, scope: &ChatScope) -> Result<ChatState> {
        self.read_chat_state(&chat_key(scope)).await
    }
    async fn update_chat_state(&self, scope: &ChatScope, patch: ChatStatePatch) -> Result<()> {
        let key = chat_key(scope);
        let mut state = self.read_chat_state(&key).await?;
        patch.apply(&mut state);
        let state = bson::to_bson(&state).context("encoding chat state")?;
        self.upsert(&key, doc! { "state": state }).await
    }
    async fn get_business_link(&self, bot: &str, id: &str) -> Result<Option<BusinessLink>> {
        let key = biz_key(bot, id);
        let found = self
            .coll
            .find_one(doc! { "_id": &key })
            .await
            .with_context(|| format!("reading {key}"))?;
        Ok(found
            .and_then(|d| d.get("link").cloned())
            .and_then(|b| bson::from_bson(b).ok()))
    }
    async fn set_business_link(&self, bot: &str, link: &BusinessLink) -> Result<()> {
        let link_bson = bson::to_bson(link).context("encoding business link")?;
        self.upsert(&biz_key(bot, &link.id), doc! { "link": link_bson })
            .await
    }
    fn name(&self) -> &'static str {
        "mongodb"
    }
}

#[cfg(test)]
mod tests {
    //! Live tests need a server: `MONGODB_TEST_URI=mongodb://127.0.0.1:27017 cargo test mongo`.
    //! Each test uses its own collections and drops them on success.
    use super::*;
    use crate::config::Backend;

    fn test_cfg(suffix: &str) -> Option<StorageConfig> {
        let Ok(uri) = std::env::var("MONGODB_TEST_URI") else {
            eprintln!("MONGODB_TEST_URI not set; skipping MongoDB test");
            return None;
        };
        let nonce = format!(
            "{}_{}",
            chrono::Utc::now().timestamp_millis(),
            std::process::id()
        );
        Some(StorageConfig {
            backend: Backend::Mongodb,
            uri: Some(uri),
            db_name: "docsgpt_telegram_test".into(),
            collection: format!("state_{suffix}_{nonce}"),
            legacy_collection: format!("legacy_{suffix}_{nonce}"),
            ..Default::default()
        })
    }

    async fn raw(s: &MongoStorage, id: &str) -> Option<Document> {
        s.coll.find_one(doc! { "_id": id }).await.unwrap()
    }

    #[tokio::test]
    async fn requires_uri() {
        let cfg = StorageConfig {
            backend: Backend::Mongodb,
            uri: None,
            ..Default::default()
        };
        let err = MongoStorage::connect(&cfg)
            .await
            .err()
            .expect("missing uri must fail");
        assert!(err.to_string().contains("storage.uri"), "{err}");
    }

    #[tokio::test]
    async fn contract() {
        let Some(cfg) = test_cfg("contract") else {
            return;
        };
        let s = MongoStorage::connect(&cfg).await.unwrap();
        assert_eq!(s.name(), "mongodb");
        super::super::contract::run(&s).await;

        // Raw document shapes, as documented at the top of this file.
        let conv = raw(&s, "conv:bot:42:0:b")
            .await
            .expect("conversation document");
        assert_eq!(conv.get_str("conversation_id").unwrap(), "conv-2");
        assert!(conv.get_datetime("updated_at").is_ok());
        assert!(
            raw(&s, "conv:bot:42:0:a").await.is_none(),
            "cleared conversation must be deleted"
        );
        let chat = raw(&s, "chat:bot:42:0").await.expect("chat state document");
        let state = chat.get_document("state").unwrap();
        assert_eq!(state.get_str("last_question").unwrap(), "hi");
        assert_eq!(
            state.get_document("user").unwrap().get_i64("id").unwrap(),
            1
        );
        assert!(chat.get_datetime("updated_at").is_ok());
        let biz = raw(&s, "biz:bot:c1").await.expect("business link document");
        let link = biz.get_document("link").unwrap();
        assert_eq!(link.get_str("id").unwrap(), "c1");
        assert!(!link.get_bool("is_enabled").unwrap());
        assert!(biz.get_datetime("updated_at").is_ok());

        s.coll.drop().await.unwrap();
    }

    #[tokio::test]
    async fn fails_fast_on_unreachable_server() {
        let Some(mut cfg) = test_cfg("unreachable") else {
            return;
        };
        cfg.uri = Some("mongodb://127.0.0.1:9/?directConnection=true".into());
        let started = std::time::Instant::now();
        let err = MongoStorage::connect(&cfg)
            .await
            .err()
            .expect("unreachable server must fail");
        assert!(err.to_string().contains("not reachable"), "{err:#}");
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn migrates_legacy_conversation_once() {
        let Some(cfg) = test_cfg("migration") else {
            return;
        };
        let s = MongoStorage::connect(&cfg).await.unwrap();
        let legacy = s.legacy.clone().expect("legacy collection configured");
        legacy
            .insert_one(doc! {
                "_id": "12345",
                "conversation_id": "legacy-conv",
                "conversation_history": [ { "prompt": "hi", "response": "hello" } ],
                "user_info": {
                    "id": 777_i64,
                    "first_name": "Ann",
                    "last_name": Bson::Null,
                    "username": "ann",
                    "language_code": "en",
                },
                "last_updated": DateTime::now(),
            })
            .await
            .unwrap();
        // Python stored small ids as int32; no optional fields at all here.
        legacy
            .insert_one(doc! {
                "_id": "22222",
                "conversation_id": "legacy-conv-2",
                "conversation_history": [],
                "user_info": { "id": 31_i32, "first_name": "Bo" },
            })
            .await
            .unwrap();
        // Nothing to adopt: conversation never started.
        legacy
            .insert_one(doc! {
                "_id": "33333",
                "conversation_id": Bson::Null,
                "conversation_history": [],
                "user_info": { "id": 5_i32, "first_name": "Cy" },
            })
            .await
            .unwrap();

        let scope = ChatScope::new("bot", 12345, None);
        assert_eq!(
            s.get_conversation(&scope, "default")
                .await
                .unwrap()
                .as_deref(),
            Some("legacy-conv")
        );
        // Second read is served from the new collection.
        assert_eq!(
            s.get_conversation(&scope, "default")
                .await
                .unwrap()
                .as_deref(),
            Some("legacy-conv")
        );
        let st = s.get_chat_state(&scope).await.unwrap();
        assert_eq!(
            st.user,
            Some(UserInfo {
                id: 777,
                first_name: "Ann".into(),
                last_name: None,
                username: Some("ann".into()),
                language_code: Some("en".into()),
            })
        );
        let old = legacy
            .find_one(doc! { "_id": "12345" })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            old.get_str("migrated_to").unwrap(),
            "conv:bot:12345:0:default"
        );
        assert!(old.get_datetime("migrated_at").is_ok());
        assert_eq!(
            old.get_str("conversation_id").unwrap(),
            "legacy-conv",
            "legacy document must be kept"
        );
        // Another agent does not inherit the same conversation.
        assert_eq!(s.get_conversation(&scope, "other").await.unwrap(), None);
        // Topics and business chats never had legacy state.
        let topic = ChatScope::new("bot", 12345, Some(3));
        assert_eq!(s.get_conversation(&topic, "default").await.unwrap(), None);
        // Once cleared, the legacy conversation is not adopted again.
        s.clear_conversation(&scope, "default").await.unwrap();
        assert_eq!(s.get_conversation(&scope, "default").await.unwrap(), None);

        let scope2 = ChatScope::new("bot", 22222, None);
        assert_eq!(
            s.get_conversation(&scope2, "sales")
                .await
                .unwrap()
                .as_deref(),
            Some("legacy-conv-2")
        );
        let st2 = s.get_chat_state(&scope2).await.unwrap();
        assert_eq!(
            st2.user,
            Some(UserInfo {
                id: 31,
                first_name: "Bo".into(),
                ..Default::default()
            })
        );
        assert_eq!(s.get_conversation(&scope2, "default").await.unwrap(), None);

        let scope3 = ChatScope::new("bot", 33333, None);
        assert_eq!(s.get_conversation(&scope3, "default").await.unwrap(), None);
        let old3 = legacy
            .find_one(doc! { "_id": "33333" })
            .await
            .unwrap()
            .unwrap();
        assert!(!old3.contains_key("migrated_to"));
        assert_eq!(
            s.get_chat_state(&scope3).await.unwrap(),
            ChatState::default()
        );

        s.coll.drop().await.unwrap();
        legacy.drop().await.unwrap();
    }
}
