use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::{Datelike, TimeZone, Utc};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
#[cfg(unix)]
use tokio::task::JoinSet;
use tracing::{info, warn};
use uuid::Uuid;

use crate::client::{read_frame_with_limit, write_frame};
use crate::conditions::is_known;
use crate::config::{Paths, load_settings, save_settings};
use crate::engine::{RecoveryPermit, WatchEngine};
use crate::models::{EngineEvent, WatchTarget};
use crate::protocol::{
    ActivityQuery, MAX_FRAME_BYTES, PROTOCOL_VERSION, PolicyUpdate, RetryRequest, RpcError,
    RpcNotification, RpcRequest, RpcResponse, SessionMessage, SessionQuery, SessionRef, Snapshot,
    WatchAdd, WatchUpdate,
};
use crate::providers::{build_providers, start_providers};
use crate::state::{ControlStateStore, EventLogStore, ProcessLock, RuntimeState, WatchlistStore};
use crate::transport::acknowledgement_is_unknown;

const MAX_RPC_CONNECTIONS: usize = 16;
const MAX_EVENT_SUBSCRIPTIONS: usize = 4;
const MAX_RPC_REQUEST_BYTES: usize = 1024 * 1024;
const RPC_FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const EVENT_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);

#[cfg(unix)]
struct SocketCleanup(PathBuf);

#[cfg(unix)]
impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[derive(Clone)]
struct ReconcilePlan {
    now: chrono::DateTime<Utc>,
    revision: u64,
    guard_enabled: bool,
    targets: Vec<WatchTarget>,
    settings: crate::config::Settings,
    refresh_lifecycle: bool,
}

struct ReconcileOutcome {
    events: Vec<EngineEvent>,
    session_activity: Vec<(String, String, chrono::DateTime<Utc>)>,
    lifecycle_protected: HashSet<String>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct RetryOperation {
    operation_id: String,
    provider: String,
    session_id: String,
    request_key: String,
    status: String,
    result: Option<Value>,
    error: Option<String>,
    updated_at: chrono::DateTime<Utc>,
}

impl RetryOperation {
    fn value(&self) -> Value {
        json!({
            "operation_id": self.operation_id,
            "provider": self.provider,
            "session_id": self.session_id,
            "status": self.status,
            "result": self.result,
            "error": self.error,
            "updated_at": self.updated_at,
        })
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
struct RetryOperationDocument {
    version: u32,
    operations: Vec<RetryOperation>,
}

struct RetryOperationRegistry {
    operations: HashMap<String, RetryOperation>,
    path: PathBuf,
}

impl RetryOperationRegistry {
    fn load(path: PathBuf) -> Result<Self> {
        let operations = if path.exists() {
            let document: RetryOperationDocument = serde_json::from_slice(
                &fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?,
            )
            .with_context(|| format!("invalid retry operation state {}", path.display()))?;
            if document.version != 1 {
                bail!(
                    "unsupported retry operation state version {}",
                    document.version
                );
            }
            document
                .operations
                .into_iter()
                .map(|mut operation| {
                    if matches!(operation.status.as_str(), "accepted" | "running") {
                        operation.status = "unknown".into();
                        operation.error = Some(
                            "Watchcat restarted before this retry reached a confirmed result"
                                .into(),
                        );
                        operation.updated_at = Utc::now();
                    }
                    (operation.operation_id.clone(), operation)
                })
                .collect()
        } else {
            HashMap::new()
        };
        let registry = Self { operations, path };
        registry.save()?;
        Ok(registry)
    }

    fn save(&self) -> Result<()> {
        let mut operations = self.operations.values().cloned().collect::<Vec<_>>();
        operations.sort_by_key(|operation| operation.updated_at);
        let mut bytes = serde_json::to_vec_pretty(&RetryOperationDocument {
            version: 1,
            operations,
        })?;
        bytes.push(b'\n');
        crate::config::atomic_write(&self.path, &bytes)
    }

    fn accept(
        &mut self,
        session: &SessionRef,
        request_key: &str,
    ) -> Result<(RetryOperation, bool)> {
        if request_key.trim().is_empty() {
            bail!("request_key is required for manual retry");
        }
        if let Some(operation) = self
            .operations
            .values()
            .find(|operation| operation.request_key == request_key)
            .cloned()
        {
            if operation.provider != session.provider || operation.session_id != session.session_id
            {
                bail!("request_key already belongs to another session");
            }
            return Ok((operation, false));
        }
        if let Some(operation) = self
            .operations
            .values()
            .find(|operation| {
                matches!(operation.status.as_str(), "accepted" | "running")
                    && operation.provider == session.provider
                    && operation.session_id == session.session_id
            })
            .cloned()
        {
            return Ok((operation, false));
        }
        if self
            .operations
            .values()
            .filter(|operation| matches!(operation.status.as_str(), "accepted" | "running"))
            .count()
            >= 32
        {
            bail!("too many manual retry operations are already pending");
        }
        let previous = self.operations.clone();
        if self.operations.len() >= 256 {
            if let Some(oldest) = self
                .operations
                .values()
                .filter(|operation| !matches!(operation.status.as_str(), "accepted" | "running"))
                .min_by_key(|operation| operation.updated_at)
                .map(|operation| operation.operation_id.clone())
            {
                self.operations.remove(&oldest);
            }
        }
        let operation = RetryOperation {
            operation_id: Uuid::new_v4().to_string(),
            provider: session.provider.clone(),
            session_id: session.session_id.clone(),
            request_key: request_key.into(),
            status: "accepted".into(),
            result: None,
            error: None,
            updated_at: Utc::now(),
        };
        self.operations
            .insert(operation.operation_id.clone(), operation.clone());
        if let Err(error) = self.save() {
            self.operations = previous;
            return Err(error);
        }
        Ok((operation, true))
    }

    fn find_request(
        &self,
        session: &SessionRef,
        request_key: &str,
    ) -> Result<Option<RetryOperation>> {
        let Some(operation) = self
            .operations
            .values()
            .find(|operation| operation.request_key == request_key)
            .cloned()
        else {
            return Ok(None);
        };
        if operation.provider != session.provider || operation.session_id != session.session_id {
            bail!("request_key already belongs to another session");
        }
        Ok(Some(operation))
    }

    fn set_running(&mut self, operation_id: &str) -> Result<()> {
        let previous = self.operations.clone();
        if let Some(operation) = self.operations.get_mut(operation_id) {
            operation.status = "running".into();
            operation.updated_at = Utc::now();
        }
        if let Err(error) = self.save() {
            self.operations = previous;
            return Err(error);
        }
        Ok(())
    }

    fn finish(
        &mut self,
        operation_id: &str,
        result: Result<Value>,
    ) -> Result<Option<RetryOperation>> {
        let previous = self.operations.clone();
        let Some(operation) = self.operations.get_mut(operation_id) else {
            return Ok(None);
        };
        match result {
            Ok(result) => {
                operation.status = "succeeded".into();
                operation.result = Some(result);
                operation.error = None;
            }
            Err(error) => {
                operation.status = if acknowledgement_is_unknown(&error) {
                    "unknown".into()
                } else {
                    "failed".into()
                };
                operation.result = None;
                operation.error = Some(format!("{error:#}"));
            }
        }
        operation.updated_at = Utc::now();
        let operation = operation.clone();
        if let Err(error) = self.save() {
            self.operations = previous;
            return Err(error);
        }
        Ok(Some(operation))
    }
}

pub struct WatchcatDaemon {
    paths: Paths,
    watchlist: WatchlistStore,
    event_log: EventLogStore,
    control_state: ControlStateStore,
    config_modified: Option<std::time::SystemTime>,
    settings: crate::config::Settings,
    recovery_permit: RecoveryPermit,
    attention_target_keys: Vec<String>,
    metrics: crate::state::RecoveryMetrics,
    last_sweep_at: Option<chrono::DateTime<Utc>>,
}

impl WatchcatDaemon {
    pub async fn load(paths: Paths) -> Result<Self> {
        let settings = load_settings(&paths.config_file)?;
        let watchlist = WatchlistStore::new(paths.watchlist_file.clone());
        let targets = watchlist.list()?;
        let state = RuntimeState::load(paths.state_file.clone())?;
        let now = Utc::now();
        let month_start = Utc
            .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
            .single()
            .unwrap_or(now);
        let mut attention_target_keys = state.pending_target_keys().into_iter().collect::<Vec<_>>();
        attention_target_keys.sort();
        let metrics = state.metrics_since(month_start);
        let control_state = ControlStateStore::load(
            paths.control_state_file.clone(),
            state.guard_state(),
            state.revision(),
        )?;
        let recovery_permit = RecoveryPermit::new(
            control_state.guard_state().0,
            &targets,
            control_state.revision(),
        );
        let event_log =
            EventLogStore::new(paths.event_log_file.clone(), settings.engine.log_retention);
        let config_modified = modified_time(&paths.config_file);
        Ok(Self {
            paths,
            watchlist,
            event_log: event_log.clone(),
            control_state,
            config_modified,
            settings,
            recovery_permit,
            attention_target_keys,
            metrics,
            last_sweep_at: None,
        })
    }

    fn revision(&self) -> u64 {
        self.control_state.revision()
    }

    fn guard_state(&self) -> (bool, Option<chrono::DateTime<Utc>>) {
        self.control_state.guard_state()
    }

    fn update_recovery_permit(&self, targets: &[WatchTarget]) {
        self.recovery_permit
            .update(self.guard_state().0, targets, self.revision());
    }

    fn update_guard(
        &mut self,
        enabled: bool,
        paused_until: Option<chrono::DateTime<Utc>>,
    ) -> Result<()> {
        let targets = self.watchlist.list()?;
        self.control_state
            .set_guard_state_and_advance(enabled, paused_until)?;
        self.update_recovery_permit(&targets);
        Ok(())
    }

    fn bump_revision(&mut self) -> Result<()> {
        let targets = self.watchlist.list()?;
        self.control_state.next_revision()?;
        self.update_recovery_permit(&targets);
        Ok(())
    }

    fn build_engine(&self, dry_run: bool) -> Result<WatchEngine> {
        let targets = self.watchlist.list()?;
        let state = RuntimeState::load(self.paths.state_file.clone())?;
        Ok(WatchEngine::new_with_permit(
            self.settings.clone(),
            HashMap::new(),
            targets,
            state,
            self.event_log.clone(),
            dry_run,
            self.recovery_permit.clone(),
        ))
    }

    fn prepare_reconcile(&mut self) -> Result<ReconcilePlan> {
        self.reload_external_config()?;
        if self
            .guard_state()
            .1
            .is_some_and(|until| Utc::now() >= until)
        {
            self.update_guard(true, None)?;
        }
        let targets = self.watchlist.list()?;
        self.update_recovery_permit(&targets);
        let now = Utc::now();
        let refresh_lifecycle = self.last_sweep_at.is_none_or(|last| {
            now.signed_duration_since(last).num_seconds()
                >= self.settings.lifecycle.sweep_interval_seconds as i64
        });
        Ok(ReconcilePlan {
            now,
            revision: self.revision(),
            guard_enabled: self.guard_state().0,
            targets,
            settings: self.settings.clone(),
            refresh_lifecycle,
        })
    }

    async fn reconcile_engine(
        engine: &mut WatchEngine,
        plan: &ReconcilePlan,
    ) -> Result<ReconcileOutcome> {
        engine.replace_settings(plan.settings.clone()).await;
        let mut providers = plan
            .targets
            .iter()
            .map(|target| target.provider.clone())
            .collect::<HashSet<_>>();
        providers.extend(engine.pending_provider_names());
        for provider in providers {
            if let Err(error) = engine.ensure_provider(&provider).await {
                warn!(%error, %provider, "provider remains unavailable");
            }
        }
        engine.replace_watch_targets(plan.targets.clone());
        let events = if plan.guard_enabled {
            engine.run_once_authorized(plan.now, plan.revision).await
        } else {
            engine.reconcile_pending_only(plan.now).await
        }?;
        let mut session_activity = Vec::new();
        let mut lifecycle_protected = HashSet::new();
        if plan.guard_enabled && plan.refresh_lifecycle {
            for provider in plan
                .targets
                .iter()
                .map(|target| target.provider.clone())
                .collect::<HashSet<_>>()
            {
                match engine.list_sessions(&provider, 2_001).await {
                    Ok(sessions) => {
                        let complete = sessions.len() <= 2_000;
                        let observed = sessions
                            .iter()
                            .map(|session| session.key())
                            .collect::<HashSet<_>>();
                        session_activity.extend(sessions.into_iter().filter_map(|session| {
                            session
                                .updated_at
                                .map(|updated| (session.provider, session.id, updated))
                        }));
                        if !complete {
                            lifecycle_protected.extend(
                                plan.targets
                                    .iter()
                                    .filter(|target| {
                                        target.provider == provider
                                            && !observed.contains(&target.key())
                                    })
                                    .map(WatchTarget::key),
                            );
                        }
                    }
                    Err(error) => {
                        warn!(%error, %provider, "cannot refresh session activity before lifecycle sweep");
                        lifecycle_protected.extend(
                            plan.targets
                                .iter()
                                .filter(|target| target.provider == provider)
                                .map(WatchTarget::key),
                        );
                    }
                }
            }
        }
        Ok(ReconcileOutcome {
            events,
            session_activity,
            lifecycle_protected,
        })
    }

    fn commit_reconcile(
        &mut self,
        engine: &mut WatchEngine,
        plan: &ReconcilePlan,
        outcome: &ReconcileOutcome,
    ) -> Result<()> {
        if self.revision() != plan.revision || self.recovery_permit.generation() != plan.revision {
            return Ok(());
        }
        self.attention_target_keys = engine.unresolved_target_keys().into_iter().collect();
        self.attention_target_keys.sort();
        let month_start = Utc
            .with_ymd_and_hms(plan.now.year(), plan.now.month(), 1, 0, 0, 0)
            .single()
            .unwrap_or(plan.now);
        self.metrics = engine.metrics_since(month_start);
        for event in &outcome.events {
            if let Some((provider, session_id)) = event.target.split_once(':') {
                let _ = self.watchlist.touch(provider, session_id, event.timestamp);
            }
        }
        for (provider, session_id, updated_at) in &outcome.session_activity {
            let _ = self.watchlist.touch(provider, session_id, *updated_at);
        }
        if plan.guard_enabled && plan.refresh_lifecycle {
            self.sweep_stale(engine, plan.now, &outcome.lifecycle_protected)?;
        }
        Ok(())
    }

    fn sweep_stale(
        &mut self,
        engine: &mut WatchEngine,
        now: chrono::DateTime<Utc>,
        lifecycle_protected: &HashSet<String>,
    ) -> Result<()> {
        let settings = self.settings.clone();
        self.last_sweep_at = Some(now);
        let mut unresolved = if settings.lifecycle.protect_unresolved_failures {
            engine.unresolved_target_keys()
        } else {
            HashSet::new()
        };
        unresolved.extend(lifecycle_protected.iter().cloned());
        let (targets, removed) = self.watchlist.plan_stale_removal(
            now,
            settings.lifecycle.stale_after_seconds,
            &unresolved,
        )?;
        if !removed.is_empty() {
            self.bump_revision()?;
            self.watchlist.replace(targets.clone())?;
            self.update_recovery_permit(&targets);
            info!(count = removed.len(), "removed stale watch targets");
            engine.replace_watch_targets(targets);
        }
        Ok(())
    }

    fn reload_external_config(&mut self) -> Result<()> {
        let modified = modified_time(&self.paths.config_file);
        if modified == self.config_modified {
            return Ok(());
        }
        match load_settings(&self.paths.config_file) {
            Ok(settings) => {
                self.bump_revision()?;
                self.settings = settings;
                self.config_modified = modified;
                info!(revision = self.revision(), "configuration hot-reloaded");
                Ok(())
            }
            Err(error) => {
                warn!(%error, "invalid external configuration; keeping last good revision");
                self.config_modified = modified;
                Ok(())
            }
        }
    }

    pub fn handle_control(&mut self, request: RpcRequest) -> RpcResponse {
        if request.version != PROTOCOL_VERSION {
            return self.error(
                &request.id,
                "protocol_mismatch",
                format!(
                    "client protocol {} is not supported; expected {}",
                    request.version, PROTOCOL_VERSION
                ),
            );
        }
        let result = self.dispatch_control(&request);
        match result {
            Ok(result) => RpcResponse {
                version: PROTOCOL_VERSION,
                id: request.id,
                revision: self.revision(),
                result: Some(result),
                error: None,
            },
            Err(error) => self.error(&request.id, "request_failed", format!("{error:#}")),
        }
    }

    fn dispatch_control(&mut self, request: &RpcRequest) -> Result<Value> {
        match request.method.as_str() {
            "service.ping" | "daemon.ping" => {
                Ok(json!({"online": true, "version": env!("CARGO_PKG_VERSION")}))
            }
            "snapshot.get" => Ok(serde_json::to_value(self.snapshot()?)?),
            "guard.set" => {
                self.check_revision(request)?;
                let enabled = request
                    .params
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .context("enabled is required")?;
                self.update_guard(enabled, None)?;
                Ok(json!({"enabled": enabled}))
            }
            "guard.pause" => {
                self.check_revision(request)?;
                let seconds = request
                    .params
                    .get("seconds")
                    .and_then(Value::as_i64)
                    .unwrap_or(1_800);
                if !(60..=86_400).contains(&seconds) {
                    bail!("guard pause must be between 60 and 86400 seconds");
                }
                let until = Utc::now() + chrono::Duration::seconds(seconds);
                self.update_guard(false, Some(until))?;
                Ok(json!({"enabled": false, "paused_until": until}))
            }
            "watch.list" => serde_json::to_value(self.watchlist.list()?).map_err(Into::into),
            "watch.add" => {
                self.check_revision(request)?;
                let add: WatchAdd = decode(&request.params)?;
                if add.validate {
                    bail!("validated watch.add must use the provider worker");
                }
                let target = WatchTarget {
                    provider: add.session.provider,
                    session_id: add.session.session_id,
                    enabled: true,
                    protected: add.protected,
                    label: add.label,
                    added_at: Utc::now(),
                    last_event_at: None,
                };
                let mut targets = self.watchlist.list()?;
                let added = !targets
                    .iter()
                    .any(|existing| existing.key() == target.key());
                if added {
                    targets.push(target.clone());
                    self.bump_revision()?;
                    self.watchlist.replace(targets.clone())?;
                    self.update_recovery_permit(&targets);
                }
                Ok(json!({"added": added, "target": target}))
            }
            "watch.update" => {
                self.check_revision(request)?;
                let update: WatchUpdate = decode(&request.params)?;
                let mut targets = self.watchlist.list()?;
                let mut changed = false;
                if let Some(target) = targets.iter_mut().find(|target| {
                    target.provider == update.session.provider
                        && target.session_id == update.session.session_id
                }) {
                    if let Some(enabled) = update.enabled {
                        if target.enabled != enabled {
                            target.enabled = enabled;
                            changed = true;
                        }
                    }
                    if let Some(protected) = update.protected {
                        if target.protected != protected {
                            target.protected = protected;
                            changed = true;
                        }
                    }
                }
                if changed {
                    self.bump_revision()?;
                    self.watchlist.replace(targets.clone())?;
                    self.update_recovery_permit(&targets);
                }
                Ok(json!({"changed": changed}))
            }
            "watch.remove" => {
                self.check_revision(request)?;
                let session: SessionRef = decode(&request.params)?;
                let mut targets = self.watchlist.list()?;
                let original = targets.len();
                targets.retain(|target| {
                    target.provider != session.provider || target.session_id != session.session_id
                });
                let removed = targets.len() != original;
                if removed {
                    self.bump_revision()?;
                    self.watchlist.replace(targets.clone())?;
                    self.update_recovery_permit(&targets);
                }
                Ok(json!({"removed": removed}))
            }
            "policies.list" => serde_json::to_value(self.settings.policies()).map_err(Into::into),
            "policies.set" => {
                self.check_revision(request)?;
                let update: PolicyUpdate = decode(&request.params)?;
                if !is_known(&update.condition) {
                    bail!("unknown policy condition: {}", update.condition);
                }
                let mut settings = self.settings.clone();
                let entry = settings.policies.entry(update.condition).or_default();
                if update.policy.action == Some(crate::models::PolicyAction::Skip) {
                    *entry = update.policy;
                } else {
                    if update.policy.action.is_some() {
                        entry.action = update.policy.action;
                    }
                    if update.policy.backoff.is_some() {
                        entry.backoff = update.policy.backoff;
                    }
                    if update.policy.initial_delay_seconds.is_some() {
                        entry.initial_delay_seconds = update.policy.initial_delay_seconds;
                    }
                    if update.policy.max_delay_seconds.is_some() {
                        entry.max_delay_seconds = update.policy.max_delay_seconds;
                    }
                    if update.policy.max_attempts.is_some() {
                        entry.max_attempts = update.policy.max_attempts;
                    }
                    if update.policy.prompt.is_some() {
                        entry.prompt = update.policy.prompt;
                    }
                }
                self.bump_revision()?;
                save_settings(&self.paths.config_file, &settings)?;
                self.config_modified = modified_time(&self.paths.config_file);
                self.settings = settings;
                Ok(json!({"updated": true}))
            }
            "policies.reset" => {
                self.check_revision(request)?;
                let mut settings = self.settings.clone();
                match request.params.get("condition").and_then(Value::as_str) {
                    Some(condition) => {
                        if !is_known(condition) {
                            bail!("unknown policy condition: {condition}");
                        }
                        settings.policies.remove(condition);
                    }
                    None => settings.policies.clear(),
                }
                self.bump_revision()?;
                save_settings(&self.paths.config_file, &settings)?;
                self.config_modified = modified_time(&self.paths.config_file);
                self.settings = settings;
                Ok(json!({"reset": true}))
            }
            "config.get" => serde_json::to_value(&self.settings).map_err(Into::into),
            "config.set_lifecycle" => {
                self.check_revision(request)?;
                let lifecycle = decode(&request.params)?;
                let mut settings = self.settings.clone();
                settings.lifecycle = lifecycle;
                settings.validate()?;
                self.bump_revision()?;
                save_settings(&self.paths.config_file, &settings)?;
                self.config_modified = modified_time(&self.paths.config_file);
                self.settings = settings;
                Ok(json!({"updated": true}))
            }
            method => bail!("unknown RPC method: {method}"),
        }
    }

    fn snapshot(&mut self) -> Result<Snapshot> {
        let targets = self.watchlist.list()?;
        let attention = self.attention_target_keys.len();
        let now = Utc::now();
        let (guard_enabled, guard_paused_until) = self.guard_state();
        Ok(Snapshot {
            generated_at: now,
            revision: self.revision(),
            service_online: true,
            guard_enabled,
            guard_paused_until,
            watched: targets.len(),
            paused: targets.iter().filter(|target| !target.enabled).count(),
            attention,
            attention_target_keys: self.attention_target_keys.clone(),
            automatic_recoveries: self.metrics.automatic_recoveries,
            hands_free_percent: self.metrics.hands_free_percent,
        })
    }

    fn check_revision(&self, request: &RpcRequest) -> Result<()> {
        if let Some(expected) = request.expected_revision {
            if expected != self.revision() {
                bail!(
                    "revision conflict: client has {expected}, server has {}",
                    self.revision()
                );
            }
        }
        Ok(())
    }

    fn error(&self, id: &str, code: &str, message: String) -> RpcResponse {
        RpcResponse {
            version: PROTOCOL_VERSION,
            id: id.into(),
            revision: self.revision(),
            result: None,
            error: Some(RpcError {
                code: code.into(),
                message,
            }),
        }
    }
}

fn is_reliability_log(log: &crate::models::SessionLog) -> bool {
    log.source == "watchcat"
        || log.condition.is_some()
        || matches!(log.kind.as_str(), "turn.failed" | "provider.error")
}

fn provider_log_limit(query: &ActivityQuery) -> usize {
    if query.reliability_only {
        query.limit.saturating_mul(10).min(2_000)
    } else {
        query.limit
    }
}

fn requires_provider(method: &str, params: &Value) -> bool {
    matches!(
        method,
        "sessions.list" | "sessions.logs" | "sessions.send" | "sessions.interrupt"
    ) || (method == "watch.add"
        && params
            .get("validate")
            .and_then(Value::as_bool)
            .unwrap_or(true))
}

fn request_provider(request: &RpcRequest) -> String {
    request
        .params
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("codex")
        .to_owned()
}

async fn build_interactive_engine(
    settings: crate::config::Settings,
    state_file: PathBuf,
    event_log: EventLogStore,
    recovery_permit: RecoveryPermit,
    provider: &str,
) -> Result<WatchEngine> {
    let mut providers = build_providers(&settings, [provider])?;
    start_providers(&mut providers).await?;
    Ok(WatchEngine::new_with_permit(
        settings,
        providers,
        Vec::new(),
        RuntimeState::load(state_file)?,
        event_log,
        false,
        recovery_permit,
    ))
}

async fn dispatch_provider(
    engine: &mut WatchEngine,
    request: &RpcRequest,
    watchlist: &WatchlistStore,
    event_log: &EventLogStore,
) -> Result<Value> {
    if request.version != PROTOCOL_VERSION {
        bail!(
            "client protocol {} is not supported; expected {}",
            request.version,
            PROTOCOL_VERSION,
        );
    }
    match request.method.as_str() {
        "sessions.list" => {
            let query: SessionQuery = decode(&request.params)?;
            engine.ensure_provider(&query.provider).await?;
            let watched = watchlist
                .list()?
                .into_iter()
                .map(|target| target.key())
                .collect::<HashSet<_>>();
            let page_limit = query.limit.clamp(1, 500);
            let page = engine
                .search_sessions(
                    &query.provider,
                    &query.query,
                    query.cursor.as_deref(),
                    page_limit,
                )
                .await?;
            let values = page
                .sessions
                .into_iter()
                .map(|session| {
                    json!({
                        "watched": watched.contains(&session.key()),
                        "session": session,
                    })
                })
                .collect::<Vec<_>>();
            Ok(json!({
                "items": values,
                "next_cursor": page.next_cursor,
                "has_more": page.next_cursor.is_some(),
            }))
        }
        "sessions.logs" => {
            let query: ActivityQuery = decode(&request.params)?;
            query.validate()?;
            engine.ensure_provider(&query.session.provider).await?;
            let mut provider_logs = engine
                .session_logs(
                    &query.session.provider,
                    &query.session.session_id,
                    provider_log_limit(&query),
                )
                .await
                .unwrap_or_else(|error| {
                    vec![crate::models::SessionLog {
                        timestamp: Some(Utc::now()),
                        provider: query.session.provider.clone(),
                        session_id: query.session.session_id.clone(),
                        source: "provider".into(),
                        kind: "provider.error".into(),
                        role: None,
                        turn_id: None,
                        condition: None,
                        message: error.to_string(),
                        metadata: Value::Null,
                    }]
                });
            if query.reliability_only {
                provider_logs.retain(is_reliability_log);
            }
            let mut local = event_log.session_logs(
                &query.session.provider,
                &query.session.session_id,
                query.category.as_deref(),
                query.limit,
            )?;
            if query.reliability_only {
                local.retain(is_reliability_log);
            }
            let mut logs = provider_logs.into_iter().chain(local).collect::<Vec<_>>();
            logs.sort_by_key(|entry| entry.timestamp);
            if logs.len() > query.limit {
                logs.drain(..logs.len() - query.limit);
            }
            serde_json::to_value(logs).map_err(Into::into)
        }
        "sessions.send" => {
            let message: SessionMessage = decode(&request.params)?;
            engine.ensure_provider(&message.session.provider).await?;
            if message.message.trim().is_empty() {
                bail!("message cannot be empty");
            }
            serde_json::to_value(
                engine
                    .send_message(
                        &message.session.provider,
                        &message.session.session_id,
                        message.message.trim(),
                    )
                    .await?,
            )
            .map_err(Into::into)
        }
        "sessions.interrupt" => {
            let session: SessionRef = decode(&request.params)?;
            engine.ensure_provider(&session.provider).await?;
            serde_json::to_value(
                engine
                    .interrupt(&session.provider, &session.session_id)
                    .await?,
            )
            .map_err(Into::into)
        }
        "watch.add" => {
            let add: WatchAdd = decode(&request.params)?;
            engine.ensure_provider(&add.session.provider).await?;
            engine
                .validate_session(&add.session.provider, &add.session.session_id)
                .await?;
            Ok(Value::Null)
        }
        method => bail!("unknown provider RPC method: {method}"),
    }
}

fn encode_response(response: &RpcResponse) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(response)?;
    if bytes.len() <= MAX_FRAME_BYTES {
        return Ok(bytes);
    }
    serde_json::to_vec(&RpcResponse {
        version: PROTOCOL_VERSION,
        id: response.id.clone(),
        revision: response.revision,
        result: None,
        error: Some(RpcError {
            code: "response_too_large".into(),
            message: format!(
                "response exceeds the local RPC limit of {} bytes; request fewer items",
                MAX_FRAME_BYTES
            ),
        }),
    })
    .map_err(Into::into)
}

#[cfg(unix)]
pub async fn serve(paths: Paths, dry_run: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    let _lock = ProcessLock::acquire(paths.lock_file.clone())?;
    if let Some(parent) = paths.socket_file.parent() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    if paths.socket_file.exists() {
        match tokio::net::UnixStream::connect(&paths.socket_file).await {
            Ok(_) => bail!(
                "watchcatd is already listening at {}",
                paths.socket_file.display()
            ),
            Err(_) => fs::remove_file(&paths.socket_file)?,
        }
    }
    let listener = tokio::net::UnixListener::bind(&paths.socket_file)?;
    fs::set_permissions(&paths.socket_file, fs::Permissions::from_mode(0o600))?;
    let socket_path = paths.socket_file.clone();
    let _socket_cleanup = SocketCleanup(socket_path.clone());
    let retry_operations_path = paths.retry_operations_file.clone();
    let control = WatchcatDaemon::load(paths).await?;
    let engine = Arc::new(Mutex::new(control.build_engine(dry_run)?));
    let daemon = Arc::new(Mutex::new(control));
    let retry_operations = Arc::new(Mutex::new(RetryOperationRegistry::load(
        retry_operations_path,
    )?));
    let retry_tasks = Arc::new(Mutex::new(JoinSet::new()));
    let shutting_down = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let connection_slots = Arc::new(tokio::sync::Semaphore::new(MAX_RPC_CONNECTIONS));
    let subscription_slots = Arc::new(tokio::sync::Semaphore::new(MAX_EVENT_SUBSCRIPTIONS));
    let (notifications, _) = tokio::sync::broadcast::channel::<RpcNotification>(256);
    let reconcile_daemon = Arc::clone(&daemon);
    let reconcile_engine = Arc::clone(&engine);
    let reconcile_notifications = notifications.clone();
    let reconcile = tokio::spawn(async move {
        loop {
            let (interval, revision_before, plan) = {
                let mut daemon = reconcile_daemon.lock().await;
                let revision_before = daemon.revision();
                let interval = daemon.settings.engine.poll_interval_seconds;
                (interval, revision_before, daemon.prepare_reconcile())
            };
            match plan {
                Ok(plan) => {
                    let result = {
                        let mut engine = reconcile_engine.lock().await;
                        WatchcatDaemon::reconcile_engine(&mut engine, &plan)
                            .await
                            .map(|outcome| (engine, outcome))
                    };
                    match result {
                        Ok((mut engine, outcome)) => {
                            let mut daemon = reconcile_daemon.lock().await;
                            if let Err(error) =
                                daemon.commit_reconcile(&mut engine, &plan, &outcome)
                            {
                                warn!(%error, "cannot commit daemon reconciliation");
                            }
                            if !outcome.events.is_empty() {
                                let _ = reconcile_notifications.send(RpcNotification {
                                    version: PROTOCOL_VERSION,
                                    event: "engine.events".into(),
                                    revision: daemon.revision(),
                                    data: serde_json::to_value(&outcome.events)
                                        .unwrap_or(Value::Null),
                                });
                            }
                            if daemon.revision() != revision_before {
                                let _ = reconcile_notifications.send(RpcNotification {
                                    version: PROTOCOL_VERSION,
                                    event: "state.changed".into(),
                                    revision: daemon.revision(),
                                    data: json!({"method": "service.reconcile"}),
                                });
                            }
                        }
                        Err(error) => warn!(%error, "daemon reconciliation failed"),
                    }
                }
                Err(error) => warn!(%error, "cannot prepare daemon reconciliation"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        }
    });
    info!(socket = %socket_path.display(), "watchcatd listening");
    let result = loop {
        tokio::select! {
            result = listener.accept() => {
                let (mut stream, _) = result?;
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                {
                    let peer = match stream.peer_cred() {
                        Ok(peer) => peer,
                        Err(error) => {
                            warn!(%error, "rejected RPC connection without peer credentials");
                            continue;
                        }
                    };
                    let peer_uid = peer.uid();
                    if peer_uid != unsafe { libc::geteuid() } {
                        warn!(peer_uid, "rejected RPC peer owned by another user");
                        continue;
                    }
                }
                let Ok(connection_slot) = Arc::clone(&connection_slots).try_acquire_owned() else {
                    warn!(limit = MAX_RPC_CONNECTIONS, "rejected RPC connection above the local limit");
                    continue;
                };
                let daemon = Arc::clone(&daemon);
                let engine = Arc::clone(&engine);
                let notifications = notifications.clone();
                let subscription_slots = Arc::clone(&subscription_slots);
                let retry_operations = Arc::clone(&retry_operations);
                let retry_tasks = Arc::clone(&retry_tasks);
                let shutting_down = Arc::clone(&shutting_down);
                tokio::spawn(async move {
                    let _connection_slot = connection_slot;
                    let result = async {
                        let bytes = tokio::time::timeout(
                            RPC_FRAME_TIMEOUT,
                            read_frame_with_limit(&mut stream, MAX_RPC_REQUEST_BYTES),
                        )
                        .await
                        .context("timed out reading the RPC request")??;
                        let request: RpcRequest = serde_json::from_slice(&bytes)?;
                        if request.method == "events.subscribe" {
                            if request.version != PROTOCOL_VERSION {
                                bail!("client protocol is not supported");
                            }
                            let revision = daemon.lock().await.revision();
                            let Ok(_subscription_slot) = subscription_slots.try_acquire_owned()
                            else {
                                let response = RpcResponse {
                                    version: PROTOCOL_VERSION,
                                    id: request.id,
                                    revision,
                                    result: None,
                                    error: Some(RpcError {
                                        code: "too_many_subscribers".into(),
                                        message: format!(
                                            "Watchcat supports at most {MAX_EVENT_SUBSCRIPTIONS} local event subscribers"
                                        ),
                                    }),
                                };
                                write_frame(&mut stream, &encode_response(&response)?).await?;
                                return Ok(());
                            };
                            let mut receiver = notifications.subscribe();
                            let response = RpcResponse {
                                version: PROTOCOL_VERSION,
                                id: request.id,
                                revision,
                                result: Some(json!({"subscribed": true})),
                                error: None,
                            };
                            write_frame(&mut stream, &encode_response(&response)?).await?;
                            let (mut subscriber, mut publisher) = stream.into_split();
                            let mut disconnect_probe = [0_u8; 1];
                            let mut heartbeat = tokio::time::interval_at(
                                tokio::time::Instant::now() + EVENT_HEARTBEAT_INTERVAL,
                                EVENT_HEARTBEAT_INTERVAL,
                            );
                            loop {
                                tokio::select! {
                                    read = subscriber.read(&mut disconnect_probe) => match read? {
                                        0 => break,
                                        _ => bail!("event subscriber sent unexpected data"),
                                    },
                                    received = receiver.recv() => match received {
                                        Ok(notification) => write_frame(
                                            &mut publisher,
                                            &serde_json::to_vec(&notification)?,
                                        ).await?,
                                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                            let revision = daemon.lock().await.revision();
                                            write_frame(&mut publisher, &serde_json::to_vec(&RpcNotification {
                                                version: PROTOCOL_VERSION,
                                                event: "state.resync_required".into(),
                                                revision,
                                                data: Value::Null,
                                            })?).await?;
                                        }
                                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                    },
                                    _ = heartbeat.tick() => {
                                        let revision = daemon.lock().await.revision();
                                        write_frame(&mut publisher, &serde_json::to_vec(&RpcNotification {
                                            version: PROTOCOL_VERSION,
                                            event: "service.heartbeat".into(),
                                            revision,
                                            data: Value::Null,
                                        })?).await?;
                                    }
                                }
                            }
                            return Ok(());
                        }
                        let method = request.method.clone();
                        if method == "sessions.retry_status" {
                            if request.version != PROTOCOL_VERSION {
                                bail!("client protocol is not supported");
                            }
                            let operation_id = request
                                .params
                                .get("operation_id")
                                .and_then(Value::as_str)
                                .context("operation_id is required")?;
                            let revision = daemon.lock().await.revision();
                            let operation = retry_operations
                                .lock()
                                .await
                                .operations
                                .get(operation_id)
                                .cloned();
                            let response = match operation {
                                Some(operation) => RpcResponse {
                                    version: PROTOCOL_VERSION,
                                    id: request.id,
                                    revision,
                                    result: Some(operation.value()),
                                    error: None,
                                },
                                None => RpcResponse {
                                    version: PROTOCOL_VERSION,
                                    id: request.id,
                                    revision,
                                    result: None,
                                    error: Some(RpcError {
                                        code: "operation_not_found".into(),
                                        message: "manual retry operation is no longer available".into(),
                                    }),
                                },
                            };
                            write_frame(&mut stream, &encode_response(&response)?).await?;
                            return Ok(());
                        }
                        if method == "sessions.retry_now" {
                            let revision = daemon.lock().await.revision();
                            if request.version != PROTOCOL_VERSION {
                                bail!("client protocol is not supported");
                            }
                            let retry: RetryRequest = decode(&request.params)?;
                            if let Some(operation) = retry_operations
                                .lock()
                                .await
                                .find_request(&retry.session, &retry.request_key)?
                            {
                                let response = RpcResponse {
                                    version: PROTOCOL_VERSION,
                                    id: request.id,
                                    revision,
                                    result: Some(operation.value()),
                                    error: None,
                                };
                                write_frame(&mut stream, &encode_response(&response)?).await?;
                                return Ok(());
                            }
                            let prepared = async {
                                let generation = request
                                    .expected_revision
                                    .context("expected_revision is required for manual retry")?;
                                if shutting_down.load(std::sync::atomic::Ordering::Acquire) {
                                    bail!("Watchcat is shutting down");
                                }
                                let daemon = daemon.lock().await;
                                daemon.check_revision(&request)?;
                                Result::<_>::Ok((
                                    generation,
                                    retry.session,
                                    daemon.watchlist.clone(),
                                    daemon.settings.clone(),
                                    retry.request_key,
                                ))
                            };
                            let (generation, session, watchlist, settings, request_key) = match prepared.await {
                                Ok(prepared) => prepared,
                                Err(error) => {
                                    let response = RpcResponse {
                                        version: PROTOCOL_VERSION,
                                        id: request.id,
                                        revision,
                                        result: None,
                                        error: Some(RpcError {
                                            code: "retry_not_accepted".into(),
                                            message: format!("{error:#}"),
                                        }),
                                    };
                                    write_frame(&mut stream, &encode_response(&response)?).await?;
                                    return Ok(());
                                }
                            };
                            let (operation, should_start) = match retry_operations
                                .lock()
                                .await
                                .accept(&session, &request_key)
                            {
                                Ok(operation) => operation,
                                Err(error) => {
                                    let response = RpcResponse {
                                        version: PROTOCOL_VERSION,
                                        id: request.id,
                                        revision,
                                        result: None,
                                        error: Some(RpcError {
                                            code: "retry_not_accepted".into(),
                                            message: error.to_string(),
                                        }),
                                    };
                                    write_frame(&mut stream, &encode_response(&response)?).await?;
                                    return Ok(());
                                }
                            };
                            let response = RpcResponse {
                                version: PROTOCOL_VERSION,
                                id: request.id,
                                revision,
                                result: Some(operation.value()),
                                error: None,
                            };
                            if let Err(error) = write_frame(
                                &mut stream,
                                &encode_response(&response)?,
                            )
                            .await
                            {
                                if should_start {
                                    let mut registry = retry_operations.lock().await;
                                    let previous = registry.operations.clone();
                                    registry.operations.remove(&operation.operation_id);
                                    if let Err(persist_error) = registry.save() {
                                        registry.operations = previous;
                                        warn!(%persist_error, "cannot roll back unacknowledged retry acceptance");
                                    }
                                }
                                return Err(error);
                            }

                            if !should_start {
                                return Ok(());
                            }
                            let operation_id = operation.operation_id.clone();
                            let operation_engine = Arc::clone(&engine);
                            let operation_registry = Arc::clone(&retry_operations);
                            let operation_notifications = notifications.clone();
                            let operation_shutdown = Arc::clone(&shutting_down);
                            let mut tasks = retry_tasks.lock().await;
                            while let Some(completed) = tasks.try_join_next() {
                                if let Err(error) = completed {
                                    warn!(%error, "manual retry operation task failed");
                                }
                            }
                            tasks.spawn(async move {
                                let result = async {
                                    if operation_shutdown.load(std::sync::atomic::Ordering::Acquire) {
                                        bail!("manual retry was cancelled because Watchcat is shutting down");
                                    }
                                    let mut engine = operation_engine.lock().await;
                                    if operation_shutdown.load(std::sync::atomic::Ordering::Acquire) {
                                        bail!("manual retry was cancelled because Watchcat is shutting down");
                                    }
                                    operation_registry
                                        .lock()
                                        .await
                                        .set_running(&operation_id)?;
                                    engine.replace_settings(settings).await;
                                    engine.replace_watch_targets(watchlist.list()?);
                                    engine.ensure_provider(&session.provider).await?;
                                    serde_json::to_value(
                                        engine
                                            .retry_now(
                                                &session.provider,
                                                &session.session_id,
                                                Utc::now(),
                                                false,
                                                generation,
                                            )
                                            .await?,
                                    )
                                    .map_err(Into::into)
                                }
                                .await;
                                match operation_registry
                                    .lock()
                                    .await
                                    .finish(&operation_id, result)
                                {
                                    Ok(Some(operation)) => {
                                    let _ = operation_notifications.send(RpcNotification {
                                        version: PROTOCOL_VERSION,
                                        event: "retry.operation_changed".into(),
                                        revision,
                                        data: operation.value(),
                                    });
                                    }
                                    Ok(None) => {}
                                    Err(error) => warn!(%error, %operation_id, "cannot persist manual retry operation"),
                                }
                            });
                            return Ok(());
                        }
                        let provider_request = requires_provider(&method, &request.params);
                        let response = if provider_request {
                            let (
                                revision,
                                watchlist,
                                event_log,
                                settings,
                                state_file,
                                recovery_permit,
                            ) = {
                                let daemon = daemon.lock().await;
                                (
                                    daemon.revision(),
                                    daemon.watchlist.clone(),
                                    daemon.event_log.clone(),
                                    daemon.settings.clone(),
                                    daemon.paths.state_file.clone(),
                                    daemon.recovery_permit.clone(),
                                )
                            };
                            let provider_result = match build_interactive_engine(
                                settings,
                                state_file,
                                event_log.clone(),
                                recovery_permit,
                                &request_provider(&request),
                            )
                            .await
                            {
                                Ok(mut interactive) => {
                                    let result = dispatch_provider(
                                        &mut interactive,
                                        &request,
                                        &watchlist,
                                        &event_log,
                                    )
                                    .await;
                                    interactive.close().await;
                                    result
                                }
                                Err(error) => Err(error),
                            };
                            match provider_result {
                                Ok(_) if method == "watch.add" => {
                                    let mut request = request;
                                    request.params["validate"] = Value::Bool(false);
                                    daemon.lock().await.handle_control(request)
                                }
                                Ok(result) => RpcResponse {
                                    version: PROTOCOL_VERSION,
                                    id: request.id,
                                    revision,
                                    result: Some(result),
                                    error: None,
                                },
                                Err(error) => RpcResponse {
                                    version: PROTOCOL_VERSION,
                                    id: request.id,
                                    revision,
                                    result: None,
                                    error: Some(RpcError {
                                        code: "request_failed".into(),
                                        message: format!("{error:#}"),
                                    }),
                                },
                            }
                        } else if requires_recovery_boundary(&method) {
                            let permit = daemon.lock().await.recovery_permit.clone();
                            let _send_boundary = permit.enter_send_boundary().await;
                            daemon.lock().await.handle_control(request)
                        } else {
                            daemon.lock().await.handle_control(request)
                        };
                        let revision = response.revision;
                        let succeeded = response.error.is_none();
                        write_frame(&mut stream, &encode_response(&response)?).await?;
                        if succeeded && is_mutation(&method) {
                            let _ = notifications.send(RpcNotification {
                                version: PROTOCOL_VERSION,
                                event: "state.changed".into(),
                                revision,
                                data: json!({"method": method}),
                            });
                        }
                        Ok(())
                    }.await;
                    if let Err(error) = result {
                        warn!(%error, "RPC connection failed");
                    }
                });
            }
            signal = tokio::signal::ctrl_c() => {
                signal?;
                break Ok(());
            }
            _ = terminate.recv() => {
                break Ok(());
            }
        }
    };
    shutting_down.store(true, std::sync::atomic::Ordering::Release);
    let (targets, revision, permit) = {
        let daemon = daemon.lock().await;
        (
            daemon.watchlist.list().unwrap_or_default(),
            daemon.revision().saturating_add(1),
            daemon.recovery_permit.clone(),
        )
    };
    permit.update(false, &targets, revision);
    reconcile.abort();
    let mut tasks = retry_tasks.lock().await;
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    engine.lock().await.close().await;
    result
}

#[cfg(not(unix))]
pub async fn serve(_paths: Paths, _dry_run: bool) -> Result<()> {
    bail!("watchcatd named-pipe transport is not available on this platform yet")
}

fn decode<T: DeserializeOwned>(value: &Value) -> Result<T> {
    serde_json::from_value(value.clone()).map_err(Into::into)
}

fn modified_time(path: &PathBuf) -> Option<std::time::SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

fn is_mutation(method: &str) -> bool {
    matches!(
        method,
        "guard.set"
            | "guard.pause"
            | "sessions.send"
            | "sessions.interrupt"
            | "sessions.retry_now"
            | "watch.add"
            | "watch.update"
            | "watch.remove"
            | "policies.set"
            | "policies.reset"
            | "config.set_lifecycle"
    )
}

fn requires_recovery_boundary(method: &str) -> bool {
    matches!(
        method,
        "guard.set"
            | "guard.pause"
            | "watch.update"
            | "watch.remove"
            | "policies.set"
            | "policies.reset"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::SessionRef;

    fn test_paths(directory: &std::path::Path) -> Paths {
        Paths {
            config_file: directory.join("config.toml"),
            watchlist_file: directory.join("watchlist.json"),
            state_file: directory.join("state.json"),
            control_state_file: directory.join("control.json"),
            event_log_file: directory.join("events.jsonl"),
            retry_operations_file: directory.join("retry-operations.json"),
            lock_file: directory.join("watchcat.lock"),
            socket_file: directory.join("watchcat.sock"),
        }
    }

    #[test]
    fn reliability_activity_reads_a_larger_provider_window_before_filtering() {
        let query = ActivityQuery {
            session: SessionRef {
                provider: "codex".into(),
                session_id: "session".into(),
            },
            limit: 100,
            category: None,
            reliability_only: true,
        };

        assert_eq!(provider_log_limit(&query), 1_000);
    }

    #[test]
    fn full_activity_keeps_the_requested_provider_window() {
        let query = ActivityQuery {
            session: SessionRef {
                provider: "codex".into(),
                session_id: "session".into(),
            },
            limit: 100,
            category: None,
            reliability_only: false,
        };

        assert_eq!(provider_log_limit(&query), 100);
    }

    #[test]
    fn provider_methods_are_kept_off_the_control_plane() {
        for method in [
            "sessions.list",
            "sessions.logs",
            "sessions.send",
            "sessions.interrupt",
        ] {
            assert!(requires_provider(method, &json!({})), "{method}");
        }
        assert!(requires_provider("watch.add", &json!({"validate": true})));
        assert!(!requires_provider("watch.add", &json!({"validate": false})));
        for method in [
            "snapshot.get",
            "guard.set",
            "guard.pause",
            "watch.list",
            "watch.update",
            "watch.remove",
            "policies.list",
            "policies.set",
            "config.get",
            "config.set_lifecycle",
        ] {
            assert!(!requires_provider(method, &json!({})), "{method}");
        }
    }

    #[tokio::test]
    async fn session_search_finds_matches_beyond_the_first_page() {
        struct SearchProvider;

        #[async_trait::async_trait]
        impl crate::providers::Provider for SearchProvider {
            fn name(&self) -> &'static str {
                "search"
            }
            async fn start(&mut self) -> Result<()> {
                Ok(())
            }
            async fn close(&mut self) -> Result<()> {
                Ok(())
            }
            async fn list_sessions(&mut self, limit: usize) -> Result<Vec<crate::models::Session>> {
                Ok((0..limit)
                    .map(|index| crate::models::Session {
                        provider: "search".into(),
                        id: format!("session-{index}"),
                        title: if index == 125 {
                            "Needle".into()
                        } else {
                            "Ordinary".into()
                        },
                        state: crate::models::SessionState::Idle,
                        updated_at: None,
                        metadata: Value::Null,
                    })
                    .collect())
            }
            async fn search_sessions(
                &mut self,
                query: &str,
                cursor: Option<&str>,
                limit: usize,
            ) -> Result<crate::providers::SessionSearchPage> {
                assert_eq!(query, "needle");
                assert!(cursor.is_none());
                assert_eq!(limit, 100);
                Ok(crate::providers::SessionSearchPage {
                    sessions: vec![crate::models::Session {
                        provider: "search".into(),
                        id: "session-125".into(),
                        title: "Needle".into(),
                        state: crate::models::SessionState::Idle,
                        updated_at: None,
                        metadata: Value::Null,
                    }],
                    next_cursor: None,
                })
            }
            async fn session_logs(
                &mut self,
                _: &str,
                _: usize,
            ) -> Result<Vec<crate::models::SessionLog>> {
                Ok(Vec::new())
            }
            async fn latest_failure(&mut self, _: &str) -> Result<Option<crate::models::Failure>> {
                Ok(None)
            }
            async fn resume(
                &mut self,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<crate::models::ResumeReceipt> {
                bail!("unused")
            }
        }

        let mut providers: HashMap<String, Box<dyn crate::providers::Provider>> = HashMap::new();
        providers.insert("search".into(), Box::new(SearchProvider));
        let directory = tempfile::tempdir().unwrap();
        let mut engine = WatchEngine::new(
            crate::config::Settings::default(),
            providers,
            Vec::new(),
            RuntimeState::load(directory.path().join("state.json")).unwrap(),
            EventLogStore::new(directory.path().join("events.jsonl"), 100),
            false,
        );

        let page = engine
            .search_sessions("search", "needle", None, 100)
            .await
            .unwrap();
        assert_eq!(page.sessions.len(), 1);
        assert_eq!(page.sessions[0].id, "session-125");
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn failed_business_commit_still_invalidates_the_old_revision() {
        let directory = tempfile::tempdir().unwrap();
        let paths = test_paths(directory.path());
        let mut daemon = WatchcatDaemon::load(paths.clone()).await.unwrap();
        let old_revision = daemon.revision();
        std::fs::create_dir(&paths.config_file).unwrap();
        let request = RpcRequest {
            version: PROTOCOL_VERSION,
            id: "policy".into(),
            method: "policies.set".into(),
            params: json!({
                "condition": "network.timeout",
                "policy": {"action": "skip"},
            }),
            expected_revision: Some(old_revision),
        };

        let response = daemon.handle_control(request);

        assert!(response.error.is_some());
        assert!(daemon.revision() > old_revision);
        let reopened = ControlStateStore::load(paths.control_state_file, (true, None), 0).unwrap();
        assert!(reopened.revision() > old_revision);
    }

    #[test]
    fn retry_operations_are_bounded_and_deduplicated_per_session() {
        let directory = tempfile::tempdir().unwrap();
        let session = SessionRef {
            provider: "codex".into(),
            session_id: "session".into(),
        };
        let path = directory.path().join("retry-operations.json");
        let mut registry = RetryOperationRegistry::load(path.clone()).unwrap();
        let (operation, should_start) = registry.accept(&session, "request-1").unwrap();
        assert_eq!(operation.status, "accepted");
        assert!(should_start);
        let (duplicate, duplicate_should_start) = registry.accept(&session, "request-1").unwrap();
        assert_eq!(duplicate.operation_id, operation.operation_id);
        assert!(!duplicate_should_start);

        registry.set_running(&operation.operation_id).unwrap();
        let finished = registry
            .finish(&operation.operation_id, Ok(json!({"turn_id": "continued"})))
            .unwrap()
            .unwrap();
        assert_eq!(finished.status, "succeeded");
        assert_eq!(finished.result.unwrap()["turn_id"], "continued");
        assert!(registry.accept(&session, "request-2").unwrap().1);

        let mut reopened = RetryOperationRegistry::load(path).unwrap();
        let restored = reopened.accept(&session, "request-1").unwrap();
        assert_eq!(restored.0.operation_id, operation.operation_id);
        assert!(!restored.1);
    }

    #[test]
    fn unfinished_retry_operation_becomes_unknown_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("retry-operations.json");
        let session = SessionRef {
            provider: "codex".into(),
            session_id: "session".into(),
        };
        let mut registry = RetryOperationRegistry::load(path.clone()).unwrap();
        let (accepted, _) = registry.accept(&session, "request").unwrap();
        registry.set_running(&accepted.operation_id).unwrap();

        let reopened = RetryOperationRegistry::load(path).unwrap();
        let restored = reopened.operations.get(&accepted.operation_id).unwrap();
        assert_eq!(restored.status, "unknown");
        assert!(restored.error.as_deref().unwrap().contains("restarted"));
    }

    #[test]
    fn accepted_retry_can_be_found_after_the_control_revision_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("retry-operations.json");
        let session = SessionRef {
            provider: "codex".into(),
            session_id: "session".into(),
        };
        let mut registry = RetryOperationRegistry::load(path).unwrap();
        let (accepted, _) = registry.accept(&session, "stable-request-key").unwrap();

        let recovered = registry
            .find_request(&session, "stable-request-key")
            .unwrap()
            .unwrap();
        assert_eq!(recovered.operation_id, accepted.operation_id);
        assert_eq!(recovered.status, "accepted");
    }

    #[test]
    fn oversized_response_becomes_a_structured_error() {
        let response = RpcResponse {
            version: PROTOCOL_VERSION,
            id: "request".into(),
            revision: 4,
            result: Some(Value::String("x".repeat(MAX_FRAME_BYTES))),
            error: None,
        };
        let encoded = encode_response(&response).unwrap();
        assert!(encoded.len() < MAX_FRAME_BYTES);
        let fallback: RpcResponse = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(fallback.id, "request");
        assert_eq!(fallback.revision, 4);
        assert_eq!(fallback.error.unwrap().code, "response_too_large");
    }
}
