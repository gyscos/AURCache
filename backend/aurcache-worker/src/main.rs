//! AURCache remote build worker.
//!
//! A long-lived process that enrolls with an AURCache backend over mTLS, polls
//! for build jobs, builds each package in its own `devtools` chroot, and uploads
//! the results. See `design/remote-workers.md`.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

use aurcache_worker::config::Config;
use aurcache_worker::identity::Identity;
use aurcache_worker::runner::Runner;
use aurcache_worker::{credentials, enroll, oneshot};

#[derive(Parser)]
#[command(name = "aurcache-worker", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Enroll and continuously build jobs (default).
    Run,
    /// Generate the worker identity, print its fingerprint, and exit.
    Prepare,
    /// Build a single PKGBUILD directory locally without a server (dev loop).
    BuildOnce {
        /// Path to a directory containing a PKGBUILD (defaults to cwd).
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Extra makepkg flags (e.g. --nocheck).
        #[arg(long = "flag")]
        flags: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let cfg = Arc::new(Config::from_env());

    match cli.command.unwrap_or(Command::Run) {
        Command::Prepare => {
            let identity = Identity::load_or_create(&cfg.data_dir)?;
            println!("Worker fingerprint: {}", identity.fingerprint);
            println!("Name: {}", cfg.name);
            println!("Native arches: {}", cfg.native_arches.join(","));
            Ok(())
        }
        Command::BuildOnce { path, flags } => oneshot::build_once(&cfg, &path, &flags).await,
        Command::Run => run(cfg).await,
    }
}

/// Prepare the SSH key used for authenticated sources and log its public half.
///
/// Generation is a one-time, first-start event, but the public key is logged on
/// *every* start: it is what the operator has to paste into GitHub, and hunting
/// for it in old logs after a container restart is exactly the friction this is
/// meant to avoid.
///
/// Never fatal. A worker with no credential builds every package that does not
/// need one, which is nearly all of them.
async fn announce_build_credential(cfg: &Config) {
    let source = credentials::resolve(cfg);
    match credentials::ensure(&source).await {
        Ok(Some(public_key)) => {
            tracing::info!(
                "Build SSH key: {}\n    Add this public key to the account that \
                 may fetch restricted sources:\n    {}",
                source.path().display(),
                public_key.trim()
            );
        }
        Ok(None) => tracing::info!("Using configured build SSH key {}", source.path().display()),
        Err(e) => tracing::warn!(
            "No build SSH credential available ({e:#}); packages with \
             authenticated sources will fail"
        ),
    }
}

async fn run(cfg: Arc<Config>) -> Result<()> {
    let identity = Identity::load_or_create(&cfg.data_dir)?;
    announce_build_credential(&cfg).await;

    // The server may not be reachable yet (e.g. still starting in the same
    // compose stack) or may briefly go away. Retry enrollment with backoff
    // instead of crashing, so the worker is resilient to server restarts.
    let client = loop {
        match enroll::ensure_enrolled(&cfg, &identity).await {
            Ok(client) => break client,
            Err(e) => {
                tracing::warn!(
                    "Enrollment not complete ({e:#}); retrying in {}s",
                    cfg.poll_interval
                );
                tokio::time::sleep(std::time::Duration::from_secs(cfg.poll_interval)).await;
            }
        }
    };

    let runner = Runner::new(cfg, Arc::new(client));
    runner.run().await
}
