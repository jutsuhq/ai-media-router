//! The `ai-media-router` command line: every operation of `api` as a subcommand, plus the servers.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rmcp::{ServiceExt, transport::stdio};
use serde::{Serialize, de::DeserializeOwned};
use tracing_subscriber::EnvFilter;

use crate::{
    api::*,
    app::App,
    config::Store,
    interfaces::{mcp, rest},
    jobs::{self, Job, Retry, SaveTo, batches, failures::ErrorKind, preflight::DoctorParams},
    library::{assets::ReviewStatus, media::{FrameAt, ReframeMode}, references::Kind},
};

/// Media generation gateway for AI agents: your provider keys, one API, a local asset library.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Data directory (database, assets, price cache, config.toml, providers/).
    #[arg(long, global = true, env = "AI_MEDIA_ROUTER_HOME")]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server over stdio.
    Mcp,
    /// Run the HTTP server: REST under /v1, MCP under /mcp.
    Serve {
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: SocketAddr,
        /// Allow save_spec / reset_spec over HTTP on a non-loopback address.
        #[arg(long)]
        allow_spec_writes: bool,
    },
    /// List providers, or show how to call one.
    Providers { key: Option<String> },
    /// Search the price catalog, cheapest first.
    Prices {
        query: Option<String>,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        res: Option<String>,
        #[arg(long)]
        mode: Option<String>,
        /// `video` or `image`.
        #[arg(long)]
        kind: Option<String>,
        /// Image quality tier, e.g. high.
        #[arg(long)]
        quality: Option<String>,
        /// Include providers without a key.
        #[arg(long)]
        all: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Start a generation.
    Generate {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
        /// Model input as a JSON object.
        #[arg(long, default_value = "{}")]
        input: String,
        /// A saved single-generation recipe used as a preset.
        #[arg(long)]
        recipe: Option<String>,
        /// Recipe variables as a JSON object.
        #[arg(long)]
        vars: Option<String>,
        /// Wait for the result, downloading it into the library.
        #[arg(long)]
        wait: bool,
        /// File the outputs into this collection (created when missing).
        #[arg(long)]
        collection: Option<String>,
        /// File the outputs into this existing reference.
        #[arg(long)]
        reference: Option<String>,
        /// Role of the outputs inside --reference.
        #[arg(long)]
        role: Option<String>,
        /// Count the job against this budget (needs --estimate, --offer or past USD-reported jobs).
        #[arg(long)]
        budget: Option<String>,
        /// Expected cost in USD.
        #[arg(long)]
        estimate: Option<f64>,
        /// offer_id from `prices`; its price × the input's duration or image count becomes the estimate.
        #[arg(long)]
        offer: Option<String>,
        #[command(flatten)]
        retry: RetryArgs,
        #[command(flatten)]
        reuse: ReuseArgs,
        /// Check everything and show the requests without sending or spending anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Check the installation: data directory, database, ffmpeg, keys, specs and their fixtures.
    Doctor {
        /// Also send one GET to every configured provider.
        #[arg(long)]
        online: bool,
    },
    /// Estimate a generation's USD cost from an offer's price and past jobs.
    Estimate {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        model: String,
        /// Model input as a JSON object.
        #[arg(long, default_value = "{}")]
        input: String,
        /// offer_id from `prices`.
        #[arg(long)]
        offer: Option<String>,
        #[command(flatten)]
        reuse: ReuseArgs,
    },
    /// Show one job, or list recent jobs.
    Jobs {
        id: Option<String>,
        /// Poll until the job finishes.
        #[arg(long)]
        wait: bool,
        /// Include the raw request/response of each provider step.
        #[arg(long)]
        debug: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Only jobs that used or filled this reference.
        #[arg(long)]
        reference: Option<String>,
        /// Only jobs that read from or wrote into this collection.
        #[arg(long)]
        collection: Option<String>,
        /// running, done or failed.
        #[arg(long)]
        status: Option<String>,
        /// Only failed jobs of this kind: moderation, auth, quota, rate_limit, invalid_input, not_found,
        /// provider_error, network, timeout, spec, cancelled.
        #[arg(long, value_parser = parse_enum::<ErrorKind>)]
        error_kind: Option<ErrorKind>,
        /// Only jobs of this batch.
        #[arg(long)]
        batch: Option<String>,
    },
    /// Start many jobs at once from a JSON file or string: {defaults, items, matrix, label} (see generate_batch).
    Batch {
        /// The generate_batch parameters as JSON, or @path to read them from a file.
        spec: String,
        /// Wait until every job finishes.
        #[arg(long)]
        wait: bool,
    },
    /// Show a batch: counts per status, its jobs and outputs.
    BatchStatus {
        batch_id: String,
        /// Poll until every job finishes.
        #[arg(long)]
        wait: bool,
    },
    /// Cancel a waiting or running job (the provider is not told); jobs waiting on it fail too.
    Cancel { job_id: String },
    /// Search the asset library.
    Assets {
        query: Option<String>,
        #[arg(long)]
        collection: Option<String>,
        /// draft, approved or rejected.
        #[arg(long, value_parser = parse_enum::<ReviewStatus>)]
        status: Option<ReviewStatus>,
        /// Only assets rated at least this (1-5).
        #[arg(long)]
        min_rating: Option<u8>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Approve, reject, rate or annotate assets.
    Review {
        asset_ids: Vec<String>,
        /// draft, approved or rejected.
        #[arg(long, value_parser = parse_enum::<ReviewStatus>)]
        status: Option<ReviewStatus>,
        /// 1-5.
        #[arg(long)]
        rating: Option<u8>,
        #[arg(long)]
        notes: Option<String>,
    },
    /// Per provider and model: done/failed share, failure kinds, ratings, approvals, cost, speed.
    Stats {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        collection: Option<String>,
    },
    /// Assets similar in meaning to a text or to --asset (needs EMBEDDINGS_API_KEY or EMBEDDINGS_URL).
    Similar {
        query: Option<String>,
        #[arg(long)]
        asset: Option<String>,
        #[arg(long)]
        collection: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Spending caps for generate --budget.
    #[command(subcommand)]
    Budgets(BudgetCommand),
    /// Show, edit, delete one asset or trace its lineage.
    #[command(subcommand)]
    Asset(AssetCommand),
    /// Manage references: named sets of assets used as {{ref:<name>}} in generation input.
    #[command(subcommand)]
    Refs(RefCommand),
    /// Manage collections (projects).
    #[command(subcommand)]
    Collections(CollectionCommand),
    /// Copy a local file into the asset library.
    Import {
        path: PathBuf,
        #[arg(long)]
        tags: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
    },
    /// Upload a library asset to a provider; prints the reference to use in generation input.
    Upload {
        asset_id: String,
        #[arg(long)]
        provider: String,
    },
    /// Read, check, save or reset provider specs.
    #[command(subcommand)]
    Spec(SpecCommand),
    /// Reusable prompt blocks, used as {{snippet:<name>}} inside generation input strings.
    #[command(subcommand)]
    Snippets(SnippetCommand),
    /// Saved generation templates with {{var:<name>}} variables.
    #[command(subcommand)]
    Recipes(RecipeCommand),
    /// Set, remove, list or test provider API keys; `set` and `remove` are CLI-only so a key never travels through MCP or REST.
    #[command(subcommand)]
    Keys(KeysCommand),
    /// List the MCP prompts (workflows and recipes), or print one with --arg name=value.
    Prompts {
        name: Option<String>,
        #[arg(long = "arg", value_name = "NAME=VALUE")]
        args: Vec<String>,
    },
    /// Edit assets locally with ffmpeg; each result is a new asset recorded as a `local` job.
    Media {
        #[command(subcommand)]
        op: MediaCommand,
        /// File the result into this collection (created when missing).
        #[arg(long, global = true)]
        collection: Option<String>,
        /// File the result into this existing reference.
        #[arg(long, global = true)]
        reference: Option<String>,
        /// Role of the result inside --reference.
        #[arg(long, global = true)]
        role: Option<String>,
    },
}

#[derive(Subcommand)]
enum SnippetCommand {
    List,
    /// Create or replace a snippet.
    Save {
        name: String,
        text: String,
        #[arg(long)]
        description: Option<String>,
    },
    Delete { name: String },
}

#[derive(Subcommand)]
enum RecipeCommand {
    List,
    Show { name: String },
    /// Save a recipe from a job (--from-job) and/or explicit fields, or a chain (--steps, --chain).
    Save {
        name: String,
        #[arg(long)]
        from_job: Option<String>,
        /// The job that made this asset, like --from-job.
        #[arg(long)]
        from_asset: Option<String>,
        /// With --from-job / --from-asset: save the jobs that made its inputs as earlier steps.
        #[arg(long)]
        chain: bool,
        /// Steps as a JSON array: [{"name", "provider", "model", "input"} | {"name", "op", "input"}].
        #[arg(long)]
        steps: Option<String>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
        /// Input as a JSON object, with {{var:<name>}} where values change.
        #[arg(long)]
        input: Option<String>,
        /// Offer from search_prices that prices every run.
        #[arg(long)]
        offer: Option<String>,
        /// Budget every run counts against unless the call names another.
        #[arg(long)]
        budget: Option<String>,
        /// Variables as a JSON object: {"name": {"description": "...", "default": ...}}.
        #[arg(long)]
        variables: Option<String>,
    },
    /// Check a recipe run without sending anything: every step, the summed estimate, the budget.
    Check {
        name: String,
        /// Variable values as a JSON object.
        #[arg(long)]
        vars: Option<String>,
        #[arg(long)]
        budget: Option<String>,
    },
    /// Start a generation from a recipe.
    Run {
        name: String,
        /// Variable values as a JSON object.
        #[arg(long)]
        vars: Option<String>,
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        budget: Option<String>,
        #[arg(long)]
        estimate: Option<f64>,
        /// offer_id from `prices`, priced against the filled input.
        #[arg(long)]
        offer: Option<String>,
        #[command(flatten)]
        retry: RetryArgs,
        #[command(flatten)]
        reuse: ReuseArgs,
    },
    Delete { name: String },
}

#[derive(clap::ValueEnum, Clone, Copy)]
enum StoreArg {
    Auto,
    Keychain,
    File,
}

impl From<StoreArg> for Store {
    fn from(a: StoreArg) -> Self {
        match a {
            StoreArg::Auto => Self::Auto,
            StoreArg::Keychain => Self::Keychain,
            StoreArg::File => Self::File,
        }
    }
}

#[derive(Subcommand)]
enum KeysCommand {
    /// Store a provider API key; the value is read from a hidden prompt or stdin, never from an argument.
    Set {
        name: String,
        #[arg(long, value_enum)]
        store: Option<StoreArg>,
    },
    /// Show which provider keys are set, where they come from, and how to set the missing ones.
    List,
    /// Check one provider's key with a free request; spends nothing.
    Test { provider: String },
    /// Remove a key from config.toml and the OS keychain.
    Remove { name: String },
}

#[derive(clap::Args)]
struct RetryArgs {
    /// Resubmit up to N times after a retryable failure (rate_limit, provider_error, network, timeout).
    #[arg(long, value_name = "N")]
    retry: Option<u32>,
    /// Seconds before the first retry (default 30).
    #[arg(long, value_name = "SECS")]
    retry_delay: Option<u64>,
}

impl From<RetryArgs> for Option<Retry> {
    fn from(a: RetryArgs) -> Self {
        a.retry.map(|attempts| Retry { attempts, delay_secs: a.retry_delay })
    }
}

#[derive(clap::Args)]
struct ReuseArgs {
    /// Return the outputs of an identical finished or running job at no cost instead of generating again.
    #[arg(long, conflicts_with = "no_reuse")]
    reuse: bool,
    /// Always submit, even while an identical request started minutes ago is still running.
    #[arg(long)]
    no_reuse: bool,
}

impl From<ReuseArgs> for Option<bool> {
    fn from(a: ReuseArgs) -> Self {
        (a.reuse || a.no_reuse).then_some(a.reuse)
    }
}

#[derive(Subcommand)]
enum MediaCommand {
    /// Save one frame of a video as PNG.
    Frame {
        asset_id: String,
        /// first, last, or seconds from the start.
        #[arg(long, default_value = "first")]
        at: String,
    },
    /// Change the aspect ratio by centered crop or black padding.
    Reframe {
        asset_id: String,
        /// e.g. 9:16, 1:1, 16:9, 1.91.
        aspect: String,
        /// crop (default) or pad.
        #[arg(long, value_parser = parse_enum::<ReframeMode>)]
        mode: Option<ReframeMode>,
    },
    /// Cut a video or audio file to --start..--end seconds.
    Trim {
        asset_id: String,
        #[arg(long)]
        start: Option<f64>,
        #[arg(long)]
        end: Option<f64>,
    },
    /// Join videos in order.
    Concat { asset_ids: Vec<String> },
    /// Draw an image over an image or video.
    Overlay {
        base_asset_id: String,
        overlay_asset_id: String,
        #[arg(long)]
        x: Option<i64>,
        #[arg(long)]
        y: Option<i64>,
        /// Scale the overlay to this width in pixels.
        #[arg(long)]
        width: Option<i64>,
    },
    /// Replace a video's sound with an audio asset (or another video's sound).
    Mux { video_asset_id: String, audio_asset_id: String },
}

fn parse_enum<T: DeserializeOwned>(s: &str) -> Result<T, String> {
    serde_json::from_value(serde_json::Value::String(s.to_owned())).map_err(|e| e.to_string())
}

#[derive(Subcommand)]
enum AssetCommand {
    Show { asset_id: String },
    /// Replace tags and/or description.
    Edit {
        asset_id: String,
        #[arg(long)]
        tags: Option<String>,
        #[arg(long)]
        description: Option<String>,
    },
    /// Delete the file and its library entry.
    Delete { asset_id: String },
    /// What the asset was made from and what was made from it.
    Lineage { asset_id: String },
}

#[derive(Subcommand)]
enum RefCommand {
    List {
        query: Option<String>,
        #[arg(long, value_parser = parse_enum::<Kind>)]
        kind: Option<Kind>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Show { reference: String },
    /// Create or update a reference; --assets replaces its assets.
    Save {
        name: String,
        /// character, style, product, environment, composition or brand (required when creating).
        #[arg(long, value_parser = parse_enum::<Kind>)]
        kind: Option<Kind>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        tags: Option<String>,
        #[arg(long, value_delimiter = ',')]
        assets: Option<Vec<String>>,
    },
    Add {
        reference: String,
        asset_ids: Vec<String>,
        #[arg(long)]
        role: Option<String>,
    },
    Remove { reference: String, asset_ids: Vec<String> },
    Delete { reference: String },
}

#[derive(Subcommand)]
enum CollectionCommand {
    List,
    Add {
        collection: String,
        asset_ids: Vec<String>,
        #[arg(long)]
        description: Option<String>,
    },
    Remove { collection: String, asset_ids: Vec<String> },
    /// Write a ZIP with the files, previews and collection.json into <home>/exports/.
    Export {
        collection: String,
        /// Only assets approved with review.
        #[arg(long)]
        only_approved: bool,
    },
}

#[derive(Subcommand)]
enum BudgetCommand {
    List,
    /// Create a budget or change its limit.
    Set { name: String, limit_usd: f64 },
    Delete { name: String },
}

#[derive(Subcommand)]
enum SpecCommand {
    /// Print a provider's active spec and where it comes from.
    Show { provider: String },
    /// Validate a spec, replay its fixtures and optionally render the requests for a model.
    Check {
        /// Loaded provider to check (or the key fixtures are looked up by, with --file).
        provider: Option<String>,
        /// Candidate spec file (not saved).
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        model: Option<String>,
        /// Generation input as a JSON object.
        #[arg(long)]
        input: Option<String>,
        /// Captured variables for later steps as a JSON object, e.g. {"id":"t1"}.
        #[arg(long)]
        vars: Option<String>,
    },
    /// Save a spec file as an override in <home>/providers/ (active immediately).
    Save { file: PathBuf },
    /// Remove a provider's override.
    Reset { provider: String },
}

/// Parses the command line and runs the command.
pub async fn run() -> Result<()> {
    // stdout belongs to the MCP protocol; all logs go to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let cli = Cli::parse();
    let app = App::init(cli.home)?;

    match cli.command {
        Command::Mcp => {
            tokio::spawn(jobs::run_worker(app.clone()));
            let service = mcp::Server::new(app).serve(stdio()).await?;
            service.waiting().await?;
        }
        Command::Serve { bind, allow_spec_writes } => {
            let token = app.env().get("AI_MEDIA_ROUTER_TOKEN").map(str::to_owned);
            if token.is_none() && !bind.ip().is_loopback() {
                bail!("set AI_MEDIA_ROUTER_TOKEN before listening on a non-loopback address");
            }
            let mut app = app;
            app.spec_writes = bind.ip().is_loopback() || allow_spec_writes;
            tokio::spawn(jobs::run_worker(app.clone()));
            let listener = tokio::net::TcpListener::bind(bind).await?;
            tracing::info!("listening on http://{bind} (REST /v1, MCP /mcp)");
            axum::serve(listener, rest::router(app, token))
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
        Command::Providers { key: None } => print(&list_providers(&app))?,
        Command::Providers { key: Some(provider) } => print(&get_provider(&app, ProviderParams { provider })?)?,
        Command::Prices { query, provider, res, mode, kind, quality, all, limit } => {
            let p = SearchPricesParams { query, provider, res, mode, kind, quality, configured_only: if all { Some(false) } else { None }, limit: Some(limit) };
            print(&search_prices(&app, p).await?)?;
        }
        Command::Estimate { provider, model, input, offer, reuse } => {
            let input = json_arg(&input, "--input")?;
            let p = EstimateCostParams { provider: Some(provider), model: Some(model), input, offer_id: offer, reuse: reuse.into(), ..Default::default() };
            print(&estimate_cost(&app, p).await?)?;
        }
        Command::Generate { provider, model, input, recipe, vars, wait, collection, reference, role, budget, estimate, offer, retry, reuse, dry_run } => {
            let input = json_arg(&input, "--input")?;
            let vars = vars.map(|v| json_arg(&v, "--vars")).transpose()?;
            let save_to = save_to(collection, reference, role);
            let retry = retry.into();
            let reuse = reuse.into();
            let p = GenerateParams {
                provider: provider.unwrap_or_default(),
                model: model.unwrap_or_default(),
                input,
                save_to,
                budget,
                estimated_cost_usd: estimate,
                offer_id: offer,
                retry,
                reuse,
                recipe,
                vars,
            };
            if dry_run {
                let checked = preflight(&app, p.into()).await?;
                print(&checked)?;
                if !checked.ready {
                    std::process::exit(1);
                }
            } else {
                print_job(&app, generate(&app, p).await?, wait).await?;
            }
        }
        Command::Doctor { online } => {
            let report = doctor(&app, DoctorParams { online: Some(online) }).await?;
            print(&report)?;
            if !report.ok {
                std::process::exit(1);
            }
        }
        Command::Jobs { id: Some(id), wait, debug, .. } => {
            print(&jobs::wait(&app, &id, wait_timeout(&app, wait), debug).await?.with_hint())?;
        }
        Command::Jobs { id: None, limit, reference, collection, status, error_kind, batch, .. } => {
            print(&list_jobs(&app, ListJobsParams { limit: Some(limit), reference, collection, status, error_kind, batch })?)?;
        }
        Command::Batch { spec, wait } => {
            let text = match spec.strip_prefix('@') {
                Some(path) => std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?,
                None => spec,
            };
            let p = serde_json::from_str(&text).context("batch spec must be a JSON object {defaults, items, matrix, label}")?;
            let batch = generate_batch(&app, p).await?;
            if wait {
                let mut done = batches::wait(&app, &batch.id, wait_timeout(&app, true)).await?;
                done.refused = batch.refused;
                print(&done)?;
            } else {
                print(&batch)?;
            }
        }
        Command::BatchStatus { batch_id, wait } => print(&batches::wait(&app, &batch_id, wait_timeout(&app, wait)).await?)?,
        Command::Cancel { job_id } => print(&cancel_job(&app, JobParams { job_id })?)?,
        Command::Assets { query, collection, status, min_rating, limit } => {
            print(&search_assets(&app, SearchAssetsParams { query, collection, status, min_rating, limit: Some(limit) })?)?;
        }
        Command::Review { asset_ids, status, rating, notes } => {
            print(&review_asset(&app, ReviewAssetParams { asset_ids, status, rating, notes })?)?;
        }
        Command::Stats { provider, model, collection } => print(&get_stats(&app, StatsParams { provider, model, collection })?)?,
        Command::Similar { query, asset, collection, limit } => {
            let p = SimilarAssetsParams { query, asset_id: asset, collection, limit: Some(limit) };
            print(&similar_assets(&app, p).await?)?;
        }
        Command::Budgets(BudgetCommand::List) => print(&list_budgets(&app)?)?,
        Command::Budgets(BudgetCommand::Set { name, limit_usd }) => print(&set_budget(&app, SetBudgetParams { name, limit_usd })?)?,
        Command::Budgets(BudgetCommand::Delete { name }) => print(&delete_budget(&app, BudgetParams { name })?)?,
        Command::Collections(CollectionCommand::Export { collection, only_approved }) => {
            print(&export_collection(&app, CollectionParams { collection, only_approved: Some(only_approved) }).await?)?;
        }
        Command::Asset(AssetCommand::Show { asset_id }) => print(&get_asset(&app, AssetParams { asset_id }).await?)?,
        Command::Asset(AssetCommand::Edit { asset_id, tags, description }) => {
            print(&update_asset(&app, UpdateAssetParams { asset_id, tags, description })?)?;
        }
        Command::Asset(AssetCommand::Delete { asset_id }) => print(&delete_asset(&app, AssetParams { asset_id })?)?,
        Command::Asset(AssetCommand::Lineage { asset_id }) => print(&get_lineage(&app, AssetParams { asset_id })?)?,
        Command::Refs(RefCommand::List { query, kind, limit }) => {
            print(&list_references(&app, ListReferencesParams { kind, query, limit: Some(limit) })?)?;
        }
        Command::Refs(RefCommand::Show { reference }) => print(&get_reference(&app, ReferenceParams { reference })?)?,
        Command::Refs(RefCommand::Save { name, kind, description, tags, assets }) => {
            let p = SaveReferenceParams { name, kind, description, tags, asset_ids: assets };
            print(&save_reference(&app, p)?)?;
        }
        Command::Refs(RefCommand::Add { reference, asset_ids, role }) => {
            print(&add_to_reference(&app, ReferenceAssetsParams { reference, asset_ids, role })?)?;
        }
        Command::Refs(RefCommand::Remove { reference, asset_ids }) => {
            print(&remove_from_reference(&app, ReferenceAssetsParams { reference, asset_ids, role: None })?)?;
        }
        Command::Refs(RefCommand::Delete { reference }) => print(&delete_reference(&app, ReferenceParams { reference })?)?,
        Command::Collections(CollectionCommand::List) => print(&list_collections(&app)?)?,
        Command::Collections(CollectionCommand::Add { collection, asset_ids, description }) => {
            print(&add_to_collection(&app, CollectionAssetsParams { collection, asset_ids, description })?)?;
        }
        Command::Collections(CollectionCommand::Remove { collection, asset_ids }) => {
            print(&remove_from_collection(&app, CollectionAssetsParams { collection, asset_ids, description: None })?)?;
        }
        Command::Import { path, tags, prompt } => {
            let p = ImportAssetParams { path: path.to_string_lossy().into_owned(), tags, prompt };
            print(&import_asset(&app, p).await?)?;
        }
        Command::Upload { asset_id, provider } => {
            print(&upload_asset(&app, UploadAssetParams { asset_id, provider }).await?)?;
        }
        Command::Spec(SpecCommand::Show { provider }) => print(&get_spec(&app, ProviderParams { provider })?)?,
        Command::Spec(SpecCommand::Check { provider, file, model, input, vars }) => {
            let spec = file.map(|f| std::fs::read_to_string(&f).with_context(|| format!("cannot read {}", f.display())));
            let p = CheckSpecParams {
                provider,
                spec: spec.transpose()?,
                model,
                input: input.map(|i| json_arg(&i, "--input")).transpose()?,
                vars: vars.map(|v| json_arg(&v, "--vars")).transpose()?,
            };
            print(&check_spec(&app, p).await?)?;
        }
        Command::Spec(SpecCommand::Save { file }) => {
            let spec = std::fs::read_to_string(&file).with_context(|| format!("cannot read {}", file.display()))?;
            print(&save_spec(&app, SaveSpecParams { spec }).await?)?;
        }
        Command::Spec(SpecCommand::Reset { provider }) => print(&reset_spec(&app, ProviderParams { provider })?)?,
        Command::Snippets(SnippetCommand::List) => print(&list_snippets(&app)?)?,
        Command::Snippets(SnippetCommand::Save { name, text, description }) => {
            print(&save_snippet(&app, SaveSnippetParams { name, text, description })?)?;
        }
        Command::Snippets(SnippetCommand::Delete { name }) => print(&delete_snippet(&app, NameParams { name })?)?,
        Command::Recipes(RecipeCommand::List) => print(&list_recipes(&app)?)?,
        Command::Recipes(RecipeCommand::Show { name }) => print(&get_recipe(&app, NameParams { name })?)?,
        Command::Recipes(RecipeCommand::Save { name, from_job, from_asset, chain, steps, description, provider, model, input, offer, budget, variables }) => {
            let p = SaveRecipeParams {
                name,
                from_job,
                from_asset,
                chain: Some(chain),
                steps: steps.map(|s| serde_json::from_str(&s).context("--steps must be a JSON array of steps")).transpose()?,
                description,
                provider,
                model,
                input: input.map(|i| json_arg(&i, "--input")).transpose()?,
                save_to: None,
                offer_id: offer,
                budget,
                variables: variables.map(|v| json_arg(&v, "--variables")).transpose()?,
            };
            print(&save_recipe(&app, p)?)?;
        }
        Command::Recipes(RecipeCommand::Check { name, vars, budget }) => {
            let vars = vars.map(|v| json_arg(&v, "--vars")).transpose()?;
            let checked = preflight(&app, PreflightParams { recipe: Some(name), vars, budget, ..Default::default() }).await?;
            print(&checked)?;
            if !checked.ready {
                std::process::exit(1);
            }
        }
        Command::Recipes(RecipeCommand::Run { name, vars, wait, budget, estimate, offer, retry, reuse }) => {
            let vars = vars.map(|v| json_arg(&v, "--vars")).transpose()?;
            let retry = retry.into();
            let reuse = reuse.into();
            let p = RunRecipeParams { name, vars, save_to: None, budget, estimated_cost_usd: estimate, offer_id: offer, retry, reuse };
            print_job(&app, run_recipe(&app, p).await?, wait).await?;
        }
        Command::Recipes(RecipeCommand::Delete { name }) => print(&delete_recipe(&app, NameParams { name })?)?,
        Command::Keys(KeysCommand::Set { name, store }) => {
            let known = known_env_names(&app);
            if !known.contains(&name) {
                bail!("{name} is not a key any loaded provider uses; known: {}", known.into_iter().collect::<Vec<_>>().join(", "));
            }
            let key = read_key(&name)?;
            let stored = crate::config::set_key(&app.home, &name, &key, store.unwrap_or(StoreArg::Auto).into())?;
            app.providers.reload()?;
            let status = list_keys(&app)?.keys.into_iter().find(|k| k.name == name);
            if status.as_ref().is_some_and(|k| k.source.as_deref() == Some("environment")) {
                eprintln!("warning: {name} is also set in the environment; that value wins over the stored one");
            }
            print(&serde_json::json!({ "stored": stored, "status": status }))?;
        }
        Command::Keys(KeysCommand::List) => print(&list_keys(&app)?)?,
        Command::Keys(KeysCommand::Test { provider }) => print(&test_key(&app, ProviderParams { provider }).await?)?,
        Command::Keys(KeysCommand::Remove { name }) => print(&crate::config::remove_key(&app.home, &name)?)?,
        Command::Prompts { name: None, .. } => print(&list_prompts(&app)?)?,
        Command::Prompts { name: Some(name), args } => {
            let arguments = args
                .iter()
                .map(|a| a.split_once('=').map(|(k, v)| (k.to_owned(), v.into())).with_context(|| format!("--arg `{a}`: use NAME=VALUE")))
                .collect::<Result<_>>()?;
            println!("{}", get_prompt(&app, GetPromptParams { name, arguments: Some(arguments) })?.text);
        }
        Command::Media { op, collection, reference, role } => {
            let save_to = save_to(collection, reference, role);
            let edited = match op {
                MediaCommand::Frame { asset_id, at } => {
                    let at = match at.parse::<f64>() {
                        Ok(t) => FrameAt::Seconds(t),
                        Err(_) => FrameAt::Edge(parse_enum(&at).map_err(|_| anyhow::anyhow!("--at is first, last or seconds"))?),
                    };
                    extract_frame(&app, ExtractFrameParams { asset_id, at: Some(at), save_to }).await?
                }
                MediaCommand::Reframe { asset_id, aspect, mode } => reframe(&app, ReframeParams { asset_id, aspect, mode, save_to }).await?,
                MediaCommand::Trim { asset_id, start, end } => trim(&app, TrimParams { asset_id, start, end, save_to }).await?,
                MediaCommand::Concat { asset_ids } => concat(&app, ConcatParams { asset_ids, save_to }).await?,
                MediaCommand::Overlay { base_asset_id, overlay_asset_id, x, y, width } => {
                    overlay(&app, OverlayParams { base_asset_id, overlay_asset_id, x, y, width, save_to }).await?
                }
                MediaCommand::Mux { video_asset_id, audio_asset_id } => {
                    mux_audio(&app, MuxAudioParams { video_asset_id, audio_asset_id, save_to }).await?
                }
            };
            print(&edited)?;
        }
    }
    Ok(())
}

/// With `wait`, runs the worker in this process so polling makes progress; returns the wait timeout.
fn wait_timeout(app: &App, wait: bool) -> Duration {
    if !wait {
        return Duration::ZERO;
    }
    tokio::spawn(jobs::run_worker(app.clone()));
    Duration::MAX
}

/// Prints a started job, or with `wait` the finished one.
async fn print_job(app: &App, job: Job, wait: bool) -> Result<()> {
    let job = if wait { jobs::wait(app, &job.id, wait_timeout(app, true), false).await?.with_hint() } else { job };
    print(&job)
}

fn json_arg<T: DeserializeOwned>(text: &str, flag: &str) -> Result<T> {
    serde_json::from_str(text).with_context(|| format!("{flag} must be a JSON object"))
}

fn save_to(collection: Option<String>, reference: Option<String>, role: Option<String>) -> Option<SaveTo> {
    (collection.is_some() || reference.is_some()).then_some(SaveTo { collection, reference, role })
}

/// Reads the key from a hidden terminal prompt, or all of piped stdin.
fn read_key(name: &str) -> Result<String> {
    use std::io::{IsTerminal, Read};
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        #[cfg(unix)]
        {
            use std::io::Write;
            eprint!("{name} (input hidden): ");
            std::io::stderr().flush().ok();
            let _ = std::process::Command::new("stty").arg("-echo").status();
            let mut line = String::new();
            let read = stdin.read_line(&mut line);
            let _ = std::process::Command::new("stty").arg("echo").status();
            eprintln!();
            read.context("cannot read the key from stdin")?;
            Ok(line.trim_end_matches(['\n', '\r']).to_owned())
        }
        #[cfg(not(unix))]
        {
            bail!("pipe the key in instead, e.g. `Get-Clipboard | ai-media-router keys set {name}`");
        }
    } else {
        let mut buf = String::new();
        stdin.read_to_string(&mut buf).context("cannot read the key from stdin")?;
        Ok(buf.strip_suffix("\r\n").or_else(|| buf.strip_suffix('\n')).unwrap_or(&buf).to_owned())
    }
}

fn print(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
