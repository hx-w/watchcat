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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchlistDocument {
    version: u32,
    #[serde(default)]
    targets: Vec<WatchTarget>,
    excluded: std::collections::BTreeSet<String>,
}

impl Default for WatchlistDocument {
    fn default() -> Self {
        Self {
            version: 4,
            targets: Vec::new(),
            excluded: Default::default(),
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

    fn load(&self) -> Result<WatchlistDocument> {
        let document: WatchlistDocument = load_json_or_default(&self.path)?;
        if document.version != 4 {
            bail!(
                "unsupported watchlist version {}; this release requires version 4",
                document.version
            );
        }
        Ok(document)
    }

    pub fn list(&self) -> Result<Vec<WatchTarget>> {
        Ok(self.load()?.targets)
    }

    pub fn add(&self, target: WatchTarget) -> Result<bool> {
        let mut document = self.load()?;
        document.excluded.remove(&target.key());
        let added = !document
            .targets
            .iter()
            .any(|entry| entry.key() == target.key());
        if let Some(existing) = document
            .targets
            .iter_mut()
            .find(|entry| entry.key() == target.key())
        {
            existing.added_at = target.added_at;
            existing.source = crate::models::TrackingSource::Manual;
            if target.label.is_some() {
                existing.label = target.label;
            }
        } else {
            document.targets.push(target);
        }
        save_json(&self.path, &document)?;
        Ok(added)
    }

    pub fn remove(&self, key: &str) -> Result<bool> {
        let mut document = self.load()?;
        let original = document.targets.len();
        document.targets.retain(|target| target.key() != key);
        document.excluded.insert(key.into());
        save_json(&self.path, &document)?;
        Ok(original != document.targets.len())
    }

    /// Only provider activity extends membership. Manual removal persists until add.
    pub fn reconcile(
        &self,
        sessions: &[crate::models::Session],
        now: DateTime<Utc>,
        stale_after_seconds: i64,
        protected: &HashSet<String>,
    ) -> Result<Vec<WatchTarget>> {
        let mut document = self.load()?;
        let previous = document.clone();
        let cutoff = now - Duration::seconds(stale_after_seconds);
        let mut positions = document
            .targets
            .iter()
            .enumerate()
            .map(|(i, t)| (t.key(), i))
            .collect::<HashMap<_, _>>();
        for session in sessions {
            if document.excluded.contains(&session.key()) {
                continue;
            }
            if let Some(&index) = positions.get(&session.key()) {
                let target = &mut document.targets[index];
                target.title = Some(session.title.clone());
                target.last_activity_at = target.last_activity_at.max(session.updated_at);
            } else if session.updated_at.is_some_and(|updated| updated >= cutoff) {
                positions.insert(session.key(), document.targets.len());
                document.targets.push(WatchTarget {
                    source: crate::models::TrackingSource::Automatic,
                    provider: session.provider.clone(),
                    session_id: session.id.clone(),
                    label: None,
                    title: Some(session.title.clone()),
                    added_at: now,
                    last_activity_at: session.updated_at,
                });
            }
        }
        document.targets.retain(|target| {
            protected.contains(&target.key()) || target.inactivity_since(None) >= cutoff
        });
        let targets = document.targets.clone();
        if document != previous {
            save_json(&self.path, &document)?;
        }
        Ok(targets)
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
                path: PathBuf::new(),
            }
        };
        if state.version != 3 {
            bail!(
                "unsupported runtime state version {}; this release requires version 3",
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
#[serde(deny_unknown_fields)]
pub struct ControlStateStore {
    version: u32,
    revision: u64,
    #[serde(skip)]
    path: PathBuf,
}

impl ControlStateStore {
    pub fn load(path: PathBuf) -> Result<Self> {
        if path.exists() {
            let bytes = fs::read(&path)
                .with_context(|| format!("cannot read control state {}", path.display()))?;
            let mut state: Self = serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid control state {}", path.display()))?;
            if state.version != 2 {
                bail!("unsupported control state version {}", state.version);
            }
            state.path = path;
            state.next_revision()?;
            return Ok(state);
        }
        let mut state = Self {
            version: 2,
            revision: 0,
            path,
        };
        state.next_revision()?;
        Ok(state)
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
            .context("service lock has no parent directory")?;
        fs::create_dir_all(parent)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("cannot open service lock {}", path.display()))?;
        file.try_lock_exclusive()
            .with_context(|| "another Watchcat process owns this state directory")?;
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
            source: crate::models::TrackingSource::Manual,
            provider: "codex".into(),
            session_id: id.into(),
            label: None,
            title: None,
            added_at: Utc::now(),
            last_activity_at: None,
        }
    }

    #[test]
    fn discovering_recent_history_does_not_extend_its_inactivity_window() {
        let directory = tempfile::tempdir().unwrap();
        let store = WatchlistStore::new(directory.path().join("watchlist.json"));
        let now = Utc::now();
        let session = crate::models::Session {
            provider: "codex".into(),
            id: "recent-history".into(),
            title: "Old work".into(),
            state: crate::models::SessionState::Idle,
            updated_at: Some(now - Duration::hours(71)),
            metadata: serde_json::Value::Null,
        };
        assert_eq!(
            store
                .reconcile(std::slice::from_ref(&session), now, 259200, &HashSet::new())
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .reconcile(
                    &[session],
                    now + Duration::hours(2),
                    259200,
                    &HashSet::new()
                )
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn discovery_expiry_reactivation_and_manual_exclusion() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("watchlist.json");
        let store = WatchlistStore::new(path.clone());
        let now = Utc::now();
        let mut session = crate::models::Session {
            provider: "codex".into(),
            id: "new".into(),
            title: "New work".into(),
            state: crate::models::SessionState::Idle,
            updated_at: Some(now),
            metadata: serde_json::Value::Null,
        };
        let reconcile = |store: &WatchlistStore, session: &crate::models::Session, time| {
            store
                .reconcile(std::slice::from_ref(session), time, 259200, &HashSet::new())
                .unwrap()
        };
        assert_eq!(reconcile(&store, &session, now).len(), 1);
        assert!(reconcile(&store, &session, now + Duration::days(4)).is_empty());
        session.updated_at = Some(now + Duration::days(4));
        assert_eq!(
            reconcile(&store, &session, now + Duration::days(4)).len(),
            1
        );
        store.remove(&session.key()).unwrap();
        let reopened = WatchlistStore::new(path);
        assert!(reconcile(&reopened, &session, now + Duration::days(4)).is_empty());
        let mut manual = target("new");
        manual.added_at = now + Duration::days(4);
        reopened.add(manual).unwrap();
        assert_eq!(
            reconcile(&reopened, &session, now + Duration::days(4)).len(),
            1
        );
    }

    #[test]
    fn failed_discovery_preserves_membership_without_extending_activity() {
        let directory = tempfile::tempdir().unwrap();
        let store = WatchlistStore::new(directory.path().join("watchlist.json"));
        let now = Utc::now();
        let mut old = target("offline");
        old.added_at = now - Duration::days(4);
        old.last_activity_at = Some(old.added_at);
        store.add(old.clone()).unwrap();
        assert_eq!(
            store
                .reconcile(&[], now, 259200, &HashSet::from([old.key()]))
                .unwrap(),
            vec![old]
        );
        assert!(
            store
                .reconcile(&[], now, 259200, &HashSet::new())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn old_watchlist_version_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("watchlist.json");
        fs::write(&path, r#"{"version":1,"targets":[]}"#).unwrap();
        assert!(WatchlistStore::new(path).list().is_err());
    }

    #[test]
    fn obsolete_state_is_rejected_without_rewriting() {
        let directory = tempfile::tempdir().unwrap();
        for version in [1, 2, 5] {
            let path = directory.path().join("state.json");
            let data = format!(r#"{{"version":{version},"targets":[]}}"#);
            fs::write(&path, &data).unwrap();
            assert!(WatchlistStore::new(path.clone()).list().is_err());
            assert!(RuntimeState::load(path.clone()).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), data);
        }
    }

    #[test]
    fn control_state_persists_monotonic_revision() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.json");
        let mut first = ControlStateStore::load(path.clone()).unwrap();
        assert_eq!(first.revision(), 1);
        assert_eq!(first.next_revision().unwrap(), 2);
        drop(first);

        let mut reopened = ControlStateStore::load(path).unwrap();
        assert_eq!(reopened.revision(), 3);
        assert_eq!(reopened.next_revision().unwrap(), 4);
    }

    #[test]
    fn control_state_rolls_back_memory_when_persistence_fails() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.json");
        let mut state = ControlStateStore::load(path.clone()).unwrap();
        let revision = state.revision();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();

        assert!(state.next_revision().is_err());
        assert_eq!(state.revision(), revision);
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
