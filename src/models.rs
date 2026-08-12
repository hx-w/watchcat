use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub occurred_at: Option<DateTime<Utc>>,
}

impl Failure {
    pub fn key(&self) -> String {
        format!("{}:{}:{}", self.provider, self.session_id, self.turn_id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResumeReceipt {
    pub provider: String,
    pub session_id: String,
    pub turn_id: String,
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

#[derive(Clone, Debug, Serialize)]
pub struct EngineEvent {
    pub kind: String,
    pub target: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<ResumeReceipt>,
}
