use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;
use watchcat::config::Paths;

#[derive(Debug, Parser)]
#[command(
    name = "watchcatd",
    version,
    about = "Watchcat local reliability service"
)]
struct Cli {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    dry_run: bool,
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
    #[arg(long, hide = true)]
    hold_update_lock: Option<PathBuf>,
    #[arg(long, hide = true, requires = "hold_update_lock")]
    lock_ready: Option<PathBuf>,
}

fn main() {
    let task = || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(run())
    };
    #[cfg(target_os = "macos")]
    let result = watchcat::os_permissions::run_main(task);
    #[cfg(not(target_os = "macos"))]
    let result: Result<()> = task();
    if let Err(error) = result {
        eprintln!("watchcatd: {error:#}");
        std::process::exit(2);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let fallback = match cli.verbose {
        0 => "watchcat=info",
        1 => "watchcat=debug",
        _ => "watchcat=trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| fallback.into()))
        .with_target(false)
        .try_init()
        .ok();
    let paths = Paths::discover(cli.config)?;
    if let Some(lock_path) = cli.hold_update_lock {
        let _lock = watchcat::state::ProcessLock::acquire(lock_path)?;
        if let Some(ready) = cli.lock_ready {
            std::fs::write(ready, b"ready\n")?;
        }
        tokio::signal::ctrl_c().await?;
        return Ok(());
    }
    watchcat::daemon::serve(paths, cli.dry_run).await
}
