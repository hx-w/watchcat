use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;
use watchcat::config::{Paths, Settings, initialize_config, load_settings};
use watchcat::engine::WatchEngine;
use watchcat::models::WatchTarget;
use watchcat::providers::{CodexProvider, Provider};
use watchcat::state::{ProcessLock, RuntimeState, WatchlistStore};

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
    /// Write a documented default configuration.
    Init {
        /// Replace an existing configuration.
        #[arg(long)]
        force: bool,
    },
    /// List sessions from a provider.
    List(ListArgs),
    /// Add one session to the explicit watchlist.
    Add {
        session_id: String,
        #[arg(long, default_value = "codex")]
        provider: String,
        #[arg(long)]
        label: Option<String>,
        /// Add without checking the 500 most recent provider sessions.
        #[arg(long)]
        no_validate: bool,
    },
    /// Remove one session from the watchlist.
    Remove {
        session_id: String,
        #[arg(long, default_value = "codex")]
        provider: String,
    },
    /// Show watched sessions and their latest failure.
    Status(OutputArgs),
    /// Run the watchdog.
    Run {
        /// Perform one reconciliation and exit.
        #[arg(long)]
        once: bool,
        /// Report recoveries without sending prompts.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// Verify configuration and provider connectivity.
    Doctor(OutputArgs),
    /// Show configuration and state paths.
    Paths(OutputArgs),
}

#[derive(Debug, Args)]
struct ListArgs {
    #[arg(long, default_value = "codex")]
    provider: String,
    #[arg(long, default_value_t = 50, value_parser = parse_positive_usize)]
    limit: usize,
    #[arg(long)]
    json: bool,
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
        Command::Init { force } => {
            initialize_config(&paths.config_file, force)?;
            println!("Wrote {}", paths.config_file.display());
        }
        Command::Paths(args) => emit_value(
            &json!({
                "config": paths.config_file,
                "watchlist": paths.watchlist_file,
                "state": paths.state_file,
                "lock": paths.lock_file,
            }),
            args.json,
        )?,
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
    settings: Settings,
    paths: Paths,
    watchlist: WatchlistStore,
) -> Result<()> {
    match command {
        Command::List(args) => list_sessions(&settings, &watchlist, args).await,
        Command::Add {
            session_id,
            provider,
            label,
            no_validate,
        } => {
            if !no_validate {
                let mut providers = build_providers(&settings, [provider.as_str()])?;
                start_providers(&mut providers).await?;
                let sessions = providers
                    .get_mut(&provider)
                    .context("provider was not constructed")?
                    .list_sessions(500)
                    .await?;
                close_providers(&mut providers).await;
                if !sessions.iter().any(|session| session.id == session_id) {
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
        Command::Remove {
            session_id,
            provider,
        } => {
            let removed = watchlist.remove(&provider, &session_id)?;
            println!(
                "{} {provider}:{session_id}",
                if removed { "Removed" } else { "Not watched" }
            );
            if removed {
                Ok(())
            } else {
                std::process::exit(1)
            }
        }
        Command::Status(args) => status(&settings, &watchlist, args.json).await,
        Command::Run {
            once,
            dry_run,
            json,
        } => run_watchdog(settings, paths, &watchlist, once, dry_run, json).await,
        Command::Doctor(args) => doctor(&settings, &paths, args.json).await,
        Command::Init { .. } | Command::Paths(_) => unreachable!(),
    }
}

async fn list_sessions(
    settings: &Settings,
    watchlist: &WatchlistStore,
    args: ListArgs,
) -> Result<()> {
    let watched = watchlist
        .list()?
        .into_iter()
        .map(|target| target.key())
        .collect::<HashSet<_>>();
    let mut providers = build_providers(settings, [args.provider.as_str()])?;
    start_providers(&mut providers).await?;
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
                    "provider": session.provider,
                    "id": session.id,
                    "title": session.title,
                    "state": session.state,
                    "updated_at": session.updated_at,
                    "watched": watched.contains(&session.key()),
                    "metadata": session.metadata,
                })
            })
            .collect::<Vec<_>>();
        println!("{}", serde_json::to_string_pretty(&values)?);
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
    }
    Ok(())
}

async fn status(settings: &Settings, watchlist: &WatchlistStore, json_output: bool) -> Result<()> {
    let targets = watchlist.list()?;
    if targets.is_empty() {
        if json_output {
            println!("[]");
        } else {
            println!("Watchlist is empty. Use `watchcat list` and `watchcat add <session-id>`.");
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
            Ok(failure) => values.push(json!({
                "provider": target.provider,
                "session_id": target.session_id,
                "label": target.label,
                "enabled": target.enabled,
                "failure": failure,
            })),
            Err(error) => values.push(json!({
                "provider": target.provider,
                "session_id": target.session_id,
                "label": target.label,
                "enabled": target.enabled,
                "error": error.to_string(),
            })),
        }
    }
    close_providers(&mut providers).await;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&values)?);
    } else {
        let rows = values
            .iter()
            .map(|value| {
                let latest = value
                    .pointer("/failure/code")
                    .or_else(|| value.get("error"))
                    .and_then(Value::as_str)
                    .unwrap_or("ok");
                vec![
                    string_field(value, "provider"),
                    string_field(value, "session_id"),
                    value
                        .get("enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                        .to_string(),
                    latest.into(),
                    string_field(value, "label"),
                ]
            })
            .collect::<Vec<_>>();
        print_table(
            &["PROVIDER", "SESSION", "ENABLED", "LATEST", "LABEL"],
            &rows,
        );
    }
    Ok(())
}

async fn run_watchdog(
    settings: Settings,
    paths: Paths,
    watchlist: &WatchlistStore,
    once: bool,
    dry_run: bool,
    json_output: bool,
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
    let target_count = targets.len();
    let mut engine = WatchEngine::new(settings.engine, providers, targets, state, dry_run);
    let result = if once {
        let events = engine.run_once(Utc::now()).await?;
        if json_output {
            println!("{}", serde_json::to_string_pretty(&events)?);
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
        tracing::info!(targets = target_count, dry_run, "watchcat started");
        engine.run_forever_with(|| watchlist.list().map(Some)).await
    };
    engine.close().await;
    result
}

async fn doctor(settings: &Settings, paths: &Paths, json_output: bool) -> Result<()> {
    let mut checks = Vec::new();
    let executable = settings.providers.codex.command.first().cloned();
    let found = executable
        .as_deref()
        .and_then(find_executable)
        .map(|path| path.display().to_string());
    checks.push(json!({
        "name": "codex executable",
        "ok": found.is_some(),
        "detail": found.unwrap_or_else(|| "not found".into()),
    }));
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
            Ok(count) => checks.push(json!({
                "name": "Codex App Server", "ok": true,
                "detail": format!("connected; {count} session(s) sampled")
            })),
            Err(error) => checks.push(json!({
                "name": "Codex App Server", "ok": false, "detail": error.to_string()
            })),
        }
    }
    for (name, path) in [
        ("config path", &paths.config_file),
        ("state path", &paths.state_file),
    ] {
        checks.push(json!({"name": name, "ok": true, "detail": path}));
    }
    if json_output {
        println!("{}", serde_json::to_string_pretty(&checks)?);
    } else {
        let rows = checks
            .iter()
            .map(|check| {
                vec![
                    string_field(check, "name"),
                    if check.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                        "ok".into()
                    } else {
                        "failed".into()
                    },
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
            _ => bail!("unknown or unimplemented provider: {name}"),
        }
    }
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
