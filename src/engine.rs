use std::collections::HashMap;
use std::time::Duration as StdDuration;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Duration, Utc};
use tracing::{info, warn};

use crate::config::EngineSettings;
use crate::models::{EngineEvent, Failure, WatchTarget};
use crate::providers::Provider;
use crate::state::RuntimeState;

pub struct WatchEngine {
    settings: EngineSettings,
    providers: HashMap<String, Box<dyn Provider>>,
    targets: Vec<WatchTarget>,
    state: RuntimeState,
    dry_run: bool,
}

impl WatchEngine {
    pub fn new(
        settings: EngineSettings,
        providers: HashMap<String, Box<dyn Provider>>,
        targets: Vec<WatchTarget>,
        state: RuntimeState,
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
            dry_run,
        }
    }

    pub async fn run_once(&mut self, now: DateTime<Utc>) -> Result<Vec<EngineEvent>> {
        let mut events = Vec::new();
        for target in self.targets.clone() {
            let result = match self.providers.get_mut(&target.provider) {
                Some(provider) => provider.latest_failure(&target.session_id).await,
                None => Err(anyhow!("provider is unavailable: {}", target.provider)),
            };
            let failure = match result {
                Ok(Some(failure)) => failure,
                Ok(None) => continue,
                Err(error) => {
                    events.push(event("provider_error", &target, error.to_string(), None));
                    continue;
                }
            };
            if self.state.is_handled(&failure) {
                continue;
            }
            if !failure.retryable {
                self.state
                    .mark_handled(&failure, "ignored_non_retryable", now);
                events.push(event(
                    "ignored",
                    &target,
                    format!("non-retryable failure {}", failure.code),
                    Some(failure),
                ));
                continue;
            }
            let first_seen = self.state.first_seen(&failure, now);
            let attempts =
                self.state
                    .recent_attempts(&target, now, self.settings.attempt_window_seconds);
            if attempts >= self.settings.max_attempts {
                events.push(event(
                    "rate_limited",
                    &target,
                    format!(
                        "resume limit reached ({attempts}/{})",
                        self.settings.max_attempts
                    ),
                    Some(failure),
                ));
                continue;
            }
            let index = usize::min(attempts, self.settings.backoff_seconds.len() - 1);
            let ready_at = first_seen + Duration::seconds(self.settings.backoff_seconds[index]);
            if now < ready_at {
                events.push(event(
                    "waiting",
                    &target,
                    format!("retryable failure detected; resume after {ready_at}"),
                    Some(failure),
                ));
                continue;
            }
            if self.dry_run {
                events.push(event(
                    "would_resume",
                    &target,
                    format!("would resume after {}", failure.code),
                    Some(failure),
                ));
                continue;
            }

            let provider = self
                .providers
                .get_mut(&target.provider)
                .expect("provider checked above");
            let latest = provider.latest_failure(&target.session_id).await?;
            if latest.as_ref().map(|latest| &latest.turn_id) != Some(&failure.turn_id) {
                events.push(event(
                    "race_avoided",
                    &target,
                    "session changed before resume; no message sent".into(),
                    Some(failure),
                ));
                continue;
            }
            self.state.record_attempt(&target, now);
            self.state.save()?;
            match provider
                .resume(&target.session_id, &self.settings.resume_prompt)
                .await
            {
                Ok(receipt) => {
                    self.state.mark_handled(&failure, "resumed", now);
                    self.state.save()?;
                    events.push(EngineEvent {
                        kind: "resumed".into(),
                        target: target.key(),
                        message: format!("started turn {} after {}", receipt.turn_id, failure.code),
                        failure: Some(failure),
                        receipt: Some(receipt),
                    });
                }
                Err(error) => events.push(event(
                    "resume_error",
                    &target,
                    error.to_string(),
                    Some(failure),
                )),
            }
        }
        self.state.save()?;
        Ok(events)
    }

    pub async fn run_forever(&mut self) -> Result<()> {
        self.run_forever_with(|| Ok(None)).await
    }

    pub async fn run_forever_with<F>(&mut self, mut reload_targets: F) -> Result<()>
    where
        F: FnMut() -> Result<Option<Vec<WatchTarget>>>,
    {
        loop {
            if let Some(targets) = reload_targets()? {
                self.replace_targets(targets);
            }
            for event in self.run_once(Utc::now()).await? {
                if event.kind.ends_with("error") {
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
        let timeout = StdDuration::from_secs(self.settings.poll_interval_seconds);
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

fn event(
    kind: &str,
    target: &WatchTarget,
    message: String,
    failure: Option<Failure>,
) -> EngineEvent {
    EngineEvent {
        kind: kind.into(),
        target: target.key(),
        message,
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
    use crate::models::{ResumeReceipt, Session};

    struct FakeProvider {
        failures: VecDeque<Option<Failure>>,
        resumes: Arc<Mutex<Vec<String>>>,
        fail_resume: bool,
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

        async fn latest_failure(&mut self, _session_id: &str) -> Result<Option<Failure>> {
            Ok(self.failures.pop_front().unwrap_or(None))
        }

        async fn resume(&mut self, session_id: &str, prompt: &str) -> Result<ResumeReceipt> {
            self.resumes.lock().unwrap().push(prompt.into());
            if self.fail_resume {
                anyhow::bail!("simulated resume failure");
            }
            Ok(ResumeReceipt {
                provider: "fake".into(),
                session_id: session_id.into(),
                turn_id: "continued".into(),
            })
        }
    }

    fn failure(turn_id: &str, retryable: bool) -> Failure {
        Failure {
            provider: "fake".into(),
            session_id: "session".into(),
            turn_id: turn_id.into(),
            code: if retryable { "Network" } else { "Auth" }.into(),
            message: "failed".into(),
            retryable,
            occurred_at: None,
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
        engine_with_resume_behavior(failures, dry_run, false)
    }

    fn engine_with_resume_behavior(
        failures: Vec<Option<Failure>>,
        dry_run: bool,
        fail_resume: bool,
    ) -> (WatchEngine, Arc<Mutex<Vec<String>>>) {
        let directory = tempdir().unwrap().keep();
        let state = RuntimeState::load(directory.join("state.json")).unwrap();
        let resumes = Arc::new(Mutex::new(Vec::new()));
        let provider = FakeProvider {
            failures: failures.into(),
            resumes: Arc::clone(&resumes),
            fail_resume,
        };
        let mut providers: HashMap<String, Box<dyn Provider>> = HashMap::new();
        providers.insert("fake".into(), Box::new(provider));
        let settings = EngineSettings {
            backoff_seconds: vec![5],
            resume_prompt: "continue safely".into(),
            ..EngineSettings::default()
        };
        (
            WatchEngine::new(settings, providers, vec![target()], state, dry_run),
            resumes,
        )
    }

    #[tokio::test]
    async fn waits_for_backoff_before_resuming() {
        let failure = failure("turn", true);
        let (mut engine, resumes) = engine(
            vec![Some(failure.clone()), Some(failure.clone()), Some(failure)],
            false,
        );
        let now = Utc::now();
        assert_eq!(engine.run_once(now).await.unwrap()[0].kind, "waiting");
        assert_eq!(
            engine.run_once(now + Duration::seconds(5)).await.unwrap()[0].kind,
            "resumed"
        );
        assert_eq!(resumes.lock().unwrap().as_slice(), ["continue safely"]);
    }

    #[tokio::test]
    async fn dry_run_never_sends_or_handles() {
        let failure = failure("turn", true);
        let (mut engine, resumes) = engine(vec![Some(failure.clone()), Some(failure)], true);
        let now = Utc::now();
        let _ = engine.run_once(now).await.unwrap();
        assert_eq!(
            engine.run_once(now + Duration::seconds(5)).await.unwrap()[0].kind,
            "would_resume"
        );
        assert!(resumes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ignores_non_retryable_failures_once() {
        let failure = failure("turn", false);
        let (mut engine, resumes) = engine(vec![Some(failure.clone()), Some(failure)], false);
        let now = Utc::now();
        assert_eq!(engine.run_once(now).await.unwrap()[0].kind, "ignored");
        assert!(engine.run_once(now).await.unwrap().is_empty());
        assert!(resumes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn avoids_resume_when_latest_turn_changed() {
        let old = failure("old", true);
        let new = failure("new", true);
        let (mut engine, resumes) = engine(vec![Some(old.clone()), Some(old), Some(new)], false);
        let now = Utc::now();
        let _ = engine.run_once(now).await.unwrap();
        assert_eq!(
            engine.run_once(now + Duration::seconds(5)).await.unwrap()[0].kind,
            "race_avoided"
        );
        assert!(resumes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rate_limits_repeated_resume_failures() {
        let current = failure("turn", true);
        let (mut engine, resumes) = engine_with_resume_behavior(
            std::iter::repeat_n(Some(current), 8).collect(),
            false,
            true,
        );
        engine.settings.max_attempts = 3;
        engine.settings.backoff_seconds = vec![1];
        let now = Utc::now();
        assert_eq!(engine.run_once(now).await.unwrap()[0].kind, "waiting");
        assert_eq!(
            engine.run_once(now + Duration::seconds(1)).await.unwrap()[0].kind,
            "resume_error"
        );
        assert_eq!(
            engine.run_once(now + Duration::seconds(2)).await.unwrap()[0].kind,
            "resume_error"
        );
        assert_eq!(
            engine.run_once(now + Duration::seconds(3)).await.unwrap()[0].kind,
            "resume_error"
        );
        assert_eq!(
            engine.run_once(now + Duration::seconds(4)).await.unwrap()[0].kind,
            "rate_limited"
        );
        assert_eq!(resumes.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn revoked_targets_are_not_reconciled() {
        let current = failure("turn", true);
        let (mut engine, resumes) = engine(vec![Some(current)], false);
        engine.replace_targets(Vec::new());
        assert!(engine.run_once(Utc::now()).await.unwrap().is_empty());
        assert!(resumes.lock().unwrap().is_empty());
    }
}
