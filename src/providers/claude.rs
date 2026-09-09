use serde_json::Value;

use crate::providers::codex::ClassifiedError;

/// Classifies the official `StopFailure` hook payload from Claude Code.
///
/// Claude's `rate_limit` and `model_not_found` values are deliberately mapped
/// conservatively because the hook does not always distinguish account limits
/// from temporary capacity, or a missing model from missing entitlement.
pub fn classify_claude_error(error: &str, details: Option<&Value>) -> ClassifiedError {
    let detail_code = details
        .and_then(|value| value.get("type").or_else(|| value.get("code")))
        .and_then(Value::as_str);
    let condition = match (error, detail_code) {
        ("overloaded", Some("model_overloaded")) => "capacity.model_overloaded",
        ("overloaded", _) => "capacity.service_overloaded",
        ("rate_limit", Some("server_throttled")) => "capacity.server_throttled",
        ("rate_limit", Some("usage_limit")) => "quota.usage_exhausted",
        ("rate_limit", _) => "capacity.rate_limited",
        ("authentication_failed", _) => "auth.invalid",
        ("oauth_org_not_allowed", _) => "capability.access_denied",
        ("billing_error", _) => "billing.required",
        ("invalid_request", Some("feature_unsupported")) => "capability.feature_unsupported",
        ("invalid_request", _) => "request.invalid",
        ("model_not_found", Some("access_denied")) => "capability.access_denied",
        ("model_not_found", _) => "capability.model_unavailable",
        ("server_error", _) => "service.server_error",
        ("max_output_tokens", _) => "context.output_limit",
        ("unknown", _) | (_, _) => "failure.unknown",
    };
    ClassifiedError {
        condition: condition.into(),
        provider_code: error.into(),
        retry_after_seconds: details.and_then(|value| {
            value
                .get("retry_after_seconds")
                .or_else(|| value.get("retryAfterSeconds"))
                .and_then(Value::as_u64)
        }),
        scope: if condition == "capacity.model_overloaded" {
            Some("model".into())
        } else if condition == "capacity.service_overloaded" {
            Some("service".into())
        } else {
            None
        },
    }
}

pub fn classify_claude_hook(payload: &Value) -> ClassifiedError {
    let error = payload
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let details = payload.get("error_details");
    let mut classified = classify_claude_error(error, details);
    if error == "overloaded" && payload.get("model").and_then(Value::as_str).is_some() {
        classified.condition = "capacity.model_overloaded".into();
        classified.scope = Some("model".into());
    }
    if details.is_some_and(Value::is_string) {
        // Official StopFailure currently permits free-form details. Keep the
        // top-level code authoritative instead of parsing display text.
        classified.retry_after_seconds = None;
    }
    classified
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn maps_every_official_stop_failure_code() {
        let cases = [
            ("overloaded", "capacity.service_overloaded"),
            ("rate_limit", "capacity.rate_limited"),
            ("authentication_failed", "auth.invalid"),
            ("oauth_org_not_allowed", "capability.access_denied"),
            ("billing_error", "billing.required"),
            ("invalid_request", "request.invalid"),
            ("model_not_found", "capability.model_unavailable"),
            ("server_error", "service.server_error"),
            ("max_output_tokens", "context.output_limit"),
            ("unknown", "failure.unknown"),
        ];
        for (code, expected) in cases {
            assert_eq!(classify_claude_error(code, None).condition, expected);
        }
    }

    #[test]
    fn uses_structured_details_without_guessing_from_messages() {
        let error = classify_claude_error(
            "overloaded",
            Some(&json!({"type": "model_overloaded", "retry_after_seconds": 17})),
        );
        assert_eq!(error.condition, "capacity.model_overloaded");
        assert_eq!(error.retry_after_seconds, Some(17));
        assert_eq!(error.scope.as_deref(), Some("model"));
    }

    #[test]
    fn hook_payload_keeps_free_form_details_conservative() {
        let classified = classify_claude_hook(&json!({
            "error": "rate_limit",
            "error_details": "429 Too Many Requests"
        }));
        assert_eq!(classified.condition, "capacity.rate_limited");
        assert_eq!(classified.retry_after_seconds, None);
    }

    #[test]
    fn hook_payload_scopes_overload_when_the_provider_identifies_a_model() {
        let classified = classify_claude_hook(&json!({
            "error": "overloaded",
            "model": "claude-opus"
        }));
        assert_eq!(classified.condition, "capacity.model_overloaded");
        assert_eq!(classified.scope.as_deref(), Some("model"));
    }
}

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::{Provider, SessionSearchPage};
use crate::models::{Failure, ResumeReceipt, Session, SessionLog, SessionState};

struct Transcript {
    offset: u64,
    modified: Option<SystemTime>,
    identity: (u64, u64),
    uncertain: bool,
    session: Session,
}

/// Reads Claude's native session transcripts. No hooks or second Claude process.
pub struct ClaudeProvider {
    projects: PathBuf,
    transcripts: HashMap<PathBuf, Transcript>,
}

impl ClaudeProvider {
    pub fn new() -> Result<Self> {
        let root = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().join(".claude")))
            .context("cannot determine Claude configuration directory")?;
        Ok(Self {
            projects: root.join("projects"),
            transcripts: HashMap::new(),
        })
    }

    fn refresh(&mut self) -> Result<()> {
        let mut seen = HashSet::new();
        if !self.projects.exists() {
            self.transcripts.clear();
            return Ok(());
        }
        for project in fs::read_dir(&self.projects)? {
            let project = project?;
            if !project.file_type()?.is_dir() {
                continue;
            }
            // Subagent transcripts live deeper and are not independent sessions.
            for entry in fs::read_dir(project.path())? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if uuid::Uuid::parse_str(id).is_err() {
                    continue;
                }
                let metadata = entry.metadata()?;
                let modified = metadata.modified().ok();
                #[cfg(unix)]
                let identity = {
                    use std::os::unix::fs::MetadataExt;
                    (metadata.dev(), metadata.ino())
                };
                #[cfg(not(unix))]
                let identity = (0, 0);
                seen.insert(path.clone());
                let cached = self.transcripts.entry(path.clone()).or_insert_with(|| Transcript {
                    offset: 0, modified: None, identity, uncertain: false,
                    session: Session { provider: "claude".into(), id: id.into(), title: id.into(),
                        state: SessionState::Unknown, updated_at: None,
                        metadata: serde_json::json!({"capabilities": ["discover", "logs"], "transcript": path}),
                    },
                });
                if cached.identity == identity
                    && cached.modified == modified
                    && cached.offset == metadata.len()
                {
                    continue;
                }
                if cached.identity != identity || metadata.len() <= cached.offset {
                    cached.offset = 0;
                    cached.identity = identity;
                    cached.session.updated_at = None;
                }
                let refreshed: Result<()> = (|| {
                    let mut reader = BufReader::new(File::open(&path)?);
                    reader.seek(SeekFrom::Start(cached.offset))?;
                    let mut line = String::new();
                    loop {
                        line.clear();
                        let bytes = reader.read_line(&mut line)?;
                        if bytes == 0 || !line.ends_with('\n') {
                            break;
                        }
                        cached.offset += bytes as u64;
                        let value: Value = match serde_json::from_str(&line) {
                            Ok(value) => value,
                            Err(error) => {
                                cached.uncertain = true;
                                tracing::warn!(%error, path = %path.display(), "skipping malformed Claude record");
                                continue;
                            }
                        };
                        if value["isSidechain"] == true {
                            continue;
                        }
                        if matches!(value["type"].as_str(), Some("user" | "assistant")) {
                            cached.session.updated_at =
                                cached.session.updated_at.max(record_time(&value));
                            if record_time(&value).is_some() {
                                cached.uncertain = false;
                            }
                        }
                        if let Some(title) =
                            value["aiTitle"].as_str().or(value["customTitle"].as_str())
                        {
                            cached.session.title = title.into();
                        }
                    }
                    cached.modified = modified;
                    Ok(())
                })();
                if let Err(error) = refreshed {
                    cached.uncertain = true;
                    cached.modified = None;
                    tracing::warn!(%error, path = %path.display(), "cannot read Claude transcript");
                }
            }
        }
        self.transcripts.retain(|path, _| seen.contains(path));
        Ok(())
    }

    fn transcript_path(&self, id: &str) -> Result<&Path> {
        self.transcripts
            .iter()
            .find(|(_, t)| t.session.id == id)
            .map(|(path, _)| path.as_path())
            .context("Claude session not found")
    }
}

fn record_time(value: &Value) -> Option<DateTime<Utc>> {
    value["timestamp"].as_str()?.parse().ok()
}

fn message_text(value: &Value) -> String {
    let content = &value["message"]["content"];
    if let Some(text) = content.as_str() {
        return text.into();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl Provider for ClaudeProvider {
    fn name(&self) -> &'static str {
        "claude"
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn close(&mut self) -> Result<()> {
        Ok(())
    }
    async fn list_sessions(&mut self, limit: usize) -> Result<Vec<Session>> {
        self.refresh()?;
        let mut sessions = self
            .transcripts
            .values()
            .map(|t| {
                let mut session = t.session.clone();
                if t.uncertain {
                    session.updated_at = None;
                    session.metadata["activity_unknown"] = Value::Bool(true);
                }
                session
            })
            .collect::<Vec<_>>();
        sessions.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        sessions.truncate(limit);
        Ok(sessions)
    }
    async fn recent_sessions(&mut self, cutoff: DateTime<Utc>) -> Result<Vec<Session>> {
        // Unlike a remote catalog, refresh already visits every native transcript.
        // Include uncertain activity even when it sorts after old dated entries.
        let mut sessions = self.list_sessions(usize::MAX).await?;
        sessions.retain(|session| session.updated_at.is_none_or(|updated| updated >= cutoff));
        Ok(sessions)
    }

    async fn search_sessions(
        &mut self,
        query: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<SessionSearchPage> {
        let mut sessions = self.list_sessions(usize::MAX).await?;
        let query = query.to_lowercase();
        sessions.retain(|s| s.id.contains(&query) || s.title.to_lowercase().contains(&query));
        let start = cursor
            .map(str::parse::<usize>)
            .transpose()
            .context("invalid Claude session cursor")?
            .unwrap_or(0);
        let end = start.saturating_add(limit).min(sessions.len());
        let next_cursor = (end < sessions.len()).then(|| end.to_string());
        Ok(SessionSearchPage {
            sessions: sessions.into_iter().skip(start).take(limit).collect(),
            next_cursor,
        })
    }
    async fn validate_session(&mut self, session_id: &str) -> Result<()> {
        self.refresh()?;
        self.transcript_path(session_id).map(|_| ())
    }
    async fn session_logs(&mut self, session_id: &str, limit: usize) -> Result<Vec<SessionLog>> {
        self.refresh()?;
        let mut logs = VecDeque::new();
        let reader = BufReader::new(File::open(self.transcript_path(session_id)?)?);
        for line in reader.lines() {
            let line = line?;
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(role @ ("user" | "assistant")) = value["type"].as_str() else {
                continue;
            };
            if value["isSidechain"] == true {
                continue;
            }
            logs.push_back(SessionLog {
                timestamp: record_time(&value),
                provider: "claude".into(),
                session_id: session_id.into(),
                source: "claude".into(),
                kind: "message".into(),
                role: Some(role.into()),
                turn_id: None,
                condition: None,
                message: message_text(&value),
                metadata: Value::Null,
            });
            if logs.len() > limit {
                logs.pop_front();
            }
        }
        Ok(logs.into_iter().collect())
    }
    async fn latest_failure(&mut self, _session_id: &str) -> Result<Option<Failure>> {
        // Transcript prose is not an authoritative StopFailure event.
        Ok(None)
    }
    async fn resume(
        &mut self,
        _session_id: &str,
        _prompt: &str,
        _idempotency_key: &str,
    ) -> Result<ResumeReceipt> {
        bail!(
            "Claude sessions support discovery and logs; live-session message delivery is not supported"
        )
    }
}

#[cfg(test)]
mod transcript_tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    #[tokio::test]
    async fn recent_discovery_keeps_uncertain_sessions_beyond_an_old_page() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        fs::create_dir(&project).unwrap();
        for index in 1..=501 {
            fs::write(project.join(format!("{}.jsonl", uuid::Uuid::from_u128(index))),
                "{\"type\":\"user\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"message\":{\"content\":\"Old work\"}}\n").unwrap();
        }
        let broken = "f9c5cbea-3f8e-4d0d-9a63-8c7812c81fa5";
        fs::write(project.join(format!("{broken}.jsonl")), "broken record\n").unwrap();
        let mut provider = ClaudeProvider {
            projects: directory.path().into(),
            transcripts: HashMap::new(),
        };
        let sessions = provider
            .recent_sessions("2026-09-06T00:00:00Z".parse().unwrap())
            .await
            .unwrap();
        assert!(
            sessions
                .iter()
                .any(|s| s.id == broken && s.updated_at.is_none())
        );
    }

    #[tokio::test]
    async fn malformed_transcript_does_not_block_other_sessions() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        fs::create_dir(&project).unwrap();
        let broken = "f9c5cbea-3f8e-4d0d-9a63-8c7812c81fa5";
        let valid = "c61e6439-6107-4a88-970f-94d548564049";
        fs::write(project.join(format!("{broken}.jsonl")), "broken record\n").unwrap();
        fs::write(project.join(format!("{valid}.jsonl")), format!("{}\n", json!({
            "type":"user", "timestamp":"2026-09-09T02:00:00Z", "message":{"content":"Check build"}
        }))).unwrap();
        let mut provider = ClaudeProvider {
            projects: directory.path().into(),
            transcripts: HashMap::new(),
        };
        assert!(
            provider
                .list_sessions(10)
                .await
                .unwrap()
                .iter()
                .any(|s| s.id == valid)
        );
        assert_eq!(
            provider.session_logs(valid, 1).await.unwrap()[0].message,
            "Check build"
        );
    }

    #[tokio::test]
    async fn follows_native_transcript_appends_and_ignores_metadata_activity() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        fs::create_dir(&project).unwrap();
        let id = "f9c5cbea-3f8e-4d0d-9a63-8c7812c81fa5";
        let path = project.join(format!("{id}.jsonl"));
        let mut file = File::create(&path).unwrap();
        writeln!(file, "{}", json!({"type":"user","sessionId":id,"timestamp":"2026-09-08T01:00:00Z","message":{"role":"user","content":"Check the build"}})).unwrap();
        let mut provider = ClaudeProvider {
            projects: directory.path().into(),
            transcripts: HashMap::new(),
        };
        let first = provider.list_sessions(10).await.unwrap().pop().unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"system","timestamp":"2026-09-09T10:00:00Z"})
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"ai-title","aiTitle":"Build check","sessionId":id})
        )
        .unwrap();
        let metadata = provider.list_sessions(10).await.unwrap().pop().unwrap();
        assert_eq!(metadata.updated_at, first.updated_at);
        assert_eq!(metadata.title, "Build check");
        // A concurrent writer may leave the final record incomplete for a poll.
        write!(file, "{{\"type\":\"assistant\",").unwrap();
        assert_eq!(
            provider.list_sessions(10).await.unwrap()[0].updated_at,
            first.updated_at
        );
        writeln!(file, "\"timestamp\":\"2026-09-09T02:00:00Z\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"Build passed\"}}]}}}}" ).unwrap();
        assert!(provider.list_sessions(10).await.unwrap()[0].updated_at > first.updated_at);
        let logs = provider.session_logs(id, 1).await.unwrap();
        assert_eq!(logs[0].message, "Build passed");
        assert!(provider.resume(id, "Continue", "test").await.is_err());
    }
}
