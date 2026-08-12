use std::collections::HashMap;
use std::time::Duration as StdDuration;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Duration, Utc};
use tracing::{info, warn};

use crate::config::Settings;
use crate::models::{BackoffKind, EngineEvent, Failure, PolicyAction, WatchTarget};
use crate::providers::Provider;
use crate::state::{EventLogStore, RuntimeState};

pub struct WatchEngine {
    settings: Settings,
    providers: HashMap<String, Box<dyn Provider>>,
    targets: Vec<WatchTarget>,
    state: RuntimeState,
    event_log: EventLogStore,
    dry_run: bool,
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
        Self {
            settings,
            providers,
            targets: targets
                .into_iter()
                .filter(|target| target.enabled)
                .collect(),
            state,
            event_log,
            dry_run,
        }
    }

    pub async fn run_once(&mut self, now: DateTime<Utc>) -> Result<Vec<EngineEvent>> {
        let mut events = Vec::new();
        let mut persisted_events = 0;
        for target in self.targets.clone() {
            let result = match self.providers.get_mut(&target.provider) {
                Some(provider) => provider.latest_failure(&target.session_id).await,
                None => Err(anyhow!("provider is unavailable: {}", target.provider)),
            };
            let failure = match result {
                Ok(Some(failure)) => failure,
                Ok(None) => {
                    self.state.clear_attempts(&target);
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
            if matches!(self.state.handled_action(&failure), Some("resumed"))
                || (policy.action == PolicyAction::Skip
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
            let latest = provider.latest_failure(&target.session_id).await?;
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

            self.state.record_attempt(&target, now);
            self.state.save()?;
            match provider.resume(&target.session_id, &prompt).await {
                Ok(receipt) => {
                    self.state.mark_handled(&failure, "resumed", now);
                    self.state.save()?;
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
        self.state.save()?;
        if let Err(error) = self.event_log.append(&events[persisted_events..]) {
            warn!(%error, "failed to persist diagnostic events");
        }
        Ok(events)
    }

    pub async fn run_forever_with<F>(&mut self, mut reload: F) -> Result<()>
    where
        F: FnMut() -> Result<(Option<Vec<WatchTarget>>, Option<Settings>)>,
    {
        loop {
            let (targets, settings) = reload()?;
            if let Some(targets) = targets {
                self.replace_targets(targets);
            }
            if let Some(settings) = settings {
                self.settings = settings;
            }
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

    pub async fn close(&mut self) {
        for provider in self.providers.values_mut() {
            if let Err(error) = provider.close().await {
                warn!(%error, provider = provider.name(), "provider shutdown failed");
            }
        }
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
    use crate::models::{MessageTransport, ResumeReceipt, Session, SessionLog};

    struct FakeProvider {
        failures: VecDeque<Option<Failure>>,
        resumes: Arc<Mutex<Vec<String>>>,
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
        async fn resume(&mut self, session_id: &str, prompt: &str) -> Result<ResumeReceipt> {
            self.resumes.lock().unwrap().push(prompt.into());
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
            label: None,
            added_at: Utc::now(),
        }
    }

    fn engine(
        failures: Vec<Option<Failure>>,
        dry_run: bool,
    ) -> (WatchEngine, Arc<Mutex<Vec<String>>>) {
        let directory = tempdir().unwrap().keep();
        let state = RuntimeState::load(directory.join("state.json")).unwrap();
        let log = EventLogStore::new(directory.join("events.jsonl"), 100);
        let resumes = Arc::new(Mutex::new(Vec::new()));
        let provider = FakeProvider {
            failures: failures.into(),
            resumes: Arc::clone(&resumes),
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
    async fn a_healthy_latest_turn_resets_the_session_attempt_budget() {
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
        assert_eq!(engine.state.recent_attempts(&target(), now, 3_600), 0);
    }
}
