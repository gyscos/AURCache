mod compose;
mod config;
mod doctor;
mod pacman;
mod repo;
mod setup;
mod url;

use anyhow::{Context, Result, anyhow, bail};
use aurcache_client::{
    AddPackageRequest, AddPackagesRequest, AurCacheClient, Build, BulkAddAccepted, BulkAddOutcome,
    BulkAddProgress, ExtendedPackage, GitSourceSpec, GraphDataPoint, ListStats, Method,
    PackageDependency, PackageSource, PatchPackageRequest, RestoreOutcome, SearchResult,
    SimplePackage, SourceData, UpdatePackageRequest, UserInfo, Worker, looks_like_git_url,
};
use aurcache_common::build_state::{BuildState, BuildStates};
use chrono::{DateTime, Utc};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use config::{
    load_config, resolve_runtime_config, save_config, set_token, set_url, summarize_config,
};
use dialoguer::Confirm;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    #[default]
    Text,
    Json,
}

#[derive(Parser, Debug)]
#[command(
    name = "aurcache-cli",
    version,
    about = "CLI client for the AURCache API using bearer-token authentication"
)]
struct Cli {
    /// Base URL of the AURCache API.
    #[arg(long, env = "AURCACHE_URL")]
    url: Option<String>,

    /// API token used as Authorization: Bearer <token>.
    #[arg(long, env = "AURCACHE_TOKEN")]
    token: Option<String>,

    /// Output format for typed commands.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    format: OutputFormat,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug, Clone)]
enum Command {
    /// Check whether the server is healthy.
    Health,
    /// Diagnose why builds are not running.
    Doctor,
    /// Stand up a server and/or workers.
    Setup {
        // Boxed: these argument structs are much larger than any other variant,
        // and every parse would otherwise carry that size.
        #[command(subcommand)]
        command: Box<SetupCommand>,
    },
    /// Consume this instance as a pacman repository.
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// Print a shell completion script.
    Completions {
        /// Shell to generate completions for.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Inspect the authenticated user.
    UserInfo,
    /// Get dashboard statistics.
    Stats,
    /// Get monthly graph datapoints.
    Graph,
    /// Search the AUR through AURCache.
    Search {
        /// Search query.
        query: String,
    },
    /// Manage API tokens.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Manage CLI configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Manage packages.
    Pkg {
        #[command(subcommand)]
        command: PackagesCommand,
    },
    /// Export the server's authored state to a `.tar.gz`.
    Dump {
        /// Where to write the archive. Defaults to a dated name in the current
        /// directory.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Also export the CA private key, worker certificates and API token
        /// hashes. The file becomes a credential: anyone holding it can mint a
        /// worker this server accepts. Written owner-readable only.
        #[arg(long)]
        include_secrets: bool,
    },
    /// Restore an instance from a dump.
    Restore {
        /// The `.tar.gz` to read.
        archive: PathBuf,
        /// Report what would happen and change nothing.
        #[arg(long)]
        dry_run: bool,
        /// What to do about a package that already exists here.
        #[arg(long, value_enum, default_value_t = OnExisting::Skip)]
        on_existing: OnExisting,
        /// Replace what is here rather than adding to it. Destructive: every
        /// package, setting and worker this instance has is removed first.
        #[arg(long)]
        clear: bool,
        /// Take the dump's CA, worker certificates and token hashes. Replacing
        /// the CA invalidates every certificate this server's workers hold.
        #[arg(long, value_enum, default_value_t = Secrets::Ignore)]
        secrets: Secrets,
    },
    /// Manage builds.
    Builds {
        #[command(subcommand)]
        command: BuildsCommand,
    },
    /// Manage remote build workers.
    Worker {
        #[command(subcommand)]
        command: WorkerCommand,
    },
    /// Call an arbitrary API path.
    Raw(RawArgs),
}

#[derive(Subcommand, Debug, Clone)]
enum TokenCommand {
    /// Regenerate the currently authenticated user's API token.
    Regenerate,
}

#[derive(Subcommand, Debug, Clone)]
enum WorkerCommand {
    /// List enrolled remote build workers.
    List,
    /// Approve a pending worker so it can build.
    Approve {
        /// Worker id.
        id: i32,
    },
    /// Revoke a worker, immediately refusing its certificate.
    Revoke {
        /// Worker id.
        id: i32,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum SetupCommand {
    /// Write a docker-compose file to run elsewhere — TrueNAS, Portainer,
    /// Unraid, or `docker compose up` on any host.
    Compose(ComposeArgs),
    /// Run the AURCache server on this machine with `docker run`.
    Server(SetupServerArgs),
    /// Run a build worker on this machine with `docker run`.
    Worker(SetupWorkerArgs),
}

#[derive(Args, Debug, Clone)]
struct ComposeArgs {
    /// Which services the file should contain.
    #[arg(long, value_enum, default_value_t = compose::ComposeRole::Bundle)]
    role: compose::ComposeRole,

    /// Where to write it. `-` writes to stdout.
    #[arg(long, short = 'o')]
    output: Option<String>,

    /// Overwrite an existing file.
    #[arg(long)]
    force: bool,

    /// Base URL pacman clients use to fetch built packages.
    #[arg(long)]
    public_url: Option<String>,

    /// Host names the server's TLS certificate must be valid for.
    #[arg(long)]
    tls_sans: Option<String>,

    #[command(flatten)]
    worker: WorkerKnobs,

    #[command(flatten)]
    common: SetupCommon,
}

#[derive(Args, Debug, Clone)]
struct SetupServerArgs {
    /// Print the `docker run` command instead of running it.
    #[arg(long)]
    dry_run: bool,

    /// Base URL pacman clients use to fetch built packages.
    #[arg(long)]
    public_url: Option<String>,

    /// Host names the server's TLS certificate must be valid for.
    #[arg(long)]
    tls_sans: Option<String>,

    /// Extra arguments passed to `docker run`, before the image.
    #[arg(last = true)]
    docker_args: Vec<String>,

    #[command(flatten)]
    common: SetupCommon,
}

#[derive(Args, Debug, Clone)]
struct SetupWorkerArgs {
    /// Print the `docker run` command instead of running it.
    #[arg(long)]
    dry_run: bool,

    /// Worker-protocol URL of the server to join, e.g.
    /// `https://aurcache.example.com:8083`. Defaults to a server on this
    /// machine, which is also what makes the worker auto-approved.
    #[arg(long)]
    server_url: Option<String>,

    /// SHA-256 of the server's CA, from its startup log line
    /// "Worker CA fingerprint (pin this on workers)". Strongly recommended for
    /// a server reached over anything but a trusted network.
    #[arg(long)]
    ca_fingerprint: Option<String>,

    /// Container name. Give each worker on a host its own.
    #[arg(long, default_value = setup::WORKER_CONTAINER)]
    container_name: String,

    /// Volume holding the worker's identity and base chroot. Each worker on a
    /// host needs its own, or they fight over one identity.
    #[arg(long)]
    data_volume: Option<String>,

    /// Volume holding the package cache.
    #[arg(long)]
    cache_volume: Option<String>,

    /// Extra arguments passed to `docker run`, before the image.
    #[arg(last = true)]
    docker_args: Vec<String>,

    #[command(flatten)]
    worker: WorkerKnobs,

    #[command(flatten)]
    common: SetupCommon,
}

/// Worker settings shared by `setup compose` and `setup worker`.
#[derive(Args, Debug, Clone)]
struct WorkerKnobs {
    /// Name shown in the Workers UI. Defaults to the container's hostname.
    #[arg(long = "worker-name")]
    worker_name: Option<String>,

    /// Architecture built natively. Repeat for several. Defaults to the host's.
    #[arg(long = "arch")]
    arches: Vec<String>,

    /// Architecture built under emulation — slower, and worth telling apart.
    #[arg(long = "emulated-arch")]
    emulated_arches: Vec<String>,

    /// Reserve a pkgbase to this worker. Repeat for several.
    #[arg(long = "package")]
    packages: Vec<String>,

    /// Packages built in parallel.
    #[arg(long)]
    concurrency: Option<u32>,

    /// Scheduling preference; higher wins.
    #[arg(long)]
    priority: Option<i32>,

    /// Shared secret matching the server's `AURCACHE_ENROLLMENT_TOKEN`.
    #[arg(long)]
    enrollment_token: Option<String>,
}

#[derive(Args, Debug, Clone)]
struct SetupCommon {
    /// Server image to run.
    #[arg(long)]
    server_image: Option<String>,

    /// Worker image to run.
    #[arg(long)]
    worker_image: Option<String>,

    /// Log level for the containers.
    #[arg(long, default_value = "info")]
    log_level: String,
}

impl WorkerKnobs {
    fn to_env(&self) -> compose::WorkerEnv {
        compose::WorkerEnv {
            url: None,
            ca_fingerprint: None,
            enrollment_token: self.enrollment_token.clone(),
            enrollment_dir: None,
            name: self.worker_name.clone(),
            arches: self.arches.clone(),
            emulated_arches: self.emulated_arches.clone(),
            packages: self.packages.clone(),
            concurrency: self.concurrency,
            priority: self.priority,
        }
    }
}

#[derive(Subcommand, Debug, Clone)]
enum RepoCommand {
    /// Print the `pacman.conf` stanza for this instance.
    Config(RepoConfigArgs),
}

#[derive(Args, Debug, Clone)]
struct RepoConfigArgs {
    /// Add the stanza to pacman.conf instead of printing it. Asks for sudo only
    /// if the file is not already writable, and does nothing if the repository
    /// is already configured.
    #[arg(long)]
    install: bool,

    /// The pacman configuration to write to.
    #[arg(long, default_value = repo::PACMAN_CONF)]
    pacman_conf: PathBuf,

    /// Port the pacman repository is published on.
    #[arg(long)]
    port: Option<u16>,

    /// Section name, which must match the repository's database files.
    #[arg(long)]
    name: Option<String>,

    /// `SigLevel` to write. AURCache does not sign packages, so a stricter
    /// level than the default refuses everything it serves.
    #[arg(long)]
    siglevel: Option<String>,
}

#[derive(Subcommand, Debug, Clone)]
enum ConfigCommand {
    /// Show the saved config location and stored values.
    Show,
    /// Save the AURCache URL to the config file.
    SetUrl {
        /// URL to save. If omitted, it will be requested interactively.
        url: Option<String>,
    },
    /// Save the AURCache API token to the config file.
    SetToken {
        /// Token to save. If omitted, it will be requested interactively.
        token: Option<String>,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum PackagesCommand {
    /// List directly requested packages.
    List(ListPackagesArgs),
    /// Get one package.
    Get {
        /// Package name (pkgbase).
        pkgbase: String,
    },
    /// Add a package. Each entry is treated as a git repository URL if it
    /// looks like one (contains `@` or a URL scheme like `https://`),
    /// otherwise as an AUR package name.
    Add(AddPackageArgs),
    /// Trigger an update check for a package.
    Update(UpdatePackageArgs),
    /// Partially update package metadata.
    Patch(PatchPackageArgs),
    /// Remove the direct-request flag from a package.
    Delete {
        /// Package name (pkgbase).
        pkgbase: String,
    },
}

#[derive(Args, Debug, Clone)]
struct ListPackagesArgs {
    /// Maximum number of packages to return.
    #[arg(long)]
    limit: Option<u64>,

    /// Page offset used together with --limit.
    #[arg(long)]
    page: Option<u64>,

    /// Include packages that are only present as dependencies.
    #[arg(long)]
    all: bool,
}

#[derive(Args, Debug, Clone)]
struct AddPackageArgs {
    /// AUR package names and/or git repository URLs. Each entry is treated
    /// as a git URL if it looks like one (contains `@` or a URL scheme like
    /// `https://`), otherwise as an AUR package name.
    #[arg(required_unless_present = "from_installed")]
    packages: Vec<String>,

    /// Also add every AUR package already installed on this machine, as
    /// reported by `pacman -Qm`.
    #[arg(long)]
    from_installed: bool,

    /// Skip the confirmation prompt. Required with --from-installed when not
    /// running on a terminal, because that list is not one to submit unseen.
    #[arg(long, short = 'y')]
    yes: bool,

    /// Git ref to checkout. Required if any entry is a git URL.
    #[arg(long = "ref")]
    git_ref: Option<String>,

    /// Subfolder containing the PKGBUILD, for git URL entries.
    #[arg(long, default_value = "")]
    subfolder: String,

    /// Target platform. Repeat for multiple platforms.
    #[arg(long = "platform")]
    platforms: Vec<String>,

    /// Build flag. Repeat for multiple flags.
    #[arg(long = "build-flag")]
    build_flags: Vec<String>,

    /// Patch a source file before adding: either `SOURCE_PATH=LOCAL_FILE`
    /// (e.g. `--patch PKGBUILD=./fixed-PKGBUILD`) or just `LOCAL_FILE`, in
    /// which case the file's own base name is used as the source path (e.g.
    /// `--patch ./PKGBUILD` patches `PKGBUILD`). Repeat for multiple files.
    /// Only valid when adding a single package.
    #[arg(long = "patch", value_parser = parse_patch_arg)]
    patches: Vec<(String, String)>,
}

#[derive(Args, Debug, Clone)]
struct UpdatePackageArgs {
    /// Package name (pkgbase).
    pkgbase: String,

    /// Force the update even when the version did not change.
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug, Clone)]
struct PatchPackageArgs {
    /// Package name (pkgbase).
    pkgbase: String,

    /// Platform selection. Repeat to replace with multiple values.
    #[arg(long = "platform")]
    platforms: Vec<String>,

    /// Build flag selection. Repeat to replace with multiple values.
    #[arg(long = "build-flag")]
    build_flags: Vec<String>,
}

#[derive(Subcommand, Debug, Clone)]
enum BuildsCommand {
    /// List builds.
    List(ListBuildsArgs),
    /// Get one build.
    Get {
        /// Build reference, e.g. `hello/3`.
        build: BuildRef,
    },
    /// Fetch build output.
    Output(BuildOutputArgs),
    /// Retry a build.
    Retry {
        /// Build reference, e.g. `hello/3`.
        build: BuildRef,
    },
    /// Cancel a build.
    Cancel {
        /// Build reference, e.g. `hello/3`.
        build: BuildRef,
    },
    /// Delete a build.
    Delete {
        /// Build reference, e.g. `hello/3`.
        build: BuildRef,
    },
    /// Follow builds until they finish, reporting progress.
    Watch(WatchArgs),
}

#[derive(Args, Debug, Clone)]
struct WatchArgs {
    /// Only follow builds of this package.
    #[arg(long)]
    package: Option<String>,
    /// Give up after this many seconds.
    #[arg(long, default_value_t = 900)]
    timeout: u64,
    /// Fail if nothing changes for this long while nothing is building.
    ///
    /// A queue that is stuck cannot recover on its own, so waiting out the full
    /// timeout only delays the diagnosis. A running build is never treated as a
    /// stall, however slow it is.
    #[arg(long = "stall-after", default_value_t = 120)]
    stall_after: u64,
    /// Seconds between progress lines while work is in flight.
    #[arg(long, default_value_t = 60)]
    heartbeat: u64,
    /// Fail if a build returns to the queue after running.
    ///
    /// That means the server refused the worker's completion, which repeats
    /// indefinitely — the build runs, is rejected, and is queued again. Normal
    /// operation can requeue a build whose worker was lost, so this is opt-in
    /// and intended for tests.
    #[arg(long = "fail-on-requeue")]
    fail_on_requeue: bool,
}

#[derive(Args, Debug, Clone)]
struct ListBuildsArgs {
    /// Optional package name (pkgbase) to filter by.
    #[arg(long = "package")]
    pkgbase: Option<String>,

    /// Maximum number of builds to return.
    #[arg(long)]
    limit: Option<u64>,

    /// Page offset used together with --limit.
    #[arg(long)]
    page: Option<u64>,
}

#[derive(Args, Debug, Clone)]
struct BuildOutputArgs {
    /// Build reference, e.g. `hello/3`.
    build: BuildRef,

    /// Skip this many bytes of log before printing.
    ///
    /// Bytes rather than lines: the server seeks to the offset, so the cost is
    /// proportional to what is read rather than to the whole log.
    #[arg(long = "offset")]
    offset: Option<u64>,
}

#[derive(Args, Debug, Clone)]
struct RawArgs {
    /// HTTP method to use, for example GET or POST.
    method: String,

    /// API path such as /packages/list or a full URL.
    path: String,

    /// Query parameter in KEY=VALUE form. Repeat to pass multiple pairs.
    #[arg(long = "query", value_parser = parse_key_val)]
    query: Vec<(String, String)>,

    /// JSON body to send.
    #[arg(long)]
    body: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let format = cli.format;

    match cli.command {
        Command::Config { command } => run_config_command(format, command),
        // Offline, like `config`: the repo address is arithmetic on the
        // configured URL, and a new user reaching for this may not have a token
        // yet. Prompting for one to print three lines of text would be absurd.
        Command::Repo {
            command: RepoCommand::Config(args),
        } => run_repo_config(format, cli.url, args),
        Command::Completions { shell } => {
            run_completions(shell);
            Ok(())
        }
        // Also offline: the whole point is standing up an instance that does
        // not exist yet, so there is nothing to authenticate against.
        Command::Setup { command } => run_setup_command(format, *command),
        command => {
            let runtime = resolve_runtime_config(cli.url, cli.token)?;
            let used_token = runtime.token.clone();
            let client = AurCacheClient::new(runtime.url.clone(), runtime.token)?;
            match run(&client, format, command.clone()).await {
                Err(err) if is_unauthorized(&err) && config::is_interactive() => {
                    eprintln!("{err}");
                    warn_if_token_from_env(used_token.as_deref());
                    eprintln!("Please re-enter your AURCache API token to continue.");
                    let token = config::prompt_and_save_token(load_config()?)?;
                    let client = AurCacheClient::new(runtime.url, Some(token))?;
                    run(&client, format, command).await
                }
                result => result,
            }
        }
    }
}

/// Warns the user if the token that was just rejected came from the
/// `AURCACHE_TOKEN` environment variable. Env vars take precedence over the
/// config file (see `resolve_runtime_config`), so saving a freshly prompted
/// token there won't actually fix anything next run unless the stale
/// environment variable is also updated or unset.
fn warn_if_token_from_env(used_token: Option<&str>) {
    if let (Some(used), Ok(env_token)) = (used_token, std::env::var("AURCACHE_TOKEN"))
        && used == env_token
    {
        eprintln!(
            "Warning: that token came from the AURCACHE_TOKEN environment variable. \
             The new token will be saved to the config file, but AURCACHE_TOKEN still \
             takes precedence over it, so this will keep failing until you update or \
             unset that environment variable."
        );
    }
}

/// Whether the given error is an HTTP 401 Unauthorized response from the API.
fn is_unauthorized(err: &anyhow::Error) -> bool {
    err.downcast_ref::<aurcache_client::ApiError>()
        .is_some_and(aurcache_client::ApiError::is_unauthorized)
}

async fn run(client: &AurCacheClient, format: OutputFormat, command: Command) -> Result<()> {
    match command {
        Command::Health => run_health(client).await,
        Command::Doctor => doctor::run_doctor(client, format, client.base_url()).await,
        Command::Repo { .. } | Command::Completions { .. } | Command::Setup { .. } => {
            unreachable!("offline commands are handled before client setup")
        }
        Command::UserInfo => render_user_info(client, format).await,
        Command::Stats => render_stats(client, format).await,
        Command::Graph => render_graph(client, format).await,
        Command::Search { query } => render_search_results(client, format, &query).await,
        Command::Token { command } => run_token_command(client, format, command).await,
        Command::Config { .. } => unreachable!("config commands are handled before client setup"),
        Command::Pkg { command } => run_packages_command(client, format, command).await,
        Command::Dump {
            output,
            include_secrets,
        } => dump_command(client, format, output, include_secrets).await,
        Command::Restore {
            archive,
            dry_run,
            on_existing,
            clear,
            secrets,
        } => {
            restore_command(
                client,
                format,
                &archive,
                dry_run,
                on_existing,
                clear,
                secrets,
            )
            .await
        }
        Command::Builds { command } => run_builds_command(client, format, command).await,
        Command::Worker { command } => run_worker_command(client, format, command).await,
        Command::Raw(args) => run_raw_command(client, args).await,
    }
}

fn run_setup_command(format: OutputFormat, command: SetupCommand) -> Result<()> {
    match command {
        SetupCommand::Compose(args) => run_setup_compose(format, args),
        SetupCommand::Server(args) => run_setup_server(format, args),
        SetupCommand::Worker(args) => run_setup_worker(format, args),
    }
}

fn run_setup_compose(format: OutputFormat, args: ComposeArgs) -> Result<()> {
    let defaults = compose::ComposeParams::default();
    let params = compose::ComposeParams {
        role: args.role,
        server_image: args.common.server_image.unwrap_or(defaults.server_image),
        worker_image: args.common.worker_image.unwrap_or(defaults.worker_image),
        public_url: args.public_url.unwrap_or(defaults.public_url),
        log_level: args.common.log_level,
        tls_sans: args.tls_sans.unwrap_or(defaults.tls_sans),
        worker: args.worker.to_env(),
    };
    let rendered = compose::render_compose(&params);

    let target = args
        .output
        .unwrap_or_else(|| args.role.default_filename().to_string());
    if target == "-" {
        print!("{rendered}");
        return Ok(());
    }

    let path = Path::new(&target);
    if path.exists() && !args.force {
        bail!("{target} already exists; pass --force to overwrite it");
    }
    std::fs::write(path, &rendered).with_context(|| format!("failed to write {target}"))?;

    match format {
        OutputFormat::Json => print_json(
            &json!({ "path": target, "role": format!("{:?}", args.role).to_lowercase() }),
        ),
        OutputFormat::Text => {
            println!("wrote {target}");
            println!();
            println!("Start it with:");
            println!("  docker compose -f {target} up -d");
            Ok(())
        }
    }
}

fn run_setup_server(format: OutputFormat, args: SetupServerArgs) -> Result<()> {
    let defaults = compose::ComposeParams::default();
    let image = args.common.server_image.unwrap_or(defaults.server_image);
    let public_url = args.public_url.unwrap_or(defaults.public_url);
    let tls_sans = args.tls_sans.unwrap_or(defaults.tls_sans);

    let run = setup::server_run(
        &image,
        &public_url,
        &args.common.log_level,
        &tls_sans,
        &args.docker_args,
    );

    if args.dry_run {
        return report_dry_run(format, &run);
    }

    setup::ensure_local_objects(true)?;
    setup::execute(&run)?;

    match format {
        OutputFormat::Json => print_json(&run),
        OutputFormat::Text => {
            println!();
            println!("Server started. Next:");
            println!("  aurcache-cli setup worker      # a build worker on this machine");
            println!("  aurcache-cli doctor            # check it came up");
            Ok(())
        }
    }
}

fn run_setup_worker(format: OutputFormat, args: SetupWorkerArgs) -> Result<()> {
    let defaults = compose::ComposeParams::default();
    let image = args.common.worker_image.unwrap_or(defaults.worker_image);

    // A worker beside the server takes the bundled shortcut: shared network,
    // shared enrollment volume, no approval step. One pointed anywhere else
    // cannot, and has to establish trust explicitly.
    let local = match &args.server_url {
        None => true,
        Some(url) => url::host_from_url(url).is_some_and(setup::is_local_host),
    };

    let mut env = args.worker.to_env();
    env.ca_fingerprint.clone_from(&args.ca_fingerprint);
    let env = if local {
        setup::local_worker_env(env)
    } else {
        let server_url = args
            .server_url
            .clone()
            .expect("a non-local worker always has a server URL");
        if env.ca_fingerprint.is_none() && env.enrollment_token.is_none() {
            eprintln!(
                "Warning: no --ca-fingerprint and no --enrollment-token. The worker will \
                 trust the server on first contact, and will wait for you to approve it \
                 on the Workers page."
            );
        }
        setup::remote_worker_env(env, &server_url)
    };

    // Each worker on a host needs its own identity volume, so the default
    // follows the container name rather than being shared.
    let data_volume = args
        .data_volume
        .unwrap_or_else(|| format!("{}_data", args.container_name.replace('-', "_")));
    let cache_volume = args
        .cache_volume
        .unwrap_or_else(|| format!("{}_cache", args.container_name.replace('-', "_")));

    let run = setup::worker_run(&setup::WorkerRunSpec {
        image,
        container_name: args.container_name,
        env,
        log_level: args.common.log_level,
        join_network: local,
        data_volume,
        cache_volume,
        extra: args.docker_args,
    });

    if args.dry_run {
        return report_dry_run(format, &run);
    }

    setup::ensure_local_objects(local)?;
    setup::execute(&run)?;

    match format {
        OutputFormat::Json => print_json(&run),
        OutputFormat::Text => {
            println!();
            if local {
                println!(
                    "Worker started, and approves itself through the shared enrollment volume."
                );
            } else {
                println!("Worker started. Approve it once it appears:");
                println!("  aurcache-cli worker list");
            }
            println!("  aurcache-cli doctor            # check it connected");
            Ok(())
        }
    }
}

fn report_dry_run(format: OutputFormat, run: &setup::DockerRun) -> Result<()> {
    match format {
        OutputFormat::Json => print_json(run),
        OutputFormat::Text => {
            println!("{}", run.command_line());
            Ok(())
        }
    }
}

/// Print the `pacman.conf` stanza, resolving the URL without touching the token.
fn run_repo_config(
    format: OutputFormat,
    cli_url: Option<String>,
    args: RepoConfigArgs,
) -> Result<()> {
    let api_url = config::resolve_url_only(cli_url)?;
    let config = repo::repo_config(&api_url, args.port, args.name, args.siglevel)?;

    if args.install {
        let installed = repo::install_stanza(&args.pacman_conf, &config)?;
        return match format {
            OutputFormat::Json => print_json(&installed),
            OutputFormat::Text => {
                if installed.changed {
                    println!("added [{}] to {}", config.name, installed.path);
                    println!("  sudo pacman -Sy");
                } else {
                    println!(
                        "[{}] is already configured in {}",
                        config.name, installed.path
                    );
                }
                Ok(())
            }
        };
    }

    match format {
        OutputFormat::Json => print_json(&config),
        OutputFormat::Text => {
            repo::print_repo_config(&config);
            Ok(())
        }
    }
}

fn run_completions(shell: clap_complete::Shell) {
    let mut command = Cli::command();
    let name = command.get_name().to_string();
    clap_complete::generate(shell, &mut command, name, &mut std::io::stdout());
}

fn run_config_command(format: OutputFormat, command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Show => show_config(format),
        ConfigCommand::SetUrl { url } => save_url_config(format, url),
        ConfigCommand::SetToken { token } => save_token_config(format, token),
    }
}

async fn run_health(client: &AurCacheClient) -> Result<()> {
    client.health().await?;
    println!("ok");
    Ok(())
}

async fn render_user_info(client: &AurCacheClient, format: OutputFormat) -> Result<()> {
    let user = client.user_info().await?;
    render(format, &user, print_user_info)
}

async fn render_stats(client: &AurCacheClient, format: OutputFormat) -> Result<()> {
    let stats = client.stats().await?;
    render(format, &stats, print_stats)
}

async fn render_graph(client: &AurCacheClient, format: OutputFormat) -> Result<()> {
    let points = client.graph().await?;
    render(format, &points, |points| print_graph(points))
}

async fn render_search_results(
    client: &AurCacheClient,
    format: OutputFormat,
    query: &str,
) -> Result<()> {
    let results = client.search(query).await?;
    render(format, &results, |results| print_search_results(results))
}

async fn run_token_command(
    client: &AurCacheClient,
    format: OutputFormat,
    command: TokenCommand,
) -> Result<()> {
    match command {
        TokenCommand::Regenerate => {
            let response = client.regenerate_api_token().await?;

            // Persist the new token to the config file so subsequent CLI
            // invocations keep working without requiring the user to run
            // `config set-token` themselves. If AURCACHE_TOKEN is set, it
            // still takes precedence at runtime, so warn about that too.
            let config = set_token(load_config()?, Some(response.token.clone()))?;
            save_config(&config)?;
            if std::env::var("AURCACHE_TOKEN").is_ok() {
                eprintln!(
                    "Note: the new token was saved to the config file, but the \
                     AURCACHE_TOKEN environment variable is set and will keep \
                     taking precedence over it. Update or unset that environment \
                     variable to actually use the new token."
                );
            }

            match format {
                OutputFormat::Json => print_json(&response),
                OutputFormat::Text => {
                    println!("{}", response.token);
                    Ok(())
                }
            }
        }
    }
}

async fn run_packages_command(
    client: &AurCacheClient,
    format: OutputFormat,
    command: PackagesCommand,
) -> Result<()> {
    match command {
        PackagesCommand::List(args) => render_packages_list(client, format, args).await,
        PackagesCommand::Get { pkgbase } => render_package(client, format, &pkgbase).await,
        PackagesCommand::Add(args) => add_package_command(client, format, args).await,
        PackagesCommand::Update(args) => update_package_command(client, format, args).await,
        PackagesCommand::Patch(args) => patch_package_command(client, format, args).await,
        PackagesCommand::Delete { pkgbase } => {
            delete_package_command(client, format, &pkgbase).await
        }
    }
}

async fn run_builds_command(
    client: &AurCacheClient,
    format: OutputFormat,
    command: BuildsCommand,
) -> Result<()> {
    match command {
        BuildsCommand::List(args) => render_builds_list(client, format, args).await,
        BuildsCommand::Get { build } => render_build(client, format, build).await,
        BuildsCommand::Output(args) => render_build_output(client, format, args).await,
        BuildsCommand::Retry { build } => retry_build_command(client, format, build).await,
        BuildsCommand::Cancel { build } => cancel_build_command(client, format, build).await,
        BuildsCommand::Delete { build } => delete_build_command(client, format, build).await,
        BuildsCommand::Watch(args) => watch_builds_command(client, args).await,
    }
}

async fn run_worker_command(
    client: &AurCacheClient,
    format: OutputFormat,
    command: WorkerCommand,
) -> Result<()> {
    match command {
        WorkerCommand::List => render_workers_list(client, format).await,
        WorkerCommand::Approve { id } => approve_worker_command(client, format, id).await,
        WorkerCommand::Revoke { id } => revoke_worker_command(client, format, id).await,
    }
}

async fn run_raw_command(client: &AurCacheClient, args: RawArgs) -> Result<()> {
    let method = Method::from_bytes(args.method.as_bytes())
        .with_context(|| format!("unsupported HTTP method: {}", args.method))?;
    let body = args.body.as_deref().map(parse_json_body).transpose()?;
    let text = client
        .request_text(method, &args.path, &args.query, body.as_ref())
        .await?;
    if text.trim().is_empty() {
        return Ok(());
    }
    print_raw_response(&text)
}

fn show_config(format: OutputFormat) -> Result<()> {
    let summary = summarize_config(&load_config()?)?;
    match format {
        OutputFormat::Json => print_json(&summary),
        OutputFormat::Text => {
            println!("path: {}", summary.path);
            println!("url: {}", summary.url.unwrap_or_else(|| "-".to_string()));
            println!("token: {}", summary.token_state);
            Ok(())
        }
    }
}

fn save_url_config(format: OutputFormat, url: Option<String>) -> Result<()> {
    let config = set_url(load_config()?, url)?;
    save_config(&config)?;
    match format {
        OutputFormat::Json => print_json(&summarize_config(&config)?),
        OutputFormat::Text => {
            println!("saved url to config");
            Ok(())
        }
    }
}

fn save_token_config(format: OutputFormat, token: Option<String>) -> Result<()> {
    let config = set_token(load_config()?, token)?;
    save_config(&config)?;
    match format {
        OutputFormat::Json => print_json(&summarize_config(&config)?),
        OutputFormat::Text => {
            println!("saved token to config");
            Ok(())
        }
    }
}

async fn render_packages_list(
    client: &AurCacheClient,
    format: OutputFormat,
    args: ListPackagesArgs,
) -> Result<()> {
    let packages = client
        .list_packages(args.limit, args.page, args.all)
        .await?;
    render(format, &packages, |packages| print_package_list(packages))
}

async fn render_package(
    client: &AurCacheClient,
    format: OutputFormat,
    pkgbase: &str,
) -> Result<()> {
    let package = client.get_package(pkgbase).await?;
    render(format, &package, print_package)
}

/// Show what `--from-installed` found, and get a yes before submitting it.
///
/// A machine with a long AUR history produces a long list, and adding it is not
/// a quiet operation: every entry resolves its dependencies and enqueues builds
/// for them. So the list is printed and confirmed rather than acted on from one
/// flag.
fn confirm_bulk_add(packages: &[String], yes: bool) -> Result<()> {
    println!("{} package(s) to add:", packages.len());
    for name in packages {
        println!("  {name}");
    }

    if yes {
        return Ok(());
    }
    if !config::is_interactive() {
        bail!(
            "refusing to add {} package(s) unconfirmed; pass --yes",
            packages.len()
        );
    }

    let proceed = Confirm::new()
        .with_prompt("Add these packages?")
        .default(true)
        .interact()
        .context("failed to read confirmation")?;
    if !proceed {
        bail!("cancelled");
    }
    Ok(())
}

async fn add_package_command(
    client: &AurCacheClient,
    format: OutputFormat,
    args: AddPackageArgs,
) -> Result<()> {
    let packages = if args.from_installed {
        let installed = pacman::installed_foreign_packages()?;
        if installed.is_empty() {
            bail!("`pacman -Qm` reported no foreign packages, so there is nothing to add");
        }
        let packages = pacman::merge_unique(args.packages, installed);
        confirm_bulk_add(&packages, args.yes)?;
        packages
    } else {
        args.packages
    };

    if !args.patches.is_empty() && packages.len() > 1 {
        bail!("--patch can only be used when adding a single package");
    }
    let git_entries = packages.iter().filter(|p| looks_like_git_url(p)).count();
    if git_entries > 0 && args.git_ref.is_none() {
        bail!("--ref is required when adding a git repository URL");
    }
    let git_ref = args.git_ref;

    let patched_files = read_patch_files(&args.patches)?;
    let mut sources = Vec::new();
    for package in packages {
        sources.push(if looks_like_git_url(&package) {
            SourceData::Git {
                spec: GitSourceSpec {
                    url: package,
                    r#ref: git_ref.clone().ok_or_else(|| {
                        anyhow!("--ref is required when adding a git repository URL")
                    })?,
                    subfolder: args.subfolder.clone(),
                },
            }
        } else {
            SourceData::Aur { name: package }
        });
    }

    // A patch belongs to one package's sources, and the bulk endpoint has
    // nowhere to put it, so a patched add stays a single-package add. The
    // argument parser already refuses `--patch` with more than one package.
    if patched_files.is_some() {
        let source = sources
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no package given"))?;
        client
            .add_package(&AddPackageRequest {
                platforms: some_vec(args.platforms.clone()),
                build_flags: some_vec(args.build_flags.clone()),
                source,
                patched_files,
            })
            .await?;
        if format == OutputFormat::Text {
            println!("package add request complete");
        }
        return Ok(());
    }

    // One request for the whole list. Adding them one at a time made the server
    // resolve each AUR name to its pkgbase on its own -- an AUR request per
    // package, before any source was fetched.
    let accepted = client
        .add_packages(&AddPackagesRequest {
            platforms: some_vec(args.platforms.clone()),
            build_flags: some_vec(args.build_flags.clone()),
            sources,
        })
        .await?;

    follow_bulk_add(client, format, accepted).await
}

/// What an import should do about the dump's secrets.
#[derive(Copy, Clone, PartialEq, Eq, Debug, ValueEnum)]
enum Secrets {
    /// Leave this server's CA and tokens alone.
    Ignore,
    /// Take the dump's, replacing what is here.
    Copy,
}

impl Secrets {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ignore => "ignore",
            Self::Copy => "copy",
        }
    }
}

/// What an import should do about a package that is already here.
#[derive(Copy, Clone, PartialEq, Eq, Debug, ValueEnum)]
enum OnExisting {
    /// Leave it alone.
    Skip,
    /// Replace its configuration with the dump's.
    Overwrite,
    /// Leave it alone, but take the dump's patch if it has none. Refuses the
    /// import when both sides carry one.
    MergePatches,
}

impl OnExisting {
    fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Overwrite => "overwrite",
            Self::MergePatches => "merge-patches",
        }
    }
}

/// Restore a dump, reporting each package as the server gets to it.
async fn restore_command(
    client: &AurCacheClient,
    format: OutputFormat,
    archive: &Path,
    dry_run: bool,
    on_existing: OnExisting,
    clear: bool,
    secrets: Secrets,
) -> Result<()> {
    let bytes =
        std::fs::read(archive).with_context(|| format!("failed to read {}", archive.display()))?;

    let accepted = client
        .restore(
            bytes,
            dry_run,
            on_existing.as_str(),
            clear,
            secrets.as_str(),
        )
        .await?;

    // A dry run has nothing to poll: it changed nothing, and what it would have
    // done is already in hand.
    let Some(job_id) = accepted.job_id else {
        if format == OutputFormat::Json {
            println!("{}", serde_json::to_string_pretty(&accepted)?);
        } else {
            if clear {
                println!("would first remove every package, setting and worker here");
            }
            println!("would apply {} package(s):", accepted.total);
            for entry in &accepted.preview {
                println!("  {:<10} {}", outcome_label(&entry.outcome), entry.pkgbase);
                if let RestoreOutcome::Failed { error } = &entry.outcome {
                    println!("             {error}");
                }
            }
        }
        // A dry run that found something blocking exits non-zero, so
        // `restore --dry-run && restore` cannot walk into the failure it was
        // run to discover. Reporting the problem and then reporting success is
        // the one thing a preview must not do.
        let blocked = accepted
            .preview
            .iter()
            .filter(|e| matches!(e.outcome, RestoreOutcome::Failed { .. }))
            .count();
        return restore_result(i32::try_from(blocked).unwrap_or(i32::MAX));
    };

    if format == OutputFormat::Json {
        loop {
            let progress = client.restore_progress(job_id, 0).await?;
            if progress.finished {
                println!("{}", serde_json::to_string_pretty(&progress)?);
                return restore_result(progress.failed);
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    println!("restoring {} package(s), job {job_id}", accepted.total);
    let mut seen = 0_usize;
    loop {
        let progress = client.restore_progress(job_id, seen).await?;
        for entry in &progress.entries {
            println!("  {:<10} {}", outcome_label(&entry.outcome), entry.pkgbase);
            if let RestoreOutcome::Failed { error } = &entry.outcome {
                println!("             {error}");
            }
        }
        seen += progress.entries.len();
        if progress.finished {
            println!(
                "done: {} applied, {} failed, of {}",
                progress.completed, progress.failed, progress.total
            );
            return restore_result(progress.failed);
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

fn outcome_label(outcome: &RestoreOutcome) -> &'static str {
    match outcome {
        RestoreOutcome::Imported => "imported",
        RestoreOutcome::Skipped => "skipped",
        RestoreOutcome::Overwritten => "overwritten",
        RestoreOutcome::PatchAdopted => "patched",
        RestoreOutcome::Failed { .. } => "failed",
    }
}

/// A restore with failures exits non-zero, so a partly-applied dump is not
/// mistaken for a clean one by whatever called it.
fn restore_result(failed: i32) -> Result<()> {
    if failed > 0 {
        bail!("{failed} package(s) could not be restored");
    }
    Ok(())
}

/// Write the server's dump to a file.
///
/// A file rather than stdout by default: it is a `.tar.gz`, and a shell that
/// swallows it into a terminal is a worse default than one that has to be
/// redirected deliberately. `--output -` still writes to stdout for a caller
/// that wants to pipe it.
async fn dump_command(
    client: &AurCacheClient,
    format: OutputFormat,
    output: Option<PathBuf>,
    include_secrets: bool,
) -> Result<()> {
    let bytes = client.dump(include_secrets).await?;

    let path = output.unwrap_or_else(|| {
        PathBuf::from(format!(
            "aurcache-dump-{}.tar.gz",
            Utc::now().format("%Y%m%d")
        ))
    });

    if path.as_os_str() == "-" {
        std::io::stdout()
            .write_all(&bytes)
            .context("failed to write dump to stdout")?;
        return Ok(());
    }

    // A dump with secrets is a credential. Written owner-only, and created that
    // way rather than chmod'ed afterwards -- between the two there is a moment
    // where the CA private key is world-readable.
    write_dump_file(&path, &bytes, include_secrets)
        .with_context(|| format!("failed to write dump to {}", path.display()))?;

    match format {
        OutputFormat::Json => println!(
            "{}",
            serde_json::json!({ "path": path.display().to_string(), "bytes": bytes.len() })
        ),
        OutputFormat::Text => {
            println!("wrote {} ({} bytes)", path.display(), bytes.len());
            if include_secrets {
                println!(
                    "This file contains the CA private key. Anyone holding it can \
                     act as a build worker for this server."
                );
            }
        }
    }
    Ok(())
}

/// Write the archive, owner-only when it carries secrets.
fn write_dump_file(path: &Path, bytes: &[u8], private: bool) -> Result<()> {
    use std::io::Write;
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    open.open(path)?.write_all(bytes)?;
    Ok(())
}

/// Report a bulk add until it finishes.
///
/// The server does not need us here -- the job runs whether or not anything
/// watches -- so this is reporting, not driving. A non-zero exit for failures
/// is what makes it usable from a script.
async fn follow_bulk_add(
    client: &AurCacheClient,
    format: OutputFormat,
    accepted: BulkAddAccepted,
) -> Result<()> {
    if format == OutputFormat::Json {
        // Machine output waits for the end and prints the whole run at once:
        // a stream of partial states is harder to consume than one final
        // document, and the run is what the caller asked about.
        loop {
            let progress = client.bulk_add_progress(accepted.job_id, 0).await?;
            if progress.finished {
                println!("{}", serde_json::to_string_pretty(&progress)?);
                return bulk_add_result(&progress);
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    println!(
        "adding {} package(s), job {}",
        accepted.accepted, accepted.job_id
    );
    let mut seen = 0_usize;
    // Counted here rather than read off the job, which folds "added" and
    // "already present" into one number: a restore wants to know how much of it
    // was already there, and reporting an untouched package as added is a
    // small lie the summary does not need to tell.
    let (mut added, mut present) = (0_usize, 0_usize);
    loop {
        let progress = client.bulk_add_progress(accepted.job_id, seen).await?;
        for entry in &progress.entries {
            match &entry.outcome {
                BulkAddOutcome::Added => {
                    added += 1;
                    println!("  added    {}", entry.name);
                }
                BulkAddOutcome::Existed => {
                    present += 1;
                    println!("  present  {}", entry.name);
                }
                BulkAddOutcome::Failed { error } => {
                    println!("  failed   {}: {error}", entry.name);
                }
            }
        }
        seen += progress.entries.len();
        if progress.finished {
            let mut parts = vec![format!("{added} added")];
            if present > 0 {
                parts.push(format!("{present} already present"));
            }
            parts.push(format!("{} failed", progress.failed));
            println!("done: {}, of {}", parts.join(", "), progress.total);
            return bulk_add_result(&progress);
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// A run with failures exits non-zero, naming how many, so a restore that only
/// partly worked is not mistaken for a clean one by whatever called it.
fn bulk_add_result(progress: &BulkAddProgress) -> Result<()> {
    if progress.failed > 0 {
        bail!("{} package(s) could not be added", progress.failed);
    }
    Ok(())
}

/// Parses a single `--patch` argument, accepting either
/// `SOURCE_PATH=LOCAL_FILE` or just `LOCAL_FILE` (in which case the file's
/// own base name is used as the source path).
fn parse_patch_arg(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((path, file)) => {
            if path.is_empty() {
                return Err("invalid --patch value: SOURCE_PATH must not be empty".to_string());
            }
            Ok((path.to_string(), file.to_string()))
        }
        None => {
            let path = Path::new(s)
                .file_name()
                .ok_or_else(|| format!("invalid --patch value `{s}`"))?
                .to_string_lossy()
                .into_owned();
            Ok((path, s.to_string()))
        }
    }
}

/// Reads the local files referenced by `--patch` arguments into a
/// path -> content map suitable for [`AddPackageRequest::patched_files`].
fn read_patch_files(patches: &[(String, String)]) -> Result<Option<BTreeMap<String, String>>> {
    if patches.is_empty() {
        return Ok(None);
    }
    let mut files = BTreeMap::new();
    for (source_path, local_file) in patches {
        let content = std::fs::read_to_string(local_file)
            .with_context(|| format!("failed to read patch file `{local_file}`"))?;
        if files.insert(source_path.clone(), content).is_some() {
            bail!("--patch specified for `{source_path}` more than once");
        }
    }
    Ok(Some(files))
}

async fn update_package_command(
    client: &AurCacheClient,
    format: OutputFormat,
    args: UpdatePackageArgs,
) -> Result<()> {
    let queued = client
        .update_package(&args.pkgbase, &UpdatePackageRequest { force: args.force })
        .await?;
    render(format, &queued, |numbers| {
        print_queued_builds(&args.pkgbase, numbers);
    })
}

/// The numbers come back bare, so they are printed against the package they
/// belong to — `hello/4`, the same reference every other build command takes.
fn print_queued_builds(pkgbase: &str, numbers: &[i32]) {
    if numbers.is_empty() {
        println!("no builds were queued");
        return;
    }
    println!(
        "queued builds: {}",
        numbers
            .iter()
            .map(|n| format!("{pkgbase}/{n}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
}

async fn patch_package_command(
    client: &AurCacheClient,
    format: OutputFormat,
    args: PatchPackageArgs,
) -> Result<()> {
    let (pkgbase, body) = build_patch_package_request(args)?;
    client.patch_package(&pkgbase, &body).await?;
    print_done_message(format, "package updated");
    Ok(())
}

fn build_patch_package_request(args: PatchPackageArgs) -> Result<(String, PatchPackageRequest)> {
    let body = PatchPackageRequest {
        // `name`, `status`, `out_of_date`, and `latest_build` are internal,
        // server-managed fields (set by the add/build/version-check flows),
        // so they are intentionally not exposed as CLI flags here even
        // though the underlying API technically accepts them.
        name: None,
        status: None,
        out_of_date: None,
        latest_build: None,
        build_flags: some_vec(args.build_flags),
        platforms: some_vec(args.platforms),
    };
    ensure_patch_has_changes(&body)?;
    Ok((args.pkgbase, body))
}

fn ensure_patch_has_changes(body: &PatchPackageRequest) -> Result<()> {
    if body.build_flags.is_none() && body.platforms.is_none() {
        bail!("no changes specified");
    }
    Ok(())
}

async fn delete_package_command(
    client: &AurCacheClient,
    format: OutputFormat,
    pkgbase: &str,
) -> Result<()> {
    client.delete_package(pkgbase).await?;
    print_done_message(format, "package removed");
    Ok(())
}

async fn render_builds_list(
    client: &AurCacheClient,
    format: OutputFormat,
    args: ListBuildsArgs,
) -> Result<()> {
    let builds = client
        .list_builds(args.pkgbase.as_deref(), args.limit, args.page)
        .await?;
    render(format, &builds, |builds| print_build_list(builds))
}

async fn render_build(
    client: &AurCacheClient,
    format: OutputFormat,
    build: BuildRef,
) -> Result<()> {
    let build = client.get_build(&build.pkgbase, build.number).await?;
    render(format, &build, print_build)
}

async fn render_build_output(
    client: &AurCacheClient,
    format: OutputFormat,
    args: BuildOutputArgs,
) -> Result<()> {
    let output = client
        .build_output(&args.build.pkgbase, args.build.number, args.offset)
        .await?;
    match format {
        OutputFormat::Json => print_json(&json!({ "output": output })),
        OutputFormat::Text => {
            print!("{output}");
            Ok(())
        }
    }
}

async fn retry_build_command(
    client: &AurCacheClient,
    format: OutputFormat,
    build: BuildRef,
) -> Result<()> {
    let number = client.retry_build(&build.pkgbase, build.number).await?;
    match format {
        OutputFormat::Json => print_json(&number),
        OutputFormat::Text => {
            println!("enqueued build: {}/{number}", build.pkgbase);
            Ok(())
        }
    }
}

async fn cancel_build_command(
    client: &AurCacheClient,
    format: OutputFormat,
    build: BuildRef,
) -> Result<()> {
    client.cancel_build(&build.pkgbase, build.number).await?;
    print_done_message(format, "build cancelled");
    Ok(())
}

async fn delete_build_command(
    client: &AurCacheClient,
    format: OutputFormat,
    build: BuildRef,
) -> Result<()> {
    client.delete_build(&build.pkgbase, build.number).await?;
    print_done_message(format, "build deleted");
    Ok(())
}

fn print_done_message(format: OutputFormat, message: &str) {
    if format == OutputFormat::Text {
        println!("{message}");
    }
}

async fn render_workers_list(client: &AurCacheClient, format: OutputFormat) -> Result<()> {
    let workers = client.list_workers().await?;
    render(format, &workers, |workers| print_worker_list(workers))
}

async fn approve_worker_command(
    client: &AurCacheClient,
    format: OutputFormat,
    id: i32,
) -> Result<()> {
    client.approve_worker(id).await?;
    print_done_message(format, &format!("worker {id} approved"));
    Ok(())
}

async fn revoke_worker_command(
    client: &AurCacheClient,
    format: OutputFormat,
    id: i32,
) -> Result<()> {
    client.revoke_worker(id).await?;
    print_done_message(format, &format!("worker {id} revoked"));
    Ok(())
}

/// A list as comma-separated text, or a dash when there is nothing in it.
fn or_dash(items: &[String]) -> String {
    if items.is_empty() {
        "-".to_string()
    } else {
        items.join(",")
    }
}

fn print_worker_list(workers: &[Worker]) {
    let rows = workers
        .iter()
        .map(|w| {
            let fp = if w.cert_fingerprint.len() > 16 {
                format!("{}…", &w.cert_fingerprint[..16])
            } else {
                w.cert_fingerprint.clone()
            };
            vec![
                w.id.to_string(),
                w.name.clone(),
                format!("{:?}", w.status).to_lowercase(),
                or_dash(&w.native_arches),
                or_dash(&w.emulated_arches),
                or_dash(&w.package_affinity),
                if w.priority == 0 {
                    // Zero is the default and means "no preference"; printing
                    // it would suggest the fleet had been tuned when it has not.
                    "-".to_string()
                } else {
                    w.priority.to_string()
                },
                w.version.clone().unwrap_or_else(|| "-".to_string()),
                format_timestamp(w.last_seen),
                fp,
            ]
        })
        .collect::<Vec<_>>();
    print_table(
        &[
            "id",
            "name",
            "status",
            "native",
            "emulated",
            "affinity",
            "priority",
            "version",
            "last_seen",
            "fingerprint",
        ],
        &rows,
    );
}

fn parse_json_body(body: &str) -> Result<Value> {
    serde_json::from_str(body).context("invalid JSON passed to --body")
}

fn parse_key_val(input: &str) -> Result<(String, String), String> {
    let (key, value) = input
        .split_once('=')
        .ok_or_else(|| "expected KEY=VALUE".to_string())?;
    if key.is_empty() {
        return Err("query key cannot be empty".to_string());
    }
    Ok((key.to_string(), value.to_string()))
}

fn some_vec<T>(values: Vec<T>) -> Option<Vec<T>> {
    (!values.is_empty()).then_some(values)
}

/// Print `value` as JSON, or hand it to `print_text` for the human-readable form.
fn render<T: Serialize>(
    format: OutputFormat,
    value: &T,
    print_text: impl FnOnce(&T),
) -> Result<()> {
    match format {
        OutputFormat::Json => print_json(value),
        OutputFormat::Text => {
            print_text(value);
            Ok(())
        }
    }
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).context("failed to serialize JSON output")?
    );
    Ok(())
}

fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|header| header.len()).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.len());
        }
    }

    let header = headers
        .iter()
        .enumerate()
        .map(|(index, value)| format!("{value:<width$}", width = widths[index]))
        .collect::<Vec<_>>()
        .join("  ");
    println!("{header}");

    let separator = widths
        .iter()
        .map(|width| "-".repeat(*width))
        .collect::<Vec<_>>()
        .join("  ");
    println!("{separator}");

    for row in rows {
        println!(
            "{}",
            row.iter()
                .enumerate()
                .map(|(index, cell)| format!("{cell:<width$}", width = widths[index]))
                .collect::<Vec<_>>()
                .join("  ")
        );
    }
}

fn print_user_info(user: &UserInfo) {
    println!(
        "username: {}",
        user.username
            .as_deref()
            .unwrap_or("(authentication disabled)")
    );
    println!("has_api_token: {}", user.has_api_token);
}

fn print_stats(stats: &ListStats) {
    println!("total_builds: {}", stats.total_builds);
    println!("successful_builds: {}", stats.successful_builds);
    println!("failed_builds: {}", stats.failed_builds);
    println!("recent_builds: {}", stats.recent_builds);
    println!("recent_successful: {}", stats.recent_successful);
    println!("recent_failed: {}", stats.recent_failed);
    println!("avg_build_time_seconds: {}", stats.avg_build_time);
    println!("repo_size_bytes: {}", stats.repo_size);
    println!("requested_packages: {}", stats.requested_packages);
    println!("dependency_packages: {}", stats.dependency_packages);
    println!("total_build_trend: {:.2}", stats.total_build_trend);
    println!("avg_build_time_trend: {:.2}", stats.avg_build_time_trend);
}

fn print_graph(points: &[GraphDataPoint]) {
    let rows = points
        .iter()
        .map(|point| {
            vec![
                point.year.to_string(),
                format!("{:02}", point.month),
                point.count.to_string(),
                point.successful.to_string(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["year", "month", "count", "successful"], &rows);
}

fn print_search_results(results: &[SearchResult]) {
    let rows = results
        .iter()
        .map(|result| vec![result.name.clone(), result.version.clone()])
        .collect::<Vec<_>>();
    print_table(&["name", "version"], &rows);
}

fn print_package_list(packages: &[SimplePackage]) {
    let rows = packages
        .iter()
        .map(|package| {
            vec![
                package.id.to_string(),
                package.name.clone(),
                build_status_label(package.status).to_string(),
                bool_label(package.directly_requested).to_string(),
                bool_label(package.outofdate != 0).to_string(),
                option_text(package.latest_version.as_deref()),
                option_text(package.upstream_version.as_deref()),
            ]
        })
        .collect::<Vec<_>>();
    print_table(
        &[
            "id",
            "name",
            "status",
            "requested",
            "out_of_date",
            "latest_version",
            "upstream_version",
        ],
        &rows,
    );
}

fn print_package(package: &ExtendedPackage) {
    print_package_summary(package);
    println!();
    print_dependency_section("dependencies", &package.dependencies);
    println!();
    print_dependency_section("dependents", &package.dependents);
}

fn print_package_summary(package: &ExtendedPackage) {
    println!("id: {}", package.id);
    println!("name: {}", package.name);
    println!("directly_requested: {}", package.directly_requested);
    println!("status: {}", build_status_label(package.status));
    println!("out_of_date: {}", package.outofdate != 0);
    println!(
        "latest_version: {}",
        option_text(package.latest_version.as_deref())
    );
    println!(
        "upstream_version: {}",
        option_text(package.upstream_version.as_deref())
    );
    println!(
        "platforms: {}",
        join_or_dash(
            &package
                .selected_platforms
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        )
    );
    let build_flags = package
        .selected_build_flags
        .as_ref()
        .map(|flags| flags.iter().map(String::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    println!("build_flags: {}", join_or_dash(&build_flags));
    println!("source: {}", describe_source(&package.package_source));
    println!(
        "split_packages: {}",
        package
            .split_packages
            .as_ref()
            .map(|packages| join_or_dash(&packages.iter().map(String::as_str).collect::<Vec<_>>()))
            .unwrap_or_else(|| "-".to_string())
    );
}

fn print_dependency_section(title: &str, dependencies: &[PackageDependency]) {
    println!("{title}:");
    if dependencies.is_empty() {
        println!("  -");
        return;
    }
    for dependency in dependencies {
        let state = if dependency.satisfied {
            String::new()
        } else {
            match dependency.built_version.as_deref() {
                Some(built) => format!(" BLOCKING: has {built}"),
                None => " BLOCKING: never built".to_string(),
            }
        };
        println!(
            "  - {} ({}) [id={}]{}",
            dependency.name, dependency.version_constraint, dependency.id, state
        );
    }
}

fn print_build_list(builds: &[Build]) {
    let rows = builds
        .iter()
        .map(|build| {
            vec![
                // The build's public name, the same form the argument parser
                // accepts, so a row can be copied straight into another
                // command. It already names the package, so there is no
                // separate column for that.
                format!("{}/{}", build.pkg_name, build.number),
                build.platform.clone(),
                build_status_label(build.status).to_string(),
                build.version.clone(),
                format_timestamp(build.start_time),
                format_timestamp(build.end_time),
            ]
        })
        .collect::<Vec<_>>();
    print_table(
        &[
            "build",
            "platform",
            "status",
            "version",
            "start_time",
            "end_time",
        ],
        &rows,
    );
}

fn print_build(build: &Build) {
    println!("build: {}/{}", build.pkg_name, build.number);
    println!("platform: {}", build.platform);
    println!("status: {}", build_status_label(build.status));
    println!("version: {}", build.version);
    println!("start_time: {}", format_timestamp(build.start_time));
    println!("end_time: {}", format_timestamp(build.end_time));
}

fn print_raw_response(text: &str) -> Result<()> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).context("failed to pretty-print JSON")?
        );
    } else {
        print!("{text}");
    }
    Ok(())
}

/// Follow builds until they settle, printing transitions and detecting stalls.
///
/// Written for humans watching a queue and for scripts driving one: it reports
/// what changed rather than repeating the current state, and it fails fast when
/// the queue cannot progress instead of waiting out the timeout.
async fn watch_builds_command(client: &AurCacheClient, args: WatchArgs) -> Result<()> {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    let start = Instant::now();
    let mut last_change = Instant::now();
    let mut last_beat = Instant::now();
    // Keyed by the build's public identity, since the row id is no longer
    // part of the API.
    let mut seen: HashMap<(String, i32), i32> = HashMap::new();

    loop {
        let builds: Vec<_> = client
            .list_builds(None, Some(100), None)
            .await?
            .into_iter()
            .filter(|b| args.package.as_ref().is_none_or(|name| &b.pkg_name == name))
            .collect();

        const STATUS_ENQUEUED: i32 = BuildStates::ENQUEUED_BUILD;
        let mut changed = false;
        for build in &builds {
            let key = (build.pkg_name.clone(), build.number);
            if seen.get(&key) != Some(&build.status) {
                if args.fail_on_requeue
                    && seen.get(&key) == Some(&BuildStates::ACTIVE_BUILD)
                    && build.status == STATUS_ENQUEUED
                {
                    bail!(
                        "{}/{} was requeued after running: the server refused the \
                         worker's completion, which will repeat indefinitely",
                        build.pkg_name,
                        build.number
                    );
                }
                let elapsed = start.elapsed().as_secs();
                let reason = build
                    .waiting_reason
                    .as_ref()
                    .map(|r| format!(" — {r}"))
                    .unwrap_or_default();
                println!(
                    "[{elapsed:>4}s] {}/{}: {}{reason}",
                    build.pkg_name,
                    build.number,
                    build_status_label(build.status),
                );
                seen.insert(key, build.status);
                changed = true;
            }
        }
        if changed {
            last_change = Instant::now();
        }

        // Settled when every build has reached a terminal state. An empty list
        // is not settled: the caller may be watching for a build that has not
        // been queued yet.
        let settled = !builds.is_empty()
            && builds.iter().all(|b| {
                b.status == BuildStates::SUCCESSFUL_BUILD || b.status == BuildStates::FAILED_BUILD
            });
        if settled {
            let failed: Vec<&str> = builds
                .iter()
                .filter(|b| b.status == BuildStates::FAILED_BUILD)
                .map(|b| b.pkg_name.as_str())
                .collect();
            if failed.is_empty() {
                println!("all builds succeeded in {}s", start.elapsed().as_secs());
                return Ok(());
            }
            bail!("build failed: {}", failed.join(", "));
        }

        let anything_running = builds.iter().any(|b| b.status == BuildStates::ACTIVE_BUILD);

        if !anything_running && last_change.elapsed() >= Duration::from_secs(args.stall_after) {
            for build in builds
                .iter()
                .filter(|b| b.status != BuildStates::SUCCESSFUL_BUILD)
            {
                let reason = build
                    .waiting_reason
                    .as_ref()
                    .map(|r| format!(" — {r}"))
                    .unwrap_or_default();
                eprintln!(
                    "  {}/{}: {}{reason}",
                    build.pkg_name,
                    build.number,
                    build_status_label(build.status)
                );
            }
            bail!(
                "no progress for {}s and nothing is building; the queue cannot advance",
                last_change.elapsed().as_secs()
            );
        }

        if last_beat.elapsed() >= Duration::from_secs(args.heartbeat) {
            let active = builds
                .iter()
                .filter(|b| b.status == BuildStates::ACTIVE_BUILD)
                .count();
            println!(
                "[{:>4}s] {} building, {} of {} finished",
                start.elapsed().as_secs(),
                active,
                builds
                    .iter()
                    .filter(|b| {
                        b.status == BuildStates::SUCCESSFUL_BUILD
                            || b.status == BuildStates::FAILED_BUILD
                    })
                    .count(),
                builds.len()
            );
            last_beat = Instant::now();
        }

        if start.elapsed() >= Duration::from_secs(args.timeout) {
            bail!("timed out after {}s", args.timeout);
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

fn build_status_label(status: i32) -> &'static str {
    match BuildState::from_i32(status) {
        Some(BuildState::Active) => "active",
        Some(BuildState::Successful) => "successful",
        Some(BuildState::Failed) => "failed",
        Some(BuildState::Enqueued) => "enqueued",
        Some(BuildState::WaitingForDeps) => "waiting for deps",
        None => "unknown",
    }
}

fn bool_label(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn option_text(value: Option<&str>) -> String {
    value
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| "-".to_string())
}

fn join_or_dash(values: &[&str]) -> String {
    if values.is_empty() {
        "-".to_string()
    } else {
        values.join(", ")
    }
}

fn describe_source(source: &PackageSource) -> String {
    match source {
        PackageSource::Aur(aur) => format!("aur ({})", aur.aur_url),
        PackageSource::AurNotFound(_) => "aur (package metadata unavailable)".to_string(),
        PackageSource::Git(git) => {
            format!(
                "git (url={}, ref={}, subfolder={})",
                git.url, git.r#ref, git.subfolder
            )
        }
        PackageSource::Upload(_) => "upload".to_string(),
    }
}

fn format_timestamp(timestamp: Option<i64>) -> String {
    timestamp
        .and_then(|timestamp| DateTime::<Utc>::from_timestamp(timestamp, 0))
        .map(|timestamp| timestamp.to_rfc3339())
        .unwrap_or_else(|| "-".to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        AddPackageArgs, Cli, Command, RepoCommand, SetupCommand, build_status_label, compose,
        parse_key_val, repo,
    };
    use crate::config::ClientConfig;
    use clap::Parser;
    use std::path::PathBuf;

    #[test]
    fn parse_key_val_requires_separator() {
        assert!(parse_key_val("limit=10").is_ok());
        assert!(parse_key_val("missing").is_err());
    }

    #[test]
    fn build_status_label_maps_known_states() {
        assert_eq!(build_status_label(0), "active");
        assert_eq!(build_status_label(1), "successful");
        assert_eq!(build_status_label(2), "failed");
        assert_eq!(build_status_label(3), "enqueued");
        assert_eq!(build_status_label(99), "unknown");
    }

    #[test]
    fn empty_token_can_exist_in_config() {
        let config = ClientConfig {
            url: Some("http://localhost:8080/api".to_string()),
            token: Some(String::new()),
        };
        assert_eq!(config.token.as_deref(), Some(""));
    }

    #[test]
    fn add_aur_accepts_multiple_names() {
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: AddPackageArgs,
        }

        let parsed = Wrapper::parse_from([
            "test",
            "paru",
            "yay",
            "--platform",
            "x86_64",
            "--build-flag=--noconfirm",
        ]);

        assert_eq!(parsed.args.packages, vec!["paru", "yay"]);
        assert_eq!(parsed.args.platforms, vec!["x86_64"]);
        assert_eq!(parsed.args.build_flags, vec!["--noconfirm"]);
    }

    /// `--from-installed` supplies the list, so requiring a positional one too
    /// would make the flag impossible to use on its own.
    #[test]
    fn from_installed_stands_in_for_the_package_list() {
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: AddPackageArgs,
        }

        let parsed = Wrapper::parse_from(["test", "--from-installed", "--yes"]);
        assert!(parsed.args.packages.is_empty());
        assert!(parsed.args.from_installed);
        assert!(parsed.args.yes);
    }

    /// Without it, naming nothing is still a usage error rather than a no-op.
    #[test]
    fn a_bare_add_still_requires_a_package() {
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: AddPackageArgs,
        }

        assert!(Wrapper::try_parse_from(["test"]).is_err());
    }

    #[test]
    fn repo_config_takes_the_publishing_knobs() {
        let cli = Cli::parse_from([
            "aurcache-cli",
            "repo",
            "config",
            "--port",
            "9000",
            "--name",
            "mine",
        ]);
        let Command::Repo {
            command: RepoCommand::Config(args),
        } = cli.command
        else {
            panic!("expected repo config");
        };
        assert_eq!(args.port, Some(9000));
        assert_eq!(args.name.as_deref(), Some("mine"));
    }

    #[test]
    fn completions_name_a_shell() {
        let cli = Cli::parse_from(["aurcache-cli", "completions", "bash"]);
        assert!(matches!(cli.command, Command::Completions { .. }));
        assert!(Cli::try_parse_from(["aurcache-cli", "completions", "smash"]).is_err());
    }

    #[test]
    fn doctor_takes_no_arguments() {
        let cli = Cli::parse_from(["aurcache-cli", "doctor"]);
        assert!(matches!(cli.command, Command::Doctor));
    }

    #[test]
    fn setup_compose_defaults_to_the_bundle() {
        let cli = Cli::parse_from(["aurcache-cli", "setup", "compose"]);
        let Command::Setup { command } = cli.command else {
            panic!("expected setup");
        };
        let SetupCommand::Compose(args) = *command else {
            panic!("expected compose");
        };
        assert_eq!(args.role, compose::ComposeRole::Bundle);
        assert!(!args.force);
    }

    #[test]
    fn setup_worker_collects_repeated_architectures() {
        let cli = Cli::parse_from([
            "aurcache-cli",
            "setup",
            "worker",
            "--dry-run",
            "--arch",
            "aarch64",
            "--arch",
            "armv7h",
        ]);
        let Command::Setup { command } = cli.command else {
            panic!("expected setup");
        };
        let SetupCommand::Worker(args) = *command else {
            panic!("expected worker");
        };
        assert!(args.dry_run);
        assert_eq!(args.worker.arches, vec!["aarch64", "armv7h"]);
    }

    /// Everything after `--` belongs to docker, not to us.
    #[test]
    fn setup_passes_trailing_arguments_through_to_docker() {
        let cli = Cli::parse_from(["aurcache-cli", "setup", "server", "--", "--pull=always"]);
        let Command::Setup { command } = cli.command else {
            panic!("expected setup");
        };
        let SetupCommand::Server(args) = *command else {
            panic!("expected server");
        };
        assert_eq!(args.docker_args, vec!["--pull=always"]);
    }

    #[test]
    fn repo_config_can_install() {
        let cli = Cli::parse_from(["aurcache-cli", "repo", "config", "--install"]);
        let Command::Repo {
            command: RepoCommand::Config(args),
        } = cli.command
        else {
            panic!("expected repo config");
        };
        assert!(args.install);
        assert_eq!(args.pacman_conf, PathBuf::from(repo::PACMAN_CONF));
    }
}

/// A build named the way the rest of the system names it: `<pkgbase>/<number>`.
///
/// Parsed from one argument rather than two so `builds show hello/3` reads the
/// way the UI and the URLs write it. The separator is `/` because a pkgbase
/// cannot contain one — and because `#` would have to be quoted in most
/// shells, and `:` already means an epoch in a version.
#[derive(Clone, Debug)]
struct BuildRef {
    pkgbase: String,
    number: i32,
}

impl std::str::FromStr for BuildRef {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        // Split on the last `/`: everything before it is the pkgbase.
        let (pkgbase, number) = value
            .rsplit_once('/')
            .ok_or_else(|| format!("expected <pkgbase>/<number>, got {value:?}"))?;
        if pkgbase.is_empty() {
            return Err(format!("missing package name in {value:?}"));
        }
        let number = number
            .parse::<i32>()
            .map_err(|_| format!("{number:?} is not a build number"))?;
        if number < 1 {
            return Err("build numbers start at 1".to_string());
        }
        Ok(Self {
            pkgbase: pkgbase.to_string(),
            number,
        })
    }
}

#[cfg(test)]
mod build_ref_tests {
    use super::BuildRef;
    use std::str::FromStr;

    #[test]
    fn a_reference_is_a_package_and_a_number() {
        let parsed = BuildRef::from_str("hello/3").expect("parses");
        assert_eq!(parsed.pkgbase, "hello");
        assert_eq!(parsed.number, 3);
    }

    /// Package names contain the characters the AUR allows, none of which is a
    /// slash — so the last slash is always the separator.
    #[test]
    fn awkward_package_names_survive() {
        for (input, pkgbase) in [
            ("aewm++/1", "aewm++"),
            ("2048.c/12", "2048.c"),
            ("python-3.11/7", "python-3.11"),
            ("1337/2", "1337"),
        ] {
            let parsed = BuildRef::from_str(input).expect(input);
            assert_eq!(parsed.pkgbase, pkgbase, "{input}");
        }
    }

    /// A bare number is the old id form, and silently guessing what it meant
    /// would resolve to a different build than the caller intended.
    #[test]
    fn a_bare_number_is_rejected() {
        assert!(BuildRef::from_str("417").is_err());
        assert!(BuildRef::from_str("hello").is_err());
        assert!(BuildRef::from_str("/3").is_err());
        assert!(BuildRef::from_str("hello/").is_err());
        assert!(BuildRef::from_str("hello/x").is_err());
        // Numbering starts at 1, so 0 is not a build.
        assert!(BuildRef::from_str("hello/0").is_err());
    }
}
