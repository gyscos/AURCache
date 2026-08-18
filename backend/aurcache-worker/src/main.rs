//! AURCache remote build worker.
//!
//! A long-lived process that enrolls with an AURCache backend over mTLS, polls
//! for build jobs, builds each package in its own `devtools` chroot, and uploads
//! the results. See `design/remote-workers.md`.

mod build;
mod cache;
mod chroot;
mod client;
mod config;
mod enroll;
mod identity;
mod job;
mod oneshot;
mod runner;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::sync::Arc;

use crate::config::Config;
use crate::identity::Identity;
use crate::runner::Runner;

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
        path: String,
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
        Command::BuildOnce { path, flags } => {
            oneshot::build_once(&cfg, std::path::Path::new(&path), &flags).await
        }
        Command::Run => run(cfg).await,
    }
}

async fn run(cfg: Arc<Config>) -> Result<()> {
    let identity = Identity::load_or_create(&cfg.data_dir)?;
    let client = enroll::ensure_enrolled(&cfg, &identity).await?;
    let runner = Runner::new(cfg, Arc::new(client));
    runner.run().await
}
