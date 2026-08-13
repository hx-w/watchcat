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
        "version": 1,
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
fn stale_socket_falls_back_to_direct_mode_when_no_daemon_owns_state() {
    let isolated = Isolated::new();
    let state = isolated.directory.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("watchcat.sock"), b"stale").unwrap();

    isolated
        .command()
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Watchlist is empty"));
}

#[cfg(unix)]
#[test]
fn direct_mode_honors_a_persisted_disabled_guard() {
    let isolated = Isolated::new();
    isolated
        .command()
        .args(["config", "init"])
        .assert()
        .success();
    isolated
        .command()
        .args(["watch", "add", "session-1", "--no-validate"])
        .assert()
        .success();
    let state = isolated.directory.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("control.json"),
        r#"{"version":1,"guard_enabled":false,"guard_paused_until":null,"revision":8}"#,
    )
    .unwrap();

    isolated
        .command()
        .args(["run", "--once"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("guard is disabled"));
}

#[cfg(unix)]
#[test]
fn daemon_removes_its_socket_on_sigterm() {
    let isolated = Isolated::new();
    isolated
        .command()
        .args(["config", "init"])
        .assert()
        .success();
    let config_dir = isolated.directory.path().join("config");
    let state_dir = isolated.directory.path().join("state");
    let socket = state_dir.join("watchcat.sock");
    let mut daemon = std::process::Command::new(env!("CARGO_BIN_EXE_watchcatd"))
        .env("WATCHCAT_CONFIG_DIR", &config_dir)
        .env("WATCHCAT_STATE_DIR", &state_dir)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() && Instant::now() < deadline {
        if let Some(status) = daemon.try_wait().unwrap() {
            panic!("daemon exited before creating its socket: {status}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(socket.exists(), "daemon never created its socket");

    assert_eq!(unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) }, 0);
    let status = daemon.wait().unwrap();
    assert!(status.success(), "daemon did not stop cleanly: {status}");
    assert!(!socket.exists(), "daemon left a stale socket after SIGTERM");
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
    let state_dir = isolated.directory.path().join("state");
    let socket = state_dir.join("watchcat.sock");
    let mut daemon = std::process::Command::new(env!("CARGO_BIN_EXE_watchcatd"))
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
        .arg("status")
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
            "version = 3\n[engine]\npoll_interval_seconds = 1\nattempt_window_seconds = 3600\nlog_retention = 100\n[lifecycle]\nstale_after_seconds = 259200\nsweep_interval_seconds = 60\nprotect_unresolved_failures = true\n[providers.codex]\nenabled = true\ncommand = [{}]\n",
            toml::Value::String(provider.to_string_lossy().into_owned())
        ),
    )
    .unwrap();
    std::fs::write(
        config_dir.join("watchlist.json"),
        format!(
            r#"{{"version":3,"targets":[{{"provider":"codex","session_id":"slow","enabled":true,"protected":false,"label":"slow","added_at":"{}","last_event_at":null}}]}}"#,
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
    let output = isolated.command().arg("status").output().unwrap();
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
            "version = 3\n[engine]\npoll_interval_seconds = 1\nattempt_window_seconds = 3600\nlog_retention = 100\n[lifecycle]\nstale_after_seconds = 259200\nsweep_interval_seconds = 60\nprotect_unresolved_failures = true\n[providers.codex]\nenabled = true\ncommand = [{}]\n",
            toml::Value::String(provider.to_string_lossy().into_owned())
        ),
    )
    .unwrap();
    std::fs::write(
        config_dir.join("watchlist.json"),
        format!(
            r#"{{"version":3,"targets":[{{"provider":"codex","session_id":"blocked","enabled":true,"protected":false,"label":"blocked","added_at":"{}","last_event_at":null}}]}}"#,
            chrono::Utc::now().to_rfc3339()
        ),
    )
    .unwrap();

    let socket = state_dir.join("watchcat.sock");
    let mut daemon = std::process::Command::new(env!("CARGO_BIN_EXE_watchcatd"))
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
    for command in [
        "status", "run", "session", "watch", "policy", "config", "doctor",
    ] {
        assert!(help.contains(command), "missing {command} from help");
    }
    for removed in [
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

#[test]
fn watchlist_commands_are_grouped_and_idempotent() {
    let isolated = Isolated::new();
    isolated
        .command()
        .args(["watch", "add", "session-1", "--no-validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Watching codex:session-1"));
    isolated
        .command()
        .args(["watch", "add", "session-1", "--no-validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Already watching"));
    isolated
        .command()
        .args(["watch", "list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("session-1"));
    isolated
        .command()
        .args(["watch", "remove", "session-1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Removed"));
    isolated
        .command()
        .args(["watch", "remove", "session-1"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("Not watched"));
}

#[test]
fn policy_commands_discover_edit_and_reset_conditions() {
    let isolated = Isolated::new();
    isolated
        .command()
        .args(["policy", "list", "--category", "capacity"])
        .assert()
        .success()
        .stdout(predicate::str::contains("capacity.model_overloaded"))
        .stdout(predicate::str::contains("capability.model_unavailable").not());
    isolated
        .command()
        .args([
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
        .args(["policy", "show", "capacity.model_overloaded", "--json"])
        .output()
        .unwrap();
    let policy: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(policy["max_attempts"], 8);
    assert_eq!(policy["max_delay_seconds"], 300);
    assert_eq!(policy["customized"], true);
    isolated
        .command()
        .args(["policy", "reset", "capacity.model_overloaded"])
        .assert()
        .success();
    isolated
        .command()
        .args(["policy", "show", "capacity.model_overloaded", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"customized\": false"));
    isolated
        .command()
        .args(["policy", "set", "network.timeout", "--action", "skip"])
        .assert()
        .success();
    isolated
        .command()
        .args(["policy", "set", "network.timeout", "--action", "retry"])
        .assert()
        .success();
    isolated
        .command()
        .args(["policy", "show", "network.timeout", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"max_attempts\": 5"));
}

#[test]
fn rejects_unknown_conditions_and_empty_policy_updates() {
    let isolated = Isolated::new();
    isolated
        .command()
        .args(["policy", "set", "made.up", "--action", "retry"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unknown policy condition"));
    isolated
        .command()
        .args(["policy", "set", "network.timeout"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("requires at least one option"));
    isolated
        .command()
        .args([
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
        .stderr(predicate::str::contains("cannot be combined"));
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
        .stderr(predicate::str::contains("supports version 3"));
}

#[test]
fn v2_configuration_is_migrated_to_v3() {
    let isolated = Isolated::new();
    let config = isolated.directory.path().join("v2.toml");
    std::fs::write(
        &config,
        "version = 2\n[engine]\npoll_interval_seconds = 10\nattempt_window_seconds = 3600\nlog_retention = 10000\n[providers.codex]\nenabled = true\ncommand = [\"codex\", \"app-server\", \"--listen\", \"stdio://\"]\n",
    )
    .unwrap();
    cargo_bin_cmd!("watchcat")
        .args(["--config", config.to_str().unwrap(), "config", "validate"])
        .assert()
        .success();
    let migrated = std::fs::read_to_string(config).unwrap();
    assert!(migrated.contains("version = 3"));
    assert!(migrated.contains("[lifecycle]"));
}

#[test]
fn session_logs_include_provider_errors_as_structured_entries() {
    let isolated = Isolated::new();
    let fake = isolated.directory.path().join("fake-codex.sh");
    std::fs::write(&fake, "#!/bin/sh\nexit 1\n").unwrap();
    // Provider read failures stay visible without hiding Watchcat's local log.
    isolated
        .command()
        .args(["session", "logs", "missing", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("provider.error"));
}
