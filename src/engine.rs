use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Duration, Utc};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tracing::{info, warn};

use crate::config::Settings;
use crate::models::{
    BackoffKind, EngineEvent, Failure, InterruptReceipt, MessageReceipt, PolicyAction,
    ResumeReceipt, Session, SessionLog, TurnOutcome, WatchTarget,
};
use crate::providers::{Provider, SessionSearchPage, build_providers, start_providers};
use crate::state::{EventLogStore, RuntimeState};
use crate::transport::acknowledgement_is_unknown;

const PENDING_RECOVERY_MAX_AGE: Duration = Duration::days(7);
const ORPHAN_PENDING_RECOVERY_MAX_AGE: Duration = Duration::days(1);
const PENDING_RECOVERY_MAX_OBSERVATION_FAILURES: u32 = 20;

pub struct WatchEngine {
    settings: Settings,
    providers: HashMap<String, Box<dyn Provider>>,
    targets: Vec<WatchTarget>,
    state: RuntimeState,
    event_log: EventLogStore,
    unresolved_targets: HashSet<String>,
    dry_run: bool,
    recovery_permit: RecoveryPermit,
}

#[derive(Clone, Default)]
pub struct RecoveryPermit {
    state: Arc<RwLock<RecoveryPermitState>>,
    send_boundary: Arc<AsyncMutex<()>>,
}

#[derive(Default)]
struct RecoveryPermitState {
    guard_enabled: bool,
    target_keys: HashSet<String>,
    generation: u64,
}

impl RecoveryPermit {
    pub fn new(guard_enabled: bool, targets: &[WatchTarget], generation: u64) -> Self {
        let permit = Self::default();
        permit.update(guard_enabled, targets, generation);
        permit
    }

    pub fn update(&self, guard_enabled: bool, targets: &[WatchTarget], generation: u64) {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(|error| error.into_inner());
        state.guard_enabled = guard_enabled;
        state.target_keys = targets
            .iter()
            .filter(|target| target.enabled)
            .map(WatchTarget::key)
            .collect();
        state.generation = generation;
    }

    fn allows(&self, target: &WatchTarget, generation: u64) -> bool {
        let state = self.state.read().unwrap_or_else(|error| error.into_inner());
        state.guard_enabled
            && state.generation == generation
            && state.target_keys.contains(&target.key())
    }

    pub fn generation(&self) -> u64 {
        self.state
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .generation
    }

    pub async fn enter_send_boundary(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.send_boundary).lock_owned().await
    }
}

impl WatchEngine {
    pub fn new(
        settings: Settings,
        providers: HashMap<String, Box<dyn Provider>>,
        targets: Vec<WatchTarget>,
        state: RuntimeState,
        event_log: EventLogStore,
        dry_run: bool,
    ) -> Self {
        let recovery_permit = RecoveryPermit::new(true, &targets, 0);
        Self::new_with_permit(
            settings,
            providers,
            targets,
            state,
            event_log,
            dry_run,
            recovery_permit,
        )
    }

    pub fn new_with_permit(
        settings: Settings,
        providers: HashMap<String, Box<dyn Provider>>,
        targets: Vec<WatchTarget>,
        state: RuntimeState,
        event_log: EventLogStore,
        dry_run: bool,
        recovery_permit: RecoveryPermit,
    ) -> Self {
        Self {
            settings,
            providers,
            targets: targets
                .into_iter()
                .filter(|target| target.enabled)
                .collect(),
            state,
            event_log,
            unresolved_targets: HashSet::new(),
            dry_run,
            recovery_permit,
        }
    }

    pub async fn run_once(&mut self, now: DateTime<Utc>) -> Result<Vec<EngineEvent>> {
        let generation = self.recovery_permit.generation();
        self.run_once_authorized(now, generation).await
    }

    pub async fn run_once_authorized(
        &mut self,
        now: DateTime<Utc>,
        generation: u64,
    ) -> Result<Vec<EngineEvent>> {
        self.state.prune(now);
        let mut events = Vec::new();
        let mut persisted_events = 0;
        self.reconcile_pending_recoveries(now, &mut events).await;
        for target in self.targets.clone() {
            if !self.recovery_permit.allows(&target, generation) {
                continue;
            }
            let result = match self.providers.get_mut(&target.provider) {
                Some(provider) => provider.latest_failure(&target.session_id).await,
                None => Err(anyhow!("provider is unavailable: {}", target.provider)),
            };
            let failure = match result {
                Ok(Some(failure)) => {
                    self.unresolved_targets.insert(target.key());
                    failure
                }
                Ok(None) => {
                    self.unresolved_targets.remove(&target.key());
                    if !self.state.has_pending_for(&target) {
                        self.state.clear_attempts(&target);
                    }
                    continue;
                }
                Err(error) => {
                    events.push(event(
                        now,
                        "provider.error",
                        &target,
                        error.to_string(),
                        None,
                    ));
                    continue;
                }
            };
            let policy = self.settings.policy(&failure.condition);
            if matches!(
                self.state.handled_action(&failure),
                Some("resumed" | "unconfirmed")
            ) || (policy.action == PolicyAction::Skip
                && matches!(self.state.handled_action(&failure), Some("skipped")))
            {
                continue;
            }
            if policy.action == PolicyAction::Skip {
                self.state.mark_handled(&failure, "skipped", now);
                let mut skipped = event(
                    now,
                    "failure.skipped",
                    &target,
                    format!("{} matched skip policy", failure.condition),
                    Some(failure),
                );
                skipped.condition = Some(policy.condition);
                events.push(skipped);
                continue;
            }

            let first_seen = self.state.first_seen(&failure, now);
            let attempts = self.state.recent_attempts(
                &target,
                now,
                self.settings.engine.attempt_window_seconds,
            );
            if attempts >= policy.max_attempts {
                let mut exhausted = event(
                    now,
                    "retry.exhausted",
                    &target,
                    format!("retry limit reached ({attempts}/{})", policy.max_attempts),
                    Some(failure),
                );
                exhausted.condition = Some(policy.condition);
                exhausted.attempt = Some(attempts);
                exhausted.max_attempts = Some(policy.max_attempts);
                events.push(exhausted);
                continue;
            }

            let delay = retry_delay_seconds(&policy, attempts, failure.retry_after_seconds);
            let delay_from = self.state.latest_attempt(&target).unwrap_or(first_seen);
            let ready_at = delay_from + Duration::seconds(i64::try_from(delay).unwrap_or(i64::MAX));
            if now < ready_at {
                let mut waiting = event(
                    now,
                    "retry.waiting",
                    &target,
                    format!(
                        "{} matched retry policy; next attempt after {ready_at}",
                        failure.condition
                    ),
                    Some(failure),
                );
                waiting.condition = Some(policy.condition);
                waiting.attempt = Some(attempts + 1);
                waiting.max_attempts = Some(policy.max_attempts);
                events.push(waiting);
                continue;
            }

            let prompt = render_prompt(&policy.prompt, &failure, attempts + 1, policy.max_attempts);
            if self.dry_run {
                let mut would_resume = event(
                    now,
                    "retry.dry_run",
                    &target,
                    format!("would resume after {}", failure.condition),
                    Some(failure),
                );
                would_resume.condition = Some(policy.condition);
                would_resume.attempt = Some(attempts + 1);
                would_resume.max_attempts = Some(policy.max_attempts);
                would_resume.prompt = Some(prompt);
                events.push(would_resume);
                continue;
            }

            let provider = self
                .providers
                .get_mut(&target.provider)
                .expect("provider checked above");
            if !self.recovery_permit.allows(&target, generation) {
                events.push(event(
                    now,
                    "retry.cancelled",
                    &target,
                    "guard state changed before resume; no message sent".into(),
                    Some(failure),
                ));
                continue;
            }
            let latest = provider.latest_failure(&target.session_id).await?;
            if !self.recovery_permit.allows(&target, generation) {
                events.push(event(
                    now,
                    "retry.cancelled",
                    &target,
                    "guard state changed before resume; no message sent".into(),
                    Some(failure),
                ));
                continue;
            }
            if latest.as_ref().map(|latest| &latest.turn_id) != Some(&failure.turn_id) {
                events.push(event(
                    now,
                    "retry.cancelled",
                    &target,
                    "session changed before resume; no message sent".into(),
                    Some(failure),
                ));
                continue;
            }

            let _send_boundary = self.recovery_permit.enter_send_boundary().await;
            if !self.recovery_permit.allows(&target, generation) {
                events.push(event(
                    now,
                    "retry.cancelled",
                    &target,
                    "guard state changed before resume; no message sent".into(),
                    Some(failure),
                ));
                continue;
            }
            self.state.record_attempt(&target, now);
            self.state.save()?;
            match provider
                .resume(&target.session_id, &prompt, &failure.key())
                .await
            {
                Ok(receipt) => {
                    self.state
                        .begin_recovery(&failure, receipt.turn_id.clone(), now, true);
                    self.state.mark_handled(&failure, "resumed", now);
                    if let Err(error) = self.state.save() {
                        warn!(
                            %error,
                            turn = receipt.turn_id,
                            "recovery was sent but its runtime state could not be persisted"
                        );
                    }
                    events.push(EngineEvent {
                        timestamp: now,
                        kind: "retry.sent".into(),
                        target: target.key(),
                        message: format!(
                            "started turn {} after {}",
                            receipt.turn_id, failure.condition
                        ),
                        condition: Some(policy.condition),
                        attempt: Some(attempts + 1),
                        max_attempts: Some(policy.max_attempts),
                        prompt: Some(prompt),
                        failure: Some(failure),
                        receipt: Some(receipt),
                    });
                }
                Err(error) if acknowledgement_is_unknown(&error) => {
                    self.state.mark_handled(&failure, "unconfirmed", now);
                    if let Err(save_error) = self.state.save() {
                        warn!(%save_error, "cannot persist unconfirmed recovery state");
                    }
                    let mut unknown = event(
                        now,
                        "retry.unconfirmed",
                        &target,
                        "recovery may have started, but the provider acknowledgement was lost"
                            .into(),
                        Some(failure.clone()),
                    );
                    unknown.condition = Some(policy.condition.clone());
                    unknown.attempt = Some(attempts + 1);
                    unknown.max_attempts = Some(policy.max_attempts);
                    unknown.prompt = Some(prompt.clone());
                    events.push(unknown);
                }
                Err(error) => {
                    let mut failed = event(
                        now,
                        "retry.failed",
                        &target,
                        error.to_string(),
                        Some(failure),
                    );
                    failed.condition = Some(policy.condition);
                    failed.attempt = Some(attempts + 1);
                    failed.max_attempts = Some(policy.max_attempts);
                    failed.prompt = Some(prompt);
                    events.push(failed);
                }
            }
            if let Err(error) = self.event_log.append(&events[persisted_events..]) {
                warn!(%error, "failed to persist retry event; state remains authoritative");
            }
            persisted_events = events.len();
        }
        if let Err(error) = self.state.save() {
            warn!(%error, "failed to persist runtime state");
        }
        if let Err(error) = self.event_log.append(&events[persisted_events..]) {
            warn!(%error, "failed to persist diagnostic events");
        }
        Ok(events)
    }

    async fn reconcile_pending_recoveries(
        &mut self,
        now: DateTime<Utc>,
        events: &mut Vec<EngineEvent>,
    ) {
        for pending in self.state.pending_recoveries() {
            let Some(provider) = self.providers.get_mut(&pending.provider) else {
                continue;
            };
            let outcome = match provider
                .turn_outcome(&pending.session_id, &pending.recovery_turn_id)
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    warn!(%error, turn = pending.recovery_turn_id, "cannot observe recovery outcome");
                    let failures = self
                        .state
                        .record_recovery_observation_failure(&pending.failure_key, now)
                        .unwrap_or_default();
                    if self.pending_recovery_expired(&pending, failures, now) {
                        self.abandon_pending_recovery(&pending, now, events);
                    }
                    continue;
                }
            };
            let success = match outcome {
                TurnOutcome::Completed => true,
                TurnOutcome::Failed => false,
                TurnOutcome::InProgress => {
                    if now.signed_duration_since(pending.started_at) >= PENDING_RECOVERY_MAX_AGE {
                        self.abandon_pending_recovery(&pending, now, events);
                    }
                    continue;
                }
                TurnOutcome::Unknown => {
                    let failures = self
                        .state
                        .record_recovery_observation_failure(&pending.failure_key, now)
                        .unwrap_or_default();
                    if self.pending_recovery_expired(&pending, failures, now) {
                        self.abandon_pending_recovery(&pending, now, events);
                    }
                    continue;
                }
            };
            if self
                .state
                .finish_recovery(&pending.failure_key, success, now)
                .is_some()
            {
                events.push(EngineEvent {
                    timestamp: now,
                    kind: if success {
                        "recovery.completed".into()
                    } else {
                        "recovery.failed".into()
                    },
                    target: format!("{}:{}", pending.provider, pending.session_id),
                    message: if success {
                        format!("recovery turn {} completed", pending.recovery_turn_id)
                    } else {
                        format!("recovery turn {} failed", pending.recovery_turn_id)
                    },
                    condition: None,
                    attempt: None,
                    max_attempts: None,
                    prompt: None,
                    failure: None,
                    receipt: None,
                });
            }
        }
    }

    fn abandon_pending_recovery(
        &mut self,
        pending: &crate::state::PendingRecovery,
        now: DateTime<Utc>,
        events: &mut Vec<EngineEvent>,
    ) {
        if self.state.abandon_recovery(&pending.failure_key).is_none() {
            return;
        }
        events.push(EngineEvent {
            timestamp: now,
            kind: "recovery.unconfirmed".into(),
            target: format!("{}:{}", pending.provider, pending.session_id),
            message: format!(
                "stopped observing recovery turn {} after its outcome remained unavailable",
                pending.recovery_turn_id
            ),
            condition: None,
            attempt: None,
            max_attempts: None,
            prompt: None,
            failure: None,
            receipt: None,
        });
    }

    fn pending_recovery_expired(
        &self,
        pending: &crate::state::PendingRecovery,
        observation_failures: u32,
        now: DateTime<Utc>,
    ) -> bool {
        let age = now.signed_duration_since(pending.started_at);
        age >= PENDING_RECOVERY_MAX_AGE
            || (age >= ORPHAN_PENDING_RECOVERY_MAX_AGE
                && observation_failures >= PENDING_RECOVERY_MAX_OBSERVATION_FAILURES
                && !self.targets.iter().any(|target| {
                    target.provider == pending.provider && target.session_id == pending.session_id
                }))
    }

    pub async fn run_forever_with<F>(&mut self, mut reload: F) -> Result<()>
    where
        F: FnMut() -> Result<(Option<Vec<WatchTarget>>, Option<Settings>, bool)>,
    {
        loop {
            let (targets, settings, guard_enabled) = reload()?;
            self.apply_direct_reload(targets, settings, guard_enabled)
                .await;
            for event in self.run_once(Utc::now()).await? {
                if event.kind.ends_with("error") || event.kind.ends_with("failed") {
                    warn!(
                        kind = event.kind,
                        target = event.target,
                        "{}",
                        event.message
                    );
                } else {
                    info!(
                        kind = event.kind,
                        target = event.target,
                        "{}",
                        event.message
                    );
                }
            }
            tokio::select! {
                result = tokio::signal::ctrl_c() => {
                    result?;
                    break;
                }
                _ = self.wait_for_change() => {}
            }
        }
        Ok(())
    }

    async fn apply_direct_reload(
        &mut self,
        targets: Option<Vec<WatchTarget>>,
        settings: Option<Settings>,
        guard_enabled: bool,
    ) {
        if let Some(targets) = targets {
            let generation = self.recovery_permit.generation();
            self.recovery_permit
                .update(guard_enabled, &targets, generation);
            self.replace_targets(targets);
        }
        if let Some(settings) = settings {
            self.replace_settings(settings).await;
        }
    }

    pub async fn close(&mut self) {
        for provider in self.providers.values_mut() {
            if let Err(error) = provider.close().await {
                warn!(%error, provider = provider.name(), "provider shutdown failed");
            }
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub async fn replace_settings(&mut self, settings: Settings) {
        if self.settings.providers.codex != settings.providers.codex {
            if let Some(mut provider) = self.providers.remove("codex") {
                if let Err(error) = provider.close().await {
                    warn!(%error, "failed to close Codex provider during hot reload");
                }
            }
        }
        self.event_log.set_retention(settings.engine.log_retention);
        self.settings = settings;
    }

    pub fn guard_state(&self) -> (bool, Option<DateTime<Utc>>) {
        self.state.guard_state()
    }

    pub fn set_guard_state(
        &mut self,
        enabled: bool,
        paused_until: Option<DateTime<Utc>>,
    ) -> Result<()> {
        self.state.set_guard_state(enabled, paused_until)
    }

    pub fn next_revision(&mut self) -> Result<u64> {
        self.state.next_revision()
    }

    pub fn replace_watch_targets(&mut self, targets: Vec<WatchTarget>) {
        self.replace_targets(targets);
    }

    pub async fn ensure_provider(&mut self, provider: &str) -> Result<()> {
        if self.providers.contains_key(provider) {
            return Ok(());
        }
        let mut providers = build_providers(&self.settings, [provider])?;
        start_providers(&mut providers).await?;
        self.providers.extend(providers);
        Ok(())
    }

    pub fn metrics_since(&self, since: DateTime<Utc>) -> crate::state::RecoveryMetrics {
        self.state.metrics_since(since)
    }

    pub async fn list_sessions(&mut self, provider: &str, limit: usize) -> Result<Vec<Session>> {
        self.providers
            .get_mut(provider)
            .ok_or_else(|| anyhow!("provider is unavailable: {provider}"))?
            .list_sessions(limit)
            .await
    }

    pub async fn search_sessions(
        &mut self,
        provider: &str,
        query: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<SessionSearchPage> {
        self.providers
            .get_mut(provider)
            .ok_or_else(|| anyhow!("provider is unavailable: {provider}"))?
            .search_sessions(query, cursor, limit)
            .await
    }

    pub fn pending_provider_names(&self) -> HashSet<String> {
        self.state
            .pending_recoveries()
            .into_iter()
            .map(|pending| pending.provider)
            .collect()
    }

    pub async fn reconcile_pending_only(&mut self, now: DateTime<Utc>) -> Result<Vec<EngineEvent>> {
        self.state.prune(now);
        let mut events = Vec::new();
        self.reconcile_pending_recoveries(now, &mut events).await;
        if let Err(error) = self.state.save() {
            warn!(%error, "failed to persist recovery outcome state");
        }
        if let Err(error) = self.event_log.append(&events) {
            warn!(%error, "failed to persist recovery outcome events");
        }
        Ok(events)
    }

    pub async fn validate_session(&mut self, provider: &str, session_id: &str) -> Result<()> {
        self.providers
            .get_mut(provider)
            .ok_or_else(|| anyhow!("provider is unavailable: {provider}"))?
            .validate_session(session_id)
            .await
    }

    pub async fn session_logs(
        &mut self,
        provider: &str,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<SessionLog>> {
        self.providers
            .get_mut(provider)
            .ok_or_else(|| anyhow!("provider is unavailable: {provider}"))?
            .session_logs(session_id, limit)
            .await
    }

    pub async fn send_message(
        &mut self,
        provider: &str,
        session_id: &str,
        message: &str,
    ) -> Result<MessageReceipt> {
        self.providers
            .get_mut(provider)
            .ok_or_else(|| anyhow!("provider is unavailable: {provider}"))?
            .send_message(session_id, message)
            .await
    }

    pub async fn interrupt(
        &mut self,
        provider: &str,
        session_id: &str,
    ) -> Result<InterruptReceipt> {
        self.providers
            .get_mut(provider)
            .ok_or_else(|| anyhow!("provider is unavailable: {provider}"))?
            .interrupt(session_id)
            .await
    }

    pub async fn retry_now(
        &mut self,
        provider_name: &str,
        session_id: &str,
        now: DateTime<Utc>,
        automatic: bool,
        generation: u64,
    ) -> Result<ResumeReceipt> {
        let target = self
            .targets
            .iter()
            .find(|target| target.provider == provider_name && target.session_id == session_id)
            .cloned()
            .with_context(|| {
                format!("session {provider_name}:{session_id} is not being guarded")
            })?;
        if !self.recovery_permit.allows(&target, generation) {
            bail!("guard state changed before the manual retry could start");
        }
        let failure = self
            .providers
            .get_mut(provider_name)
            .ok_or_else(|| anyhow!("provider is unavailable: {provider_name}"))?
            .latest_failure(session_id)
            .await?
            .with_context(|| format!("session {session_id} has no failed latest turn"))?;
        self.unresolved_targets
            .insert(format!("{provider_name}:{session_id}"));
        let policy = self.settings.policy(&failure.condition);
        if policy.action != PolicyAction::Retry {
            return Err(anyhow!(
                "{} is configured to skip; change the policy before retrying",
                failure.condition
            ));
        }
        let attempts =
            self.state
                .recent_attempts(&target, now, self.settings.engine.attempt_window_seconds);
        if attempts >= policy.max_attempts {
            bail!(
                "retry limit reached for {provider_name}:{session_id} ({attempts}/{})",
                policy.max_attempts
            );
        }
        let prompt = render_prompt(&policy.prompt, &failure, attempts + 1, policy.max_attempts);
        let _send_boundary = self.recovery_permit.enter_send_boundary().await;
        if !self.recovery_permit.allows(&target, generation) {
            bail!("guard state changed before the manual retry was sent");
        }
        self.state.record_attempt(&target, now);
        self.state.save()?;
        let resume_result = self
            .providers
            .get_mut(provider_name)
            .ok_or_else(|| anyhow!("provider is unavailable: {provider_name}"))?
            .resume(session_id, &prompt, &failure.key())
            .await;
        let receipt = match resume_result {
            Ok(receipt) => receipt,
            Err(error) if acknowledgement_is_unknown(&error) => {
                self.state.mark_handled(&failure, "unconfirmed", now);
                if let Err(save_error) = self.state.save() {
                    warn!(%save_error, "cannot persist unconfirmed manual recovery state");
                }
                if let Err(log_error) = self.event_log.append(&[EngineEvent {
                    timestamp: now,
                    kind: "retry.unconfirmed".into(),
                    target: target.key(),
                    message: "recovery may have started, but the provider acknowledgement was lost"
                        .into(),
                    condition: Some(policy.condition),
                    attempt: Some(attempts + 1),
                    max_attempts: Some(policy.max_attempts),
                    prompt: Some(prompt),
                    failure: Some(failure),
                    receipt: None,
                }]) {
                    warn!(%log_error, "cannot persist unconfirmed manual recovery event");
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        self.state
            .begin_recovery(&failure, receipt.turn_id.clone(), now, automatic);
        self.state.mark_handled(&failure, "resumed", now);
        if let Err(error) = self.state.save() {
            warn!(
                %error,
                turn = receipt.turn_id,
                "manual recovery was sent but its runtime state could not be persisted"
            );
        }
        if let Err(error) = self.event_log.append(&[EngineEvent {
            timestamp: now,
            kind: "retry.sent".into(),
            target: target.key(),
            message: format!(
                "started turn {} after {}",
                receipt.turn_id, failure.condition
            ),
            condition: Some(policy.condition),
            attempt: Some(attempts + 1),
            max_attempts: Some(policy.max_attempts),
            prompt: Some(prompt),
            failure: Some(failure),
            receipt: Some(receipt.clone()),
        }]) {
            warn!(
                %error,
                turn = receipt.turn_id,
                "manual recovery was sent but its diagnostic event could not be persisted"
            );
        }
        Ok(receipt)
    }

    pub fn unresolved_target_keys(&self) -> HashSet<String> {
        let mut targets = self.unresolved_targets.clone();
        targets.extend(self.state.pending_target_keys());
        targets
    }

    async fn wait_for_change(&mut self) {
        let mut sessions = HashMap::<String, Vec<String>>::new();
        for target in &self.targets {
            sessions
                .entry(target.provider.clone())
                .or_default()
                .push(target.session_id.clone());
        }
        let timeout = StdDuration::from_secs(self.settings.engine.poll_interval_seconds);
        if sessions.is_empty() {
            tokio::time::sleep(timeout).await;
            return;
        }
        for (name, provider) in &mut self.providers {
            let Some(session_ids) = sessions.get(name) else {
                continue;
            };
            if let Err(error) = provider.wait_for_change(session_ids, timeout).await {
                warn!(%error, "provider change watcher failed; reconciliation will continue");
            }
        }
    }

    fn replace_targets(&mut self, targets: Vec<WatchTarget>) {
        self.targets = targets
            .into_iter()
            .filter(|target| target.enabled)
            .collect();
        let active = self
            .targets
            .iter()
            .map(WatchTarget::key)
            .collect::<HashSet<_>>();
        self.unresolved_targets.retain(|key| active.contains(key));
    }
}

fn retry_delay_seconds(
    policy: &crate::config::ResolvedPolicy,
    attempts: usize,
    retry_after: Option<u64>,
) -> u64 {
    let configured = match policy.backoff.unwrap_or(BackoffKind::Exponential) {
        BackoffKind::Fixed => policy.initial_delay_seconds,
        BackoffKind::Exponential => policy
            .initial_delay_seconds
            .saturating_mul(2_u64.saturating_pow(u32::try_from(attempts).unwrap_or(u32::MAX)))
            .min(policy.max_delay_seconds),
    };
    configured.max(retry_after.unwrap_or(0))
}

fn render_prompt(template: &str, failure: &Failure, attempt: usize, max_attempts: usize) -> String {
    template
        .replace("{provider}", &failure.provider)
        .replace("{model}", failure.model.as_deref().unwrap_or("unknown"))
        .replace("{condition}", &failure.condition)
        .replace("{provider_code}", &failure.provider_code)
        .replace("{attempt}", &attempt.to_string())
        .replace("{max_attempts}", &max_attempts.to_string())
}

fn event(
    timestamp: DateTime<Utc>,
    kind: &str,
    target: &WatchTarget,
    message: String,
    failure: Option<Failure>,
) -> EngineEvent {
    let condition = failure.as_ref().map(|failure| failure.condition.clone());
    EngineEvent {
        timestamp,
        kind: kind.into(),
        target: target.key(),
        message,
        condition,
        attempt: None,
        max_attempts: None,
        prompt: None,
        failure,
        receipt: None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use async_trait::async_trait;
    use tempfile::tempdir;

    use super::*;
    use crate::models::{MessageTransport, ResumeReceipt, Session, SessionLog, TurnOutcome};

    struct FakeProvider {
        failures: VecDeque<Option<Failure>>,
        outcomes: VecDeque<TurnOutcome>,
        resumes: Arc<Mutex<Vec<String>>>,
        resume_ack_unknown: bool,
    }

    #[async_trait]
    impl Provider for FakeProvider {
        fn name(&self) -> &'static str {
            "fake"
        }
        async fn start(&mut self) -> Result<()> {
            Ok(())
        }
        async fn close(&mut self) -> Result<()> {
            Ok(())
        }
        async fn list_sessions(&mut self, _limit: usize) -> Result<Vec<Session>> {
            Ok(Vec::new())
        }
        async fn search_sessions(
            &mut self,
            _query: &str,
            _cursor: Option<&str>,
            _limit: usize,
        ) -> Result<SessionSearchPage> {
            Ok(SessionSearchPage {
                sessions: Vec::new(),
                next_cursor: None,
            })
        }
        async fn session_logs(
            &mut self,
            _session_id: &str,
            _limit: usize,
        ) -> Result<Vec<SessionLog>> {
            Ok(Vec::new())
        }
        async fn latest_failure(&mut self, _session_id: &str) -> Result<Option<Failure>> {
            Ok(self.failures.pop_front().unwrap_or(None))
        }
        async fn turn_outcome(&mut self, _session_id: &str, _turn_id: &str) -> Result<TurnOutcome> {
            Ok(self.outcomes.pop_front().unwrap_or(TurnOutcome::InProgress))
        }
        async fn resume(
            &mut self,
            session_id: &str,
            prompt: &str,
            _idempotency_key: &str,
        ) -> Result<ResumeReceipt> {
            self.resumes.lock().unwrap().push(prompt.into());
            if self.resume_ack_unknown {
                return Err(crate::transport::AcknowledgementUnknown("test timeout".into()).into());
            }
            Ok(ResumeReceipt {
                provider: "fake".into(),
                session_id: session_id.into(),
                turn_id: "continued".into(),
                transport: MessageTransport::AppServer,
            })
        }
    }

    fn failure(condition: &str) -> Failure {
        Failure {
            provider: "fake".into(),
            session_id: "session".into(),
            turn_id: "turn".into(),
            condition: condition.into(),
            provider_code: "test".into(),
            message: "failed".into(),
            occurred_at: None,
            retry_after_seconds: None,
            model: Some("model".into()),
            scope: None,
            metadata: serde_json::Value::Null,
        }
    }

    fn target() -> WatchTarget {
        WatchTarget {
            provider: "fake".into(),
            session_id: "session".into(),
            enabled: true,
            protected: false,
            label: None,
            added_at: Utc::now(),
            last_event_at: None,
        }
    }

    fn engine(
        failures: Vec<Option<Failure>>,
        dry_run: bool,
    ) -> (WatchEngine, Arc<Mutex<Vec<String>>>) {
        engine_with_outcomes(failures, Vec::new(), dry_run)
    }

    #[tokio::test]
    async fn recovery_send_boundary_drains_before_revocation_is_acknowledged() {
        let permit = RecoveryPermit::new(true, &[target()], 1);
        let active_send = permit.enter_send_boundary().await;
        let waiting = tokio::spawn({
            let permit = permit.clone();
            async move {
                let boundary = permit.enter_send_boundary().await;
                permit.update(false, &[], 2);
                drop(boundary);
            }
        });

        tokio::time::sleep(StdDuration::from_millis(20)).await;
        assert!(!waiting.is_finished());
        drop(active_send);
        tokio::time::timeout(StdDuration::from_secs(1), waiting)
            .await
            .expect("revocation boundary should drain")
            .expect("boundary waiter should not panic");
        assert!(!permit.allows(&target(), 1));
    }

    #[tokio::test]
    async fn manual_retry_requires_current_guard_authorization() {
        let (mut engine, resumes) = engine(vec![Some(failure("network.timeout"))], false);
        let generation = engine.recovery_permit.generation();
        engine.recovery_permit.update(false, &[], generation + 1);

        let error = engine
            .retry_now("fake", "session", Utc::now(), false, generation)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("guard state changed"));
        assert!(resumes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn direct_reload_authorizes_a_newly_watched_session() {
        let current = failure("network.timeout");
        let (mut engine, resumes) = engine(
            vec![Some(current.clone()), Some(current.clone()), Some(current)],
            false,
        );
        engine.replace_watch_targets(Vec::new());
        engine.recovery_permit.update(true, &[], 0);
        engine
            .apply_direct_reload(Some(vec![target()]), None, true)
            .await;

        let now = Utc::now();
        assert_eq!(engine.run_once(now).await.unwrap()[0].kind, "retry.waiting");
        assert_eq!(
            engine.run_once(now + Duration::seconds(10)).await.unwrap()[0].kind,
            "retry.sent"
        );
        assert_eq!(resumes.lock().unwrap().len(), 1);
    }

    fn engine_with_outcomes(
        failures: Vec<Option<Failure>>,
        outcomes: Vec<TurnOutcome>,
        dry_run: bool,
    ) -> (WatchEngine, Arc<Mutex<Vec<String>>>) {
        let directory = tempdir().unwrap().keep();
        let state = RuntimeState::load(directory.join("state.json")).unwrap();
        let log = EventLogStore::new(directory.join("events.jsonl"), 100);
        let resumes = Arc::new(Mutex::new(Vec::new()));
        let provider = FakeProvider {
            failures: failures.into(),
            outcomes: outcomes.into(),
            resumes: Arc::clone(&resumes),
            resume_ack_unknown: false,
        };
        let mut providers: HashMap<String, Box<dyn Provider>> = HashMap::new();
        providers.insert("fake".into(), Box::new(provider));
        (
            WatchEngine::new(
                Settings::default(),
                providers,
                vec![target()],
                state,
                log,
                dry_run,
            ),
            resumes,
        )
    }

    #[tokio::test]
    async fn retries_with_exponential_policy_and_rendered_prompt() {
        let current = failure("capacity.model_overloaded");
        let (mut engine, resumes) = engine(
            vec![Some(current.clone()), Some(current.clone()), Some(current)],
            false,
        );
        let now = Utc::now();
        assert_eq!(engine.run_once(now).await.unwrap()[0].kind, "retry.waiting");
        assert_eq!(
            engine.run_once(now + Duration::seconds(15)).await.unwrap()[0].kind,
            "retry.sent"
        );
        assert_eq!(resumes.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unknown_provider_ack_is_audited_and_not_retried() {
        let directory = tempdir().unwrap().keep();
        let current = failure("network.timeout");
        let resumes = Arc::new(Mutex::new(Vec::new()));
        let provider = FakeProvider {
            failures: vec![
                Some(current.clone()),
                Some(current.clone()),
                Some(current.clone()),
                Some(current.clone()),
                Some(current.clone()),
            ]
            .into(),
            outcomes: VecDeque::new(),
            resumes: Arc::clone(&resumes),
            resume_ack_unknown: true,
        };
        let mut providers: HashMap<String, Box<dyn Provider>> = HashMap::new();
        providers.insert("fake".into(), Box::new(provider));
        let mut engine = WatchEngine::new(
            Settings::default(),
            providers,
            vec![target()],
            RuntimeState::load(directory.join("state.json")).unwrap(),
            EventLogStore::new(directory.join("events.jsonl"), 100),
            false,
        );
        let now = Utc::now();

        assert_eq!(engine.run_once(now).await.unwrap()[0].kind, "retry.waiting");
        assert_eq!(
            engine.run_once(now + Duration::seconds(10)).await.unwrap()[0].kind,
            "retry.unconfirmed"
        );
        assert!(
            engine
                .run_once(now + Duration::seconds(30))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(resumes.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn capability_failures_are_skipped_once() {
        let current = failure("capability.model_unavailable");
        let (mut engine, resumes) = engine(vec![Some(current.clone()), Some(current)], false);
        let now = Utc::now();
        assert_eq!(
            engine.run_once(now).await.unwrap()[0].kind,
            "failure.skipped"
        );
        assert!(engine.run_once(now).await.unwrap().is_empty());
        assert!(resumes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn changing_a_skip_policy_to_retry_reconsiders_the_failure() {
        let current = failure("capability.model_unavailable");
        let (mut engine, resumes) = engine(vec![Some(current.clone()), Some(current)], false);
        let now = Utc::now();
        assert_eq!(
            engine.run_once(now).await.unwrap()[0].kind,
            "failure.skipped"
        );
        engine.settings.policies.insert(
            "capability.model_unavailable".into(),
            crate::config::PolicyOverride {
                action: Some(PolicyAction::Retry),
                ..Default::default()
            },
        );
        assert_eq!(engine.run_once(now).await.unwrap()[0].kind, "retry.waiting");
        assert!(resumes.lock().unwrap().is_empty());
    }

    #[test]
    fn later_attempts_wait_from_the_previous_attempt() {
        let policy = Settings::default().policy("network.connection_failed");
        assert_eq!(retry_delay_seconds(&policy, 0, None), 5);
        assert_eq!(retry_delay_seconds(&policy, 1, None), 10);
        assert_eq!(retry_delay_seconds(&policy, 9, None), 120);
        assert_eq!(retry_delay_seconds(&policy, 0, Some(45)), 45);
    }

    #[tokio::test]
    async fn dry_run_logs_prompt_without_sending() {
        let current = failure("network.timeout");
        let (mut engine, resumes) = engine(vec![Some(current.clone()), Some(current)], true);
        let now = Utc::now();
        let _ = engine.run_once(now).await.unwrap();
        let event = &engine.run_once(now + Duration::seconds(10)).await.unwrap()[0];
        assert_eq!(event.kind, "retry.dry_run");
        assert!(event.prompt.is_some());
        assert!(resumes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_in_progress_recovery_keeps_the_session_attempt_budget() {
        let current = failure("network.timeout");
        let (mut engine, _) = engine(
            vec![
                Some(current.clone()),
                Some(current.clone()),
                Some(current),
                None,
            ],
            false,
        );
        let now = Utc::now();
        let _ = engine.run_once(now).await.unwrap();
        let _ = engine.run_once(now + Duration::seconds(10)).await.unwrap();
        assert_eq!(engine.state.recent_attempts(&target(), now, 3_600), 1);
        assert!(
            engine
                .run_once(now + Duration::seconds(11))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(engine.state.recent_attempts(&target(), now, 3_600), 1);
        assert_eq!(
            engine
                .metrics_since(now - Duration::seconds(1))
                .automatic_recoveries,
            0
        );
    }

    #[tokio::test]
    async fn only_a_completed_recovery_counts_as_success() {
        let current = failure("network.timeout");
        let (mut engine, _) = engine_with_outcomes(
            vec![
                Some(current.clone()),
                Some(current.clone()),
                Some(current),
                None,
            ],
            vec![TurnOutcome::Completed],
            false,
        );
        let now = Utc::now();
        let _ = engine.run_once(now).await.unwrap();
        let _ = engine.run_once(now + Duration::seconds(10)).await.unwrap();
        let events = engine.run_once(now + Duration::seconds(11)).await.unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "recovery.completed");
        assert_eq!(engine.state.recent_attempts(&target(), now, 3_600), 0);
        let metrics = engine.metrics_since(now - Duration::seconds(1));
        assert_eq!(metrics.automatic_recoveries, 1);
        assert_eq!(metrics.manual_recoveries, 0);
        assert_eq!(metrics.hands_free_percent, 100);
    }

    #[tokio::test]
    async fn pending_recovery_is_reconciled_after_its_watch_target_is_removed() {
        let directory = tempdir().unwrap().keep();
        let state_path = directory.join("state.json");
        let current = failure("network.timeout");
        let now = Utc::now();
        let mut state = RuntimeState::load(state_path.clone()).unwrap();
        state.begin_recovery(&current, "continued".into(), now, true);
        state.save().unwrap();

        let provider = FakeProvider {
            failures: VecDeque::new(),
            outcomes: vec![TurnOutcome::Completed].into(),
            resumes: Arc::new(Mutex::new(Vec::new())),
            resume_ack_unknown: false,
        };
        let mut providers: HashMap<String, Box<dyn Provider>> = HashMap::new();
        providers.insert("fake".into(), Box::new(provider));
        let mut engine = WatchEngine::new(
            Settings::default(),
            providers,
            Vec::new(),
            RuntimeState::load(state_path).unwrap(),
            EventLogStore::new(directory.join("events.jsonl"), 100),
            false,
        );

        assert!(engine.pending_provider_names().contains("fake"));
        let events = engine
            .reconcile_pending_only(now + Duration::seconds(1))
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "recovery.completed");
        assert_eq!(engine.metrics_since(now).automatic_recoveries, 1);
        assert!(engine.pending_provider_names().is_empty());
    }

    #[tokio::test]
    async fn orphaned_unknown_recovery_is_abandoned_after_bounded_observation() {
        let directory = tempdir().unwrap();
        let now = Utc::now();
        let mut state = RuntimeState::load(directory.path().join("state.json")).unwrap();
        let current = failure("network.timeout");
        state.begin_recovery(
            &current,
            "missing-turn".into(),
            now - ORPHAN_PENDING_RECOVERY_MAX_AGE,
            true,
        );
        for _ in 0..PENDING_RECOVERY_MAX_OBSERVATION_FAILURES - 1 {
            state.record_recovery_observation_failure(&current.key(), now);
        }
        state.save().unwrap();
        let provider = FakeProvider {
            failures: VecDeque::new(),
            outcomes: vec![TurnOutcome::Unknown].into(),
            resumes: Arc::new(Mutex::new(Vec::new())),
            resume_ack_unknown: false,
        };
        let mut providers: HashMap<String, Box<dyn Provider>> = HashMap::new();
        providers.insert("fake".into(), Box::new(provider));
        let mut engine = WatchEngine::new(
            Settings::default(),
            providers,
            Vec::new(),
            state,
            EventLogStore::new(directory.path().join("events.jsonl"), 100),
            false,
        );

        let events = engine.reconcile_pending_only(now).await.unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "recovery.unconfirmed");
        assert!(engine.pending_provider_names().is_empty());
    }
}
