mod config;

use anyhow::{Context, Result, bail};
use aurcache_client::{
    AddPackageRequest, AddPackageSource, AurCacheClient, Build, ExtendedPackage, GraphDataPoint,
    ListStats, Method, PackageDependency, PackageSource, PatchPackageRequest, SearchResult,
    SimplePackage, UpdatePackageRequest, UserInfo, Worker,
};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use config::{
    load_config, resolve_runtime_config, save_config, set_token, set_url, summarize_config,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

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
}

#[derive(Args, Debug, Clone)]
struct AddPackageArgs {
    /// AUR package names and/or git repository URLs. Each entry is treated
    /// as a git URL if it looks like one (contains `@` or a URL scheme like
    /// `https://`), otherwise as an AUR package name.
    #[arg(required = true)]
    packages: Vec<String>,

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

    /// Skip output lines before this index.
    #[arg(long = "start-line")]
    start_line: Option<i32>,
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
        Command::UserInfo => render_user_info(client, format).await,
        Command::Stats => render_stats(client, format).await,
        Command::Graph => render_graph(client, format).await,
        Command::Search { query } => render_search_results(client, format, &query).await,
        Command::Token { command } => run_token_command(client, format, command).await,
        Command::Config { .. } => unreachable!("config commands are handled before client setup"),
        Command::Pkg { command } => run_packages_command(client, format, command).await,
        Command::Builds { command } => run_builds_command(client, format, command).await,
        Command::Worker { command } => run_worker_command(client, format, command).await,
        Command::Raw(args) => run_raw_command(client, args).await,
    }
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
    let packages = client.list_packages(args.limit, args.page).await?;
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

async fn add_package_command(
    client: &AurCacheClient,
    format: OutputFormat,
    args: AddPackageArgs,
) -> Result<()> {
    if !args.patches.is_empty() && args.packages.len() > 1 {
        bail!("--patch can only be used when adding a single package");
    }
    let git_entries = args
        .packages
        .iter()
        .filter(|p| looks_like_git_url(p))
        .count();
    if git_entries > 0 && args.git_ref.is_none() {
        bail!("--ref is required when adding a git repository URL");
    }

    let patched_files = read_patch_files(&args.patches)?;
    for package in args.packages {
        let source = if looks_like_git_url(&package) {
            if format == OutputFormat::Text {
                println!("adding package from git: {package}");
            }
            AddPackageSource::Git {
                url: package,
                git_ref: args.git_ref.clone().expect("checked above"),
                subfolder: args.subfolder.clone(),
            }
        } else {
            if format == OutputFormat::Text {
                println!("adding package: {package}");
            }
            AddPackageSource::Aur { name: package }
        };
        let body = AddPackageRequest {
            platforms: some_vec(args.platforms.clone()),
            build_flags: some_vec(args.build_flags.clone()),
            source,
            patched_files: patched_files.clone(),
        };
        client.add_package(&body).await?;
    }

    if format == OutputFormat::Text {
        println!("package add request complete");
    }
    Ok(())
}

/// Heuristic used to route a `pkg add` entry to the git or AUR source: git
/// URLs either use the SCP-like `user@host:path` shorthand (any user, not
/// just `git`, e.g. `aur@aur.archlinux.org:foo.git`) or an explicit URL
/// scheme (`https://`, `ssh://`, `git://`, ...). AUR package names can't
/// contain `@`, so any entry with one is unambiguously a git remote. A bare
/// `.git` suffix with no scheme/user isn't enough on its own though - AUR
/// package names can legitimately contain one (and there's no local
/// filesystem to resolve a scheme-less path against anyway) - so those fall
/// through to being treated as AUR package names.
fn looks_like_git_url(s: &str) -> bool {
    s.contains('@') || s.contains("://")
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
        .build_output(&args.build.pkgbase, args.build.number, args.start_line)
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
                w.status.clone(),
                if w.native_arches.is_empty() {
                    "-".to_string()
                } else {
                    w.native_arches.clone()
                },
                if w.emulated_arches.is_empty() {
                    "-".to_string()
                } else {
                    w.emulated_arches.clone()
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
    println!("avg_build_time_seconds: {}", stats.avg_build_time);
    println!("repo_size_bytes: {}", stats.repo_size);
    println!("total_packages: {}", stats.total_packages);
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
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["year", "month", "count"], &rows);
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

/// Terminal build states: nothing further will happen to these on its own.
const STATUS_SUCCESS: i32 = 1;
const STATUS_FAILED: i32 = 2;
const STATUS_ACTIVE: i32 = 0;

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

        const STATUS_ENQUEUED: i32 = 3;
        let mut changed = false;
        for build in &builds {
            let key = (build.pkg_name.clone(), build.number);
            if seen.get(&key) != Some(&build.status) {
                if args.fail_on_requeue
                    && seen.get(&key) == Some(&STATUS_ACTIVE)
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
            && builds
                .iter()
                .all(|b| b.status == STATUS_SUCCESS || b.status == STATUS_FAILED);
        if settled {
            let failed: Vec<&str> = builds
                .iter()
                .filter(|b| b.status == STATUS_FAILED)
                .map(|b| b.pkg_name.as_str())
                .collect();
            if failed.is_empty() {
                println!("all builds succeeded in {}s", start.elapsed().as_secs());
                return Ok(());
            }
            bail!("build failed: {}", failed.join(", "));
        }

        let anything_running = builds.iter().any(|b| b.status == STATUS_ACTIVE);

        if !anything_running && last_change.elapsed() >= Duration::from_secs(args.stall_after) {
            for build in builds.iter().filter(|b| b.status != STATUS_SUCCESS) {
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
            let active = builds.iter().filter(|b| b.status == STATUS_ACTIVE).count();
            println!(
                "[{:>4}s] {} building, {} of {} finished",
                start.elapsed().as_secs(),
                active,
                builds
                    .iter()
                    .filter(|b| b.status == STATUS_SUCCESS || b.status == STATUS_FAILED)
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
    match status {
        0 => "active",
        1 => "successful",
        2 => "failed",
        3 => "enqueued",
        4 => "waiting for deps",
        _ => "unknown",
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
    use super::{AddPackageArgs, build_status_label, looks_like_git_url, parse_key_val};
    use crate::config::ClientConfig;
    use clap::Parser;

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

    #[test]
    fn detects_scp_like_git_urls() {
        assert!(looks_like_git_url("aur@aur.archlinux.org:paru"));
        assert!(looks_like_git_url("git@github.com:user/project"));
    }

    #[test]
    fn detects_scheme_git_urls() {
        assert!(looks_like_git_url("https://github.com/user/project"));
    }

    #[test]
    fn does_not_treat_git_like_aur_names_as_urls() {
        assert!(!looks_like_git_url("paru-git"));
        assert!(!looks_like_git_url("lab.git"));
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
