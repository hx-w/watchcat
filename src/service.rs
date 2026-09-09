//! User service registration for the CLI. The OS supervises watchcatd.

use anyhow::Result;
use clap::Subcommand;
use watchcat::config::Paths;

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// Register watchcatd to start at login (does not start it now).
    Install {
        /// Print the service definition without writing files.
        #[arg(long)]
        dry_run: bool,
    },
    /// Start the installed background service.
    Start,
    /// Stop the background service until the next login or start.
    Stop,
    /// Stop and start the installed background service.
    Restart,
    /// Show background service and recovery status.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Stop and unregister the service, preserving configuration and state.
    Uninstall,
}

pub fn run(command: ServiceCommand, paths: &Paths) -> Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return supported::run(command, paths);

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (command, paths);
        anyhow::bail!("background service management requires macOS or Linux")
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod supported {
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use anyhow::{Context, Result, bail};
    use directories::BaseDirs;
    use watchcat::config::{Paths, load_settings};

    use super::ServiceCommand;

    #[cfg(target_os = "macos")]
    const LABEL: &str = "ai.watchcat.watchcatd";
    #[cfg(target_os = "linux")]
    const LABEL: &str = "watchcatd.service";

    fn definition_path() -> Result<PathBuf> {
        let dirs = BaseDirs::new().context("cannot determine user service directory")?;
        #[cfg(target_os = "macos")]
        return Ok(dirs
            .home_dir()
            .join(format!("Library/LaunchAgents/{LABEL}.plist")));
        #[cfg(target_os = "linux")]
        return Ok(dirs.config_dir().join("systemd/user").join(LABEL));
    }

    fn checked(command: &mut Command) -> Result<()> {
        let output = command
            .output()
            .with_context(|| format!("cannot run {command:?}"))?;
        if !output.status.success() {
            bail!(
                "{command:?} failed ({}): {}{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        print!("{}", String::from_utf8_lossy(&output.stdout));
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn domain() -> String {
        format!("gui/{}", unsafe { libc::geteuid() })
    }

    #[cfg(target_os = "macos")]
    fn target() -> String {
        format!("{}/{LABEL}", domain())
    }

    fn start(path: &Path) -> Result<()> {
        if !path.is_file() {
            bail!("service is not installed; run watchcat service install first");
        }
        #[cfg(target_os = "macos")]
        {
            checked(Command::new("launchctl").args(["enable", &target()]))?;
            checked(
                Command::new("launchctl")
                    .args(["bootstrap", &domain()])
                    .arg(path),
            )
        }
        #[cfg(target_os = "linux")]
        checked(Command::new("systemctl").args(["--user", "start", LABEL]))
    }

    fn stop() -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let loaded = Command::new("launchctl")
                .args(["print", &target()])
                .output()?;
            if loaded.status.success() {
                checked(Command::new("launchctl").args(["bootout", &target()]))?;
            } else if loaded.status.code() != Some(113) {
                bail!(
                    "cannot inspect service before stopping: {}",
                    String::from_utf8_lossy(&loaded.stderr)
                );
            }
            Ok(())
        }
        #[cfg(target_os = "linux")]
        return checked(Command::new("systemctl").args(["--user", "stop", LABEL]));
    }

    pub fn run(command: ServiceCommand, paths: &Paths) -> Result<()> {
        let path = definition_path()?;
        match command {
            ServiceCommand::Install { dry_run } => {
                let executable = std::env::current_exe()?;
                let daemon = executable
                    .parent()
                    .context("CLI has no parent directory")?
                    .join("watchcatd");
                if !daemon.is_file() {
                    bail!(
                        "watchcatd must be installed alongside watchcat at {}",
                        daemon.display()
                    );
                }
                let definition = definition(&daemon, paths)?;
                if dry_run {
                    print!("{definition}");
                    return Ok(());
                }
                if path.exists() {
                    bail!(
                        "{} already exists; uninstall the service before replacing its registration",
                        path.display()
                    );
                }
                load_settings(&paths.config_file)?;
                let parent = path.parent().context("service definition has no parent")?;
                std::fs::create_dir_all(parent)?;
                // The daemon secures this directory too; logs must be private from creation.
                let state_dir = paths
                    .state_file
                    .parent()
                    .context("state file has no parent")?;
                std::fs::create_dir_all(state_dir)?;
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700))?;
                let mut file = tempfile::NamedTempFile::new_in(parent)?;
                file.write_all(definition.as_bytes())?;
                file.persist_noclobber(&path)?;
                #[cfg(target_os = "linux")]
                {
                    checked(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
                    checked(Command::new("systemctl").args(["--user", "enable", LABEL]))?;
                }
                println!("Installed {}\nRun watchcat service start", path.display());
                Ok(())
            }
            ServiceCommand::Start => start(&path),
            ServiceCommand::Stop => stop(),
            ServiceCommand::Restart => {
                stop()?;
                start(&path)
            }
            ServiceCommand::Status { .. } => unreachable!(),
            ServiceCommand::Uninstall => {
                if !path.is_file() {
                    bail!("service is not installed at {}", path.display());
                }
                #[cfg(target_os = "macos")]
                stop()?;
                #[cfg(target_os = "linux")]
                checked(Command::new("systemctl").args(["--user", "disable", "--now", LABEL]))?;
                std::fs::remove_file(&path)?;
                #[cfg(target_os = "linux")]
                checked(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
                println!(
                    "Removed {} (configuration and state preserved)",
                    path.display()
                );
                Ok(())
            }
        }
    }

    fn text(path: &Path) -> Result<&str> {
        path.to_str().context("service paths must be valid UTF-8")
    }

    fn definition(daemon: &Path, paths: &Paths) -> Result<String> {
        let config = std::path::absolute(&paths.config_file)?;
        let state = paths
            .state_file
            .parent()
            .context("state file has no parent")?;
        let search_path =
            std::env::var("PATH").context("PATH must be set for provider discovery")?;
        let mut environment = vec![
            ("PATH", search_path.as_str()),
            ("WATCHCAT_STATE_DIR", text(state)?),
            ("WATCHCAT_WATCHLIST", text(&paths.watchlist_file)?),
        ];
        let claude_config = std::env::var("CLAUDE_CONFIG_DIR").ok();
        let codex_home = std::env::var("CODEX_HOME").ok();
        if let Some(value) = claude_config.as_deref() {
            environment.push(("CLAUDE_CONFIG_DIR", value));
        }
        if let Some(value) = codex_home.as_deref() {
            environment.push(("CODEX_HOME", value));
        }
        let arguments = [text(daemon)?, "--config", text(&config)?];
        #[cfg(target_os = "macos")]
        {
            fn xml(value: &str) -> Result<String> {
                if value.chars().any(|c| c.is_control()) {
                    bail!("service paths and environment cannot contain control characters");
                }
                Ok(value
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
                    .replace('"', "&quot;")
                    .replace('\'', "&apos;"))
            }
            let mut body = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{LABEL}</string>\n<key>ProgramArguments</key><array>\n"
            );
            for argument in arguments {
                body.push_str(&format!("<string>{}</string>\n", xml(argument)?));
            }
            body.push_str("</array>\n<key>EnvironmentVariables</key><dict>\n");
            for (key, value) in environment {
                body.push_str(&format!(
                    "<key>{key}</key><string>{}</string>\n",
                    xml(value)?
                ));
            }
            body.push_str(&format!("</dict>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><true/>\n<key>ThrottleInterval</key><integer>10</integer>\n<key>StandardOutPath</key><string>{}</string>\n<key>StandardErrorPath</key><string>{}</string>\n</dict></plist>\n", xml(text(&state.join("watchcatd.log"))?)?, xml(text(&state.join("watchcatd.err.log"))?)?));
            Ok(body)
        }
        #[cfg(target_os = "linux")]
        {
            fn quote(value: &str, command: bool) -> Result<String> {
                if value.chars().any(|c| c.is_control()) {
                    bail!("service paths and environment cannot contain control characters");
                }
                let mut value = value
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('%', "%%");
                if command {
                    value = value.replace('$', "$$");
                }
                Ok(format!("\"{value}\""))
            }
            let command = arguments
                .iter()
                .map(|value| quote(value, true))
                .collect::<Result<Vec<_>>>()?
                .join(" ");
            let mut body = format!(
                "[Unit]\nDescription=Watchcat local reliability service\nAfter=network-online.target\n\n[Service]\nExecStart={command}\nRestart=on-failure\nRestartSec=10\nUMask=0077\n"
            );
            for (key, value) in environment {
                body.push_str(&format!(
                    "Environment={}\n",
                    quote(&format!("{key}={value}"), false)?
                ));
            }
            body.push_str("\n[Install]\nWantedBy=default.target\n");
            Ok(body)
        }
    }
}
