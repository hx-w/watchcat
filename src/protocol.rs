use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::PolicyOverride;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ACTIVITY_ITEMS: usize = 500;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RpcRequest {
    pub version: u32,
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RpcResponse {
    pub version: u32,
    pub id: String,
    pub revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RpcError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RpcNotification {
    pub version: u32,
    pub event: String,
    pub revision: u64,
    #[serde(default)]
    pub data: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PolicyUpdate {
    pub condition: String,
    pub policy: PolicyOverride,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionQuery {
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_session_limit")]
    pub limit: usize,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionRef {
    #[serde(default = "default_provider")]
    pub provider: String,
    pub session_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RetryRequest {
    #[serde(flatten)]
    pub session: SessionRef,
    pub request_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionMessage {
    #[serde(flatten)]
    pub session: SessionRef,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WatchAdd {
    #[serde(flatten)]
    pub session: SessionRef,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub protected: bool,
    #[serde(default = "default_true")]
    pub validate: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WatchUpdate {
    #[serde(flatten)]
    pub session: SessionRef,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub protected: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ActivityQuery {
    #[serde(flatten)]
    pub session: SessionRef,
    #[serde(default = "default_activity_limit")]
    pub limit: usize,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub reliability_only: bool,
}

impl ActivityQuery {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.limit == 0 || self.limit > MAX_ACTIVITY_ITEMS {
            anyhow::bail!("activity limit must be between 1 and {MAX_ACTIVITY_ITEMS}");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Snapshot {
    pub generated_at: DateTime<Utc>,
    pub revision: u64,
    pub service_online: bool,
    pub guard_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guard_paused_until: Option<DateTime<Utc>>,
    pub watched: usize,
    pub paused: usize,
    pub attention: usize,
    pub attention_target_keys: Vec<String>,
    pub automatic_recoveries: usize,
    pub hands_free_percent: u8,
}

fn default_provider() -> String {
    "codex".into()
}

fn default_session_limit() -> usize {
    100
}

fn default_activity_limit() -> usize {
    50
}

fn default_true() -> bool {
    true
}
