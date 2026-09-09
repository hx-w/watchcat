mod claude;
mod codex;

use std::collections::HashMap;

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::models::{
    Failure, InterruptReceipt, MessageReceipt, ResumeReceipt, Session, SessionLog, TurnOutcome,
};

pub use claude::{ClaudeProvider, classify_claude_error, classify_claude_hook};
pub use codex::{CodexProvider, classify_codex_error};

use crate::config::Settings;

/// Capabilities exposed by the current adapters, independent of installation state.
pub fn supports_recovery(provider: &str) -> bool {
    provider == "codex"
}

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
            "claude" if settings.providers.claude.enabled => {
                providers.insert(name.into(), Box::new(ClaudeProvider::new()?));
            }
            "claude" => bail!("provider is disabled: claude"),
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
    async fn recent_sessions(
        &mut self,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<Session>> {
        let mut sessions = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        loop {
            let page = self.search_sessions("", cursor.as_deref(), 500).await?;
            let reached_cutoff = page
                .sessions
                .iter()
                .any(|s| s.updated_at.is_some_and(|t| t < cutoff));
            sessions.extend(page.sessions);
            if reached_cutoff {
                break;
            }
            let Some(next) = page.next_cursor else {
                break;
            };
            if !seen.insert(next.clone()) {
                bail!("{} repeated a discovery cursor", self.name());
            }
            cursor = Some(next);
        }
        Ok(sessions)
    }
    /// Return sessions newest-first by provider activity; cursors must advance.
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
}
