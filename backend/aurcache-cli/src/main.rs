mod config;

use anyhow::{Context, Result, bail};
use aurcache_client::{
    AddPackageRequest, AddPackageSource, AurCacheClient, Build, ExtendedPackage, GraphDataPoint,
    ListStats, Method, PackageDependency, PackageSource, PatchPackageRequest, SearchResult,
    SimplePackage, UpdatePackageRequest, UserInfo,
};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use config::{
    load_config, resolve_runtime_config, save_config, set_token, set_url, summarize_config,
};
use serde::Serialize;
use serde_json::{Value, json};

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
    /// Call an arbitrary API path.
    Raw(RawArgs),
}

#[derive(Subcommand, Debug, Clone)]
enum TokenCommand {
    /// Regenerate the currently authenticated user's API token.
    Regenerate,
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
        /// Package id.
        id: i32,
    },
    /// Add a package.
    Add {
        #[command(subcommand)]
        command: AddPackageCommand,
    },
    /// Trigger an update check for a package.
    Update(UpdatePackageArgs),
    /// Partially update package metadata.
    Patch(PatchPackageArgs),
    /// Remove the direct-request flag from a package.
    Delete {
        /// Package id.
        id: i32,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum AddPackageCommand {
    /// Add a package from the AUR.
    Aur(AddAurPackageArgs),
    /// Add a package from git.
    Git(AddGitPackageArgs),
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
struct AddAurPackageArgs {
    /// AUR package names.
    #[arg(required = true)]
    names: Vec<String>,

    /// Target platform. Repeat for multiple platforms.
    #[arg(long = "platform")]
    platforms: Vec<String>,

    /// Build flag. Repeat for multiple flags.
    #[arg(long = "build-flag")]
    build_flags: Vec<String>,
}

#[derive(Args, Debug, Clone)]
struct AddGitPackageArgs {
    /// Git repository URL.
    #[arg(long)]
    url: String,

    /// Git ref to checkout.
    #[arg(long = "ref")]
    git_ref: String,

    /// Subfolder containing the PKGBUILD.
    #[arg(long, default_value = "")]
    subfolder: String,

    /// Target platform. Repeat for multiple platforms.
    #[arg(long = "platform")]
    platforms: Vec<String>,

    /// Build flag. Repeat for multiple flags.
    #[arg(long = "build-flag")]
    build_flags: Vec<String>,
}

#[derive(Args, Debug, Clone)]
struct UpdatePackageArgs {
    /// Package id.
    id: i32,

    /// Force the update even when the version did not change.
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug, Clone)]
struct PatchPackageArgs {
    /// Package id.
    id: i32,

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
        /// Build id.
        id: i32,
    },
    /// Fetch build output.
    Output(BuildOutputArgs),
    /// Retry a build.
    Retry {
        /// Build id.
        id: i32,
    },
    /// Cancel a build.
    Cancel {
        /// Build id.
        id: i32,
    },
    /// Delete a build.
    Delete {
        /// Build id.
        id: i32,
    },
}

#[derive(Args, Debug, Clone)]
struct ListBuildsArgs {
    /// Optional package id to filter by.
    #[arg(long = "package-id")]
    package_id: Option<i32>,

    /// Maximum number of builds to return.
    #[arg(long)]
    limit: Option<u64>,

    /// Page offset used together with --limit.
    #[arg(long)]
    page: Option<u64>,
}

#[derive(Args, Debug, Clone)]
struct BuildOutputArgs {
    /// Build id.
    id: i32,

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
    match format {
        OutputFormat::Json => print_json(&user),
        OutputFormat::Text => {
            print_user_info(&user);
            Ok(())
        }
    }
}

async fn render_stats(client: &AurCacheClient, format: OutputFormat) -> Result<()> {
    let stats = client.stats().await?;
    match format {
        OutputFormat::Json => print_json(&stats),
        OutputFormat::Text => {
            print_stats(&stats);
            Ok(())
        }
    }
}

async fn render_graph(client: &AurCacheClient, format: OutputFormat) -> Result<()> {
    let points = client.graph().await?;
    match format {
        OutputFormat::Json => print_json(&points),
        OutputFormat::Text => {
            print_graph(&points);
            Ok(())
        }
    }
}

async fn render_search_results(
    client: &AurCacheClient,
    format: OutputFormat,
    query: &str,
) -> Result<()> {
    let results = client.search(query).await?;
    match format {
        OutputFormat::Json => print_json(&results),
        OutputFormat::Text => {
            print_search_results(&results);
            Ok(())
        }
    }
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
        PackagesCommand::Get { id } => render_package(client, format, id).await,
        PackagesCommand::Add { command } => add_package_command(client, format, command).await,
        PackagesCommand::Update(args) => update_package_command(client, format, args).await,
        PackagesCommand::Patch(args) => patch_package_command(client, format, args).await,
        PackagesCommand::Delete { id } => delete_package_command(client, format, id).await,
    }
}

async fn run_builds_command(
    client: &AurCacheClient,
    format: OutputFormat,
    command: BuildsCommand,
) -> Result<()> {
    match command {
        BuildsCommand::List(args) => render_builds_list(client, format, args).await,
        BuildsCommand::Get { id } => render_build(client, format, id).await,
        BuildsCommand::Output(args) => render_build_output(client, format, args).await,
        BuildsCommand::Retry { id } => retry_build_command(client, format, id).await,
        BuildsCommand::Cancel { id } => cancel_build_command(client, format, id).await,
        BuildsCommand::Delete { id } => delete_build_command(client, format, id).await,
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
    match format {
        OutputFormat::Json => print_json(&packages),
        OutputFormat::Text => {
            print_package_list(&packages);
            Ok(())
        }
    }
}

async fn render_package(client: &AurCacheClient, format: OutputFormat, id: i32) -> Result<()> {
    let package = client.get_package(id).await?;
    match format {
        OutputFormat::Json => print_json(&package),
        OutputFormat::Text => {
            print_package(&package);
            Ok(())
        }
    }
}

async fn add_package_command(
    client: &AurCacheClient,
    format: OutputFormat,
    command: AddPackageCommand,
) -> Result<()> {
    match command {
        AddPackageCommand::Aur(args) => add_aur_packages(client, format, args).await?,
        AddPackageCommand::Git(args) => add_git_package(client, args).await?,
    }
    if format == OutputFormat::Text {
        println!("package add request complete");
    }
    Ok(())
}

async fn add_aur_packages(
    client: &AurCacheClient,
    format: OutputFormat,
    args: AddAurPackageArgs,
) -> Result<()> {
    for name in args.names {
        if format == OutputFormat::Text {
            println!("adding package: {name}");
        }
        let body = aur_add_request(name, &args.platforms, &args.build_flags);
        client.add_package(&body).await?;
    }
    Ok(())
}

async fn add_git_package(client: &AurCacheClient, args: AddGitPackageArgs) -> Result<()> {
    let body = AddPackageRequest {
        platforms: some_vec(args.platforms),
        build_flags: some_vec(args.build_flags),
        source: AddPackageSource::Git {
            url: args.url,
            git_ref: args.git_ref,
            subfolder: args.subfolder,
        },
    };
    client.add_package(&body).await
}

fn aur_add_request(
    name: String,
    platforms: &[String],
    build_flags: &[String],
) -> AddPackageRequest {
    AddPackageRequest {
        platforms: some_vec(platforms.to_vec()),
        build_flags: some_vec(build_flags.to_vec()),
        source: AddPackageSource::Aur { name },
    }
}

async fn update_package_command(
    client: &AurCacheClient,
    format: OutputFormat,
    args: UpdatePackageArgs,
) -> Result<()> {
    let updated_ids = client
        .update_package(args.id, &UpdatePackageRequest { force: args.force })
        .await?;
    match format {
        OutputFormat::Json => print_json(&updated_ids),
        OutputFormat::Text => {
            print_updated_package_ids(&updated_ids);
            Ok(())
        }
    }
}

fn print_updated_package_ids(updated_ids: &[i32]) {
    if updated_ids.is_empty() {
        println!("no builds were queued");
        return;
    }
    println!(
        "queued package ids: {}",
        updated_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
}

async fn patch_package_command(
    client: &AurCacheClient,
    format: OutputFormat,
    args: PatchPackageArgs,
) -> Result<()> {
    let body = build_patch_package_request(args)?;
    client.patch_package(body.0, &body.1).await?;
    print_done_message(format, "package updated")
}

fn build_patch_package_request(args: PatchPackageArgs) -> Result<(i32, PatchPackageRequest)> {
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
    Ok((args.id, body))
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
    id: i32,
) -> Result<()> {
    client.delete_package(id).await?;
    print_done_message(format, "package removed")
}

async fn render_builds_list(
    client: &AurCacheClient,
    format: OutputFormat,
    args: ListBuildsArgs,
) -> Result<()> {
    let builds = client
        .list_builds(args.package_id, args.limit, args.page)
        .await?;
    match format {
        OutputFormat::Json => print_json(&builds),
        OutputFormat::Text => {
            print_build_list(&builds);
            Ok(())
        }
    }
}

async fn render_build(client: &AurCacheClient, format: OutputFormat, id: i32) -> Result<()> {
    let build = client.get_build(id).await?;
    match format {
        OutputFormat::Json => print_json(&build),
        OutputFormat::Text => {
            print_build(&build);
            Ok(())
        }
    }
}

async fn render_build_output(
    client: &AurCacheClient,
    format: OutputFormat,
    args: BuildOutputArgs,
) -> Result<()> {
    let output = client.build_output(args.id, args.start_line).await?;
    match format {
        OutputFormat::Json => print_json(&json!({ "output": output })),
        OutputFormat::Text => {
            print!("{output}");
            Ok(())
        }
    }
}

async fn retry_build_command(client: &AurCacheClient, format: OutputFormat, id: i32) -> Result<()> {
    let build_id = client.retry_build(id).await?;
    match format {
        OutputFormat::Json => print_json(&build_id),
        OutputFormat::Text => {
            println!("enqueued build: {build_id}");
            Ok(())
        }
    }
}

async fn cancel_build_command(
    client: &AurCacheClient,
    format: OutputFormat,
    id: i32,
) -> Result<()> {
    client.cancel_build(id).await?;
    print_done_message(format, "build cancelled")
}

async fn delete_build_command(
    client: &AurCacheClient,
    format: OutputFormat,
    id: i32,
) -> Result<()> {
    client.delete_build(id).await?;
    print_done_message(format, "build deleted")
}

fn print_done_message(format: OutputFormat, message: &str) -> Result<()> {
    if format == OutputFormat::Text {
        println!("{message}");
    }
    Ok(())
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
                package.upstream_version.clone(),
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
    println!("upstream_version: {}", package.upstream_version);
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
        println!(
            "  - {} ({}) [id={}]",
            dependency.name, dependency.version_constraint, dependency.id
        );
    }
}

fn print_build_list(builds: &[Build]) {
    let rows = builds
        .iter()
        .map(|build| {
            vec![
                build.id.to_string(),
                build.pkg_id.to_string(),
                build.pkg_name.clone(),
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
            "id",
            "pkg_id",
            "package",
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
    println!("id: {}", build.id);
    println!("pkg_id: {}", build.pkg_id);
    println!("package: {}", build.pkg_name);
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

fn build_status_label(status: i32) -> &'static str {
    match status {
        0 => "active",
        1 => "successful",
        2 => "failed",
        3 => "enqueued",
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
                git.git_url, git.git_ref, git.subfolder
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
    use super::{AddAurPackageArgs, build_status_label, parse_key_val};
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
            args: AddAurPackageArgs,
        }

        let parsed = Wrapper::parse_from([
            "test",
            "paru",
            "yay",
            "--platform",
            "x86_64",
            "--build-flag=--noconfirm",
        ]);

        assert_eq!(parsed.args.names, vec!["paru", "yay"]);
        assert_eq!(parsed.args.platforms, vec!["x86_64"]);
        assert_eq!(parsed.args.build_flags, vec!["--noconfirm"]);
    }
}
