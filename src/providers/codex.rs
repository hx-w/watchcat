use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::{Map, Value, json};
use tracing::debug;
use uuid::Uuid;

use crate::config::CodexSettings;
use crate::models::{Failure, ResumeReceipt, Session, SessionState};
use crate::providers::Provider;
use crate::transport::jsonrpc::{JsonRpcClient, JsonRpcError};

const RETRYABLE_CODES: &[&str] = &[
    "HttpConnectionFailed",
    "ResponseStreamConnectionFailed",
    "ResponseStreamDisconnected",
    "ResponseTooManyFailedAttempts",
];

const SOURCE_KINDS: &[&str] = &[
    "cli",
    "vscode",
    "exec",
    "appServer",
    "subAgent",
    "subAgentReview",
    "subAgentCompact",
    "subAgentThreadSpawn",
    "subAgentOther",
    "unknown",
];

pub struct CodexProvider {
    client: JsonRpcClient,
    started: bool,
    sessions: HashMap<String, Session>,
    watches: HashMap<String, String>,
    watch_supported: bool,
}

impl CodexProvider {
    pub fn new(settings: &CodexSettings) -> Result<Self> {
        Ok(Self {
            client: JsonRpcClient::new(settings.command.clone())?,
            started: false,
            sessions: HashMap::new(),
            watches: HashMap::new(),
            watch_supported: true,
        })
    }

    fn require_started(&self) -> Result<()> {
        if !self.started {
            bail!("Codex provider has not been started");
        }
        Ok(())
    }

    async fn ensure_watches(&mut self, session_ids: &[String]) -> Result<()> {
        if session_ids
            .iter()
            .any(|session_id| !self.sessions.contains_key(session_id))
        {
            let _ = self.list_sessions(500).await?;
        }
        for session_id in session_ids {
            if self.watches.contains_key(session_id) {
                continue;
            }
            let Some(path) = self
                .sessions
                .get(session_id)
                .and_then(|session| session.metadata.get("path"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            if !Path::new(path).is_absolute() {
                continue;
            }
            let watch_id = Uuid::new_v5(
                &Uuid::NAMESPACE_URL,
                format!("watchcat:codex:{session_id}").as_bytes(),
            )
            .to_string();
            match self
                .client
                .request("fs/watch", json!({"watchId": watch_id, "path": path}))
                .await
            {
                Ok(_) => {
                    self.watches.insert(session_id.clone(), watch_id);
                }
                Err(error) => {
                    debug!(%error, "Codex fs/watch unavailable; using polling");
                    self.watch_supported = false;
                    self.watches.clear();
                    break;
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Provider for CodexProvider {
    fn name(&self) -> &'static str {
        "codex"
    }

    async fn start(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }
        self.client.start().await?;
        self.client
            .request(
                "initialize",
                json!({
                    "clientInfo": {"name": "watchcat", "title": "Watchcat", "version": env!("CARGO_PKG_VERSION")},
                    "capabilities": {"experimentalApi": true}
                }),
            )
            .await
            .context("cannot initialize Codex App Server")?;
        self.client.notify("initialized", json!({})).await?;
        self.started = true;
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        if !self.started {
            return Ok(());
        }
        if self.watch_supported {
            for watch_id in self.watches.values() {
                if self
                    .client
                    .request("fs/unwatch", json!({"watchId": watch_id}))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
        self.client.close().await?;
        self.watches.clear();
        self.started = false;
        Ok(())
    }

    async fn list_sessions(&mut self, limit: usize) -> Result<Vec<Session>> {
        self.require_started()?;
        let mut sessions = Vec::new();
        let mut cursor: Option<String> = None;
        while sessions.len() < limit {
            let page = self
                .client
                .request(
                    "thread/list",
                    json!({
                        "cursor": cursor,
                        "limit": usize::min(100, limit - sessions.len()),
                        "sortKey": "updated_at",
                        "sortDirection": "desc",
                        "sourceKinds": SOURCE_KINDS,
                    }),
                )
                .await?;
            let data = page
                .get("data")
                .and_then(Value::as_array)
                .context("Codex thread/list returned invalid data")?;
            if data.is_empty() {
                break;
            }
            for raw in data {
                let session = parse_session(raw)?;
                self.sessions.insert(session.id.clone(), session.clone());
                sessions.push(session);
            }
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        sessions.truncate(limit);
        Ok(sessions)
    }

    async fn latest_failure(&mut self, session_id: &str) -> Result<Option<Failure>> {
        self.require_started()?;
        let result = self
            .client
            .request(
                "thread/turns/list",
                json!({
                    "threadId": session_id,
                    "limit": 1,
                    "sortDirection": "desc",
                    "itemsView": "summary",
                }),
            )
            .await;
        let turn = match result {
            Ok(page) => page
                .get("data")
                .and_then(Value::as_array)
                .and_then(|turns| turns.first())
                .cloned(),
            Err(error) if is_unsupported_method(&error) => {
                let result = self
                    .client
                    .request(
                        "thread/read",
                        json!({"threadId": session_id, "includeTurns": true}),
                    )
                    .await?;
                result
                    .pointer("/thread/turns")
                    .and_then(Value::as_array)
                    .and_then(|turns| turns.last())
                    .cloned()
            }
            Err(error) => return Err(error),
        };
        let Some(turn) = turn else {
            return Ok(None);
        };
        if turn.get("status").and_then(Value::as_str) != Some("failed") {
            return Ok(None);
        }
        let fallback = json!({"message": "Codex turn failed without structured error details"});
        let error = turn.get("error").unwrap_or(&fallback);
        let turn_id = turn
            .get("id")
            .and_then(Value::as_str)
            .context("Codex returned a failed turn without an id")?;
        let (code, retryable) = classify_codex_error(error);
        Ok(Some(Failure {
            provider: "codex".into(),
            session_id: session_id.into(),
            turn_id: turn_id.into(),
            code,
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Codex turn failed")
                .into(),
            retryable,
            occurred_at: parse_timestamp(turn.get("completedAt")),
        }))
    }

    async fn resume(&mut self, session_id: &str, prompt: &str) -> Result<ResumeReceipt> {
        self.require_started()?;
        self.client
            .request("thread/resume", json!({"threadId": session_id}))
            .await?;
        let result = self
            .client
            .request(
                "turn/start",
                json!({
                    "threadId": session_id,
                    "input": [{"type": "text", "text": prompt}],
                }),
            )
            .await?;
        let turn_id = result
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .context("Codex accepted turn/start without returning a turn id")?;
        Ok(ResumeReceipt {
            provider: "codex".into(),
            session_id: session_id.into(),
            turn_id: turn_id.into(),
        })
    }

    async fn wait_for_change(
        &mut self,
        session_ids: &[String],
        timeout: Duration,
    ) -> Result<Vec<String>> {
        self.require_started()?;
        if session_ids.is_empty() {
            tokio::time::sleep(timeout).await;
            return Ok(Vec::new());
        }
        if self.watch_supported {
            self.ensure_watches(session_ids).await?;
        }
        if !self.watch_supported {
            tokio::time::sleep(timeout).await;
            return Ok(session_ids.to_vec());
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(session_ids.to_vec());
            }
            let Some(notification) = self.client.next_notification(remaining).await? else {
                return Ok(session_ids.to_vec());
            };
            if notification.get("method").and_then(Value::as_str) != Some("fs/changed") {
                continue;
            }
            let watch_id = notification
                .pointer("/params/watchId")
                .and_then(Value::as_str);
            let changed = self
                .watches
                .iter()
                .filter(|(_, registered)| Some(registered.as_str()) == watch_id)
                .map(|(session_id, _)| session_id.clone())
                .collect::<Vec<_>>();
            if !changed.is_empty() {
                return Ok(changed);
            }
        }
    }
}

pub fn classify_codex_error(error: &Value) -> (String, bool) {
    let mut code = codex_error_code(error.get("codexErrorInfo"));
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if code == "Other" {
        code = if message.contains("too many failed attempts") {
            "ResponseTooManyFailedAttempts".into()
        } else if message.contains("stream disconnected") {
            "ResponseStreamDisconnected".into()
        } else {
            code
        };
    }
    let mut retryable = RETRYABLE_CODES.contains(&code.as_str());
    if code == "HttpConnectionFailed" {
        if let Some(status) = http_status(error.get("codexErrorInfo")) {
            retryable = matches!(status, 408 | 409 | 425 | 429) || status >= 500;
        }
    }
    (code, retryable)
}

fn codex_error_code(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(code)) => canonical_error_code(code),
        Some(Value::Object(object)) => {
            for key in ["type", "kind", "code"] {
                if let Some(code) = object.get(key).and_then(Value::as_str) {
                    return canonical_error_code(code);
                }
            }
            let known = object
                .keys()
                .map(|key| canonical_error_code(key))
                .filter(|key| RETRYABLE_CODES.contains(&key.as_str()))
                .collect::<Vec<_>>();
            if known.len() == 1 {
                known[0].clone()
            } else {
                "Other".into()
            }
        }
        _ => "Other".into(),
    }
}

fn canonical_error_code(code: &str) -> String {
    match code {
        "httpConnectionFailed" | "HttpConnectionFailed" => "HttpConnectionFailed".into(),
        "responseStreamConnectionFailed" | "ResponseStreamConnectionFailed" => {
            "ResponseStreamConnectionFailed".into()
        }
        "responseStreamDisconnected" | "ResponseStreamDisconnected" => {
            "ResponseStreamDisconnected".into()
        }
        "responseTooManyFailedAttempts" | "ResponseTooManyFailedAttempts" => {
            "ResponseTooManyFailedAttempts".into()
        }
        _ => "Other".into(),
    }
}

fn http_status(value: Option<&Value>) -> Option<u64> {
    let object = value?.as_object()?;
    if let Some(status) = object.get("httpStatusCode").and_then(Value::as_u64) {
        return Some(status);
    }
    object
        .values()
        .find_map(|nested| nested.as_object()?.get("httpStatusCode")?.as_u64())
}

fn parse_session(raw: &Value) -> Result<Session> {
    let id = raw
        .get("id")
        .and_then(Value::as_str)
        .context("Codex thread/list returned a session without an id")?;
    let title = raw
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| {
            raw.get("preview")
                .and_then(Value::as_str)
                .map(|preview| preview.replace('\n', " ").chars().take(80).collect())
        })
        .unwrap_or_else(|| id.into());
    let state = match raw.pointer("/status/type").and_then(Value::as_str) {
        Some("active") => SessionState::Active,
        Some("idle" | "notLoaded") => SessionState::Idle,
        Some("systemError") => SessionState::Failed,
        _ => SessionState::Unknown,
    };
    let mut metadata = Map::new();
    metadata.insert(
        "path".into(),
        raw.get("path").cloned().unwrap_or(Value::Null),
    );
    metadata.insert(
        "is_pinned".into(),
        Value::Bool(
            raw.get("isPinned")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
    );
    metadata.insert(
        "source".into(),
        raw.get("source").cloned().unwrap_or(Value::Null),
    );
    Ok(Session {
        provider: "codex".into(),
        id: id.into(),
        title,
        state,
        updated_at: parse_timestamp(raw.get("updatedAt")),
        metadata: Value::Object(metadata),
    })
}

fn parse_timestamp(value: Option<&Value>) -> Option<DateTime<Utc>> {
    match value {
        Some(Value::Number(number)) => number
            .as_i64()
            .and_then(|timestamp| Utc.timestamp_opt(timestamp, 0).single()),
        Some(Value::String(text)) => DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|parsed| parsed.with_timezone(&Utc)),
        _ => None,
    }
}

fn is_unsupported_method(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<JsonRpcError>()
        .is_some_and(|error| matches!(error.code, -32601 | -32602))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_structured_network_failures() {
        let (code, retryable) = classify_codex_error(&json!({
            "message": "failed",
            "codexErrorInfo": {"responseStreamDisconnected": {"httpStatusCode": null}}
        }));
        assert_eq!(code, "ResponseStreamDisconnected");
        assert!(retryable);
    }

    #[test]
    fn refuses_non_transient_http_failures() {
        let (_, retryable) = classify_codex_error(&json!({
            "message": "unauthorized",
            "codexErrorInfo": {"httpConnectionFailed": {"httpStatusCode": 401}}
        }));
        assert!(!retryable);
    }

    #[test]
    fn accepts_legacy_pascal_case_structured_code() {
        let (code, retryable) = classify_codex_error(&json!({
            "message": "failed",
            "codexErrorInfo": {"type": "ResponseStreamConnectionFailed"}
        }));
        assert_eq!(code, "ResponseStreamConnectionFailed");
        assert!(retryable);
    }

    #[test]
    fn recognizes_legacy_disconnect_message() {
        let (code, retryable) = classify_codex_error(&json!({
            "message": "stream disconnected before completion: error sending request"
        }));
        assert_eq!(code, "ResponseStreamDisconnected");
        assert!(retryable);
    }

    #[test]
    fn unknown_error_fails_closed() {
        let (code, retryable) = classify_codex_error(&json!({"message": "sandbox exploded"}));
        assert_eq!(code, "Other");
        assert!(!retryable);
    }
}
