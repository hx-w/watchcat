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
use crate::models::{
    Failure, InterruptReceipt, MessageDelivery, MessageReceipt, MessageTransport, ResumeReceipt,
    Session, SessionLog, SessionState,
};
use crate::providers::Provider;
use crate::transport::codex_desktop::{CodexDesktopIpc, DesktopMessageDelivery};
use crate::transport::jsonrpc::{JsonRpcClient, JsonRpcError};

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

    async fn session_logs(&mut self, session_id: &str, limit: usize) -> Result<Vec<SessionLog>> {
        self.require_started()?;
        let result = self
            .client
            .request(
                "thread/read",
                json!({"threadId": session_id, "includeTurns": true}),
            )
            .await?;
        let turns = result
            .pointer("/thread/turns")
            .and_then(Value::as_array)
            .context("Codex thread/read returned invalid turns")?;
        let start = turns.len().saturating_sub(limit);
        let mut logs = Vec::new();
        for turn in &turns[start..] {
            logs.extend(codex_turn_logs(session_id, turn));
        }
        if logs.len() > limit {
            logs.drain(..logs.len() - limit);
        }
        Ok(logs)
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
        let classified = classify_codex_error(error);
        Ok(Some(Failure {
            provider: "codex".into(),
            session_id: session_id.into(),
            turn_id: turn_id.into(),
            condition: classified.condition,
            provider_code: classified.provider_code,
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Codex turn failed")
                .into(),
            occurred_at: parse_timestamp(turn.get("completedAt")),
            retry_after_seconds: classified.retry_after_seconds,
            model: turn.get("model").and_then(Value::as_str).map(str::to_owned),
            scope: classified.scope,
            metadata: error
                .get("additionalDetails")
                .cloned()
                .unwrap_or(Value::Null),
        }))
    }

    async fn resume(&mut self, session_id: &str, prompt: &str) -> Result<ResumeReceipt> {
        self.require_started()?;
        if let Some(mut desktop) = CodexDesktopIpc::connect_default().await? {
            if let Some(turn_id) = desktop.start_recovery(session_id, prompt).await? {
                return Ok(ResumeReceipt {
                    provider: "codex".into(),
                    session_id: session_id.into(),
                    turn_id,
                    transport: MessageTransport::DesktopIpc,
                });
            }
        }
        self.client
            .request("thread/resume", json!({"threadId": session_id}))
            .await?;
        let result = self
            .client
            .request("turn/start", codex_message_params(session_id, prompt))
            .await?;
        let turn_id = result
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .context("Codex accepted turn/start without returning a turn id")?;
        Ok(ResumeReceipt {
            provider: "codex".into(),
            session_id: session_id.into(),
            turn_id: turn_id.into(),
            transport: MessageTransport::AppServer,
        })
    }

    async fn send_message(&mut self, session_id: &str, message: &str) -> Result<MessageReceipt> {
        self.require_started()?;
        if let Some(mut desktop) = CodexDesktopIpc::connect_default().await? {
            let cwd = self.session_cwd(session_id).await;
            if let Some(receipt) = desktop.send_message(session_id, message, &cwd).await? {
                return Ok(MessageReceipt {
                    provider: "codex".into(),
                    session_id: session_id.into(),
                    turn_id: receipt.turn_id,
                    delivery: match receipt.delivery {
                        DesktopMessageDelivery::Started => MessageDelivery::Started,
                        DesktopMessageDelivery::Steered => MessageDelivery::Steered,
                    },
                    transport: MessageTransport::DesktopIpc,
                });
            }
        }
        let resumed = self
            .client
            .request("thread/resume", json!({"threadId": session_id}))
            .await?;
        let (method, params, delivery) = codex_message_request(&resumed, session_id, message);
        let result = self.client.request(method, params).await?;
        let turn_id = result
            .pointer(if delivery == MessageDelivery::Steered {
                "/turnId"
            } else {
                "/turn/id"
            })
            .and_then(Value::as_str)
            .with_context(|| format!("Codex accepted {method} without returning a turn id"))?;
        Ok(MessageReceipt {
            provider: "codex".into(),
            session_id: session_id.into(),
            turn_id: turn_id.into(),
            delivery,
            transport: MessageTransport::AppServer,
        })
    }

    async fn interrupt(&mut self, session_id: &str) -> Result<InterruptReceipt> {
        self.require_started()?;
        let turn_id = self
            .active_turn_id(session_id)
            .await?
            .with_context(|| format!("Codex session {session_id} has no active turn"))?;
        if let Some(mut desktop) = CodexDesktopIpc::connect_default().await? {
            if let Some(interrupted) = desktop.interrupt(session_id, &turn_id).await? {
                return Ok(InterruptReceipt {
                    provider: "codex".into(),
                    session_id: session_id.into(),
                    turn_id: interrupted,
                    transport: MessageTransport::DesktopIpc,
                });
            }
        }
        self.client
            .request("thread/resume", json!({"threadId": session_id}))
            .await?;
        self.client
            .request(
                "turn/interrupt",
                json!({"threadId": session_id, "turnId": turn_id}),
            )
            .await?;
        Ok(InterruptReceipt {
            provider: "codex".into(),
            session_id: session_id.into(),
            turn_id,
            transport: MessageTransport::AppServer,
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

impl CodexProvider {
    async fn session_cwd(&self, session_id: &str) -> String {
        match self
            .client
            .request(
                "thread/read",
                json!({"threadId": session_id, "includeTurns": false}),
            )
            .await
        {
            Ok(thread) => thread
                .pointer("/thread/cwd")
                .and_then(Value::as_str)
                .filter(|cwd| !cwd.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(current_directory),
            Err(error) => {
                debug!(%error, %session_id, "cannot read session cwd for Desktop steer");
                current_directory()
            }
        }
    }

    async fn active_turn_id(&self, session_id: &str) -> Result<Option<String>> {
        let result = match self
            .client
            .request(
                "thread/turns/list",
                json!({
                    "threadId": session_id,
                    "limit": 1,
                    "sortDirection": "desc",
                    "itemsView": "notLoaded",
                }),
            )
            .await
        {
            Ok(result) => result,
            Err(error) if is_unsupported_method(&error) => {
                self.client
                    .request(
                        "thread/read",
                        json!({"threadId": session_id, "includeTurns": true}),
                    )
                    .await?
            }
            Err(error) => return Err(error),
        };
        let turn = result
            .get("data")
            .and_then(Value::as_array)
            .and_then(|turns| turns.first())
            .or_else(|| {
                result
                    .pointer("/thread/turns")
                    .and_then(Value::as_array)
                    .and_then(|turns| turns.last())
            });
        Ok(turn
            .filter(|turn| turn.get("status").and_then(Value::as_str) == Some("inProgress"))
            .and_then(|turn| turn.get("id").and_then(Value::as_str))
            .map(str::to_owned))
    }
}

fn current_directory() -> String {
    std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into())
}

fn codex_message_params(session_id: &str, message: &str) -> Value {
    json!({
        "threadId": session_id,
        "input": [{"type": "text", "text": message}],
    })
}

fn codex_steer_params(session_id: &str, turn_id: &str, message: &str) -> Value {
    json!({
        "threadId": session_id,
        "expectedTurnId": turn_id,
        "input": [{"type": "text", "text": message}],
    })
}

fn codex_message_request(
    resumed: &Value,
    session_id: &str,
    message: &str,
) -> (&'static str, Value, MessageDelivery) {
    let active_turn = resumed
        .pointer("/thread/turns")
        .and_then(Value::as_array)
        .and_then(|turns| turns.last())
        .filter(|turn| turn.get("status").and_then(Value::as_str) == Some("inProgress"))
        .and_then(|turn| turn.get("id").and_then(Value::as_str));
    match active_turn {
        Some(turn_id) => (
            "turn/steer",
            codex_steer_params(session_id, turn_id, message),
            MessageDelivery::Steered,
        ),
        None => (
            "turn/start",
            codex_message_params(session_id, message),
            MessageDelivery::Started,
        ),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClassifiedError {
    pub condition: String,
    pub provider_code: String,
    pub retry_after_seconds: Option<u64>,
    pub scope: Option<String>,
}

pub fn classify_codex_error(error: &Value) -> ClassifiedError {
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
    let status = http_status(error.get("codexErrorInfo"));
    let (condition, scope) = match code.as_str() {
        "ContextWindowExceeded" => ("context.window_exceeded", None),
        "UsageLimitExceeded" => ("quota.usage_exhausted", None),
        "HttpConnectionFailed" => match status {
            Some(408) => ("network.timeout", None),
            Some(409 | 425) => ("service.conflict", None),
            Some(429) => ("capacity.rate_limited", None),
            Some(529) => ("capacity.service_overloaded", Some("service")),
            Some(401 | 403) => ("auth.invalid", None),
            Some(413) => ("request.too_large", None),
            Some(value) if (400..500).contains(&value) => ("request.invalid", None),
            Some(value) if value >= 500 => ("service.server_error", None),
            _ => ("network.connection_failed", None),
        },
        "ResponseStreamConnectionFailed" | "ResponseStreamDisconnected" => {
            ("network.stream_failed", None)
        }
        "ResponseTooManyFailedAttempts" => ("retry.provider_exhausted", None),
        "BadRequest" => ("request.invalid", None),
        "Unauthorized" => ("auth.invalid", None),
        "SandboxError" => ("sandbox.failed", None),
        "InternalServerError" => ("service.server_error", None),
        _ => ("failure.unknown", None),
    };
    ClassifiedError {
        condition: condition.into(),
        provider_code: code,
        retry_after_seconds: retry_after(error),
        scope: scope.map(str::to_owned),
    }
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
                .filter(|key| key != "Other")
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
        "contextWindowExceeded" | "ContextWindowExceeded" => "ContextWindowExceeded".into(),
        "usageLimitExceeded" | "UsageLimitExceeded" => "UsageLimitExceeded".into(),
        "badRequest" | "BadRequest" => "BadRequest".into(),
        "unauthorized" | "Unauthorized" => "Unauthorized".into(),
        "sandboxError" | "SandboxError" => "SandboxError".into(),
        "internalServerError" | "InternalServerError" => "InternalServerError".into(),
        _ => "Other".into(),
    }
}

fn retry_after(error: &Value) -> Option<u64> {
    error
        .pointer("/additionalDetails/retryAfterSeconds")
        .and_then(Value::as_u64)
        .or_else(|| {
            error
                .pointer("/codexErrorInfo/retryAfterSeconds")
                .and_then(Value::as_u64)
        })
}

fn codex_turn_logs(session_id: &str, turn: &Value) -> Vec<SessionLog> {
    let status = turn
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let (condition, message) = if status == "failed" {
        let fallback = json!({"message": "Codex turn failed without structured error details"});
        let error = turn.get("error").unwrap_or(&fallback);
        (
            Some(classify_codex_error(error).condition),
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Codex turn failed")
                .to_owned(),
        )
    } else {
        (None, format!("turn {status}"))
    };
    let timestamp =
        parse_timestamp(turn.get("completedAt")).or_else(|| parse_timestamp(turn.get("startedAt")));
    let turn_id = turn.get("id").and_then(Value::as_str).map(str::to_owned);
    let mut logs = turn
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| codex_item_log(session_id, turn_id.clone(), timestamp, item))
        .collect::<Vec<_>>();
    logs.push(SessionLog {
        timestamp,
        provider: "codex".into(),
        session_id: session_id.into(),
        source: "provider".into(),
        kind: format!("turn.{status}"),
        role: None,
        turn_id,
        condition,
        message,
        metadata: json!({"model": turn.get("model")}),
    });
    logs
}

fn codex_item_log(
    session_id: &str,
    turn_id: Option<String>,
    timestamp: Option<DateTime<Utc>>,
    item: &Value,
) -> Option<SessionLog> {
    let item_type = item.get("type")?.as_str()?;
    let (role, message) = match item_type {
        "userMessage" => {
            let message = item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|content| content.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|content| content.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            ("user", message)
        }
        "agentMessage" => ("assistant", item.get("text")?.as_str()?.to_owned()),
        _ => return None,
    };
    Some(SessionLog {
        timestamp,
        provider: "codex".into(),
        session_id: session_id.into(),
        source: "provider".into(),
        kind: "message".into(),
        role: Some(role.into()),
        turn_id,
        condition: None,
        message,
        metadata: json!({"item_id": item.get("id")}),
    })
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
    fn message_params_preserve_multiline_text() {
        let params = codex_message_params("session-1", "line one\nline two");
        assert_eq!(params["threadId"], "session-1");
        assert_eq!(params["input"][0]["type"], "text");
        assert_eq!(params["input"][0]["text"], "line one\nline two");
    }

    #[test]
    fn steer_params_pin_the_active_turn() {
        let params = codex_steer_params("session-1", "turn-1", "change direction");
        assert_eq!(params["threadId"], "session-1");
        assert_eq!(params["expectedTurnId"], "turn-1");
        assert_eq!(params["input"][0]["text"], "change direction");
    }

    #[test]
    fn active_turn_is_steered_and_idle_thread_starts_a_turn() {
        let active = json!({
            "thread": {"turns": [{"id": "turn-1", "status": "inProgress"}]}
        });
        let (method, params, delivery) = codex_message_request(&active, "session-1", "guide");
        assert_eq!(method, "turn/steer");
        assert_eq!(params["expectedTurnId"], "turn-1");
        assert_eq!(delivery, MessageDelivery::Steered);

        let idle = json!({
            "thread": {"turns": [{"id": "turn-1", "status": "completed"}]}
        });
        let (method, params, delivery) = codex_message_request(&idle, "session-1", "next");
        assert_eq!(method, "turn/start");
        assert!(params.get("expectedTurnId").is_none());
        assert_eq!(delivery, MessageDelivery::Started);
    }

    #[test]
    fn classifies_structured_network_failures() {
        let classified = classify_codex_error(&json!({
            "message": "failed",
            "codexErrorInfo": {"responseStreamDisconnected": {"httpStatusCode": null}}
        }));
        assert_eq!(classified.provider_code, "ResponseStreamDisconnected");
        assert_eq!(classified.condition, "network.stream_failed");
    }

    #[test]
    fn maps_unauthorized_http_failures_to_auth() {
        let classified = classify_codex_error(&json!({
            "message": "unauthorized",
            "codexErrorInfo": {"httpConnectionFailed": {"httpStatusCode": 401}}
        }));
        assert_eq!(classified.condition, "auth.invalid");
    }

    #[test]
    fn does_not_retry_other_client_errors_as_network_failures() {
        let classified = classify_codex_error(&json!({
            "message": "unprocessable",
            "codexErrorInfo": {"httpConnectionFailed": {"httpStatusCode": 422}}
        }));
        assert_eq!(classified.condition, "request.invalid");
    }

    #[test]
    fn accepts_legacy_pascal_case_structured_code() {
        let classified = classify_codex_error(&json!({
            "message": "failed",
            "codexErrorInfo": {"type": "ResponseStreamConnectionFailed"}
        }));
        assert_eq!(classified.provider_code, "ResponseStreamConnectionFailed");
        assert_eq!(classified.condition, "network.stream_failed");
    }

    #[test]
    fn recognizes_legacy_disconnect_message() {
        let classified = classify_codex_error(&json!({
            "message": "stream disconnected before completion: error sending request"
        }));
        assert_eq!(classified.provider_code, "ResponseStreamDisconnected");
        assert_eq!(classified.condition, "network.stream_failed");
    }

    #[test]
    fn unknown_error_fails_closed() {
        let classified = classify_codex_error(&json!({"message": "sandbox exploded"}));
        assert_eq!(classified.provider_code, "Other");
        assert_eq!(classified.condition, "failure.unknown");
    }

    #[test]
    fn maps_overload_and_usage_limit_distinctly() {
        let overload = classify_codex_error(&json!({
            "message": "overloaded",
            "codexErrorInfo": {"type": "HttpConnectionFailed", "httpStatusCode": 529}
        }));
        assert_eq!(overload.condition, "capacity.service_overloaded");
        assert_eq!(overload.scope.as_deref(), Some("service"));
        let quota = classify_codex_error(&json!({
            "message": "usage exhausted",
            "codexErrorInfo": {"type": "UsageLimitExceeded"}
        }));
        assert_eq!(quota.condition, "quota.usage_exhausted");
    }
}
