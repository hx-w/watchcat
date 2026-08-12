use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::config::atomic_write;
use crate::models::{Failure, WatchTarget};

#[derive(Debug, Serialize, Deserialize)]
struct WatchlistDocument {
    version: u32,
    #[serde(default)]
    targets: Vec<WatchTarget>,
}

impl Default for WatchlistDocument {
    fn default() -> Self {
        Self {
            version: 1,
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
        if document.version != 1 {
            bail!("unsupported watchlist version {}", document.version);
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
                version: 1,
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
                version: 1,
                handled: HashMap::new(),
                observed: HashMap::new(),
                attempts: HashMap::new(),
                path: PathBuf::new(),
            }
        };
        if state.version != 1 {
            bail!("unsupported runtime state version {}", state.version);
        }
        state.path = path;
        Ok(state)
    }

    pub fn save(&self) -> Result<()> {
        save_json(&self.path, self)
    }

    pub fn is_handled(&self, failure: &Failure) -> bool {
        self.handled.contains_key(&failure.key())
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
        use std::io::Write;
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
    fn unknown_watchlist_version_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("watchlist.json");
        fs::write(&path, r#"{"version":2,"targets":[]}"#).unwrap();
        assert!(WatchlistStore::new(path).list().is_err());
    }

    #[test]
    fn process_lock_rejects_a_second_runner() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("watchcat.lock");
        let first = ProcessLock::acquire(path.clone()).unwrap();
        assert!(ProcessLock::acquire(path.clone()).is_err());
        drop(first);
        assert!(ProcessLock::acquire(path).is_ok());
    }
}
