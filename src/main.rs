use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;
use watchcat::conditions::is_known;
use watchcat::config::{
    Paths, Settings, display_settings, initialize_config, load_settings, save_settings,
};
use watchcat::engine::WatchEngine;
use watchcat::models::{BackoffKind, PolicyAction, SessionLog, WatchTarget};
use watchcat::providers::{CodexProvider, Provider};
use watchcat::state::{EventLogStore, ProcessLock, RuntimeState, WatchlistStore};

#[derive(Debug, Parser)]
#[command(
    name = "watchcat",
    version,
    about = "Safely resume interrupted AI coding sessions"
)]
struct Cli {
    /// Use an alternate configuration file.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Increase log detail. Repeat for transport logs.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect Watchcat and watched sessions.
    Status(OutputArgs),
    /// Run the watchdog.
    Run(RunArgs),
    /// Discover sessions and inspect their logs.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Manage the explicit session watchlist.
    Watch {
        #[command(subcommand)]
        command: WatchCommand,
    },
    /// Inspect and edit recovery policies.
    Policy {
        #[command(subcommand)]
        command: PolicyCommand,
    },
    /// Initialize, inspect, and validate configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Verify configuration and provider connectivity.
    Doctor(OutputArgs),
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// List recent sessions from a provider.
    List(SessionListArgs),
    /// Show one provider session.
    Show(SessionIdArgs),
    /// Show recent provider and Watchcat events for one session.
    Logs(SessionLogsArgs),
    /// Send a message by steering an active turn or starting a new turn.
    Send(SessionSendArgs),
}

#[derive(Debug, Subcommand)]
enum WatchCommand {
    /// List explicitly watched sessions.
    List(OutputArgs),
    /// Add one session to the watchlist.
    Add {
        session_id: String,
        #[arg(long, default_value = "codex")]
        provider: String,
        #[arg(long)]
        label: Option<String>,
        /// Skip provider-side session validation.
        #[arg(long)]
        no_validate: bool,
    },
    /// Remove one session from the watchlist.
    Remove(SessionIdArgs),
}

#[derive(Debug, Subcommand)]
enum PolicyCommand {
    /// List every configurable failure condition.
    List(PolicyListArgs),
    /// Show one resolved policy.
    Show {
        condition: String,
        #[arg(long)]
        json: bool,
    },
    /// Set one or more fields on a policy.
    Set(PolicySetArgs),
    /// Restore one policy, or every policy, to built-in defaults.
    Reset {
        condition: Option<String>,
        #[arg(long, conflicts_with = "condition")]
        all: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Write a documented default configuration.
    Init {
        #[arg(long)]
        force: bool,
    },
    /// Print the effective configuration.
    Show(OutputArgs),
    /// Print native configuration and state paths.
    Path(OutputArgs),
    /// Validate the effective configuration.
    Validate(OutputArgs),
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Perform one reconciliation and exit.
    #[arg(long)]
    once: bool,
    /// Report recovery actions without sending prompts.
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct SessionListArgs {
    #[arg(long, default_value = "codex")]
    provider: String,
    #[arg(long, default_value_t = 50, value_parser = parse_positive_usize)]
    limit: usize,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct SessionIdArgs {
    session_id: String,
    #[arg(long, default_value = "codex")]
    provider: String,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct SessionLogsArgs {
    session_id: String,
    #[arg(long, default_value = "codex")]
    provider: String,
    #[arg(long, default_value_t = 20, value_parser = parse_positive_usize)]
    limit: usize,
    /// Filter by condition or event category, such as network or retry.
    #[arg(long)]
    category: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct SessionSendArgs {
    session_id: String,
    /// Message text. Omit to read a multi-line message from standard input.
    message: Option<String>,
    #[arg(long, default_value = "codex")]
    provider: String,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct PolicyListArgs {
    /// Filter by condition category, such as capacity or capability.
    #[arg(long)]
    category: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ActionArg {
    Retry,
    Skip,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum BackoffArg {
    Fixed,
    Exponential,
}

#[derive(Debug, Args)]
struct PolicySetArgs {
    condition: String,
    #[arg(long, value_enum)]
    action: Option<ActionArg>,
    #[arg(long, value_enum)]
    backoff: Option<BackoffArg>,
    #[arg(long, value_parser = parse_duration)]
    initial_delay: Option<u64>,
    #[arg(long, value_parser = parse_duration)]
    max_delay: Option<u64>,
    #[arg(long, value_parser = parse_positive_usize)]
    max_attempts: Option<usize>,
    #[arg(long)]
    prompt: Option<String>,
}

#[derive(Debug, Args)]
struct OutputArgs {
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("watchcat: {error:#}");
        std::process::exit(2);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    configure_logging(cli.verbose)?;
    let paths = Paths::discover(cli.config)?;
    match cli.command {
        Command::Config {
            command: ConfigCommand::Init { force },
        } => {
            initialize_config(&paths.config_file, force)?;
            println!("Wrote {}", paths.config_file.display());
        }
        Command::Config {
            command: ConfigCommand::Path(args),
        } => emit_value(&path_value(&paths), args.json)?,
        command => {
            let settings = load_settings(&paths.config_file)?;
            let watchlist = WatchlistStore::new(paths.watchlist_file.clone());
            dispatch(command, settings, paths, watchlist).await?;
        }
    }
    Ok(())
}

async fn dispatch(
    command: Command,
    mut settings: Settings,
    paths: Paths,
    watchlist: WatchlistStore,
) -> Result<()> {
    match command {
        Command::Status(args) => status(&settings, &watchlist, args.json).await,
        Command::Run(args) => run_watchdog(settings, paths, &watchlist, args).await,
        Command::Session { command } => {
            session_command(&settings, &paths, &watchlist, command).await
        }
        Command::Watch { command } => watch_command(&settings, &watchlist, command).await,
        Command::Policy { command } => policy_command(&mut settings, &paths, command),
        Command::Config { command } => config_command(&settings, &paths, command),
        Command::Doctor(args) => doctor(&settings, &paths, args.json).await,
    }
}

async fn session_command(
    settings: &Settings,
    paths: &Paths,
    watchlist: &WatchlistStore,
    command: SessionCommand,
) -> Result<()> {
    match command {
        SessionCommand::List(args) => list_sessions(settings, watchlist, args).await,
        SessionCommand::Show(args) => {
            let mut providers = started_provider(settings, &args.provider).await?;
            let sessions = providers
                .get_mut(&args.provider)
                .context("provider was not constructed")?
                .list_sessions(500)
                .await?;
            close_providers(&mut providers).await;
            let session = sessions
                .into_iter()
                .find(|session| session.id == args.session_id)
                .with_context(|| {
                    format!("{} session not found: {}", args.provider, args.session_id)
                })?;
            emit_serializable(&session, args.json)
        }
        SessionCommand::Logs(args) => session_logs(settings, paths, args).await,
        SessionCommand::Send(args) => send_session_message(settings, args).await,
    }
}

async fn send_session_message(settings: &Settings, args: SessionSendArgs) -> Result<()> {
    let message = message_input(args.message)?;
    let mut providers = started_provider(settings, &args.provider).await?;
    let result = async {
        let provider = providers
            .get_mut(&args.provider)
            .context("provider was not constructed")?;
        provider
            .send_message(&args.session_id, &message)
            .await
            .with_context(|| {
                format!(
                    "cannot send to {} session {}",
                    args.provider, args.session_id
                )
            })
    }
    .await;
    close_providers(&mut providers).await;
    let receipt = result?;
    if args.json {
        emit_serializable(&receipt, true)
    } else {
        let action = match receipt.delivery {
            watchcat::models::MessageDelivery::Started => "started",
            watchcat::models::MessageDelivery::Steered => "steered",
        };
        println!(
            "Sent message to {}:{}; {action} turn {}",
            receipt.provider, receipt.session_id, receipt.turn_id
        );
        Ok(())
    }
}

fn message_input(argument: Option<String>) -> Result<String> {
    let message = match argument {
        Some(message) => message,
        None if io::stdin().is_terminal() => {
            bail!("message is required as an argument or via standard input")
        }
        None => {
            let mut message = String::new();
            io::stdin()
                .read_to_string(&mut message)
                .context("cannot read message from standard input")?;
            message
        }
    };
    let message = message.trim();
    if message.is_empty() {
        bail!("message cannot be empty");
    }
    Ok(message.to_owned())
}

async fn watch_command(
    settings: &Settings,
    watchlist: &WatchlistStore,
    command: WatchCommand,
) -> Result<()> {
    match command {
        WatchCommand::List(args) => emit_watchlist(watchlist, args.json),
        WatchCommand::Add {
            session_id,
            provider,
            label,
            no_validate,
        } => {
            if !no_validate {
                let mut providers = started_provider(settings, &provider).await?;
                let exists = providers
                    .get_mut(&provider)
                    .context("provider was not constructed")?
                    .list_sessions(500)
                    .await?
                    .iter()
                    .any(|session| session.id == session_id);
                close_providers(&mut providers).await;
                if !exists {
                    bail!(
                        "{provider} session not found in the 500 most recent sessions: {session_id}; use --no-validate only when the id is known to be valid"
                    );
                }
            }
            let target = WatchTarget {
                provider,
                session_id,
                enabled: true,
                label,
                added_at: Utc::now(),
            };
            let added = watchlist.add(target.clone())?;
            println!(
                "{} {}",
                if added {
                    "Watching"
                } else {
                    "Already watching"
                },
                target.key()
            );
            Ok(())
        }
        WatchCommand::Remove(args) => {
            let removed = watchlist.remove(&args.provider, &args.session_id)?;
            println!(
                "{} {}:{}",
                if removed { "Removed" } else { "Not watched" },
                args.provider,
                args.session_id
            );
            if !removed {
                std::process::exit(1)
            }
            Ok(())
        }
    }
}

fn policy_command(settings: &mut Settings, paths: &Paths, command: PolicyCommand) -> Result<()> {
    match command {
        PolicyCommand::List(args) => {
            let policies = settings
                .policies()
                .into_iter()
                .filter(|policy| {
                    args.category
                        .as_deref()
                        .is_none_or(|category| policy.condition.split('.').next() == Some(category))
                })
                .collect::<Vec<_>>();
            if args.json {
                emit_serializable(&policies, true)
            } else {
                print_policies(&policies);
                Ok(())
            }
        }
        PolicyCommand::Show { condition, json } => {
            require_condition(&condition)?;
            let policy = settings.policy(&condition);
            if json {
                emit_serializable(&policy, true)
            } else {
                print_policy_details(&policy);
                Ok(())
            }
        }
        PolicyCommand::Set(args) => {
            require_condition(&args.condition)?;
            if args.action.is_none()
                && args.backoff.is_none()
                && args.initial_delay.is_none()
                && args.max_delay.is_none()
                && args.max_attempts.is_none()
                && args.prompt.is_none()
            {
                bail!("policy set requires at least one option");
            }
            if matches!(args.action, Some(ActionArg::Skip))
                && (args.backoff.is_some()
                    || args.initial_delay.is_some()
                    || args.max_delay.is_some()
                    || args.max_attempts.is_some())
            {
                bail!("--action skip cannot be combined with retry options");
            }
            let entry = settings.policies.entry(args.condition.clone()).or_default();
            if let Some(action) = args.action {
                entry.action = Some(match action {
                    ActionArg::Retry => PolicyAction::Retry,
                    ActionArg::Skip => {
                        entry.backoff = None;
                        entry.initial_delay_seconds = None;
                        entry.max_delay_seconds = None;
                        entry.max_attempts = None;
                        PolicyAction::Skip
                    }
                });
            }
            if let Some(backoff) = args.backoff {
                entry.backoff = Some(match backoff {
                    BackoffArg::Fixed => BackoffKind::Fixed,
                    BackoffArg::Exponential => BackoffKind::Exponential,
                });
            }
            if let Some(value) = args.initial_delay {
                entry.initial_delay_seconds = Some(value);
            }
            if let Some(value) = args.max_delay {
                entry.max_delay_seconds = Some(value);
            }
            if let Some(value) = args.max_attempts {
                entry.max_attempts = Some(value);
            }
            if let Some(value) = args.prompt {
                entry.prompt = Some(value);
            }
            save_settings(&paths.config_file, settings)?;
            println!("Updated {}", args.condition);
            Ok(())
        }
        PolicyCommand::Reset { condition, all } => {
            if all {
                settings.policies.clear();
                save_settings(&paths.config_file, settings)?;
                println!("Reset all policies");
            } else {
                let condition = condition.context("provide CONDITION or use --all")?;
                require_condition(&condition)?;
                settings.policies.remove(&condition);
                save_settings(&paths.config_file, settings)?;
                println!("Reset {condition}");
            }
            Ok(())
        }
    }
}

fn config_command(settings: &Settings, paths: &Paths, command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Show(args) => {
            if args.json {
                emit_serializable(settings, true)
            } else {
                print!("{}", display_settings(settings)?);
                Ok(())
            }
        }
        ConfigCommand::Validate(args) => {
            settings.validate()?;
            if args.json {
                println!("{{\"ok\":true}}");
            } else {
                println!("Configuration is valid.");
            }
            Ok(())
        }
        ConfigCommand::Path(args) => emit_value(&path_value(paths), args.json),
        ConfigCommand::Init { .. } => unreachable!(),
    }
}

async fn list_sessions(
    settings: &Settings,
    watchlist: &WatchlistStore,
    args: SessionListArgs,
) -> Result<()> {
    let watched = watchlist
        .list()?
        .into_iter()
        .map(|target| target.key())
        .collect::<HashSet<_>>();
    let mut providers = started_provider(settings, &args.provider).await?;
    let sessions = providers
        .get_mut(&args.provider)
        .context("provider was not constructed")?
        .list_sessions(args.limit)
        .await?;
    close_providers(&mut providers).await;
    if args.json {
        let values = sessions
            .iter()
            .map(|session| {
                json!({
                    "provider": session.provider, "id": session.id, "title": session.title,
                    "state": session.state, "updated_at": session.updated_at,
                    "watched": watched.contains(&session.key()), "metadata": session.metadata,
                })
            })
            .collect::<Vec<_>>();
        emit_serializable(&values, true)
    } else {
        let rows = sessions
            .iter()
            .map(|session| {
                vec![
                    if watched.contains(&session.key()) {
                        "*"
                    } else {
                        ""
                    }
                    .into(),
                    session.id.clone(),
                    format!("{:?}", session.state).to_ascii_lowercase(),
                    session.title.clone(),
                ]
            })
            .collect::<Vec<_>>();
        print_table(&["WATCH", "SESSION", "STATE", "TITLE"], &rows);
        Ok(())
    }
}

async fn session_logs(settings: &Settings, paths: &Paths, args: SessionLogsArgs) -> Result<()> {
    let provider_logs = match started_provider(settings, &args.provider).await {
        Ok(mut providers) => {
            let result = providers
                .get_mut(&args.provider)
                .context("provider was not constructed")?
                .session_logs(&args.session_id, args.limit)
                .await;
            close_providers(&mut providers).await;
            match result {
                Ok(logs) => logs,
                Err(error) => vec![provider_log_error(&args.provider, &args.session_id, error)],
            }
        }
        Err(error) => vec![provider_log_error(&args.provider, &args.session_id, error)],
    };
    let watchcat_logs =
        EventLogStore::new(paths.event_log_file.clone(), settings.engine.log_retention)
            .session_logs(
                &args.provider,
                &args.session_id,
                args.category.as_deref(),
                args.limit,
            )?;
    let mut logs = provider_logs
        .into_iter()
        .chain(watchcat_logs)
        .filter(|entry| {
            args.category.as_deref().is_none_or(|category| {
                entry
                    .condition
                    .as_deref()
                    .is_some_and(|condition| condition.split('.').next() == Some(category))
                    || entry.kind.split('.').next() == Some(category)
            })
        })
        .collect::<Vec<_>>();
    logs.sort_by_key(|entry| entry.timestamp);
    if logs.len() > args.limit {
        logs.drain(..logs.len() - args.limit);
    }
    if args.json {
        emit_serializable(&logs, true)
    } else {
        print_logs(&logs);
        Ok(())
    }
}

async fn status(settings: &Settings, watchlist: &WatchlistStore, json_output: bool) -> Result<()> {
    let targets = watchlist.list()?;
    if targets.is_empty() {
        if json_output {
            println!("[]");
        } else {
            println!(
                "Watchlist is empty. Use `watchcat session list` and `watchcat watch add <session-id>`."
            );
        }
        return Ok(());
    }
    let mut providers = build_providers(
        settings,
        targets.iter().map(|target| target.provider.as_str()),
    )?;
    start_providers(&mut providers).await?;
    let mut values = Vec::new();
    for target in &targets {
        let result = providers
            .get_mut(&target.provider)
            .context("provider was not constructed")?
            .latest_failure(&target.session_id)
            .await;
        match result {
            Ok(failure) => values.push(json!({"provider": target.provider, "session_id": target.session_id, "label": target.label, "enabled": target.enabled, "failure": failure})),
            Err(error) => values.push(json!({"provider": target.provider, "session_id": target.session_id, "label": target.label, "enabled": target.enabled, "error": error.to_string()})),
        }
    }
    close_providers(&mut providers).await;
    if json_output {
        emit_serializable(&values, true)
    } else {
        let rows = values
            .iter()
            .map(|value| {
                let latest = value
                    .pointer("/failure/condition")
                    .or_else(|| value.get("error"))
                    .and_then(Value::as_str)
                    .unwrap_or("ok");
                vec![
                    string_field(value, "provider"),
                    string_field(value, "session_id"),
                    latest.into(),
                    string_field(value, "label"),
                ]
            })
            .collect::<Vec<_>>();
        print_table(&["PROVIDER", "SESSION", "LATEST", "LABEL"], &rows);
        Ok(())
    }
}

async fn run_watchdog(
    settings: Settings,
    paths: Paths,
    watchlist: &WatchlistStore,
    args: RunArgs,
) -> Result<()> {
    let targets = watchlist
        .list()?
        .into_iter()
        .filter(|target| target.enabled)
        .collect::<Vec<_>>();
    if targets.is_empty() {
        bail!("watchlist is empty; add at least one session before running");
    }
    let _lock = ProcessLock::acquire(paths.lock_file)?;
    let mut providers = build_providers(
        &settings,
        targets.iter().map(|target| target.provider.as_str()),
    )?;
    start_providers(&mut providers).await?;
    let state = RuntimeState::load(paths.state_file)?;
    let event_log = EventLogStore::new(paths.event_log_file, settings.engine.log_retention);
    let config_file = paths.config_file.clone();
    let target_count = targets.len();
    let mut engine = WatchEngine::new(settings, providers, targets, state, event_log, args.dry_run);
    let result = if args.once {
        let events = engine.run_once(Utc::now()).await?;
        if args.json {
            emit_serializable(&events, true)?;
        } else if events.is_empty() {
            println!("No recovery action needed.");
        } else {
            let rows = events
                .iter()
                .map(|event| {
                    vec![
                        event.kind.clone(),
                        event.target.clone(),
                        event.message.clone(),
                    ]
                })
                .collect::<Vec<_>>();
            print_table(&["EVENT", "TARGET", "DETAIL"], &rows);
        }
        Ok(())
    } else {
        tracing::info!(
            targets = target_count,
            dry_run = args.dry_run,
            "watchcat started"
        );
        engine
            .run_forever_with(|| Ok((Some(watchlist.list()?), Some(load_settings(&config_file)?))))
            .await
    };
    engine.close().await;
    result
}

async fn doctor(settings: &Settings, paths: &Paths, json_output: bool) -> Result<()> {
    let executable = settings.providers.codex.command.first().cloned();
    let found = executable
        .as_deref()
        .and_then(find_executable)
        .map(|path| path.display().to_string());
    let mut checks = vec![
        json!({"name": "codex executable", "ok": found.is_some(), "detail": found.unwrap_or_else(|| "not found".into())}),
    ];
    if settings.providers.codex.enabled && checks[0]["ok"] == true {
        let mut providers = build_providers(settings, ["codex"])?;
        let result = async {
            start_providers(&mut providers).await?;
            let count = providers
                .get_mut("codex")
                .context("Codex provider missing")?
                .list_sessions(1)
                .await?
                .len();
            Result::<usize>::Ok(count)
        }
        .await;
        close_providers(&mut providers).await;
        match result {
            Ok(count) => checks.push(json!({"name": "Codex App Server", "ok": true, "detail": format!("connected; {count} session(s) sampled")})),
            Err(error) => checks.push(json!({"name": "Codex App Server", "ok": false, "detail": error.to_string()})),
        }
    }
    checks.push(json!({"name": "configuration", "ok": true, "detail": paths.config_file}));
    checks.push(json!({"name": "event log", "ok": true, "detail": paths.event_log_file}));
    if json_output {
        emit_serializable(&checks, true)?;
    } else {
        let rows = checks
            .iter()
            .map(|check| {
                vec![
                    string_field(check, "name"),
                    if check.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                        "ok"
                    } else {
                        "failed"
                    }
                    .into(),
                    string_field(check, "detail"),
                ]
            })
            .collect::<Vec<_>>();
        print_table(&["CHECK", "RESULT", "DETAIL"], &rows);
    }
    if checks
        .iter()
        .all(|check| check.get("ok").and_then(Value::as_bool) == Some(true))
    {
        Ok(())
    } else {
        std::process::exit(1)
    }
}

fn emit_watchlist(watchlist: &WatchlistStore, json_output: bool) -> Result<()> {
    let targets = watchlist.list()?;
    if json_output {
        emit_serializable(&targets, true)
    } else {
        let rows = targets
            .iter()
            .map(|target| {
                vec![
                    target.provider.clone(),
                    target.session_id.clone(),
                    target.label.clone().unwrap_or_default(),
                ]
            })
            .collect::<Vec<_>>();
        print_table(&["PROVIDER", "SESSION", "LABEL"], &rows);
        Ok(())
    }
}

fn print_policies(policies: &[watchcat::config::ResolvedPolicy]) {
    let rows = policies
        .iter()
        .map(|policy| {
            vec![
                policy.condition.clone(),
                format!("{:?}", policy.action).to_ascii_lowercase(),
                policy
                    .backoff
                    .map(|value| format!("{value:?}").to_ascii_lowercase())
                    .unwrap_or_else(|| "-".into()),
                if policy.action == PolicyAction::Retry {
                    policy.max_attempts.to_string()
                } else {
                    "-".into()
                },
                if policy.customized { "yes" } else { "no" }.into(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["CONDITION", "ACTION", "BACKOFF", "MAX", "CUSTOM"], &rows);
}

fn print_policy_details(policy: &watchcat::config::ResolvedPolicy) {
    let rows = vec![
        vec!["condition".into(), policy.condition.clone()],
        vec!["description".into(), policy.description.clone()],
        vec![
            "action".into(),
            format!("{:?}", policy.action).to_ascii_lowercase(),
        ],
        vec![
            "backoff".into(),
            policy
                .backoff
                .map(|value| format!("{value:?}").to_ascii_lowercase())
                .unwrap_or_else(|| "-".into()),
        ],
        vec![
            "initial delay".into(),
            format!("{}s", policy.initial_delay_seconds),
        ],
        vec!["max delay".into(), format!("{}s", policy.max_delay_seconds)],
        vec!["max attempts".into(), policy.max_attempts.to_string()],
        vec![
            "prompt".into(),
            if policy.action == PolicyAction::Retry {
                policy.prompt.clone()
            } else {
                "-".into()
            },
        ],
        vec!["customized".into(), policy.customized.to_string()],
    ];
    print_table(&["FIELD", "VALUE"], &rows);
}

fn provider_log_error(provider: &str, session_id: &str, error: anyhow::Error) -> SessionLog {
    SessionLog {
        timestamp: Some(Utc::now()),
        provider: provider.into(),
        session_id: session_id.into(),
        source: "provider".into(),
        kind: "provider.error".into(),
        role: None,
        turn_id: None,
        condition: None,
        message: error.to_string(),
        metadata: Value::Null,
    }
}

fn print_logs(logs: &[SessionLog]) {
    let rows = logs
        .iter()
        .map(|entry| {
            vec![
                entry
                    .timestamp
                    .map(|value| value.to_rfc3339())
                    .unwrap_or_else(|| "-".into()),
                entry.source.clone(),
                entry.kind.clone(),
                entry.condition.clone().unwrap_or_else(|| "-".into()),
                entry.message.clone(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["TIME", "SOURCE", "EVENT", "CONDITION", "MESSAGE"], &rows);
}

fn require_condition(condition: &str) -> Result<()> {
    if is_known(condition) {
        Ok(())
    } else {
        bail!("unknown policy condition: {condition}; run `watchcat policy list`")
    }
}

fn path_value(paths: &Paths) -> Value {
    json!({"config": paths.config_file, "watchlist": paths.watchlist_file, "state": paths.state_file, "events": paths.event_log_file, "lock": paths.lock_file})
}

fn build_providers<'a>(
    settings: &Settings,
    names: impl IntoIterator<Item = &'a str>,
) -> Result<HashMap<String, Box<dyn Provider>>> {
    let mut providers = HashMap::<String, Box<dyn Provider>>::new();
    for name in names {
        if providers.contains_key(name) {
            continue;
        }
        match name {
            "codex" if settings.providers.codex.enabled => {
                providers.insert(
                    name.into(),
                    Box::new(CodexProvider::new(&settings.providers.codex)?),
                );
            }
            "codex" => bail!("provider is disabled: codex"),
            "claude" => bail!(
                "Claude error definitions are available, but the Claude session adapter is not enabled in this release"
            ),
            _ => bail!("unknown provider: {name}"),
        }
    }
    Ok(providers)
}

async fn started_provider(
    settings: &Settings,
    provider: &str,
) -> Result<HashMap<String, Box<dyn Provider>>> {
    let mut providers = build_providers(settings, [provider])?;
    start_providers(&mut providers).await?;
    Ok(providers)
}

async fn start_providers(providers: &mut HashMap<String, Box<dyn Provider>>) -> Result<()> {
    for provider in providers.values_mut() {
        provider.start().await?;
    }
    Ok(())
}

async fn close_providers(providers: &mut HashMap<String, Box<dyn Provider>>) {
    for provider in providers.values_mut() {
        if let Err(error) = provider.close().await {
            tracing::warn!(%error, provider = provider.name(), "provider shutdown failed");
        }
    }
}

fn emit_serializable(value: &impl Serialize, json_output: bool) -> Result<()> {
    if json_output {
        println!("{}", serde_json::to_string_pretty(value)?);
    }
    Ok(())
}

fn emit_value(value: &Value, json_output: bool) -> Result<()> {
    if json_output {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else if let Some(object) = value.as_object() {
        let rows = object
            .iter()
            .map(|(name, value)| vec![name.clone(), value.as_str().unwrap_or_default().into()])
            .collect::<Vec<_>>();
        print_table(&["NAME", "PATH"], &rows);
    }
    Ok(())
}

fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths = headers
        .iter()
        .map(|header| header.len())
        .collect::<Vec<_>>();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = usize::min(72, usize::max(widths[index], cell.chars().count()));
        }
    }
    print_row(
        &headers
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>(),
        &widths,
    );
    print_row(
        &widths
            .iter()
            .map(|width| "-".repeat(*width))
            .collect::<Vec<_>>(),
        &widths,
    );
    for row in rows {
        print_row(row, &widths);
    }
}

fn print_row(row: &[String], widths: &[usize]) {
    let cells = row
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            let mut rendered = cell.chars().take(widths[index]).collect::<String>();
            if cell.chars().count() > widths[index] && widths[index] > 0 {
                rendered.pop();
                rendered.push('…');
            }
            format!("{rendered:<width$}", width = widths[index])
        })
        .collect::<Vec<_>>();
    println!("{}", cells.join("  "));
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let candidate = PathBuf::from(name);
    if candidate.components().count() > 1 && candidate.is_file() {
        return Some(candidate);
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .flat_map(|directory| {
                let base = directory.join(name);
                #[cfg(windows)]
                let candidates = vec![base.clone(), base.with_extension("exe")];
                #[cfg(not(windows))]
                let candidates = vec![base];
                candidates
            })
            .find(|path| path.is_file())
    })
}

fn configure_logging(verbose: u8) -> Result<()> {
    let fallback = match verbose {
        0 => "watchcat=warn",
        1 => "watchcat=info",
        _ => "watchcat=debug",
    };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| fallback.into()))
        .with_target(false)
        .try_init()
        .map_err(|error| anyhow::anyhow!(error))
}

fn parse_positive_usize(value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|error| format!("invalid number: {error}"))?;
    if parsed == 0 {
        return Err("must be greater than zero".into());
    }
    Ok(parsed)
}

fn parse_duration(value: &str) -> Result<u64, String> {
    let value = value.trim();
    let (number, multiplier) = if let Some(value) = value.strip_suffix("ms") {
        (value, 0)
    } else if let Some(value) = value.strip_suffix('s') {
        (value, 1)
    } else if let Some(value) = value.strip_suffix('m') {
        (value, 60)
    } else if let Some(value) = value.strip_suffix('h') {
        (value, 3600)
    } else {
        (value, 1)
    };
    let number = number
        .parse::<u64>()
        .map_err(|error| format!("invalid duration: {error}"))?;
    if multiplier == 0 || number == 0 {
        return Err("duration must be at least one second".into());
    }
    number
        .checked_mul(multiplier)
        .ok_or_else(|| "duration is too large".into())
}
