use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    Retry,
    Skip,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackoffKind {
    Fixed,
    Exponential,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    #[default]
    Unknown,
    Idle,
    Active,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub provider: String,
    pub id: String,
    pub title: String,
    pub state: SessionState,
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub metadata: Value,
}

impl Session {
    pub fn key(&self) -> String {
        format!("{}:{}", self.provider, self.id)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    pub provider: String,
    pub session_id: String,
    pub turn_id: String,
    pub condition: String,
    pub provider_code: String,
    pub message: String,
    pub occurred_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub metadata: Value,
}

impl Failure {
    pub fn key(&self) -> String {
        format!("{}:{}:{}", self.provider, self.session_id, self.turn_id)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageDelivery {
    #[default]
    Started,
    Steered,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageTransport {
    #[default]
    AppServer,
    DesktopIpc,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResumeReceipt {
    pub provider: String,
    pub session_id: String,
    pub turn_id: String,
    #[serde(default)]
    pub transport: MessageTransport,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageReceipt {
    pub provider: String,
    pub session_id: String,
    pub turn_id: String,
    #[serde(default)]
    pub delivery: MessageDelivery,
    #[serde(default)]
    pub transport: MessageTransport,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InterruptReceipt {
    pub provider: String,
    pub session_id: String,
    pub turn_id: String,
    #[serde(default)]
    pub transport: MessageTransport,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WatchTarget {
    pub provider: String,
    pub session_id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub label: Option<String>,
    pub added_at: DateTime<Utc>,
}

impl WatchTarget {
    pub fn key(&self) -> String {
        format!("{}:{}", self.provider, self.session_id)
    }
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EngineEvent {
    pub timestamp: DateTime<Utc>,
    pub kind: String,
    pub target: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<ResumeReceipt>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionLog {
    pub timestamp: Option<DateTime<Utc>>,
    pub provider: String,
    pub session_id: String,
    pub source: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub metadata: Value,
}

impl EngineEvent {
    pub fn as_session_log(&self) -> Option<SessionLog> {
        let (provider, session_id) = self.target.split_once(':')?;
        let metadata = serde_json::json!({
            "attempt": self.attempt,
            "max_attempts": self.max_attempts,
            "prompt": self.prompt,
            "receipt": self.receipt,
            "provider_code": self.failure.as_ref().map(|failure| &failure.provider_code),
        });
        Some(SessionLog {
            timestamp: Some(self.timestamp),
            provider: provider.into(),
            session_id: session_id.into(),
            source: "watchcat".into(),
            kind: self.kind.clone(),
            role: None,
            turn_id: self
                .failure
                .as_ref()
                .map(|failure| failure.turn_id.clone())
                .or_else(|| self.receipt.as_ref().map(|receipt| receipt.turn_id.clone())),
            condition: self.condition.clone(),
            message: self.message.clone(),
            metadata,
        })
    }
}
