use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;
use watchcat::client::WatchcatClient;
use watchcat::conditions::is_known;
use watchcat::config::{
    Paths, PolicyOverride, Settings, display_settings, initialize_config, load_settings,
};
use watchcat::models::{BackoffKind, PolicyAction, SessionLog, WatchTarget};
use watchcat::state::ProcessLock;

mod service;

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
    /// Manage the background server and start-at-login registration.
    Service {
        #[command(subcommand)]
        command: service::ServiceCommand,
    },
    /// Discover sessions and inspect their logs.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Initialize, inspect, and validate configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// List managed sessions across providers.
    List(SessionListArgs),
    /// Search provider sessions, including sessions outside the managed list.
    Search(SessionSearchArgs),
    /// Show one provider session.
    Show(SessionIdArgs),
    /// Show recent provider and Watchcat events for one session.
    Logs(SessionLogsArgs),
    /// Send a message by steering an active turn or starting a new turn.
    Send(SessionSendArgs),
    /// Interrupt the active turn in a provider session.
    Interrupt(SessionIdArgs),
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
    /// Inspect and edit recovery policies stored in this configuration.
    Policy {
        #[command(subcommand)]
        command: PolicyCommand,
    },
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
struct SessionListArgs {
    #[arg(long)]
    provider: Option<String>,
    #[arg(long, default_value_t = 50, value_parser = parse_positive_usize)]
    limit: usize,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct SessionSearchArgs {
    #[arg(default_value = "")]
    query: String,
    #[arg(long)]
    cursor: Option<String>,
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
        Command::Service {
            command: service::ServiceCommand::Status { json },
        } => {
            let client = WatchcatClient::new(paths.socket_file.clone());
            match client.request("snapshot.get", json!({}), None).await {
                Ok((value, _)) => {
                    if json {
                        emit_value(&value, true)?;
                    } else {
                        println!(
                            "Watchcat service online · {} sessions · {} need attention",
                            value["watched"], value["attention"]
                        );
                    }
                }
                Err(error) => {
                    if json {
                        emit_value(
                            &json!({"service_online": false, "error": error.to_string()}),
                            true,
                        )?;
                    } else {
                        println!("Watchcat service offline: {error}");
                    }
                }
            }
        }
        Command::Service { command } => service::run(command, &paths)?,
        Command::Config {
            command: ConfigCommand::Policy { command },
        } => {
            policy_command(&WatchcatClient::new(paths.socket_file.clone()), command).await?;
        }
        Command::Config {
            command: ConfigCommand::Init { force },
        } => {
            let _lock = ProcessLock::acquire(paths.lock_file.clone())?;
            initialize_config(&paths.config_file, force)?;
            println!("Wrote {}", paths.config_file.display());
        }
        Command::Config {
            command: ConfigCommand::Path(args),
        } => emit_value(&path_value(&paths), args.json)?,
        Command::Config { command } => {
            let settings = load_settings(&paths.config_file)?;
            config_command(&settings, &paths, command)?;
        }
        command => dispatch(command, &paths).await?,
    }
    Ok(())
}

async fn dispatch(command: Command, paths: &Paths) -> Result<()> {
    let client = WatchcatClient::new(paths.socket_file.clone());
    match command {
        Command::Session { command } => session_command(&client, command).await,
        Command::Config { .. } | Command::Service { .. } => unreachable!(),
    }
}

async fn session_command(client: &WatchcatClient, command: SessionCommand) -> Result<()> {
    match command {
        SessionCommand::Search(args) => {
            let (value, _) = client
                .request(
                    "sessions.search",
                    json!({"provider": args.provider, "limit": args.limit, "query": args.query, "cursor": args.cursor}),
                    None,
                )
                .await?;
            let items = value["items"].as_array().cloned().unwrap_or_default();
            if args.json {
                emit_value(&value, true)
            } else {
                let rows = items
                    .iter()
                    .map(|item| {
                        vec![
                            if item["watched"].as_bool().unwrap_or(false) {
                                "*"
                            } else {
                                ""
                            }
                            .into(),
                            item["session"]["id"].as_str().unwrap_or_default().into(),
                            item["session"]["state"].as_str().unwrap_or_default().into(),
                            item["session"]["title"].as_str().unwrap_or_default().into(),
                        ]
                    })
                    .collect::<Vec<_>>();
                print_table(&["WATCH", "SESSION", "STATE", "TITLE"], &rows);
                Ok(())
            }
        }
        SessionCommand::Show(args) => {
            let (value, _) = client
                .request(
                    "sessions.search",
                    json!({"provider": args.provider, "limit": 1, "query": args.session_id}),
                    None,
                )
                .await?;
            let session = value["items"]
                .as_array()
                .and_then(|items| items.first())
                .and_then(|item| item.get("session"))
                .cloned()
                .context("session not found")?;
            emit_value(&session, args.json)
        }
        SessionCommand::Logs(args) => {
            let (value, _) = client
                .request(
                    "sessions.logs",
                    json!({
                        "provider": args.provider,
                        "session_id": args.session_id,
                        "limit": args.limit,
                        "category": args.category,
                    }),
                    None,
                )
                .await?;
            if args.json {
                emit_value(&value, true)
            } else {
                let logs: Vec<SessionLog> = serde_json::from_value(value)?;
                print_logs(&logs);
                Ok(())
            }
        }
        SessionCommand::Send(args) => {
            let message = message_input(args.message)?;
            let (value, _) = client
                .request(
                    "sessions.send",
                    json!({"provider": args.provider, "session_id": args.session_id, "message": message}),
                    None,
                )
                .await?;
            if args.json {
                emit_value(&value, true)
            } else {
                println!(
                    "Sent message to {}:{}",
                    value["provider"], value["session_id"]
                );
                Ok(())
            }
        }
        SessionCommand::Interrupt(args) => {
            let (value, _) = client
                .request(
                    "sessions.interrupt",
                    json!({"provider": args.provider, "session_id": args.session_id}),
                    None,
                )
                .await?;
            if args.json {
                emit_value(&value, true)
            } else {
                println!("Interrupted {}:{}", value["provider"], value["session_id"]);
                Ok(())
            }
        }
        SessionCommand::List(args) => {
            let (value, _) = client
                .request(
                    "sessions.list",
                    json!({"provider": args.provider, "limit": args.limit}),
                    None,
                )
                .await?;
            let targets: Vec<WatchTarget> = serde_json::from_value(value)?;
            if args.json {
                let value = Value::Array(
                    targets
                        .iter()
                        .map(|target| {
                            let mut value =
                                serde_json::to_value(target).expect("session target serialization");
                            value["recovery_supported"] =
                                json!(watchcat::providers::supports_recovery(&target.provider));
                            value
                        })
                        .collect(),
                );
                emit_value(&value, true)
            } else {
                let rows = targets
                    .iter()
                    .map(|target| {
                        vec![
                            target.provider.clone(),
                            target.session_id.clone(),
                            if watchcat::providers::supports_recovery(&target.provider) {
                                "recovery"
                            } else {
                                "observe"
                            }
                            .into(),
                            target
                                .last_activity_at
                                .map(|t| t.to_rfc3339())
                                .unwrap_or_default(),
                            target
                                .label
                                .clone()
                                .or_else(|| target.title.clone())
                                .unwrap_or_default(),
                        ]
                    })
                    .collect::<Vec<_>>();
                print_table(
                    &["PROVIDER", "SESSION", "MODE", "LAST ACTIVITY", "LABEL"],
                    &rows,
                );
                Ok(())
            }
        }
        SessionCommand::Add {
            session_id,
            provider,
            label,
            no_validate,
        } => {
            let (_, revision) = client.request("snapshot.get", json!({}), None).await?;
            let (value, _) = client.request(
                "sessions.add",
                json!({"provider": provider, "session_id": session_id, "label": label, "validate": !no_validate}),
                Some(revision),
            ).await?;
            println!(
                "{}",
                if value["added"].as_bool() == Some(true) {
                    "Watching"
                } else {
                    "Already watching"
                }
            );
            Ok(())
        }
        SessionCommand::Remove(args) => {
            let (_, revision) = client.request("snapshot.get", json!({}), None).await?;
            let (value, _) = client
                .request(
                    "sessions.remove",
                    json!({"provider": args.provider, "session_id": args.session_id}),
                    Some(revision),
                )
                .await?;
            if args.json {
                return emit_value(&value, true);
            }
            println!(
                "{}",
                if value["removed"].as_bool() == Some(true) {
                    "Removed"
                } else {
                    "Excluded from automatic discovery"
                }
            );
            Ok(())
        }
    }
}

async fn policy_command(client: &WatchcatClient, command: PolicyCommand) -> Result<()> {
    let (_, revision) = client.request("snapshot.get", json!({}), None).await?;
    match command {
        PolicyCommand::List(args) => {
            let (value, _) = client
                .request("config.policies.list", json!({}), None)
                .await?;
            let mut policies: Vec<watchcat::config::ResolvedPolicy> =
                serde_json::from_value(value)?;
            policies.retain(|policy| {
                args.category
                    .as_deref()
                    .is_none_or(|category| policy.condition.split('.').next() == Some(category))
            });
            if args.json {
                emit_serializable(&policies, true)
            } else {
                print_policies(&policies);
                Ok(())
            }
        }
        PolicyCommand::Show { condition, json } => {
            require_condition(&condition)?;
            let (value, _) = client
                .request("config.policies.list", json!({}), None)
                .await?;
            let policies: Vec<watchcat::config::ResolvedPolicy> = serde_json::from_value(value)?;
            let policy = policies
                .into_iter()
                .find(|policy| policy.condition == condition)
                .context("policy not found")?;
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
            let policy = PolicyOverride {
                action: args.action.map(|value| match value {
                    ActionArg::Retry => PolicyAction::Retry,
                    ActionArg::Skip => PolicyAction::Skip,
                }),
                backoff: args.backoff.map(|value| match value {
                    BackoffArg::Fixed => BackoffKind::Fixed,
                    BackoffArg::Exponential => BackoffKind::Exponential,
                }),
                initial_delay_seconds: args.initial_delay,
                max_delay_seconds: args.max_delay,
                max_attempts: args.max_attempts,
                prompt: args.prompt,
            };
            client
                .request(
                    "config.policies.set",
                    json!({"condition": args.condition, "policy": policy}),
                    Some(revision),
                )
                .await?;
            println!("Updated {}", args.condition);
            Ok(())
        }
        PolicyCommand::Reset { condition, all } => {
            let params = if all {
                json!({})
            } else {
                json!({"condition": condition.context("provide CONDITION or use --all")?})
            };
            client
                .request("config.policies.reset", params, Some(revision))
                .await?;
            println!("Reset policy configuration");
            Ok(())
        }
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
        ConfigCommand::Init { .. } | ConfigCommand::Policy { .. } => unreachable!(),
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
        bail!("unknown policy condition: {condition}; run `watchcat config policy list`")
    }
}

fn path_value(paths: &Paths) -> Value {
    json!({"config": paths.config_file, "watchlist": paths.watchlist_file, "state": paths.state_file, "control": paths.control_state_file, "events": paths.event_log_file, "retry_operations": paths.retry_operations_file, "lock": paths.lock_file})
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
