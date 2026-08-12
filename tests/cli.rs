use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use serde_json::Value;
use std::path::Path;
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

#[test]
fn reports_version() {
    cargo_bin_cmd!("watchcat")
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("watchcat "));
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
fn old_configuration_is_rejected_without_migration() {
    let isolated = Isolated::new();
    let config = isolated.directory.path().join("old.toml");
    std::fs::write(&config, "version = 1\n").unwrap();
    cargo_bin_cmd!("watchcat")
        .args(["--config", config.to_str().unwrap(), "config", "validate"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("supports version 2"));
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
