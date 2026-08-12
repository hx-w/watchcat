mod codex;

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::models::{Failure, ResumeReceipt, Session};

pub use codex::{CodexProvider, classify_codex_error};

#[async_trait]
pub trait Provider: Send {
    fn name(&self) -> &'static str;
    async fn start(&mut self) -> Result<()>;
    async fn close(&mut self) -> Result<()>;
    async fn list_sessions(&mut self, limit: usize) -> Result<Vec<Session>>;
    async fn latest_failure(&mut self, session_id: &str) -> Result<Option<Failure>>;
    async fn resume(&mut self, session_id: &str, prompt: &str) -> Result<ResumeReceipt>;

    async fn wait_for_change(
        &mut self,
        session_ids: &[String],
        timeout: Duration,
    ) -> Result<Vec<String>> {
        tokio::time::sleep(timeout).await;
        Ok(session_ids.to_vec())
    }
}
