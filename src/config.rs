use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

pub const DEFAULT_RESUME_PROMPT: &str = "Continue the previous unfinished task. First inspect \
the last checkpoint and any changes already persisted to disk. Do not redo completed work.";

pub const DEFAULT_CONFIG: &str = r#"# Watchcat configuration
version = 1

[engine]
poll_interval_seconds = 10
max_attempts = 3
attempt_window_seconds = 3600
backoff_seconds = [5, 30, 120]
resume_prompt = "Continue the previous unfinished task. First inspect the last checkpoint and any changes already persisted to disk. Do not redo completed work."

[providers.codex]
enabled = true
command = ["codex", "app-server", "--listen", "stdio://"]
"#;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub version: u32,
    pub engine: EngineSettings,
    pub providers: ProviderSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            engine: EngineSettings::default(),
            providers: ProviderSettings::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineSettings {
    pub poll_interval_seconds: u64,
    pub max_attempts: usize,
    pub attempt_window_seconds: i64,
    pub backoff_seconds: Vec<i64>,
    pub resume_prompt: String,
}

impl Default for EngineSettings {
    fn default() -> Self {
        Self {
            poll_interval_seconds: 10,
            max_attempts: 3,
            attempt_window_seconds: 3_600,
            backoff_seconds: vec![5, 30, 120],
            resume_prompt: DEFAULT_RESUME_PROMPT.to_owned(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderSettings {
    pub codex: CodexSettings,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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

impl Settings {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!(
                "unsupported config version {}; this release supports version 1",
                self.version
            );
        }
        if self.engine.poll_interval_seconds == 0
            || self.engine.max_attempts == 0
            || self.engine.attempt_window_seconds <= 0
            || self.engine.backoff_seconds.is_empty()
            || self.engine.backoff_seconds.iter().any(|value| *value <= 0)
            || self.engine.resume_prompt.trim().is_empty()
        {
            bail!("engine settings must be positive and resume_prompt cannot be empty");
        }
        if self.providers.codex.enabled
            && (self.providers.codex.command.is_empty()
                || self
                    .providers
                    .codex
                    .command
                    .iter()
                    .any(|part| part.is_empty()))
        {
            bail!("providers.codex.command must be a non-empty array of strings");
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub config_file: PathBuf,
    pub watchlist_file: PathBuf,
    pub state_file: PathBuf,
    pub lock_file: PathBuf,
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
            lock_file: state_dir.join("watchcat.lock"),
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
    let settings: Settings = toml::from_str(&text)
        .with_context(|| format!("invalid configuration {}", path.display()))?;
    settings.validate()?;
    Ok(settings)
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
        let error = toml::from_str::<Settings>("version = 1\nunknown = true").unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn default_config_is_valid() {
        let settings: Settings = toml::from_str(DEFAULT_CONFIG).unwrap();
        settings.validate().unwrap();
    }

    #[test]
    fn relative_config_override_becomes_absolute() {
        let paths = Paths::discover(Some(PathBuf::from("local-config.toml"))).unwrap();
        assert!(paths.config_file.is_absolute());
        assert!(paths.config_file.ends_with("local-config.toml"));
    }
}
