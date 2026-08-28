//! Persistent state. DocsGPT keeps the transcript server-side, so the bot only
//! stores which conversation a chat is in, the active agent, and business links.

pub mod memory;
pub mod mongo;
pub mod sqlite;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::config::{Backend, StorageConfig};

/// Where a conversation lives: one bot, one chat, one topic (0 = none),
/// optionally one business connection.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChatScope {
    pub bot: String,
    pub chat_id: i64,
    pub thread_id: i32,
    /// Extra partition: `biz:<connection>` for business chats, `guest:<user>` for guest queries.
    pub namespace: Option<String>,
}

impl ChatScope {
    pub fn new(bot: &str, chat_id: i64, thread_id: Option<i32>) -> Self {
        Self {
            bot: bot.to_string(),
            chat_id,
            thread_id: thread_id.unwrap_or(0),
            namespace: None,
        }
    }
    pub fn business(bot: &str, chat_id: i64, connection_id: &str) -> Self {
        Self {
            bot: bot.to_string(),
            chat_id,
            thread_id: 0,
            namespace: Some(format!("biz:{connection_id}")),
        }
    }
    pub fn guest(bot: &str, chat_id: i64, user_id: u64) -> Self {
        Self {
            bot: bot.to_string(),
            chat_id,
            thread_id: 0,
            namespace: Some(format!("guest:{user_id}")),
        }
    }
    /// Stable storage key.
    pub fn key(&self) -> String {
        match &self.namespace {
            Some(ns) => format!("{}:{}:{}:{}", self.bot, ns, self.chat_id, self.thread_id),
            None => format!("{}:{}:{}", self.bot, self.chat_id, self.thread_id),
        }
    }
    /// True for a plain private/group chat on the default topic — the only
    /// shape the v1 bot stored, and therefore the only one we migrate.
    pub fn is_legacy_shape(&self) -> bool {
        self.thread_id == 0 && self.namespace.is_none()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserInfo {
    pub id: u64,
    pub first_name: String,
    #[serde(default)]
    pub last_name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub language_code: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatState {
    #[serde(default)]
    pub active_agent: Option<String>,
    #[serde(default)]
    pub last_question: Option<String>,
    #[serde(default)]
    pub user: Option<UserInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BusinessLink {
    pub id: String,
    /// Telegram user id of the business account owner.
    pub user_id: u64,
    pub user_chat_id: i64,
    pub is_enabled: bool,
    pub can_reply: bool,
}

#[async_trait]
pub trait Storage: Send + Sync {
    async fn get_conversation(&self, scope: &ChatScope, agent: &str) -> Result<Option<String>>;
    async fn set_conversation(
        &self,
        scope: &ChatScope,
        agent: &str,
        conversation_id: &str,
    ) -> Result<()>;
    async fn clear_conversation(&self, scope: &ChatScope, agent: &str) -> Result<()>;
    async fn get_chat_state(&self, scope: &ChatScope) -> Result<ChatState>;
    async fn update_chat_state(&self, scope: &ChatScope, patch: ChatStatePatch) -> Result<()>;
    async fn get_business_link(&self, bot: &str, id: &str) -> Result<Option<BusinessLink>>;
    async fn set_business_link(&self, bot: &str, link: &BusinessLink) -> Result<()>;
    /// Human-readable backend name for logs.
    fn name(&self) -> &'static str;
}

/// Fields to change on a chat's state; `None` leaves a field untouched.
#[derive(Debug, Default, Clone)]
pub struct ChatStatePatch {
    pub active_agent: Option<Option<String>>,
    pub last_question: Option<String>,
    pub user: Option<UserInfo>,
}

impl ChatStatePatch {
    pub fn active_agent(agent: Option<&str>) -> Self {
        Self {
            active_agent: Some(agent.map(str::to_string)),
            ..Default::default()
        }
    }
    pub fn apply(&self, state: &mut ChatState) {
        if let Some(a) = &self.active_agent {
            state.active_agent = a.clone();
        }
        if let Some(q) = &self.last_question {
            state.last_question = Some(q.clone());
        }
        if let Some(u) = &self.user {
            state.user = Some(u.clone());
        }
    }
}

pub async fn open(cfg: &StorageConfig) -> Result<Arc<dyn Storage>> {
    let storage: Arc<dyn Storage> = match cfg.backend {
        Backend::Memory => Arc::new(memory::MemoryStorage::default()),
        Backend::Sqlite => Arc::new(sqlite::SqliteStorage::open(&cfg.path).await?),
        Backend::Mongodb => Arc::new(mongo::MongoStorage::connect(cfg).await?),
    };
    tracing::info!(backend = storage.name(), "storage ready");
    Ok(storage)
}

#[cfg(test)]
pub(crate) mod contract {
    //! Behavioural tests every backend must pass.
    use super::*;

    pub async fn run(s: &dyn Storage) {
        let scope = ChatScope::new("bot", 42, None);
        assert_eq!(s.get_conversation(&scope, "a").await.unwrap(), None);
        s.set_conversation(&scope, "a", "conv-1").await.unwrap();
        s.set_conversation(&scope, "b", "conv-2").await.unwrap();
        assert_eq!(
            s.get_conversation(&scope, "a").await.unwrap().as_deref(),
            Some("conv-1")
        );
        assert_eq!(
            s.get_conversation(&scope, "b").await.unwrap().as_deref(),
            Some("conv-2")
        );
        let topic = ChatScope::new("bot", 42, Some(7));
        assert_eq!(s.get_conversation(&topic, "a").await.unwrap(), None);
        s.set_conversation(&scope, "a", "conv-3").await.unwrap();
        assert_eq!(
            s.get_conversation(&scope, "a").await.unwrap().as_deref(),
            Some("conv-3")
        );
        s.clear_conversation(&scope, "a").await.unwrap();
        assert_eq!(s.get_conversation(&scope, "a").await.unwrap(), None);
        assert_eq!(
            s.get_conversation(&scope, "b").await.unwrap().as_deref(),
            Some("conv-2")
        );

        let st = s.get_chat_state(&scope).await.unwrap();
        assert_eq!(st, ChatState::default());
        s.update_chat_state(&scope, ChatStatePatch::active_agent(Some("b")))
            .await
            .unwrap();
        s.update_chat_state(
            &scope,
            ChatStatePatch {
                last_question: Some("hi".into()),
                user: Some(UserInfo {
                    id: 1,
                    first_name: "A".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let st = s.get_chat_state(&scope).await.unwrap();
        assert_eq!(st.active_agent.as_deref(), Some("b"));
        assert_eq!(st.last_question.as_deref(), Some("hi"));
        assert_eq!(st.user.as_ref().map(|u| u.id), Some(1));
        s.update_chat_state(&scope, ChatStatePatch::active_agent(None))
            .await
            .unwrap();
        assert_eq!(s.get_chat_state(&scope).await.unwrap().active_agent, None);

        assert!(s.get_business_link("bot", "c1").await.unwrap().is_none());
        let link = BusinessLink {
            id: "c1".into(),
            user_id: 9,
            user_chat_id: 9,
            is_enabled: true,
            can_reply: true,
        };
        s.set_business_link("bot", &link).await.unwrap();
        assert_eq!(
            s.get_business_link("bot", "c1").await.unwrap(),
            Some(link.clone())
        );
        let link2 = BusinessLink {
            is_enabled: false,
            ..link
        };
        s.set_business_link("bot", &link2).await.unwrap();
        assert_eq!(s.get_business_link("bot", "c1").await.unwrap(), Some(link2));
        assert!(s.get_business_link("other", "c1").await.unwrap().is_none());
    }
}
