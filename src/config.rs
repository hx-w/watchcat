use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::conditions::{CONDITIONS, DEFAULT_PROMPT, definition, is_known};
use crate::models::{BackoffKind, PolicyAction};

pub const DEFAULT_CONFIG: &str = r#"# Watchcat configuration
version = 3

[engine]
poll_interval_seconds = 10
attempt_window_seconds = 3600
log_retention = 10000

[lifecycle]
stale_after_seconds = 259200
sweep_interval_seconds = 60
protect_unresolved_failures = true

[providers.codex]
enabled = true
command = ["codex", "app-server", "--listen", "stdio://"]

"#;

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub version: u32,
    pub engine: EngineSettings,
    pub lifecycle: LifecycleSettings,
    pub providers: ProviderSettings,
    pub policies: BTreeMap<String, PolicyOverride>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 3,
            engine: EngineSettings::default(),
            lifecycle: LifecycleSettings::default(),
            providers: ProviderSettings::default(),
            policies: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LifecycleSettings {
    pub stale_after_seconds: i64,
    pub sweep_interval_seconds: u64,
    pub protect_unresolved_failures: bool,
}

impl Default for LifecycleSettings {
    fn default() -> Self {
        Self {
            stale_after_seconds: 3 * 24 * 60 * 60,
            sweep_interval_seconds: 60,
            protect_unresolved_failures: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineSettings {
    pub poll_interval_seconds: u64,
    pub attempt_window_seconds: i64,
    pub log_retention: usize,
}

impl Default for EngineSettings {
    fn default() -> Self {
        Self {
            poll_interval_seconds: 10,
            attempt_window_seconds: 3_600,
            log_retention: 10_000,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderSettings {
    pub codex: CodexSettings,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CodexSettings {
    pub enabled: bool,
    pub command: Vec<String>,
}

impl Default for CodexSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            command: vec![
                "codex".into(),
                "app-server".into(),
                "--listen".into(),
                "stdio://".into(),
            ],
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyOverride {
    pub action: Option<PolicyAction>,
    pub backoff: Option<BackoffKind>,
    pub initial_delay_seconds: Option<u64>,
    pub max_delay_seconds: Option<u64>,
    pub max_attempts: Option<usize>,
    pub prompt: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct ResolvedPolicy {
    pub condition: String,
    pub description: String,
    pub action: PolicyAction,
    pub backoff: Option<BackoffKind>,
    pub initial_delay_seconds: u64,
    pub max_delay_seconds: u64,
    pub max_attempts: usize,
    pub prompt: String,
    pub customized: bool,
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        if self.version != 3 {
            bail!(
                "unsupported config version {}; this release supports version 3",
                self.version
            );
        }
        if self.engine.poll_interval_seconds == 0
            || self.engine.attempt_window_seconds <= 0
            || self.engine.log_retention == 0
        {
            bail!("engine settings must be positive");
        }
        if self.lifecycle.stale_after_seconds <= 0 || self.lifecycle.sweep_interval_seconds == 0 {
            bail!("lifecycle settings must be positive");
        }
        validate_command(
            "providers.codex.command",
            self.providers.codex.enabled,
            &self.providers.codex.command,
        )?;
        for (condition, policy) in &self.policies {
            if !is_known(condition) {
                bail!("unknown policy condition: {condition}");
            }
            validate_override(condition, policy)?;
        }
        for condition in CONDITIONS {
            let policy = self.policy(condition.name);
            if policy.action == PolicyAction::Retry
                && (policy.initial_delay_seconds == 0
                    || policy.max_delay_seconds < policy.initial_delay_seconds
                    || policy.max_attempts == 0)
            {
                bail!(
                    "policy {} has invalid resolved retry settings",
                    condition.name
                );
            }
        }
        Ok(())
    }

    pub fn policy(&self, condition: &str) -> ResolvedPolicy {
        let fallback = definition(condition)
            .or_else(|| definition("failure.unknown"))
            .expect("unknown condition is registered");
        let custom = self.policies.get(condition);
        let action = custom
            .and_then(|policy| policy.action)
            .unwrap_or(fallback.action);
        let backoff = match action {
            PolicyAction::Retry => custom
                .and_then(|policy| policy.backoff)
                .or(fallback.backoff)
                .or(Some(BackoffKind::Exponential)),
            PolicyAction::Skip => None,
        };
        let retry_defaults =
            definition("network.connection_failed").expect("retry default is registered");
        let retry_initial = if fallback.initial_delay_seconds > 0 {
            fallback.initial_delay_seconds
        } else {
            retry_defaults.initial_delay_seconds
        };
        let retry_maximum = if fallback.max_delay_seconds > 0 {
            fallback.max_delay_seconds
        } else {
            retry_defaults.max_delay_seconds
        };
        let retry_attempts = if fallback.max_attempts > 0 {
            fallback.max_attempts
        } else {
            retry_defaults.max_attempts
        };
        ResolvedPolicy {
            condition: condition.into(),
            description: fallback.description.into(),
            action,
            backoff,
            initial_delay_seconds: custom
                .and_then(|policy| policy.initial_delay_seconds)
                .unwrap_or(if action == PolicyAction::Retry {
                    retry_initial
                } else {
                    0
                }),
            max_delay_seconds: custom
                .and_then(|policy| policy.max_delay_seconds)
                .unwrap_or(if action == PolicyAction::Retry {
                    retry_maximum
                } else {
                    0
                }),
            max_attempts: custom.and_then(|policy| policy.max_attempts).unwrap_or(
                if action == PolicyAction::Retry {
                    retry_attempts
                } else {
                    0
                },
            ),
            prompt: if action == PolicyAction::Retry {
                custom
                    .and_then(|policy| policy.prompt.clone())
                    .unwrap_or_else(|| DEFAULT_PROMPT.into())
            } else {
                String::new()
            },
            customized: custom.is_some(),
        }
    }

    pub fn policies(&self) -> Vec<ResolvedPolicy> {
        CONDITIONS
            .iter()
            .map(|condition| self.policy(condition.name))
            .collect()
    }
}

fn validate_command(name: &str, enabled: bool, command: &[String]) -> Result<()> {
    if enabled && (command.is_empty() || command.iter().any(|part| part.is_empty())) {
        bail!("{name} must be a non-empty array of strings");
    }
    Ok(())
}

fn validate_override(condition: &str, policy: &PolicyOverride) -> Result<()> {
    if policy
        .prompt
        .as_ref()
        .is_some_and(|prompt| prompt.trim().is_empty())
    {
        bail!("policy {condition} prompt cannot be empty");
    }
    if matches!(policy.initial_delay_seconds, Some(0))
        || matches!(policy.max_delay_seconds, Some(0))
        || matches!(policy.max_attempts, Some(0))
    {
        bail!("policy {condition} retry settings must be positive");
    }
    if let (Some(initial), Some(maximum)) = (policy.initial_delay_seconds, policy.max_delay_seconds)
    {
        if maximum < initial {
            bail!("policy {condition} max_delay_seconds cannot be less than initial_delay_seconds");
        }
    }
    if policy.action == Some(PolicyAction::Skip)
        && (policy.backoff.is_some()
            || policy.initial_delay_seconds.is_some()
            || policy.max_delay_seconds.is_some()
            || policy.max_attempts.is_some())
    {
        bail!("policy {condition} cannot set retry fields when action is skip");
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub config_file: PathBuf,
    pub watchlist_file: PathBuf,
    pub state_file: PathBuf,
    pub control_state_file: PathBuf,
    pub event_log_file: PathBuf,
    pub retry_operations_file: PathBuf,
    pub lock_file: PathBuf,
    pub socket_file: PathBuf,
}

impl Paths {
    pub fn discover(config_override: Option<PathBuf>) -> Result<Self> {
        let dirs = ProjectDirs::from("ai", "watchcat", "watchcat")
            .context("cannot determine configuration directories")?;
        let config_file = match config_override {
            Some(path) => absolute_path(path)?,
            None => std::env::var_os("WATCHCAT_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| dirs.config_dir().to_path_buf())
                .join("config.toml"),
        };
        let config_dir = config_file
            .parent()
            .context("configuration path has no parent")?
            .to_path_buf();
        let state_dir = absolute_path(
            std::env::var_os("WATCHCAT_STATE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    dirs.state_dir()
                        .unwrap_or(dirs.data_local_dir())
                        .to_path_buf()
                }),
        )?;
        let watchlist_file = absolute_path(
            std::env::var_os("WATCHCAT_WATCHLIST")
                .map(PathBuf::from)
                .unwrap_or_else(|| config_dir.join("watchlist.json")),
        )?;
        Ok(Self {
            config_file,
            watchlist_file,
            state_file: state_dir.join("state.json"),
            control_state_file: state_dir.join("control.json"),
            event_log_file: state_dir.join("events.jsonl"),
            retry_operations_file: state_dir.join("retry-operations.json"),
            lock_file: state_dir.join("watchcat.lock"),
            socket_file: state_dir.join("watchcat.sock"),
        })
    }
}

fn absolute_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()
            .context("cannot determine current directory")?
            .join(path))
    }
}

pub fn load_settings(path: &Path) -> Result<Settings> {
    if !path.exists() {
        return Ok(Settings::default());
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("cannot read configuration {}", path.display()))?;
    let mut document: toml::Value = toml::from_str(&text)
        .with_context(|| format!("invalid configuration {}", path.display()))?;
    let version = document
        .get("version")
        .and_then(toml::Value::as_integer)
        .unwrap_or(2);
    if version == 2 {
        document["version"] = toml::Value::Integer(3);
    }
    let settings: Settings = document
        .try_into()
        .with_context(|| format!("invalid configuration {}", path.display()))?;
    settings.validate()?;
    if version == 2 {
        save_settings(path, &settings)?;
    }
    Ok(settings)
}

pub fn save_settings(path: &Path, settings: &Settings) -> Result<()> {
    settings.validate()?;
    let text = toml::to_string_pretty(settings)?;
    atomic_write(path, text.as_bytes())
}

pub fn display_settings(settings: &Settings) -> Result<String> {
    let mut visible = settings.clone();
    if visible.policies.is_empty() {
        return Ok(DEFAULT_CONFIG.to_owned());
    }
    let defaults: Settings = toml::from_str(DEFAULT_CONFIG)?;
    visible.engine = settings.engine.clone();
    visible.lifecycle = settings.lifecycle.clone();
    visible.providers = settings.providers.clone();
    let mut text = toml::to_string_pretty(&visible)?;
    if visible.engine == defaults.engine
        && visible.lifecycle == defaults.lifecycle
        && visible.providers.codex.enabled == defaults.providers.codex.enabled
        && visible.providers.codex.command == defaults.providers.codex.command
    {
        text.insert_str(0, "# Effective Watchcat configuration\n");
    }
    Ok(text)
}

pub fn initialize_config(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!("configuration already exists: {}", path.display());
    }
    atomic_write(path, DEFAULT_CONFIG.as_bytes())
}

pub fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("cannot create directory {}", parent.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("cannot create temporary file in {}", parent.display()))?;
    temporary.write_all(content)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_future_config() {
        let error = toml::from_str::<Settings>("version = 2\nunknown = true").unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn default_config_is_valid() {
        let settings: Settings = toml::from_str(DEFAULT_CONFIG).unwrap();
        settings.validate().unwrap();
    }

    #[test]
    fn policy_override_resolves_without_mutating_defaults() {
        let mut settings = Settings::default();
        settings.policies.insert(
            "capacity.model_overloaded".into(),
            PolicyOverride {
                max_attempts: Some(8),
                prompt: Some("Continue {model}".into()),
                ..PolicyOverride::default()
            },
        );
        let policy = settings.policy("capacity.model_overloaded");
        assert_eq!(policy.max_attempts, 8);
        assert_eq!(policy.prompt, "Continue {model}");
        assert_eq!(settings.policy("network.timeout").max_attempts, 5);
    }
}
