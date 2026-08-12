use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::config::atomic_write;
use crate::models::{EngineEvent, Failure, SessionLog, WatchTarget};

#[derive(Debug, Serialize, Deserialize)]
struct WatchlistDocument {
    version: u32,
    #[serde(default)]
    targets: Vec<WatchTarget>,
}

impl Default for WatchlistDocument {
    fn default() -> Self {
        Self {
            version: 2,
            targets: Vec::new(),
        }
    }
}

pub struct WatchlistStore {
    path: PathBuf,
}

impl WatchlistStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn list(&self) -> Result<Vec<WatchTarget>> {
        let document: WatchlistDocument = load_json_or_default(&self.path)?;
        if document.version != 2 {
            bail!(
                "unsupported watchlist version {}; this release requires version 2",
                document.version
            );
        }
        Ok(document.targets)
    }

    pub fn add(&self, target: WatchTarget) -> Result<bool> {
        let mut targets = self.list()?;
        if targets
            .iter()
            .any(|existing| existing.key() == target.key())
        {
            return Ok(false);
        }
        targets.push(target);
        self.save(targets)?;
        Ok(true)
    }

    pub fn remove(&self, provider: &str, session_id: &str) -> Result<bool> {
        let mut targets = self.list()?;
        let original = targets.len();
        targets.retain(|target| target.provider != provider || target.session_id != session_id);
        if original == targets.len() {
            return Ok(false);
        }
        self.save(targets)?;
        Ok(true)
    }

    fn save(&self, targets: Vec<WatchTarget>) -> Result<()> {
        save_json(
            &self.path,
            &WatchlistDocument {
                version: 2,
                targets,
            },
        )
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RuntimeState {
    version: u32,
    #[serde(default)]
    handled: HashMap<String, HandledFailure>,
    #[serde(default)]
    observed: HashMap<String, DateTime<Utc>>,
    #[serde(default)]
    attempts: HashMap<String, Vec<DateTime<Utc>>>,
    #[serde(skip)]
    path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct HandledFailure {
    action: String,
    at: DateTime<Utc>,
}

impl RuntimeState {
    pub fn load(path: PathBuf) -> Result<Self> {
        let mut state: Self = if path.exists() {
            let bytes = fs::read(&path)
                .with_context(|| format!("cannot read runtime state {}", path.display()))?;
            serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid runtime state {}", path.display()))?
        } else {
            Self {
                version: 2,
                handled: HashMap::new(),
                observed: HashMap::new(),
                attempts: HashMap::new(),
                path: PathBuf::new(),
            }
        };
        if state.version != 2 {
            bail!(
                "unsupported runtime state version {}; this release requires version 2",
                state.version
            );
        }
        state.path = path;
        Ok(state)
    }

    pub fn save(&self) -> Result<()> {
        save_json(&self.path, self)
    }

    pub fn handled_action(&self, failure: &Failure) -> Option<&str> {
        self.handled
            .get(&failure.key())
            .map(|handled| handled.action.as_str())
    }

    pub fn mark_handled(&mut self, failure: &Failure, action: &str, now: DateTime<Utc>) {
        self.handled.insert(
            failure.key(),
            HandledFailure {
                action: action.into(),
                at: now,
            },
        );
    }

    pub fn first_seen(&mut self, failure: &Failure, now: DateTime<Utc>) -> DateTime<Utc> {
        *self.observed.entry(failure.key()).or_insert(now)
    }

    pub fn recent_attempts(
        &mut self,
        target: &WatchTarget,
        now: DateTime<Utc>,
        window_seconds: i64,
    ) -> usize {
        let cutoff = now - Duration::seconds(window_seconds);
        let attempts = self.attempts.entry(target.key()).or_default();
        attempts.retain(|attempt| *attempt >= cutoff);
        attempts.len()
    }

    pub fn record_attempt(&mut self, target: &WatchTarget, now: DateTime<Utc>) {
        self.attempts.entry(target.key()).or_default().push(now);
    }

    pub fn latest_attempt(&self, target: &WatchTarget) -> Option<DateTime<Utc>> {
        self.attempts
            .get(&target.key())
            .and_then(|attempts| attempts.last())
            .copied()
    }

    pub fn clear_attempts(&mut self, target: &WatchTarget) {
        self.attempts.remove(&target.key());
    }
}

#[derive(Clone)]
pub struct EventLogStore {
    path: PathBuf,
    retention: usize,
}

impl EventLogStore {
    pub fn new(path: PathBuf, retention: usize) -> Self {
        Self { path, retention }
    }

    pub fn append(&self, events: &[EngineEvent]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let parent = self.path.parent().context("event log path has no parent")?;
        fs::create_dir_all(parent)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        for event in events {
            serde_json::to_writer(&mut file, event)?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
        if self.count_lines()? > self.retention.saturating_mul(2) {
            self.compact()?;
        }
        Ok(())
    }

    pub fn session_logs(
        &self,
        provider: &str,
        session_id: &str,
        category: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SessionLog>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let file = File::open(&self.path)?;
        let mut logs = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            let event: EngineEvent = serde_json::from_str(&line)
                .with_context(|| format!("invalid event log entry in {}", self.path.display()))?;
            let Some(entry) = event.as_session_log() else {
                continue;
            };
            if entry.provider != provider || entry.session_id != session_id {
                continue;
            }
            if !category.is_none_or(|category| {
                entry
                    .condition
                    .as_deref()
                    .is_some_and(|condition| condition_category(condition) == category)
                    || entry.kind.split('.').next() == Some(category)
            }) {
                continue;
            }
            logs.push(entry);
            if logs.len() > limit {
                logs.remove(0);
            }
        }
        Ok(logs)
    }

    fn count_lines(&self) -> Result<usize> {
        if !self.path.exists() {
            return Ok(0);
        }
        Ok(BufReader::new(File::open(&self.path)?).lines().count())
    }

    fn compact(&self) -> Result<()> {
        let lines = BufReader::new(File::open(&self.path)?)
            .lines()
            .collect::<std::io::Result<Vec<_>>>()?;
        let kept = lines
            .into_iter()
            .rev()
            .take(self.retention)
            .collect::<Vec<_>>();
        let mut bytes = Vec::new();
        for line in kept.into_iter().rev() {
            bytes.extend_from_slice(line.as_bytes());
            bytes.push(b'\n');
        }
        atomic_write(&self.path, &bytes)
    }
}

fn condition_category(condition: &str) -> &str {
    condition.split('.').next().unwrap_or(condition)
}

pub struct ProcessLock {
    file: File,
}

impl ProcessLock {
    pub fn acquire(path: PathBuf) -> Result<Self> {
        let parent = path
            .parent()
            .context("runner lock has no parent directory")?;
        fs::create_dir_all(parent)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("cannot open runner lock {}", path.display()))?;
        file.try_lock_exclusive()
            .with_context(|| "another watchcat runner is active")?;
        file.set_len(0)?;
        (&file).write_all(std::process::id().to_string().as_bytes())?;
        Ok(Self { file })
    }
}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn load_json_or_default<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de> + Default,
{
    if !path.exists() {
        return Ok(T::default());
    }
    let bytes = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("invalid JSON in {}", path.display()))
}

fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    atomic_write(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(id: &str) -> WatchTarget {
        WatchTarget {
            provider: "codex".into(),
            session_id: id.into(),
            enabled: true,
            label: None,
            added_at: Utc::now(),
        }
    }

    #[test]
    fn watchlist_add_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let store = WatchlistStore::new(directory.path().join("watchlist.json"));
        assert!(store.add(target("one")).unwrap());
        assert!(!store.add(target("one")).unwrap());
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn old_watchlist_version_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("watchlist.json");
        fs::write(&path, r#"{"version":1,"targets":[]}"#).unwrap();
        assert!(WatchlistStore::new(path).list().is_err());
    }

    #[test]
    fn event_log_filters_session_category_and_limit() {
        let directory = tempfile::tempdir().unwrap();
        let store = EventLogStore::new(directory.path().join("events.jsonl"), 10);
        let event = |condition: &str, timestamp| EngineEvent {
            timestamp,
            kind: "failure.observed".into(),
            target: "codex:one".into(),
            message: condition.into(),
            condition: Some(condition.into()),
            attempt: None,
            max_attempts: None,
            prompt: None,
            failure: None,
            receipt: None,
        };
        store
            .append(&[
                event("network.timeout", Utc::now()),
                event("capacity.model_overloaded", Utc::now()),
                event("capacity.service_overloaded", Utc::now()),
            ])
            .unwrap();
        let logs = store
            .session_logs("codex", "one", Some("capacity"), 1)
            .unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(
            logs[0].condition.as_deref(),
            Some("capacity.service_overloaded")
        );
    }
}
