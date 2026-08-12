use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use serde_json::Value;
use tempfile::tempdir;

fn isolated_command() -> assert_cmd::Command {
    let directory = tempdir().expect("temporary directory").keep();
    let mut command = cargo_bin_cmd!("watchcat");
    command
        .env("WATCHCAT_CONFIG_DIR", directory.join("config"))
        .env("WATCHCAT_STATE_DIR", directory.join("state"));
    command
}

#[test]
fn reports_version() {
    isolated_command()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("watchcat 0.1.0"));
}

#[test]
fn initializes_and_reports_native_paths() {
    let directory = tempdir().expect("temporary directory");
    let config = directory.path().join("config");
    let state = directory.path().join("state");

    cargo_bin_cmd!("watchcat")
        .env("WATCHCAT_CONFIG_DIR", &config)
        .env("WATCHCAT_STATE_DIR", &state)
        .arg("init")
        .assert()
        .success();

    let output = cargo_bin_cmd!("watchcat")
        .env("WATCHCAT_CONFIG_DIR", &config)
        .env("WATCHCAT_STATE_DIR", &state)
        .args(["paths", "--json"])
        .output()
        .expect("run paths command");
    assert!(output.status.success());
    let paths: Value = serde_json::from_slice(&output.stdout).expect("parse paths JSON");
    assert_eq!(
        paths["config"].as_str().map(std::path::Path::new),
        Some(config.join("config.toml").as_path())
    );
    assert_eq!(
        paths["state"].as_str().map(std::path::Path::new),
        Some(state.join("state.json").as_path())
    );
}

#[test]
fn watchlist_add_is_idempotent_and_remove_reports_absence() {
    let directory = tempdir().expect("temporary directory");
    let config = directory.path().join("config");
    let state = directory.path().join("state");

    let command = || {
        let mut command = cargo_bin_cmd!("watchcat");
        command
            .env("WATCHCAT_CONFIG_DIR", &config)
            .env("WATCHCAT_STATE_DIR", &state);
        command
    };

    command()
        .args(["add", "session-1", "--no-validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Watching codex:session-1"));
    command()
        .args(["add", "session-1", "--no-validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Already watching"));
    command()
        .args(["remove", "session-1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Removed"));
    command()
        .args(["remove", "session-1"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("Not watched"));
}

#[test]
fn fails_closed_on_unknown_config_fields() {
    let directory = tempdir().expect("temporary directory");
    let config_file = directory.path().join("config.toml");
    std::fs::write(&config_file, "version = 1\nunknown = true\n").expect("write config");

    cargo_bin_cmd!("watchcat")
        .args(["--config", config_file.to_str().unwrap(), "doctor"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unknown field"));
}
