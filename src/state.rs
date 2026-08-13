use std::collections::{HashMap, HashSet};
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
            version: 3,
            targets: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct WatchlistStore {
    path: PathBuf,
}

impl WatchlistStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn list(&self) -> Result<Vec<WatchTarget>> {
        let document: WatchlistDocument = load_json_or_default(&self.path)?;
        match document.version {
            2 => self.save(document.targets.clone())?,
            3 => {}
            version => {
                bail!(
                    "unsupported watchlist version {version}; this release requires version 2 or 3"
                )
            }
        }
        Ok(document.targets)
    }

    pub fn replace(&self, targets: Vec<WatchTarget>) -> Result<()> {
        self.save(targets)
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

    pub fn set_enabled(&self, provider: &str, session_id: &str, enabled: bool) -> Result<bool> {
        self.update(provider, session_id, |target| target.enabled = enabled)
    }

    pub fn set_protected(&self, provider: &str, session_id: &str, protected: bool) -> Result<bool> {
        self.update(provider, session_id, |target| target.protected = protected)
    }

    pub fn touch(
        &self,
        provider: &str,
        session_id: &str,
        timestamp: DateTime<Utc>,
    ) -> Result<bool> {
        self.update(provider, session_id, |target| {
            target.last_event_at = Some(timestamp)
        })
    }

    pub fn remove_stale(
        &self,
        now: DateTime<Utc>,
        stale_after_seconds: i64,
        unresolved: &HashSet<String>,
    ) -> Result<Vec<WatchTarget>> {
        let (targets, removed) = self.plan_stale_removal(now, stale_after_seconds, unresolved)?;
        if !removed.is_empty() {
            self.save(targets)?;
        }
        Ok(removed)
    }

    pub fn plan_stale_removal(
        &self,
        now: DateTime<Utc>,
        stale_after_seconds: i64,
        unresolved: &HashSet<String>,
    ) -> Result<(Vec<WatchTarget>, Vec<WatchTarget>)> {
        let mut targets = self.list()?;
        let cutoff = now - Duration::seconds(stale_after_seconds);
        let mut removed = Vec::new();
        targets.retain(|target| {
            let latest = target.last_event_at.unwrap_or(target.added_at);
            let keep = target.protected || unresolved.contains(&target.key()) || latest >= cutoff;
            if !keep {
                removed.push(target.clone());
            }
            keep
        });
        Ok((targets, removed))
    }

    fn update(
        &self,
        provider: &str,
        session_id: &str,
        update: impl FnOnce(&mut WatchTarget),
    ) -> Result<bool> {
        let mut targets = self.list()?;
        let Some(target) = targets
            .iter_mut()
            .find(|target| target.provider == provider && target.session_id == session_id)
        else {
            return Ok(false);
        };
        update(target);
        self.save(targets)?;
        Ok(true)
    }

    fn save(&self, targets: Vec<WatchTarget>) -> Result<()> {
        save_json(
            &self.path,
            &WatchlistDocument {
                version: 3,
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
    #[serde(default)]
    pending_recoveries: HashMap<String, PendingRecovery>,
    #[serde(default)]
    recovery_outcomes: Vec<RecoveryOutcome>,
    #[serde(default = "default_true")]
    guard_enabled: bool,
    #[serde(default)]
    guard_paused_until: Option<DateTime<Utc>>,
    #[serde(default)]
    revision: u64,
    #[serde(skip)]
    path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct HandledFailure {
    action: String,
    at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingRecovery {
    pub failure_key: String,
    pub provider: String,
    pub session_id: String,
    pub failed_turn_id: String,
    pub recovery_turn_id: String,
    pub started_at: DateTime<Utc>,
    pub automatic: bool,
    #[serde(default)]
    pub observation_failures: u32,
    #[serde(default)]
    pub last_observation_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RecoveryOutcome {
    completed_at: DateTime<Utc>,
    automatic: bool,
    success: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecoveryMetrics {
    pub automatic_recoveries: usize,
    pub manual_recoveries: usize,
    pub failed_recoveries: usize,
    pub hands_free_percent: u8,
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
                version: 3,
                handled: HashMap::new(),
                observed: HashMap::new(),
                attempts: HashMap::new(),
                pending_recoveries: HashMap::new(),
                recovery_outcomes: Vec::new(),
                guard_enabled: true,
                guard_paused_until: None,
                revision: 0,
                path: PathBuf::new(),
            }
        };
        let migrated = state.version == 2;
        match state.version {
            2 => state.version = 3,
            3 => {}
            version => bail!(
                "unsupported runtime state version {version}; this release requires version 2 or 3"
            ),
        }
        state.path = path;
        if migrated {
            state.save()?;
        }
        Ok(state)
    }

    pub fn save(&self) -> Result<()> {
        save_json(&self.path, self)
    }

    pub fn guard_state(&self) -> (bool, Option<DateTime<Utc>>) {
        (self.guard_enabled, self.guard_paused_until)
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn set_guard_state(
        &mut self,
        enabled: bool,
        paused_until: Option<DateTime<Utc>>,
    ) -> Result<()> {
        let previous = (self.guard_enabled, self.guard_paused_until);
        self.guard_enabled = enabled;
        self.guard_paused_until = paused_until;
        if let Err(error) = self.save() {
            self.guard_enabled = previous.0;
            self.guard_paused_until = previous.1;
            return Err(error);
        }
        Ok(())
    }

    pub fn next_revision(&mut self) -> Result<u64> {
        let previous = self.revision;
        self.revision = self.revision.saturating_add(1).max(1);
        if let Err(error) = self.save() {
            self.revision = previous;
            return Err(error);
        }
        Ok(self.revision)
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

    pub fn begin_recovery(
        &mut self,
        failure: &Failure,
        recovery_turn_id: String,
        now: DateTime<Utc>,
        automatic: bool,
    ) {
        self.pending_recoveries.insert(
            failure.key(),
            PendingRecovery {
                failure_key: failure.key(),
                provider: failure.provider.clone(),
                session_id: failure.session_id.clone(),
                failed_turn_id: failure.turn_id.clone(),
                recovery_turn_id,
                started_at: now,
                automatic,
                observation_failures: 0,
                last_observation_at: None,
            },
        );
    }

    pub fn pending_recoveries(&self) -> Vec<PendingRecovery> {
        self.pending_recoveries.values().cloned().collect()
    }

    pub fn pending_target_keys(&self) -> HashSet<String> {
        self.pending_recoveries
            .values()
            .map(|pending| format!("{}:{}", pending.provider, pending.session_id))
            .collect()
    }

    pub fn has_pending_for(&self, target: &WatchTarget) -> bool {
        self.pending_recoveries.values().any(|pending| {
            pending.provider == target.provider && pending.session_id == target.session_id
        })
    }

    pub fn finish_recovery(
        &mut self,
        failure_key: &str,
        success: bool,
        now: DateTime<Utc>,
    ) -> Option<PendingRecovery> {
        let pending = self.pending_recoveries.remove(failure_key)?;
        self.recovery_outcomes.push(RecoveryOutcome {
            completed_at: now,
            automatic: pending.automatic,
            success,
        });
        Some(pending)
    }

    pub fn record_recovery_observation_failure(
        &mut self,
        failure_key: &str,
        now: DateTime<Utc>,
    ) -> Option<u32> {
        let pending = self.pending_recoveries.get_mut(failure_key)?;
        pending.observation_failures = pending.observation_failures.saturating_add(1);
        pending.last_observation_at = Some(now);
        Some(pending.observation_failures)
    }

    pub fn abandon_recovery(&mut self, failure_key: &str) -> Option<PendingRecovery> {
        self.pending_recoveries.remove(failure_key)
    }

    pub fn metrics_since(&self, since: DateTime<Utc>) -> RecoveryMetrics {
        let mut metrics = RecoveryMetrics::default();
        for outcome in self
            .recovery_outcomes
            .iter()
            .filter(|outcome| outcome.completed_at >= since)
        {
            match (outcome.success, outcome.automatic) {
                (true, true) => metrics.automatic_recoveries += 1,
                (true, false) => metrics.manual_recoveries += 1,
                (false, _) => metrics.failed_recoveries += 1,
            }
        }
        let resolved = metrics.automatic_recoveries + metrics.manual_recoveries;
        metrics.hands_free_percent = metrics
            .automatic_recoveries
            .saturating_mul(100)
            .checked_div(resolved)
            .and_then(|percent| u8::try_from(percent).ok())
            .unwrap_or(100);
        metrics
    }

    pub fn prune(&mut self, now: DateTime<Utc>) {
        let cutoff = now - Duration::days(30);
        self.handled.retain(|_, handled| handled.at >= cutoff);
        self.observed.retain(|_, observed| *observed >= cutoff);
        self.attempts.retain(|_, attempts| {
            attempts.retain(|attempt| *attempt >= cutoff);
            !attempts.is_empty()
        });
        self.recovery_outcomes
            .retain(|outcome| outcome.completed_at >= cutoff);
        if self.recovery_outcomes.len() > 10_000 {
            self.recovery_outcomes
                .drain(..self.recovery_outcomes.len() - 10_000);
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ControlStateStore {
    version: u32,
    guard_enabled: bool,
    #[serde(default)]
    guard_paused_until: Option<DateTime<Utc>>,
    revision: u64,
    #[serde(skip)]
    path: PathBuf,
}

impl ControlStateStore {
    pub fn load(
        path: PathBuf,
        legacy_guard: (bool, Option<DateTime<Utc>>),
        legacy_revision: u64,
    ) -> Result<Self> {
        if path.exists() {
            let bytes = fs::read(&path)
                .with_context(|| format!("cannot read control state {}", path.display()))?;
            let mut state: Self = serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid control state {}", path.display()))?;
            if state.version != 1 {
                bail!("unsupported control state version {}", state.version);
            }
            state.path = path;
            state.next_revision()?;
            return Ok(state);
        }
        let mut state = Self {
            version: 1,
            guard_enabled: legacy_guard.0,
            guard_paused_until: legacy_guard.1,
            revision: legacy_revision,
            path,
        };
        state.next_revision()?;
        Ok(state)
    }

    pub fn guard_state(&self) -> (bool, Option<DateTime<Utc>>) {
        (self.guard_enabled, self.guard_paused_until)
    }

    pub fn set_guard_state(
        &mut self,
        enabled: bool,
        paused_until: Option<DateTime<Utc>>,
    ) -> Result<()> {
        let previous = (self.guard_enabled, self.guard_paused_until);
        self.guard_enabled = enabled;
        self.guard_paused_until = paused_until;
        if let Err(error) = self.save() {
            self.guard_enabled = previous.0;
            self.guard_paused_until = previous.1;
            return Err(error);
        }
        Ok(())
    }

    pub fn set_guard_state_and_advance(
        &mut self,
        enabled: bool,
        paused_until: Option<DateTime<Utc>>,
    ) -> Result<u64> {
        let previous = (self.guard_enabled, self.guard_paused_until, self.revision);
        self.guard_enabled = enabled;
        self.guard_paused_until = paused_until;
        self.revision = self.revision.saturating_add(1).max(1);
        if let Err(error) = self.save() {
            self.guard_enabled = previous.0;
            self.guard_paused_until = previous.1;
            self.revision = previous.2;
            return Err(error);
        }
        Ok(self.revision)
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn next_revision(&mut self) -> Result<u64> {
        let previous = self.revision;
        self.revision = self.revision.saturating_add(1).max(1);
        if let Err(error) = self.save() {
            self.revision = previous;
            return Err(error);
        }
        Ok(self.revision)
    }

    fn save(&self) -> Result<()> {
        save_json(&self.path, self)
    }
}

fn default_true() -> bool {
    true
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

    pub fn set_retention(&mut self, retention: usize) {
        self.retention = retention;
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
            protected: false,
            label: None,
            added_at: Utc::now(),
            last_event_at: None,
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
    fn version_two_watchlist_migrates_without_losing_targets() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("watchlist.json");
        fs::write(
            &path,
            format!(
                r#"{{"version":2,"targets":[{{"provider":"codex","session_id":"one","enabled":true,"label":null,"added_at":"{}"}}]}}"#,
                Utc::now().to_rfc3339()
            ),
        )
        .unwrap();

        let targets = WatchlistStore::new(path.clone()).list().unwrap();
        assert_eq!(targets.len(), 1);
        assert!(!targets[0].protected);
        let document: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(document["version"], 3);
    }

    #[test]
    fn stale_cleanup_keeps_protected_unresolved_and_recent_targets() {
        let directory = tempfile::tempdir().unwrap();
        let store = WatchlistStore::new(directory.path().join("watchlist.json"));
        let now = Utc::now();
        let mut old = target("old");
        old.added_at = now - Duration::days(4);
        let mut protected = target("protected");
        protected.added_at = now - Duration::days(4);
        protected.protected = true;
        let mut unresolved = target("unresolved");
        unresolved.added_at = now - Duration::days(4);
        let mut recent = target("recent");
        recent.added_at = now - Duration::days(4);
        recent.last_event_at = Some(now - Duration::days(1));
        for item in [old, protected, unresolved.clone(), recent] {
            store.add(item).unwrap();
        }

        let removed = store
            .remove_stale(now, 3 * 86_400, &HashSet::from([unresolved.key()]))
            .unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].session_id, "old");
        let remaining = store
            .list()
            .unwrap()
            .into_iter()
            .map(|item| item.session_id)
            .collect::<HashSet<_>>();
        assert_eq!(
            remaining,
            HashSet::from(["protected".into(), "unresolved".into(), "recent".into()])
        );
    }

    #[test]
    fn control_state_persists_guard_and_monotonic_revision() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.json");
        let mut first = ControlStateStore::load(path.clone(), (true, None), 7).unwrap();
        assert_eq!(first.revision(), 8);
        assert_eq!(first.set_guard_state_and_advance(false, None).unwrap(), 9);
        drop(first);

        let mut reopened = ControlStateStore::load(path, (true, None), 0).unwrap();
        assert_eq!(reopened.guard_state(), (false, None));
        assert_eq!(reopened.revision(), 10);
        assert_eq!(reopened.next_revision().unwrap(), 11);
    }

    #[test]
    fn control_state_rolls_back_memory_when_persistence_fails() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.json");
        let mut state = ControlStateStore::load(path.clone(), (true, None), 4).unwrap();
        let revision = state.revision();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();

        assert!(state.next_revision().is_err());
        assert_eq!(state.revision(), revision);
        assert!(state.set_guard_state(false, None).is_err());
        assert_eq!(state.guard_state(), (true, None));
        assert!(state.set_guard_state_and_advance(false, None).is_err());
        assert_eq!(state.guard_state(), (true, None));
        assert_eq!(state.revision(), revision);
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
