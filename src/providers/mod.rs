mod claude;
mod codex;

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::models::{
    Failure, InterruptReceipt, MessageReceipt, ResumeReceipt, Session, SessionLog, TurnOutcome,
};

pub use claude::{classify_claude_error, classify_claude_hook};
pub use codex::{CodexProvider, classify_codex_error};

use crate::config::Settings;

pub fn build_providers<'a>(
    settings: &Settings,
    names: impl IntoIterator<Item = &'a str>,
) -> Result<HashMap<String, Box<dyn Provider>>> {
    let mut providers = HashMap::<String, Box<dyn Provider>>::new();
    for name in names {
        if providers.contains_key(name) {
            continue;
        }
        match name {
            "codex" if settings.providers.codex.enabled => {
                providers.insert(
                    name.into(),
                    Box::new(CodexProvider::new(&settings.providers.codex)?),
                );
            }
            "codex" => bail!("provider is disabled: codex"),
            "claude" => bail!(
                "Claude error definitions are available, but the Claude session adapter is not enabled in this release"
            ),
            _ => bail!("unknown provider: {name}"),
        }
    }
    Ok(providers)
}

pub async fn start_providers(providers: &mut HashMap<String, Box<dyn Provider>>) -> Result<()> {
    for provider in providers.values_mut() {
        provider.start().await?;
    }
    Ok(())
}

#[derive(Debug)]
pub struct SessionSearchPage {
    pub sessions: Vec<Session>,
    pub next_cursor: Option<String>,
}

#[async_trait]
pub trait Provider: Send {
    fn name(&self) -> &'static str;
    async fn start(&mut self) -> Result<()>;
    async fn close(&mut self) -> Result<()>;
    async fn list_sessions(&mut self, limit: usize) -> Result<Vec<Session>>;
    async fn search_sessions(
        &mut self,
        query: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<SessionSearchPage>;
    async fn validate_session(&mut self, session_id: &str) -> Result<()> {
        self.session_logs(session_id, 1).await.map(|_| ())
    }
    async fn session_logs(&mut self, session_id: &str, limit: usize) -> Result<Vec<SessionLog>>;
    async fn latest_failure(&mut self, session_id: &str) -> Result<Option<Failure>>;
    async fn turn_outcome(&mut self, _session_id: &str, _turn_id: &str) -> Result<TurnOutcome> {
        Ok(TurnOutcome::Unknown)
    }
    async fn resume(
        &mut self,
        session_id: &str,
        prompt: &str,
        idempotency_key: &str,
    ) -> Result<ResumeReceipt>;

    async fn interrupt(&mut self, _session_id: &str) -> Result<InterruptReceipt> {
        bail!("{} does not support interrupting sessions", self.name())
    }

    async fn send_message(&mut self, session_id: &str, message: &str) -> Result<MessageReceipt> {
        let receipt = self
            .resume(session_id, message, &uuid::Uuid::new_v4().to_string())
            .await?;
        Ok(MessageReceipt {
            provider: receipt.provider,
            session_id: receipt.session_id,
            turn_id: receipt.turn_id,
            delivery: crate::models::MessageDelivery::Started,
            transport: receipt.transport,
        })
    }

    async fn wait_for_change(
        &mut self,
        session_ids: &[String],
        timeout: Duration,
    ) -> Result<Vec<String>> {
        tokio::time::sleep(timeout).await;
        Ok(session_ids.to_vec())
    }
}
