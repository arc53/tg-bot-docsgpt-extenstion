use super::*;
use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub struct SqliteStorage {
    conn: Arc<Mutex<Connection>>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS conversations (
    scope TEXT NOT NULL,
    agent TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (scope, agent)
);
CREATE TABLE IF NOT EXISTS chat_state (
    scope TEXT PRIMARY KEY,
    state TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS business_links (
    bot TEXT NOT NULL,
    id TEXT NOT NULL,
    link TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (bot, id)
);
"#;

impl SqliteStorage {
    pub async fn open(path: &str) -> Result<Self> {
        let path = path.to_string();
        tokio::task::spawn_blocking(move || Self::open_blocking(&path)).await?
    }

    pub fn open_blocking(path: &str) -> Result<Self> {
        if path != ":memory:"
            && let Some(dir) = Path::new(path).parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("opening sqlite database {path}"))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;",
        )?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|_| anyhow::anyhow!("sqlite mutex poisoned"))?;
            f(&guard)
        })
        .await?
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[async_trait]
impl Storage for SqliteStorage {
    async fn get_conversation(&self, scope: &ChatScope, agent: &str) -> Result<Option<String>> {
        let (scope, agent) = (scope.key(), agent.to_string());
        self.run(move |c| {
            Ok(c.query_row(
                "SELECT conversation_id FROM conversations WHERE scope=?1 AND agent=?2",
                params![scope, agent],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
        })
        .await
    }
    async fn set_conversation(
        &self,
        scope: &ChatScope,
        agent: &str,
        conversation_id: &str,
    ) -> Result<()> {
        let (scope, agent, id) = (scope.key(), agent.to_string(), conversation_id.to_string());
        self.run(move |c| {
            c.execute(
                "INSERT INTO conversations(scope, agent, conversation_id, updated_at) VALUES(?1,?2,?3,?4)
                 ON CONFLICT(scope, agent) DO UPDATE SET conversation_id=excluded.conversation_id, updated_at=excluded.updated_at",
                params![scope, agent, id, now()],
            )?;
            Ok(())
        })
        .await
    }
    async fn clear_conversation(&self, scope: &ChatScope, agent: &str) -> Result<()> {
        let (scope, agent) = (scope.key(), agent.to_string());
        self.run(move |c| {
            c.execute(
                "DELETE FROM conversations WHERE scope=?1 AND agent=?2",
                params![scope, agent],
            )?;
            Ok(())
        })
        .await
    }
    async fn get_chat_state(&self, scope: &ChatScope) -> Result<ChatState> {
        let scope = scope.key();
        self.run(move |c| {
            let raw: Option<String> = c
                .query_row(
                    "SELECT state FROM chat_state WHERE scope=?1",
                    params![scope],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(raw
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default())
        })
        .await
    }
    async fn update_chat_state(&self, scope: &ChatScope, patch: ChatStatePatch) -> Result<()> {
        let scope = scope.key();
        self.run(move |c| {
            let raw: Option<String> = c
                .query_row("SELECT state FROM chat_state WHERE scope=?1", params![scope], |r| r.get(0))
                .optional()?;
            let mut st: ChatState = raw.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
            patch.apply(&mut st);
            c.execute(
                "INSERT INTO chat_state(scope, state, updated_at) VALUES(?1,?2,?3)
                 ON CONFLICT(scope) DO UPDATE SET state=excluded.state, updated_at=excluded.updated_at",
                params![scope, serde_json::to_string(&st)?, now()],
            )?;
            Ok(())
        })
        .await
    }
    async fn get_business_link(&self, bot: &str, id: &str) -> Result<Option<BusinessLink>> {
        let (bot, id) = (bot.to_string(), id.to_string());
        self.run(move |c| {
            let raw: Option<String> = c
                .query_row(
                    "SELECT link FROM business_links WHERE bot=?1 AND id=?2",
                    params![bot, id],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(raw.and_then(|s| serde_json::from_str(&s).ok()))
        })
        .await
    }
    async fn set_business_link(&self, bot: &str, link: &BusinessLink) -> Result<()> {
        let (bot, id, raw) = (
            bot.to_string(),
            link.id.clone(),
            serde_json::to_string(link)?,
        );
        self.run(move |c| {
            c.execute(
                "INSERT INTO business_links(bot, id, link, updated_at) VALUES(?1,?2,?3,?4)
                 ON CONFLICT(bot, id) DO UPDATE SET link=excluded.link, updated_at=excluded.updated_at",
                params![bot, id, raw, now()],
            )?;
            Ok(())
        })
        .await
    }
    fn name(&self) -> &'static str {
        "sqlite"
    }
}

#[cfg(test)]
mod tests {
    use super::super::Storage;

    #[tokio::test]
    async fn contract() {
        let s = super::SqliteStorage::open(":memory:").await.unwrap();
        super::super::contract::run(&s).await;
    }

    #[tokio::test]
    async fn persists_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("bot.db");
        let path_str = path.to_string_lossy().to_string();
        {
            let s = super::SqliteStorage::open(&path_str).await.unwrap();
            let scope = super::ChatScope::new("b", 1, None);
            s.set_conversation(&scope, "a", "c1").await.unwrap();
        }
        let s = super::SqliteStorage::open(&path_str).await.unwrap();
        let scope = super::ChatScope::new("b", 1, None);
        assert_eq!(
            s.get_conversation(&scope, "a").await.unwrap().as_deref(),
            Some("c1")
        );
    }
}
