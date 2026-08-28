use super::*;
use std::collections::HashMap;
use tokio::sync::RwLock;

#[derive(Default)]
pub struct MemoryStorage {
    conversations: RwLock<HashMap<(String, String), String>>,
    states: RwLock<HashMap<String, ChatState>>,
    business: RwLock<HashMap<(String, String), BusinessLink>>,
}

#[async_trait]
impl Storage for MemoryStorage {
    async fn get_conversation(&self, scope: &ChatScope, agent: &str) -> Result<Option<String>> {
        Ok(self
            .conversations
            .read()
            .await
            .get(&(scope.key(), agent.to_string()))
            .cloned())
    }
    async fn set_conversation(
        &self,
        scope: &ChatScope,
        agent: &str,
        conversation_id: &str,
    ) -> Result<()> {
        self.conversations.write().await.insert(
            (scope.key(), agent.to_string()),
            conversation_id.to_string(),
        );
        Ok(())
    }
    async fn clear_conversation(&self, scope: &ChatScope, agent: &str) -> Result<()> {
        self.conversations
            .write()
            .await
            .remove(&(scope.key(), agent.to_string()));
        Ok(())
    }
    async fn get_chat_state(&self, scope: &ChatScope) -> Result<ChatState> {
        Ok(self
            .states
            .read()
            .await
            .get(&scope.key())
            .cloned()
            .unwrap_or_default())
    }
    async fn update_chat_state(&self, scope: &ChatScope, patch: ChatStatePatch) -> Result<()> {
        let mut states = self.states.write().await;
        let st = states.entry(scope.key()).or_default();
        patch.apply(st);
        Ok(())
    }
    async fn get_business_link(&self, bot: &str, id: &str) -> Result<Option<BusinessLink>> {
        Ok(self
            .business
            .read()
            .await
            .get(&(bot.to_string(), id.to_string()))
            .cloned())
    }
    async fn set_business_link(&self, bot: &str, link: &BusinessLink) -> Result<()> {
        self.business
            .write()
            .await
            .insert((bot.to_string(), link.id.clone()), link.clone());
        Ok(())
    }
    fn name(&self) -> &'static str {
        "memory"
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn contract() {
        super::super::contract::run(&super::MemoryStorage::default()).await;
    }
}
