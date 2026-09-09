use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use serde_json::Value;
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
#[cfg(unix)]
use std::time::{Duration, Instant};
use tempfile::{TempDir, tempdir};

struct Isolated {
    directory: TempDir,
}

impl Isolated {
    fn new() -> Self {
        Self {
            directory: tempdir().expect("temporary directory"),
        }
    }

    fn command(&self) -> assert_cmd::Command {
        let mut command = cargo_bin_cmd!("watchcat");
        command
            .env("WATCHCAT_CONFIG_DIR", self.directory.path().join("config"))
            .env("WATCHCAT_STATE_DIR", self.directory.path().join("state"));
        command
    }
}

#[cfg(unix)]
struct Daemon(std::process::Child);

#[cfg(unix)]
impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.0.id() as i32, libc::SIGTERM);
        }
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
impl Isolated {
    fn start_daemon(&self) -> Daemon {
        let config_dir = self.directory.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config = config_dir.join("config.toml");
        if !config.exists() {
            std::fs::write(config, "version = 4\n[providers.claude]\nenabled = false\n[providers.codex]\nenabled = false\n").unwrap();
        }
        let state_dir = self.directory.path().join("state");
        let mut daemon = Daemon(
            std::process::Command::new(env!("CARGO_BIN_EXE_watchcatd"))
                .env("CLAUDE_CONFIG_DIR", self.directory.path().join("claude"))
                .env("WATCHCAT_CONFIG_DIR", config_dir)
                .env("WATCHCAT_STATE_DIR", &state_dir)
                .spawn()
                .unwrap(),
        );
        wait_for_daemon(&state_dir.join("watchcat.sock"), &mut daemon.0);
        daemon
    }
}

#[cfg(unix)]
fn wait_for_daemon(socket: &Path, daemon: &mut std::process::Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(status) = daemon.try_wait().unwrap() {
            panic!("daemon exited before becoming ready: {status}");
        }
        if let Ok((stream, response)) = rpc_request(socket, "ready", "service.ping") {
            drop(stream);
            if response["error"].is_null() {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("daemon did not become ready at {}", socket.display());
}

#[cfg(unix)]
fn rpc_request(
    socket: &Path,
    id: &str,
    method: &str,
) -> std::io::Result<(std::os::unix::net::UnixStream, Value)> {
    rpc_request_with(socket, id, method, serde_json::json!({}), None)
}

#[cfg(unix)]
fn rpc_request_with(
    socket: &Path,
    id: &str,
    method: &str,
    params: Value,
    expected_revision: Option<u64>,
) -> std::io::Result<(std::os::unix::net::UnixStream, Value)> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let request = serde_json::to_vec(&serde_json::json!({
        "version": 2,
        "id": id,
        "method": method,
        "params": params,
        "expected_revision": expected_revision,
    }))
    .unwrap();
    stream.write_all(&(request.len() as u32).to_be_bytes())?;
    stream.write_all(&request)?;
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length)?;
    let mut response = vec![0; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut response)?;
    Ok((stream, serde_json::from_slice(&response).unwrap()))
}

#[cfg(unix)]
fn subscribe(socket: &Path, id: &str) -> (std::os::unix::net::UnixStream, Value) {
    rpc_request(socket, id, "events.subscribe").unwrap()
}

#[test]
fn reports_version() {
    cargo_bin_cmd!("watchcat")
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("watchcat "));
}

#[cfg(unix)]
#[test]
fn runtime_commands_require_the_server_even_with_a_stale_socket() {
    let isolated = Isolated::new();
    let state = isolated.directory.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("watchcat.sock"), b"stale").unwrap();
    for args in [
        vec!["session", "list"],
        vec!["session", "add", "session-1", "--no-validate"],
        vec!["config", "policy", "list"],
    ] {
        isolated
            .command()
            .args(args)
            .assert()
            .code(2)
            .stderr(predicate::str::contains(
                "cannot connect to Watchcat service",
            ));
    }
    assert!(
        !isolated
            .directory
            .path()
            .join("config/watchlist.json")
            .exists()
    );
    assert!(!state.join("control.json").exists());
}

#[cfg(unix)]
#[test]
fn membership_and_manual_exclusion_survive_restart() {
    let isolated = Isolated::new();
    let daemon = isolated.start_daemon();
    let socket = isolated.directory.path().join("state/watchcat.sock");
    isolated
        .command()
        .args(["session", "add", "session-1", "--no-validate"])
        .assert()
        .success();
    isolated
        .command()
        .args(["session", "remove", "session-1"])
        .assert()
        .success();
    drop(daemon);
    assert!(!socket.exists(), "daemon left a stale socket after SIGTERM");
    let _daemon = isolated.start_daemon();
    isolated
        .command()
        .args(["session", "list", "--json"])
        .assert()
        .success()
        .stdout("[]\n");
    let state: Value = serde_json::from_slice(
        &std::fs::read(isolated.directory.path().join("config/watchlist.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["excluded"], serde_json::json!(["codex:session-1"]));
}

#[cfg(unix)]
#[test]
fn event_subscriptions_are_bounded_and_disconnections_release_capacity() {
    let isolated = Isolated::new();
    isolated
        .command()
        .args(["config", "init"])
        .assert()
        .success();
    let config_dir = isolated.directory.path().join("config");
    std::fs::write(
        config_dir.join("config.toml"),
        "version = 4\n[providers.codex]\nenabled = false\n[providers.claude]\nenabled = false\n",
    )
    .unwrap();
    let state_dir = isolated.directory.path().join("state");
    let socket = state_dir.join("watchcat.sock");
    let mut daemon = std::process::Command::new(env!("CARGO_BIN_EXE_watchcatd"))
        .env(
            "CLAUDE_CONFIG_DIR",
            isolated.directory.path().join("claude"),
        )
        .env("WATCHCAT_CONFIG_DIR", &config_dir)
        .env("WATCHCAT_STATE_DIR", &state_dir)
        .spawn()
        .unwrap();
    wait_for_daemon(&socket, &mut daemon);

    let mut subscriptions = Vec::new();
    for index in 0..4 {
        let (stream, response) = subscribe(&socket, &format!("subscription-{index}"));
        assert_eq!(response["result"]["subscribed"], true);
        subscriptions.push(stream);
    }
    let (_, rejected) = subscribe(&socket, "subscription-over-limit");
    assert_eq!(rejected["error"]["code"], "too_many_subscribers");

    isolated
        .command()
        .args(["service", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("service online"));

    drop(subscriptions.pop());
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let (stream, response) = subscribe(&socket, "subscription-after-close");
        if response["result"]["subscribed"] == true {
            subscriptions.push(stream);
            break;
        }
        assert_eq!(response["error"]["code"], "too_many_subscribers");
        assert!(
            Instant::now() < deadline,
            "closed event subscription did not release capacity"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    drop(subscriptions);
    assert_eq!(unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) }, 0);
    assert!(daemon.wait().unwrap().success());
}

#[cfg(unix)]
#[test]
fn slow_provider_does_not_block_the_daemon_control_plane() {
    let isolated = Isolated::new();
    let config_dir = isolated.directory.path().join("config");
    let state_dir = isolated.directory.path().join("state");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    let provider = isolated.directory.path().join("slow-provider.sh");
    let marker = isolated.directory.path().join("provider-blocked");
    std::fs::write(
        &provider,
        r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"id":%s,"result":{}}\n' "$id" ;;
    *'"method":"thread/turns/list"'*)
      : > "$WATCHCAT_TEST_SLOW_MARKER"
      sleep 10
      printf '{"id":%s,"result":{"data":[]}}\n' "$id"
      ;;
    *'"method":"thread/list"'*) printf '{"id":%s,"result":{"data":[]}}\n' "$id" ;;
  esac
done
"#,
    )
    .unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "version = 4\n[providers.claude]\nenabled = false\n[engine]\npoll_interval_seconds = 1\nattempt_window_seconds = 3600\nlog_retention = 100\n[lifecycle]\nstale_after_seconds = 259200\n[providers.codex]\nenabled = true\ncommand = [{}]\n",
            toml::Value::String(provider.to_string_lossy().into_owned())
        ),
    )
    .unwrap();
    std::fs::write(
        config_dir.join("watchlist.json"),
        format!(
            r#"{{"version":4,"excluded":[],"targets":[{{"source":"manual","provider":"codex","session_id":"slow","label":"slow","added_at":"{}","last_activity_at":null}}]}}"#,
            chrono::Utc::now().to_rfc3339()
        ),
    )
    .unwrap();

    let mut daemon_command = std::process::Command::new(env!("CARGO_BIN_EXE_watchcatd"));
    daemon_command
        .env("WATCHCAT_CONFIG_DIR", &config_dir)
        .env("WATCHCAT_STATE_DIR", &state_dir)
        .env("WATCHCAT_TEST_SLOW_MARKER", &marker);
    let mut daemon = daemon_command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker.exists() && Instant::now() < deadline {
        if let Some(status) = daemon.try_wait().unwrap() {
            panic!("daemon exited before entering the slow provider request: {status}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if !marker.exists() {
        let _ = daemon.kill();
        let status = daemon.wait().unwrap();
        panic!("provider never entered the slow request; daemon status: {status}");
    }

    let started = Instant::now();
    let output = isolated
        .command()
        .args(["service", "status"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("service online"),
        "unexpected status path: {stdout}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "control plane waited for the provider: {:?}",
        started.elapsed()
    );

    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[cfg(unix)]
#[test]
fn sigterm_cancels_accepted_retry_before_it_can_send() {
    let isolated = Isolated::new();
    let config_dir = isolated.directory.path().join("config");
    let state_dir = isolated.directory.path().join("state");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    let provider = isolated.directory.path().join("blocked-provider.sh");
    let blocked = isolated.directory.path().join("provider-blocked");
    let sent = isolated.directory.path().join("recovery-sent");
    std::fs::write(
        &provider,
        r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"id":%s,"result":{}}\n' "$id" ;;
    *'"method":"thread/turns/list"'*)
      : > "$WATCHCAT_TEST_BLOCKED_MARKER"
      sleep 10
      printf '{"id":%s,"result":{"data":[]}}\n' "$id"
      ;;
    *'"method":"thread/resume"'*|*'"method":"turn/start"'*)
      : > "$WATCHCAT_TEST_SENT_MARKER"
      printf '{"id":%s,"result":{}}\n' "$id"
      ;;
    *'"method":"thread/list"'*) printf '{"id":%s,"result":{"data":[]}}\n' "$id" ;;
  esac
done
"#,
    )
    .unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "version = 4\n[providers.claude]\nenabled = false\n[engine]\npoll_interval_seconds = 1\nattempt_window_seconds = 3600\nlog_retention = 100\n[lifecycle]\nstale_after_seconds = 259200\n[providers.codex]\nenabled = true\ncommand = [{}]\n",
            toml::Value::String(provider.to_string_lossy().into_owned())
        ),
    )
    .unwrap();
    std::fs::write(
        config_dir.join("watchlist.json"),
        format!(
            r#"{{"version":4,"excluded":[],"targets":[{{"source":"manual","provider":"codex","session_id":"blocked","label":"blocked","added_at":"{}","last_activity_at":null}}]}}"#,
            chrono::Utc::now().to_rfc3339()
        ),
    )
    .unwrap();

    let socket = state_dir.join("watchcat.sock");
    let mut daemon = std::process::Command::new(env!("CARGO_BIN_EXE_watchcatd"))
        .env(
            "CLAUDE_CONFIG_DIR",
            isolated.directory.path().join("claude"),
        )
        .env("WATCHCAT_CONFIG_DIR", &config_dir)
        .env("WATCHCAT_STATE_DIR", &state_dir)
        .env("WATCHCAT_TEST_BLOCKED_MARKER", &blocked)
        .env("WATCHCAT_TEST_SENT_MARKER", &sent)
        .spawn()
        .unwrap();
    wait_for_daemon(&socket, &mut daemon);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !blocked.exists() && Instant::now() < deadline {
        if let Some(status) = daemon.try_wait().unwrap() {
            panic!("daemon exited before provider blocked: {status}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        blocked.exists(),
        "provider did not enter its blocking request"
    );

    let (_, ping) = rpc_request(&socket, "revision", "service.ping").unwrap();
    let revision = ping["revision"].as_u64().unwrap();
    let (_, accepted) = rpc_request_with(
        &socket,
        "manual-retry",
        "sessions.retry_now",
        serde_json::json!({
            "provider": "codex",
            "session_id": "blocked",
            "request_key": "sigterm-retry",
        }),
        Some(revision),
    )
    .unwrap();
    assert_eq!(accepted["result"]["status"], "accepted");

    let stopped_at = Instant::now();
    assert_eq!(unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) }, 0);
    let status = daemon.wait().unwrap();
    assert!(status.success(), "daemon did not stop cleanly: {status}");
    assert!(
        stopped_at.elapsed() < Duration::from_secs(4),
        "daemon waited for the queued retry: {:?}",
        stopped_at.elapsed()
    );
    assert!(!sent.exists(), "recovery was sent after SIGTERM");
}

#[test]
fn exposes_only_the_new_top_level_command_shape() {
    let output = cargo_bin_cmd!("watchcat").arg("--help").output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for command in ["session", "config", "service"] {
        assert!(help.contains(command), "missing {command} from help");
    }
    for removed in [
        "\n  guard ",
        "\n  watch ",
        "\n  policy ",
        "\n  status ",
        "\n  doctor ",
        "\n  run ",
        "\n  add ",
        "\n  remove ",
        "\n  list ",
        "\n  paths ",
        "\n  codes ",
        "\n  capabilities ",
    ] {
        assert!(
            !help.contains(removed),
            "unexpected removed command {removed}"
        );
    }
}

#[test]
fn session_send_is_grouped_and_rejects_empty_stdin() {
    cargo_bin_cmd!("watchcat")
        .args(["session", "send", "session-1"])
        .write_stdin("  \n")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("message cannot be empty"));

    cargo_bin_cmd!("watchcat")
        .args(["session", "send", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("standard input"))
        .stdout(predicate::str::contains("--provider"))
        .stdout(predicate::str::contains("--json"));
}

#[test]
fn session_interrupt_is_grouped_with_provider_neutral_options() {
    cargo_bin_cmd!("watchcat")
        .args(["session", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("interrupt"));
    cargo_bin_cmd!("watchcat")
        .args(["session", "interrupt", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--provider"))
        .stdout(predicate::str::contains("--json"));
}

#[test]
fn initializes_valid_configuration_and_reports_native_paths() {
    let isolated = Isolated::new();
    isolated
        .command()
        .args(["config", "init"])
        .assert()
        .success();
    isolated
        .command()
        .args(["config", "validate", "--json"])
        .assert()
        .success()
        .stdout("{\"ok\":true}\n");
    let output = isolated
        .command()
        .args(["config", "path", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let paths: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(Path::new(paths["config"].as_str().unwrap()).ends_with("config/config.toml"));
    assert!(Path::new(paths["events"].as_str().unwrap()).ends_with("state/events.jsonl"));
}

#[cfg(unix)]
#[test]
fn watchlist_commands_are_grouped_and_idempotent() {
    let isolated = Isolated::new();
    let _daemon = isolated.start_daemon();
    isolated
        .command()
        .args(["session", "add", "session-1", "--no-validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Watching"));
    isolated
        .command()
        .args(["session", "add", "session-1", "--no-validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Already watching"));
    isolated
        .command()
        .args(["session", "list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("session-1"));
    isolated
        .command()
        .args(["session", "remove", "session-1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Removed"));
    isolated
        .command()
        .args(["session", "remove", "session-1"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Excluded from automatic discovery",
        ));
}

#[cfg(unix)]
#[test]
fn policy_commands_discover_edit_and_reset_conditions() {
    let isolated = Isolated::new();
    let _daemon = isolated.start_daemon();
    isolated
        .command()
        .args(["config", "policy", "list", "--category", "capacity"])
        .assert()
        .success()
        .stdout(predicate::str::contains("capacity.model_overloaded"))
        .stdout(predicate::str::contains("capability.model_unavailable").not());
    isolated
        .command()
        .args([
            "config",
            "policy",
            "set",
            "capacity.model_overloaded",
            "--action",
            "retry",
            "--backoff",
            "exponential",
            "--initial-delay",
            "15s",
            "--max-delay",
            "5m",
            "--max-attempts",
            "8",
            "--prompt",
            "Continue {model}, attempt {attempt}/{max_attempts}",
        ])
        .assert()
        .success();
    let output = isolated
        .command()
        .args([
            "config",
            "policy",
            "show",
            "capacity.model_overloaded",
            "--json",
        ])
        .output()
        .unwrap();
    let policy: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(policy["max_attempts"], 8);
    assert_eq!(policy["max_delay_seconds"], 300);
    assert_eq!(policy["customized"], true);
    isolated
        .command()
        .args(["config", "policy", "reset", "capacity.model_overloaded"])
        .assert()
        .success();
    isolated
        .command()
        .args([
            "config",
            "policy",
            "show",
            "capacity.model_overloaded",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"customized\": false"));
    isolated
        .command()
        .args([
            "config",
            "policy",
            "set",
            "network.timeout",
            "--action",
            "skip",
        ])
        .assert()
        .success();
    isolated
        .command()
        .args([
            "config",
            "policy",
            "set",
            "network.timeout",
            "--action",
            "retry",
        ])
        .assert()
        .success();
    isolated
        .command()
        .args(["config", "policy", "show", "network.timeout", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"max_attempts\": 5"));
}

#[cfg(unix)]
#[test]
fn rejects_unknown_conditions_and_empty_policy_updates() {
    let isolated = Isolated::new();
    let _daemon = isolated.start_daemon();
    isolated
        .command()
        .args(["config", "policy", "set", "made.up", "--action", "retry"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unknown policy condition"));
    isolated
        .command()
        .args(["config", "policy", "set", "network.timeout"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("requires at least one option"));
    isolated
        .command()
        .args([
            "config",
            "policy",
            "set",
            "network.timeout",
            "--action",
            "skip",
            "--max-attempts",
            "2",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "cannot set retry fields when action is skip",
        ));
}

#[test]
fn unsupported_v1_configuration_is_rejected() {
    let isolated = Isolated::new();
    let config = isolated.directory.path().join("old.toml");
    std::fs::write(&config, "version = 1\n").unwrap();
    cargo_bin_cmd!("watchcat")
        .args(["--config", config.to_str().unwrap(), "config", "validate"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("supports version 4"));
}

#[test]
fn obsolete_or_unversioned_configuration_is_rejected_without_rewriting() {
    let isolated = Isolated::new();
    let config = isolated.directory.path().join("config.toml");
    for data in [
        "version = 2\n",
        "version = 3\n",
        "version = 5\n",
        "[engine]\npoll_interval_seconds = 10\n",
    ] {
        std::fs::write(&config, data).unwrap();
        isolated
            .command()
            .arg("--config")
            .arg(&config)
            .args(["config", "validate"])
            .assert()
            .code(2)
            .stderr(predicate::str::contains("supports version 4"));
        assert_eq!(std::fs::read_to_string(&config).unwrap(), data);
    }
}

#[cfg(target_os = "macos")]
#[test]
fn service_preview_is_a_valid_standalone_launchagent_and_does_not_write_state() {
    let isolated = Isolated::new();
    let config = isolated.directory.path().join("config & <custom>.toml");
    let watchlist = isolated.directory.path().join("watch & <list>.json");
    let output = isolated
        .command()
        .env("WATCHCAT_WATCHLIST", &watchlist)
        .arg("--config")
        .arg(&config)
        .args(["service", "install", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plist = isolated.directory.path().join("service.plist");
    std::fs::write(&plist, output.stdout).unwrap();
    let parsed = std::process::Command::new("plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(plist)
        .output()
        .unwrap();
    assert!(parsed.status.success());
    let definition: Value = serde_json::from_slice(&parsed.stdout).unwrap();
    assert_eq!(
        definition["ProgramArguments"],
        serde_json::json!([env!("CARGO_BIN_EXE_watchcatd"), "--config", config])
    );
    assert_eq!(
        definition["EnvironmentVariables"]["WATCHCAT_WATCHLIST"],
        watchlist.to_str().unwrap()
    );
    assert_eq!(
        definition["EnvironmentVariables"]["WATCHCAT_STATE_DIR"],
        isolated.directory.path().join("state").to_str().unwrap()
    );
    assert!(!config.exists());
    assert!(!isolated.directory.path().join("state").exists());
}

#[cfg(unix)]
#[test]
fn discovers_new_claude_sessions_and_honors_manual_exclusion() {
    let isolated = Isolated::new();
    let config_dir = isolated.directory.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "version = 4\n[engine]\npoll_interval_seconds = 1\n[providers.codex]\nenabled = false\n",
    )
    .unwrap();
    let daemon = isolated.start_daemon();
    let socket = isolated.directory.path().join("state/watchcat.sock");
    let project = isolated.directory.path().join("claude/projects/example");
    std::fs::create_dir_all(&project).unwrap();
    let id = "c61e6439-6107-4a88-970f-94d548564049";
    let path = project.join(format!("{id}.jsonl"));
    std::fs::write(
        &path,
        format!(
            "{}\n",
            serde_json::json!({
                "type": "user", "sessionId": id, "timestamp": chrono::Utc::now().to_rfc3339(),
                "message": {"role": "user", "content": "Inspect the build"}
            })
        ),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (_, response) = rpc_request(&socket, "list", "sessions.list").unwrap();
        if response["result"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["session_id"] == id)
        {
            break;
        }
        assert!(Instant::now() < deadline, "new session was not discovered");
        std::thread::sleep(Duration::from_millis(50));
    }
    isolated
        .command()
        .args(["session", "remove", id, "--provider", "claude"])
        .assert()
        .success();
    drop(daemon);
    let _daemon = isolated.start_daemon();
    // A new provider event cannot undo a persisted manual exclusion.
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{}", serde_json::json!({"type":"assistant","timestamp":chrono::Utc::now().to_rfc3339(),"message":{"content":"Build passed"}})).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    isolated
        .command()
        .args(["session", "list", "--json"])
        .assert()
        .success()
        .stdout("[]\n");
    isolated
        .command()
        .args(["session", "add", id, "--provider", "claude"])
        .assert()
        .success();
    isolated
        .command()
        .args(["session", "logs", id, "--provider", "claude", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Build passed"));
}

#[cfg(target_os = "macos")]
#[test]
fn restart_starts_an_installed_but_unloaded_launchagent() {
    let isolated = Isolated::new();
    let fake_home = isolated.directory.path().join("home");
    let agents = fake_home.join("Library/LaunchAgents");
    let bin = isolated.directory.path().join("bin");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::create_dir(&bin).unwrap();
    std::fs::write(agents.join("ai.watchcat.watchcatd.plist"), "test service").unwrap();
    let marker = isolated.directory.path().join("started");
    let launchctl = bin.join("launchctl");
    std::fs::write(
        &launchctl,
        r#"#!/bin/sh
case "$1" in
  print|bootout) exit 113 ;;
  enable) exit 0 ;;
  bootstrap) : > "$WATCHCAT_TEST_STARTED" ;;
  *) exit 2 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&launchctl, std::fs::Permissions::from_mode(0o755)).unwrap();
    isolated
        .command()
        .env("HOME", &fake_home)
        .env("PATH", &bin)
        .env("WATCHCAT_TEST_STARTED", &marker)
        .args(["service", "restart"])
        .assert()
        .success();
    assert!(
        marker.exists(),
        "restart did not bootstrap the unloaded service"
    );
}
