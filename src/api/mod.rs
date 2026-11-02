//! Operations shared by the MCP tools, the REST API and the CLI. Parameter and result types
//! double as MCP tool schemas, so every interface exposes exactly the same contract.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{
    app::{App, NotFound, default_home},
    config::{self, KEYCHAIN_PREFIX},
    jobs::{self, Job, Retry, SaveTo, batches::{self, Batch}, failures::ErrorKind, preflight::{self, Doctor, DoctorParams, Preflight}, reuse, stats::{self, ModelStats}},
    library::{assets::{self, Asset, ReviewFilter, ReviewStatus}, collections::{self, Collection, Export}, embeddings::{self, Similar}, lineage::{self, Lineage}, media::{self, Edited, FrameAt, Op, ReframeMode}, provenance::Imported, references::{self, Kind, Reference}},
    pricing::{budgets::{self, Budget}, catalog::{self, PriceRow}, costs::{self, Estimate, StepEstimate}},
    providers::{Origin, Provider, Registry, SpecProblem, builtin_spec, exchange::{Exchange, Trace}, fixtures::{self, Fixture, FixtureResult}, oauth, spec::{AuthKind, Spec}},
    templates::{prompts::{self, PromptInfo, PromptText}, recipes::{self, Recipe, Snippet, Step, Variable}},
};

const MAX_WAIT_SECS: u64 = 50;
const DEFAULT_LIMIT: usize = 20;

#[derive(Serialize, JsonSchema)]
pub struct ProviderSummary {
    pub key: String,
    pub name: String,
    pub configured: bool,
    /// Environment variables still to set before this provider can be used.
    pub missing_env: Vec<String>,
    /// `builtin`, or `override` when `<home>/providers/` replaces or adds this spec.
    pub source: String,
}

#[derive(Serialize, JsonSchema)]
pub struct Providers {
    pub providers: Vec<ProviderSummary>,
    /// Override files that failed to load (the built-in spec stays active). Fix them with check_spec.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub spec_errors: Vec<SpecProblem>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProviderParams {
    /// Provider key from list_providers, e.g. "fal".
    pub provider: String,
}

#[derive(Serialize, JsonSchema)]
pub struct ProviderInfo {
    pub key: String,
    pub name: String,
    pub configured: bool,
    pub missing_env: Vec<String>,
    /// What to pass as `model` and `input` for this provider.
    pub model_hint: String,
    /// A complete generate call known to match this spec; adapt model and input.
    pub example: Option<Value>,
    /// Where to look up model ids and input fields.
    pub docs: String,
    /// How references are uploaded: multipart, json, put, post_form, data_uri, or null if not supported.
    pub upload: Option<String>,
    /// How to put an upload_asset result into `input`, when it is not a plain URL.
    pub upload_usage: Option<String>,
    /// Provider names in the price catalog that this provider serves.
    pub catalog: Vec<String>,
    /// Unit of job `cost` for this provider, if known.
    pub cost_unit: Option<String>,
    pub notes: Option<String>,
    /// `builtin` or `override`.
    pub source: String,
    /// Spec file: `providers/<key>.toml` in the repo, or the override path.
    pub spec_path: String,
    /// For an OAuth provider: whether the account is signed in (null otherwise).
    pub signed_in: Option<bool>,
    /// The signed-in account's email (null otherwise).
    pub account: Option<String>,
    /// The sign-in command for an OAuth provider (null otherwise).
    pub login_command: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct KeyStatus {
    /// Environment variable name, e.g. "KIE_API_KEY".
    pub name: String,
    /// Provider keys whose spec reads it.
    pub providers: Vec<String>,
    pub set: bool,
    /// `environment`, `keychain` or `config_file`; null when the key is not set.
    pub source: Option<String>,
    /// `…` plus the last 4 characters; never the value.
    pub masked: Option<String>,
    /// For the user to run in a terminal, e.g. `ai-media-router keys set KIE_API_KEY`.
    pub set_command: String,
    /// For an OAuth session slot: whether the account is signed in (null for plain keys).
    pub signed_in: Option<bool>,
    /// The signed-in account's email (null for plain keys).
    pub account: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct Keys {
    pub keys: Vec<KeyStatus>,
    pub config_file: String,
    /// Things that would hide a key or expose the file, each with its fix.
    pub warnings: Vec<String>,
    pub hint: String,
}

#[derive(Serialize, JsonSchema)]
pub struct KeyTest {
    pub provider: String,
    /// `not_configured`, `not_supported`, `accepted`, `refused` or `inconclusive`.
    pub result: String,
    /// Why a check that could not decide came back this way; the key is redacted.
    pub detail: Option<String>,
    pub hint: String,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct SearchPricesParams {
    /// Substring of the model family or name, e.g. "seedance" or "kling 2.6".
    pub query: Option<String>,
    /// Substring of the provider name, e.g. "kie".
    pub provider: Option<String>,
    /// Resolution, e.g. "720p" or "1080p".
    pub res: Option<String>,
    /// Video: `generate`, `video input`, `motion / avatar`; image: `generate`, `edit`, `generate + edit`.
    pub mode: Option<String>,
    /// `video` (USD per second) or `image` (USD per image); both when omitted, videos first.
    pub kind: Option<String>,
    /// Image quality tier, e.g. "high".
    pub quality: Option<String>,
    /// Only offers from providers whose keys are set. Default: true when any key is set.
    pub configured_only: Option<bool>,
    /// Default 20.
    pub limit: Option<usize>,
}

#[derive(Serialize, JsonSchema)]
pub struct Offer {
    /// Stable id of this offer; pass it to estimate_cost or generate.
    pub offer_id: String,
    /// `video` (priced per second) or `image` (priced per image).
    pub kind: String,
    #[serde(flatten)]
    pub row: PriceRow,
    /// Key to pass as `provider` to generate; null if no spec serves this provider yet.
    pub provider_key: Option<String>,
    pub configured: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct Prices {
    /// Matches before `limit` was applied.
    pub total: usize,
    pub offers: Vec<Offer>,
    pub hint: String,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct GenerateParams {
    /// Provider key, e.g. "kie". See get_provider for what `model` means there. Required unless `recipe` is given.
    #[serde(default)]
    pub provider: String,
    /// The provider's own model id or endpoint, e.g. "kling-2.6/image-to-video" on kie. Required unless `recipe` is given.
    #[serde(default)]
    pub model: String,
    /// Model input passed to the provider unchanged (prompt, image_urls, duration, ...). Library files go in
    /// as placeholders, uploaded through the provider before submit: "{{asset:<id>}}" or "{{ref:<name>}}" as
    /// a whole string value. In an array a ref expands to all its assets (`"image_urls": ["{{ref:hero}}"]`);
    /// as a single value it is the ref's first asset. "{{job:<id>}}" (all outputs in an array, else the first)
    /// and "{{job:<id>:<n>}}" (output n, from 0) chain jobs, "{{job:<id>:last_frame}}" / "first_frame" take a frame of
    /// its video: while that job runs, this one waits. Text placeholders work inside any string:
    /// "{{snippet:<name>}}", "{{ref:<name>.description}}", "{{ref:<name>.tags}}".
    pub input: Map<String, Value>,
    /// A saved single-generation recipe used as a preset: its provider, model, input (variables filled,
    /// defaults applied), save_to, offer_id and budget are the defaults. Fields given here win; `input` merges
    /// over the recipe input field by field.
    pub recipe: Option<String>,
    /// The recipe's variables, as in run_recipe.
    pub vars: Option<Map<String, Value>>,
    /// File the outputs into a collection and/or an existing reference when the job finishes.
    pub save_to: Option<SaveTo>,
    /// Count this job against a budget (set_budget); the job is refused if it would exceed it.
    pub budget: Option<String>,
    /// Expected cost in USD. With `budget` and no value, it is computed like estimate_cost.
    pub estimated_cost_usd: Option<f64>,
    /// Offer from search_prices this job buys; its price × the duration or image count in `input` becomes
    /// the job's estimate when `estimated_cost_usd` is not given.
    #[serde(default)]
    pub offer_id: Option<String>,
    /// Resubmit after a retryable failure (rate_limit, provider_error, network, timeout): {attempts: 1-5, delay_secs}.
    /// Off by default, since a provider may bill a failed attempt.
    pub retry: Option<Retry>,
    /// true: when a job with the same provider, model and input (library files compared by content) is done and
    /// its outputs are still in the library, or is still running, return those outputs as a new job at no cost.
    /// Off by default, since a repeat is usually wanted as a new variant. Even without it, an identical request
    /// made while the first one runs (started under 5 minutes ago) returns that job; false always submits.
    pub reuse: Option<bool>,
}

/// generate's arguments, or a saved recipe with its variables.
#[derive(Deserialize, JsonSchema, Default)]
pub struct PreflightParams {
    /// As in generate; required unless `recipe` is given.
    pub provider: Option<String>,
    /// As in generate; required unless `recipe` is given.
    pub model: Option<String>,
    /// As in generate.
    #[serde(default)]
    pub input: Map<String, Value>,
    pub save_to: Option<SaveTo>,
    /// With a recipe of steps, the sum of every step's estimate is checked against it.
    pub budget: Option<String>,
    pub estimated_cost_usd: Option<f64>,
    pub offer_id: Option<String>,
    pub retry: Option<Retry>,
    /// As in generate: report (with cost 0) when the run would return an identical job's outputs.
    pub reuse: Option<bool>,
    /// Check a run_recipe instead: every step of the recipe, their summed estimate and the budget for the sum.
    pub recipe: Option<String>,
    /// The recipe's variables, as in run_recipe.
    pub vars: Option<Map<String, Value>>,
}

impl From<GenerateParams> for PreflightParams {
    fn from(p: GenerateParams) -> Self {
        Self {
            provider: (!p.provider.is_empty()).then_some(p.provider),
            model: (!p.model.is_empty()).then_some(p.model),
            input: p.input,
            save_to: p.save_to,
            budget: p.budget,
            estimated_cost_usd: p.estimated_cost_usd,
            offer_id: p.offer_id,
            retry: p.retry,
            reuse: p.reuse,
            recipe: p.recipe,
            vars: p.vars,
        }
    }
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct EstimateCostParams {
    /// Provider key, e.g. "kie". Required unless `recipe` is given.
    pub provider: Option<String>,
    /// The provider's model id, as passed to generate. Required unless `recipe` is given.
    pub model: Option<String>,
    /// Generation input. Seconds are read from duration, duration_seconds, duration_secs, seconds, video_length
    /// or length; the output count from num_images, n, num_outputs, image_count, number_of_images, sample_count,
    /// batch_size, num_videos or count (numbers or strings like "5s").
    #[serde(default)]
    pub input: Map<String, Value>,
    /// Offer from search_prices to price; without it only the history median is used.
    pub offer_id: Option<String>,
    /// As in generate: report (with cost 0) when the run would return an identical job's outputs.
    pub reuse: Option<bool>,
    /// Price a run_recipe instead: each step from its own estimated_cost_usd or offer_id (edits cost 0), and the sum.
    pub recipe: Option<String>,
    /// The recipe's variables, as in run_recipe.
    pub vars: Option<Map<String, Value>>,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct GenerateBatchParams {
    /// Shared by every job; items override provider, model, save_to and estimated_cost_usd, and merge
    /// their input over this input.
    pub defaults: Option<BatchDefaults>,
    /// One entry per variant, e.g. the same prompt on several provider/model pairs (a bench).
    pub items: Option<Vec<BatchItem>>,
    /// Top-level input fields to vary: {"aspect_ratio": ["16:9", "9:16"], "seed": [1, 2]}. Every item runs
    /// with every combination. At most 100 jobs in total.
    pub matrix: Option<BTreeMap<String, Vec<Value>>>,
    /// Free text to recognise the batch by.
    pub label: Option<String>,
    /// Run a saved recipe: its provider, model, input and save_to are the defaults, and `matrix` keys,
    /// `defaults.vars` and item `vars` fill its "{{var:<name>}}" variables instead of input fields.
    pub recipe: Option<String>,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct BatchDefaults {
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Same placeholders as generate, including "{{job:<id>}}".
    pub input: Option<Map<String, Value>>,
    pub save_to: Option<SaveTo>,
    /// Budget for the whole batch; the sum of all estimates is admitted before anything is submitted.
    pub budget: Option<String>,
    /// USD per job, unless an item gives its own.
    pub estimated_cost_usd: Option<f64>,
    /// Offer from search_prices for every job; each job's estimate is computed from it and its input.
    pub offer_id: Option<String>,
    /// Recipe variables shared by every job (with `recipe`).
    pub vars: Option<Map<String, Value>>,
    /// Every job: resubmit after a retryable failure (rate_limit, provider_error, network, timeout): {attempts: 1-5, delay_secs}.
    /// Off by default, since a provider may bill a failed attempt.
    pub retry: Option<Retry>,
    /// Every job: return the outputs of an identical finished or running job at no cost, as in generate.
    /// Identical items of one batch are never merged unless this is true.
    pub reuse: Option<bool>,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct BatchItem {
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Merged over defaults.input, field by field.
    pub input: Option<Map<String, Value>>,
    pub estimated_cost_usd: Option<f64>,
    pub offer_id: Option<String>,
    /// Recipe variables of this item, over defaults.vars (with `recipe`).
    pub vars: Option<Map<String, Value>>,
    pub save_to: Option<SaveTo>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetBatchParams {
    pub batch_id: String,
    /// Block up to this many seconds (max 50) for every job to finish. Default 0.
    pub wait_secs: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct JobParams {
    pub job_id: String,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct GetJobParams {
    pub job_id: String,
    /// Block up to this many seconds (max 50) for the job to finish. Default 0.
    pub wait_secs: Option<u64>,
    /// Include `trace`: the exact request and raw provider response of each step. Use it when a job
    /// failed or stays running, to see which pointer or URL in the spec is wrong.
    pub debug: Option<bool>,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct ListJobsParams {
    /// Default 20.
    pub limit: Option<usize>,
    /// Only jobs that used this reference (name or id) as input or filed outputs into it.
    pub reference: Option<String>,
    /// Only jobs that read from or wrote into this collection.
    pub collection: Option<String>,
    /// `running`, `done` or `failed`.
    pub status: Option<String>,
    /// Only failed jobs of this kind, e.g. "moderation" or "rate_limit".
    pub error_kind: Option<ErrorKind>,
    /// Only jobs of this batch (generate_batch).
    pub batch: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct Jobs {
    pub jobs: Vec<Job>,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct SearchAssetsParams {
    /// Words matched against the generation prompt, tags, description and review notes; omit to list the newest assets.
    pub query: Option<String>,
    /// Only assets in this collection.
    pub collection: Option<String>,
    /// Only assets with this review status.
    pub status: Option<ReviewStatus>,
    /// Only assets rated at least this (1-5).
    pub min_rating: Option<u8>,
    /// Default 20.
    pub limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct SimilarAssetsParams {
    /// Text to find similar assets for, e.g. "red-haired woman in neon rain". Or use asset_id.
    pub query: Option<String>,
    /// Find assets similar to this one.
    pub asset_id: Option<String>,
    /// Only assets in this collection.
    pub collection: Option<String>,
    /// Default 20.
    pub limit: Option<usize>,
}

#[derive(Serialize, JsonSchema)]
pub struct SimilarAssets {
    /// Most similar first.
    pub assets: Vec<Similar>,
}

#[derive(Deserialize, JsonSchema)]
pub struct CollectionParams {
    pub collection: String,
    /// Only assets approved with review_asset.
    pub only_approved: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ReviewAssetParams {
    pub asset_ids: Vec<String>,
    /// `approved` (ready to use), `rejected` (skipped by {{ref:…}} expansion) or `draft`.
    pub status: Option<ReviewStatus>,
    /// 1-5.
    pub rating: Option<u8>,
    /// What is right or wrong with it, e.g. "extra fingers, logo warped"; replaces earlier notes, searched by search_assets.
    pub notes: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct StatsParams {
    /// Provider key, e.g. "kie".
    pub provider: Option<String>,
    /// The provider's model id.
    pub model: Option<String>,
    /// Only jobs that read from or wrote into this collection.
    pub collection: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct Stats {
    /// Per provider and model, most used first.
    pub models: Vec<ModelStats>,
    pub hint: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SetBudgetParams {
    /// Budget name, e.g. a session or project; passed as `budget` to generate.
    pub name: String,
    /// Spending cap in USD.
    pub limit_usd: f64,
}

#[derive(Deserialize, JsonSchema)]
pub struct BudgetParams {
    pub name: String,
}

#[derive(Serialize, JsonSchema)]
pub struct Budgets {
    pub budgets: Vec<Budget>,
}

#[derive(Deserialize, JsonSchema)]
pub struct AssetParams {
    pub asset_id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct UpdateAssetParams {
    pub asset_id: String,
    /// Replaces the tags (space-separated).
    pub tags: Option<String>,
    /// Replaces the description; searched like prompt and tags.
    pub description: Option<String>,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct SaveReferenceParams {
    /// Unique name, used as "{{ref:<name>}}" in generate input, e.g. "main-character".
    pub name: String,
    /// Required when creating: character, style, product, environment, composition or brand.
    pub kind: Option<Kind>,
    /// What must stay consistent, e.g. "red-haired woman, cyberpunk jacket".
    pub description: Option<String>,
    /// Space-separated tags.
    pub tags: Option<String>,
    /// When given, the reference holds exactly these assets in this order (first = default image).
    pub asset_ids: Option<Vec<String>>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ReferenceParams {
    /// Reference name or id.
    pub reference: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ListReferencesParams {
    pub kind: Option<Kind>,
    /// Substring of name, description or tags.
    pub query: Option<String>,
    /// Default 20.
    pub limit: Option<usize>,
}

#[derive(Serialize, JsonSchema)]
pub struct References {
    pub references: Vec<Reference>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ReferenceAssetsParams {
    /// Reference name or id.
    pub reference: String,
    pub asset_ids: Vec<String>,
    /// add_to_reference only: role of these assets, e.g. "front", "side", "palette".
    pub role: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct CollectionAssetsParams {
    /// Collection name; add_to_collection creates it when missing.
    pub collection: String,
    pub asset_ids: Vec<String>,
    /// add_to_collection only: replaces the collection's description.
    pub description: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct Collections {
    pub collections: Vec<Collection>,
}

#[derive(Serialize, JsonSchema)]
pub struct Assets {
    pub assets: Vec<Asset>,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct ImportAssetParams {
    /// Absolute path of a local file.
    pub path: String,
    /// Space-separated tags, e.g. "character hero".
    pub tags: Option<String>,
    /// Description used for search; defaults to the prompt stored in the file by ai-media-router.
    pub prompt: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct UploadAssetParams {
    pub asset_id: String,
    /// Provider key, e.g. "kie".
    pub provider: String,
}

#[derive(Serialize, JsonSchema)]
pub struct Uploaded {
    /// The value to put into generation `input` (URL, data URI, or provider file id).
    pub reference: Value,
    /// `url`, `data_uri` or `id`.
    pub kind: String,
    /// Where `reference` goes in `input`.
    pub usage: String,
}

#[derive(Serialize, JsonSchema)]
pub struct SpecText {
    pub key: String,
    /// `builtin` or `override`.
    pub source: String,
    pub path: String,
    /// The active spec (TOML). Edit it and pass it to check_spec, then save_spec.
    pub text: String,
    /// The built-in spec, when an override replaces it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub builtin_text: Option<String>,
    /// Recorded request/response fixtures replayed by check_spec.
    pub fixtures: Vec<String>,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct CheckSpecParams {
    /// Check the loaded spec of this provider. Omit when passing `spec`.
    pub provider: Option<String>,
    /// TOML text of a candidate spec (not saved). Format: docs/provider-spec.md.
    pub spec: Option<String>,
    /// With `model`, also render the exact HTTP requests a generation would send (nothing is sent).
    pub model: Option<String>,
    /// Generation input for the rendered requests.
    pub input: Option<Map<String, Value>>,
    /// Values for variables later steps capture from earlier responses (e.g. {"id": "task-123"}).
    pub vars: Option<Map<String, Value>>,
}

#[derive(Serialize, JsonSchema)]
pub struct SpecCheck {
    pub key: Option<String>,
    pub valid: bool,
    /// Parse or validation error (TOML line/column, field, pointer or template variable).
    pub error: Option<String>,
    pub fixtures: Vec<FixtureResult>,
    /// Rendered requests, credentials shown as `<ENV_NAME>`; only when `model` is given.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub requests: Vec<Exchange>,
    pub hint: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SaveSpecParams {
    /// Full TOML text of the spec. Saved as `<home>/providers/<key>.toml`, active immediately in every process.
    pub spec: String,
}

#[derive(Serialize, JsonSchema)]
pub struct SpecSaved {
    pub key: String,
    pub path: String,
    /// What the new file replaced: `builtin`, `override` or `nothing` (new provider).
    pub replaced: String,
    pub check: SpecCheck,
}

#[derive(Serialize, JsonSchema)]
pub struct SpecReset {
    pub key: String,
    /// The override file that was removed.
    pub removed: String,
    /// `builtin` if a built-in spec is active again, `none` if the provider no longer exists.
    pub source: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
pub struct ExtractFrameParams {
    /// A video asset.
    pub asset_id: String,
    /// "first", "last", or seconds from the start (e.g. 1.5). Default "first".
    pub at: Option<FrameAt>,
    /// File the new asset into a collection and/or reference.
    #[serde(skip_serializing)]
    pub save_to: Option<SaveTo>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ReframeParams {
    /// An image or video asset.
    pub asset_id: String,
    /// Target aspect ratio "W:H", e.g. "9:16", "1:1", "16:9", or a number like "1.91".
    pub aspect: String,
    /// `crop` (default): cut the edges away, centered. `pad`: add black bars.
    pub mode: Option<ReframeMode>,
    #[serde(skip_serializing)]
    pub save_to: Option<SaveTo>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
pub struct TrimParams {
    /// A video or audio asset.
    pub asset_id: String,
    /// Seconds; default 0.
    pub start: Option<f64>,
    /// Seconds; default the end of the file.
    pub end: Option<f64>,
    #[serde(skip_serializing)]
    pub save_to: Option<SaveTo>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ConcatParams {
    /// Two or more video assets in playback order. Output takes the first clip's size (others are
    /// letterboxed) at 30 fps; clips without sound get silence when any clip has sound.
    pub asset_ids: Vec<String>,
    #[serde(skip_serializing)]
    pub save_to: Option<SaveTo>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
pub struct OverlayParams {
    /// Image or video to draw on (e.g. a generated plate).
    pub base_asset_id: String,
    /// Image drawn on top (e.g. a real packshot or a logo; PNG transparency is kept).
    pub overlay_asset_id: String,
    /// Left edge in pixels of the base (get_asset shows its width and height). Default 0.
    pub x: Option<i64>,
    /// Top edge in pixels of the base. Default 0.
    pub y: Option<i64>,
    /// Scale the overlay to this width in pixels, keeping its aspect. Default: its own size.
    pub width: Option<i64>,
    #[serde(skip_serializing)]
    pub save_to: Option<SaveTo>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct MuxAudioParams {
    pub video_asset_id: String,
    /// An audio asset, or a video whose sound is taken. Replaces the video's sound; padded with
    /// silence or cut to the video's length.
    pub audio_asset_id: String,
    #[serde(skip_serializing)]
    pub save_to: Option<SaveTo>,
}

pub fn list_providers(app: &App) -> Providers {
    let registry = app.registry();
    let providers = registry
        .iter()
        .map(|p| ProviderSummary {
            key: p.spec.key.clone(),
            name: p.spec.name.clone(),
            missing_env: p.missing_env(),
            configured: p.configured(),
            source: p.origin.kind().to_owned(),
        })
        .collect();
    Providers { providers, spec_errors: registry.problems.clone() }
}

pub fn get_provider(app: &App, p: ProviderParams) -> Result<ProviderInfo> {
    let provider = app.find_provider(&p.provider)?;
    let s = &provider.spec;
    let (signed_in, account, login_command) = if s.auth.kind == AuthKind::Oauth {
        let name = s.auth.env.first().map(String::as_str).unwrap_or("");
        let status = oauth::status(app.env().get(name));
        (Some(status.signed_in), status.email, Some(login_command(app, &s.key)))
    } else {
        (None, None, None)
    };
    Ok(ProviderInfo {
        key: s.key.clone(),
        name: s.name.clone(),
        configured: provider.configured(),
        missing_env: provider.missing_env(),
        model_hint: s.model_hint.clone(),
        example: example(app, &provider),
        docs: s.docs.clone(),
        upload: s.upload.as_ref().map(|u| u.kind.as_str().to_owned()),
        upload_usage: s.upload.as_ref().and_then(|u| u.usage.clone()),
        catalog: s.catalog.clone(),
        cost_unit: s.cost_unit.clone(),
        notes: s.notes.clone(),
        source: provider.origin.kind().to_owned(),
        spec_path: provider.origin.path(),
        signed_in,
        account,
        login_command,
    })
}

/// Every env var any loaded spec reads (`[auth]`, each route's auth, `base_url`), sorted.
pub fn known_env_names(app: &App) -> BTreeSet<String> {
    providers_reading_keys(&app.registry()).into_keys().collect()
}

/// Env var -> the provider keys whose spec reads it; both sorted.
fn providers_reading_keys(registry: &Registry) -> BTreeMap<String, Vec<String>> {
    let mut uses: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in registry.iter() {
        let mut names: BTreeSet<&str> = p.spec.auth.env.iter().map(String::as_str).collect();
        for route in &p.spec.routes {
            if let Some(auth) = &route.auth {
                names.extend(auth.env.iter().map(String::as_str));
            }
        }
        names.extend(p.spec.base_url_env());
        for name in names {
            uses.entry(name.to_owned()).or_default().push(p.spec.key.clone());
        }
    }
    uses
}

/// The provider key whose oauth `[auth] env` slot is `name`, if any.
fn oauth_provider_key(app: &App, name: &str) -> Option<String> {
    app.registry()
        .iter()
        .find(|p| p.spec.auth.kind == AuthKind::Oauth && p.spec.auth.env.iter().any(|e| e == name))
        .map(|p| p.spec.key.clone())
}

/// The sign-in command for an OAuth provider, with `--home` like the other hints.
fn login_command(app: &App, key: &str) -> String {
    let mut command = format!("ai-media-router login {key}");
    if default_home().as_deref() != Some(app.home.as_path()) {
        command.push_str(&format!(" --home '{}'", app.home.display()));
    }
    command
}

/// The `keys set` command for `name`, with `--home` when this process uses a non-default directory.
/// An OAuth session slot gets its `login` command instead.
pub fn set_command(app: &App, name: &str) -> String {
    if let Some(key) = oauth_provider_key(app, name) {
        return login_command(app, &key);
    }
    let mut command = format!("ai-media-router keys set {name}");
    if default_home().as_deref() != Some(app.home.as_path()) {
        command.push_str(&format!(" --home '{}'", app.home.display()));
    }
    command
}

/// Which keys exist, whether each is set and how to set it; never a key value.
pub fn list_keys(app: &App) -> Result<Keys> {
    let env = app.env();
    let raw = config::raw_env(&app.home);
    let config_file = app.home.join("config.toml");
    let mut warnings = Vec::new();
    if config_file.exists() && config::loose_permissions(&config_file) {
        warnings.push(format!("chmod 600 {}", config_file.display()));
    }
    let keys = providers_reading_keys(&app.registry())
        .into_iter()
        .map(|(name, providers)| {
            let value = env.get(&name);
            let command = set_command(app, &name);
            let marker = raw.get(&name).filter(|v| v.starts_with(KEYCHAIN_PREFIX));
            if value.is_none() && marker.is_some() {
                warnings.push(format!("{name}: keychain entry missing; run {command}"));
            }
            let source = if std::env::var_os(&name).is_some_and(|v| !v.is_empty()) {
                Some("environment")
            } else if marker.is_some() {
                Some("keychain")
            } else if raw.contains_key(&name) {
                Some("config_file")
            } else {
                None
            };
            let is_oauth = oauth_provider_key(app, &name).is_some();
            let (signed_in, account) = if is_oauth {
                let status = oauth::status(value);
                (Some(status.signed_in), status.email)
            } else {
                (None, None)
            };
            KeyStatus {
                name,
                providers,
                set: value.is_some(),
                source: source.map(str::to_owned),
                masked: if is_oauth { None } else { value.map(config::mask) },
                set_command: command,
                signed_in,
                account,
            }
        })
        .collect();
    let store = config::keychain_name().unwrap_or("config.toml");
    let hint = format!(
        "Keys are added by the user in a terminal, never through this conversation: give them set_command. The value is \
         read from a hidden prompt and stored in the {store}. Changes apply without a restart. test_key {{provider}} \
         checks a key without spending."
    );
    Ok(Keys { keys, config_file: config_file.display().to_string(), warnings, hint })
}

#[derive(Serialize, JsonSchema)]
pub struct LoginResult {
    pub provider: String,
    pub email: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct LogoutResult {
    pub provider: String,
    pub signed_out: bool,
}

/// Signs an OAuth provider in. CLI-only: it opens a browser and listens on a loopback port, so it
/// cannot run inside MCP or REST.
pub async fn login(app: &App, provider: &str, open_browser: bool, store: config::Store) -> Result<LoginResult> {
    let spec = app.find_provider(provider)?.spec.clone();
    let account = oauth::login(&app.home, &spec, &app.http, open_browser, store).await?;
    app.providers.reload()?;
    Ok(LoginResult { provider: spec.key, email: account.email })
}

/// Signs an OAuth provider out, keeping its registered client for reuse.
pub fn logout(app: &App, provider: &str) -> Result<LogoutResult> {
    let spec = app.find_provider(provider)?.spec.clone();
    let signed_out = oauth::logout(&app.home, &spec)?;
    app.providers.reload()?;
    Ok(LogoutResult { provider: spec.key, signed_out })
}

/// Calls the provider's free `[auth_check]`, so a key can be tried without generating anything.
pub async fn test_key(app: &App, p: ProviderParams) -> Result<KeyTest> {
    let provider = app.find_provider(&p.provider)?;
    let key = provider.spec.key.clone();
    let missing = provider.missing_env();
    if !missing.is_empty() {
        let commands: Vec<String> = missing.iter().map(|name| set_command(app, name)).collect();
        return Ok(KeyTest {
            provider: key,
            result: "not_configured".into(),
            detail: None,
            hint: format!(
                "{} is not set: run {} in a terminal (the value is read from a hidden prompt, never from this \
                 conversation); it applies without a restart.",
                missing.join(", "),
                commands.join("; ")
            ),
        });
    }
    if provider.spec.auth_check.is_none() {
        return Ok(KeyTest {
            provider: key,
            result: "not_supported".into(),
            detail: None,
            hint: "this provider has no free check; a small generation or get_provider is the way to try it".into(),
        });
    }
    match provider.check_auth(&mut Trace::new()).await {
        Ok(()) => {
            let hint = format!("the provider accepted the key; get_provider {{provider: \"{key}\"}} shows model ids, then generate.");
            Ok(KeyTest { provider: key, result: "accepted".into(), detail: None, hint })
        }
        Err(e) => {
            let error = format!("{e:#}");
            if crate::jobs::failures::classify(&error, provider.spec.error_kinds()) == ErrorKind::Auth {
                let command = credential_name(&provider).map_or_else(|| "ai-media-router keys set NAME".to_owned(), |n| set_command(app, &n));
                let hint = format!("the provider refused the key: create a new one at {} and run {command}", provider.spec.docs);
                return Ok(KeyTest { provider: key, result: "refused".into(), detail: None, hint });
            }
            let hint = "the check did not decide whether the key works (network or provider trouble): generate a small job or run doctor --online.".to_owned();
            Ok(KeyTest { provider: key, result: "inconclusive".into(), detail: Some(truncate(&provider.redact(&error), 300)), hint })
        }
    }
}

/// The first env var the provider's auth reads, for a `keys set` hint.
fn credential_name(provider: &Provider) -> Option<String> {
    std::iter::once(&provider.spec.auth)
        .chain(provider.spec.routes.iter().filter_map(|r| r.auth.as_ref()))
        .flat_map(|auth| auth.env.iter())
        .next()
        .cloned()
}

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_owned(),
    }
}

/// The spec's `[example]`, else the first generation fixture.
fn example(app: &App, provider: &Provider) -> Option<Value> {
    let key = &provider.spec.key;
    let (model, input) = match &provider.spec.example {
        Some(e) => (e.model.clone(), e.input.clone()),
        None => fixtures::for_key(key, Some(&app.home))
            .iter()
            .filter_map(|(_, text)| serde_json::from_str::<Fixture>(text).ok())
            .find_map(|f| Some((f.model?, f.input)))?,
    };
    Some(json!({ "provider": key, "model": model, "input": input }))
}

pub async fn search_prices(app: &App, p: SearchPricesParams) -> Result<Prices> {
    let filter = catalog::Filter {
        query: p.query.as_deref(),
        provider: p.provider.as_deref(),
        res: p.res.as_deref(),
        mode: p.mode.as_deref(),
        quality: p.quality.as_deref(),
        kind: p.kind.as_deref(),
    };
    let registry = app.registry();
    let configured_count = registry.iter().filter(|p| p.configured()).count();
    let any_configured = configured_count > 0;
    let configured_only = p.configured_only.unwrap_or(any_configured);
    let scoped_by_default = p.configured_only.is_none() && any_configured;
    let all: Vec<Offer> = catalog::search(app.catalog().await?, &filter)
        .into_iter()
        .map(|row| {
            let provider = registry.for_catalog(&row.provider);
            Offer {
                offer_id: row.offer_id(),
                kind: row.kind().into(),
                provider_key: provider.map(|p| p.spec.key.clone()),
                configured: provider.is_some_and(|p| p.configured()),
                row,
            }
        })
        .collect();
    let unscoped = all.len();
    let mut offers: Vec<Offer> = all.into_iter().filter(|o| !configured_only || o.configured).collect();
    let total = offers.len();
    offers.truncate(p.limit.unwrap_or(DEFAULT_LIMIT));
    let base = "Catalog names are not provider model ids: call get_provider with the offer's provider_key; its \
                model_hint and example show how to write that model as `model`, and the offer's `source` page \
                usually names the exact id. estimate_cost {provider, model, input, offer_id} prices a job. \
                kind: \"image\" lists per-image offers; `base` rows are the ~1 MP prices, `notes` the surcharges.";
    let hint = if scoped_by_default && total == 0 && unscoped > 0 {
        "None of your configured providers offers this; configured_only: false shows who does (keys set adds a key)."
            .to_string()
    } else if scoped_by_default {
        format!("Only offers from your {configured_count} configured providers (configured_only: false lists every provider). {base}")
    } else {
        base.to_string()
    };
    Ok(Prices { total, offers, hint })
}

pub async fn estimate_cost(app: &App, p: EstimateCostParams) -> Result<Estimate> {
    let Some(name) = p.recipe else {
        let (Some(provider), Some(model)) = (p.provider, p.model) else { bail!("pass provider and model, or recipe") };
        return estimate_one(app, &provider, &model, &Value::Object(p.input), p.offer_id.as_deref(), p.reuse).await;
    };
    let recipe = recipes::get_recipe(app, &name)?;
    let vars = p.vars.unwrap_or_default();
    if recipe.provider.is_some() && recipe.model.is_some() {
        let mut g = GenerateParams {
            input: p.input.clone(),
            offer_id: p.offer_id.clone(),
            recipe: Some(name.clone()),
            vars: Some(vars.clone()),
            ..Default::default()
        };
        apply_recipe(app, &mut g)?;
        return estimate_one(app, &g.provider, &g.model, &Value::Object(g.input), g.offer_id.as_deref(), p.reuse).await;
    }
    ensure!(p.offer_id.is_none(), "{}", per_step_pricing(&recipe));
    let mut steps = Vec::new();
    for s in recipes::fill_steps(&recipe, &vars)? {
        let (estimate_usd, how) = step_cost(app, &s).await.with_context(|| format!("step `{}`", s.name))?;
        steps.push(StepEstimate { step: s.name, estimate_usd, how });
    }
    let unpriced: Vec<&str> = steps.iter().filter(|s| s.estimate_usd.is_none()).map(|s| s.step.as_str()).collect();
    Ok(Estimate {
        estimate_usd: steps.iter().map(|s| s.estimate_usd).sum(),
        formula: Some(format!("sum of {} steps", steps.len())),
        note: (!unpriced.is_empty()).then(|| {
            format!("no estimate for step(s) {}: give them estimated_cost_usd or offer_id in the recipe", unpriced.join(", "))
        }),
        steps,
        ..Default::default()
    })
}

/// One recipe step's cost in USD and how it was found.
async fn step_cost(app: &App, s: &Step) -> Result<(Option<f64>, String)> {
    match (s.op, &s.provider, &s.model, s.estimated_cost_usd) {
        (Some(op), ..) => Ok((Some(0.0), format!("{} runs locally", op.as_str()))),
        (None, _, _, Some(usd)) => Ok((Some(usd), "given".into())),
        (None, Some(provider), Some(model), None) => {
            let est = costs::estimate(app, provider, model, &Value::Object(s.input.clone()), s.offer_id.as_deref()).await?;
            Ok((est.estimate_usd, est.formula.or(est.note).unwrap_or_default()))
        }
        _ => bail!("the step has neither op nor provider and model; save_recipe the recipe again"),
    }
}

fn per_step_pricing(recipe: &Recipe) -> String {
    format!(
        "recipe `{}` has {} steps: give estimated_cost_usd or offer_id to each generation step (save_recipe steps[]), \
         not to the whole run",
        recipe.name,
        recipe.steps.len()
    )
}

/// One generation's estimate; 0 when generate would return an identical job's outputs instead.
async fn estimate_one(app: &App, provider: &str, model: &str, input: &Value, offer_id: Option<&str>, reuse: Option<bool>) -> Result<Estimate> {
    let mut est = costs::estimate(app, provider, model, input, offer_id).await?;
    if let Some(found) = reuse::lookup(app, provider, model, input, reuse)? {
        est.estimate_usd = Some(0.0);
        est.note = Some(format!("generate would return the outputs of job {} at no cost; reuse: false submits anew", found.job_id()));
        est.reuse = Some(found);
    }
    Ok(est)
}

/// What a job is expected to cost, for job_estimate.
struct Pricing<'a> {
    provider: &'a str,
    model: &'a str,
    input: &'a Value,
    estimated_cost_usd: Option<f64>,
    offer_id: Option<&'a str>,
    budget: Option<&'a str>,
}

/// The given estimate, else the offer or history estimate when an offer or a budget asks for one.
/// A budget with no estimate at all is refused with the reason.
async fn job_estimate(app: &App, p: Pricing<'_>) -> Result<Option<f64>> {
    if p.estimated_cost_usd.is_some() || (p.offer_id.is_none() && p.budget.is_none()) {
        return Ok(p.estimated_cost_usd);
    }
    let computed = costs::estimate(app, p.provider, p.model, p.input, p.offer_id).await?;
    if let (Some(budget), None) = (p.budget, computed.estimate_usd) {
        let name = budgets::admit(app, budget, Some(0.0))?;
        bail!(
            "budget `{name}` needs estimated_cost_usd and none could be computed: {}. Pass offer_id from search_prices \
             (with the duration or image count in input), or estimated_cost_usd yourself.",
            computed.note.unwrap_or_default()
        );
    }
    Ok(computed.estimate_usd)
}

pub async fn generate(app: &App, mut p: GenerateParams) -> Result<Job> {
    apply_recipe(app, &mut p)?;
    let input = Value::Object(p.input.clone());
    let pricing = Pricing {
        provider: &p.provider,
        model: &p.model,
        input: &input,
        estimated_cost_usd: p.estimated_cost_usd,
        offer_id: p.offer_id.as_deref(),
        budget: p.budget.as_deref(),
    };
    let spend = jobs::Spend {
        budget: p.budget.as_deref(),
        estimate_usd: job_estimate(app, pricing).await?,
        retry: p.retry.unwrap_or_default(),
        reuse: p.reuse,
    };
    Ok(jobs::create(app, &p.provider, &p.model, Value::Object(p.input), p.save_to, spend).await?.with_hint())
}

/// Applies a saved single-generation recipe as the defaults, then requires provider and model.
fn apply_recipe(app: &App, p: &mut GenerateParams) -> Result<()> {
    if let Some(name) = &p.recipe {
        let recipe = recipes::get_recipe(app, name)?;
        let Value::Object(mut input) = recipes::fill(&recipe, &p.vars.take().unwrap_or_default())? else {
            bail!("recipe `{name}` input is not an object")
        };
        if p.provider.is_empty() {
            p.provider = recipe.provider.unwrap_or_default();
        }
        if p.model.is_empty() {
            p.model = recipe.model.unwrap_or_default();
        }
        input.extend(std::mem::take(&mut p.input));
        p.input = input;
        p.save_to = p.save_to.take().or(recipe.save_to);
        p.offer_id = p.offer_id.take().or(recipe.offer_id);
        p.budget = p.budget.take().or(recipe.budget);
    }
    if p.provider.is_empty() || p.model.is_empty() {
        bail!("pass provider and model, or recipe (a saved preset; list_recipes shows them)");
    }
    Ok(())
}

/// Everything generate (or run_recipe) would check and send, without sending or spending anything.
pub async fn preflight(app: &App, p: PreflightParams) -> Result<Preflight> {
    let recipe = p.recipe.as_deref().map(|name| recipes::get_recipe(app, name)).transpose()?;
    if let Some(recipe) = recipe.filter(|r| r.input.is_none()) {
        ensure!(p.estimated_cost_usd.is_none() && p.offer_id.is_none(), "{}", per_step_pricing(&recipe));
        let steps = recipes::fill_steps(&recipe, &p.vars.unwrap_or_default())?;
        return Ok(Box::pin(preflight::recipe(app, &steps, p.budget.as_deref(), p.retry)).await);
    }
    let mut g = GenerateParams {
        provider: p.provider.unwrap_or_default(),
        model: p.model.unwrap_or_default(),
        input: p.input,
        recipe: p.recipe,
        vars: p.vars,
        save_to: p.save_to,
        budget: p.budget,
        estimated_cost_usd: p.estimated_cost_usd,
        offer_id: p.offer_id,
        retry: p.retry,
        reuse: p.reuse,
    };
    apply_recipe(app, &mut g)?;
    Ok(preflight::preflight(app, &g).await)
}

pub async fn doctor(app: &App, p: DoctorParams) -> Result<Doctor> {
    Ok(preflight::doctor(app, p).await)
}

fn wait_for(wait_secs: Option<u64>) -> Duration {
    Duration::from_secs(wait_secs.unwrap_or(0).min(MAX_WAIT_SECS))
}

pub async fn get_job(app: &App, p: GetJobParams) -> Result<Job> {
    Ok(jobs::wait(app, &p.job_id, wait_for(p.wait_secs), p.debug.unwrap_or(false)).await?.with_hint())
}

pub fn list_jobs(app: &App, p: ListJobsParams) -> Result<Jobs> {
    let filter = jobs::Filter {
        reference: p.reference.as_deref(),
        collection: p.collection.as_deref(),
        status: p.status.as_deref(),
        error_kind: p.error_kind,
        batch: p.batch.as_deref(),
    };
    Ok(Jobs { jobs: jobs::list(app, &filter, p.limit.unwrap_or(DEFAULT_LIMIT))? })
}

pub async fn generate_batch(app: &App, p: GenerateBatchParams) -> Result<Batch> {
    let defaults = p.defaults.unwrap_or_default();
    let recipe = p.recipe.as_deref().map(|name| recipes::get_recipe(app, name)).transpose()?;
    if let Some(recipe) = &recipe {
        recipes::single(recipe)?;
    }
    let items = p.items.unwrap_or_default();
    let items = if items.is_empty() { vec![BatchItem::default()] } else { items };
    let combos = batches::combinations(&p.matrix.unwrap_or_default())?;
    let budget = defaults.budget.as_deref().or(recipe.as_ref().and_then(|r| r.budget.as_deref()));
    let mut planned = Vec::new();
    for (i, item) in items.into_iter().enumerate() {
        let provider = item.provider.or(defaults.provider.clone()).or(recipe.as_ref().and_then(|r| r.provider.clone()));
        let model = item.model.or(defaults.model.clone()).or(recipe.as_ref().and_then(|r| r.model.clone()));
        let (Some(provider), Some(model)) = (provider, model) else {
            bail!("item {i} has no provider or model: set them in defaults or in the item");
        };
        for combo in &combos {
            let mut input = Map::new();
            if let Some(recipe) = &recipe {
                let mut vars = defaults.vars.clone().unwrap_or_default();
                vars.extend(item.vars.clone().unwrap_or_default());
                vars.extend(combo.clone());
                let filled = recipes::fill(recipe, &vars).with_context(|| format!("item {i}"))?;
                input.extend(filled.as_object().cloned().unwrap_or_default());
            }
            input.extend(defaults.input.clone().unwrap_or_default());
            input.extend(item.input.clone().unwrap_or_default());
            if recipe.is_none() {
                input.extend(combo.clone());
            }
            let pricing = Pricing {
                provider: &provider,
                model: &model,
                input: &Value::Object(input.clone()),
                estimated_cost_usd: item.estimated_cost_usd.or(defaults.estimated_cost_usd),
                offer_id: item.offer_id.as_deref().or(defaults.offer_id.as_deref()).or(recipe.as_ref().and_then(|r| r.offer_id.as_deref())),
                budget,
            };
            let estimate_usd = job_estimate(app, pricing).await.with_context(|| format!("item {i}"))?;
            let save_to = item.save_to.clone().or(defaults.save_to.clone()).or(recipe.as_ref().and_then(|r| r.save_to.clone()));
            let (retry, reuse) = (defaults.retry.unwrap_or_default(), defaults.reuse);
            planned.push(batches::Planned { provider: provider.clone(), model: model.clone(), input, save_to, estimate_usd, retry, reuse });
        }
    }
    batches::run(app, p.label, budget, planned).await
}

pub async fn get_batch(app: &App, p: GetBatchParams) -> Result<Batch> {
    batches::wait(app, &p.batch_id, wait_for(p.wait_secs)).await
}

/// Marks the job failed with `cancelled`; jobs waiting on it fail too.
pub fn cancel_job(app: &App, p: JobParams) -> Result<Job> {
    let mut job = jobs::cancel(app, &p.job_id)?;
    job.hint = Some(
        "Cancelled here only: the provider was not asked to stop, so a running generation may still finish and bill \
         there. Its result is not downloaded."
            .into(),
    );
    Ok(job)
}

pub fn search_assets(app: &App, p: SearchAssetsParams) -> Result<Assets> {
    let limit = p.limit.unwrap_or(DEFAULT_LIMIT);
    let review = ReviewFilter { status: p.status, min_rating: p.min_rating };
    Ok(Assets { assets: assets::search(app, p.query.as_deref(), p.collection.as_deref(), &review, limit)? })
}

pub fn review_asset(app: &App, p: ReviewAssetParams) -> Result<Assets> {
    ensure!(!p.asset_ids.is_empty(), "pass at least one asset id in asset_ids");
    ensure!(
        p.status.is_some() || p.rating.is_some() || p.notes.is_some(),
        "pass status (approved, rejected, draft), rating (1-5) and/or notes"
    );
    if let Some(r) = p.rating {
        ensure!((1..=5).contains(&r), "rating must be 1-5, got {r}");
    }
    Ok(Assets { assets: assets::review(app, &p.asset_ids, p.status, p.rating, p.notes.as_deref())? })
}

pub fn get_stats(app: &App, p: StatsParams) -> Result<Stats> {
    let filter = stats::Filter { provider: p.provider.as_deref(), model: p.model.as_deref(), collection: p.collection.as_deref() };
    let models = stats::get(app, &filter)?;
    let hint = if models.is_empty() {
        "No jobs match yet; stats fill in as jobs finish and their outputs are reviewed with review_asset.".into()
    } else {
        "Prefer models with a high done_share, approved_share and average_rating; failure_kinds says why the others \
         failed (list_jobs {error_kind} shows those jobs)."
            .into()
    };
    Ok(Stats { models, hint })
}

/// Also makes a missing preview and reads missing metadata (assets stored before ffmpeg was installed).
pub async fn get_asset(app: &App, p: AssetParams) -> Result<Asset> {
    Ok(media::enrich(app, assets::get(app, &p.asset_id)?).await)
}

pub fn update_asset(app: &App, p: UpdateAssetParams) -> Result<Asset> {
    assets::update(app, &p.asset_id, p.tags.as_deref(), p.description.as_deref())
}

pub fn delete_asset(app: &App, p: AssetParams) -> Result<Asset> {
    assets::delete(app, &p.asset_id)
}

pub async fn similar_assets(app: &App, p: SimilarAssetsParams) -> Result<SimilarAssets> {
    let limit = p.limit.unwrap_or(DEFAULT_LIMIT);
    let assets =
        embeddings::similar(app, p.query.as_deref(), p.asset_id.as_deref(), p.collection.as_deref(), limit).await?;
    Ok(SimilarAssets { assets })
}

pub async fn export_collection(app: &App, p: CollectionParams) -> Result<Export> {
    collections::export(app, &p.collection, p.only_approved.unwrap_or(false)).await
}

pub fn set_budget(app: &App, p: SetBudgetParams) -> Result<Budget> {
    budgets::set(app, &p.name, p.limit_usd)
}

pub fn list_budgets(app: &App) -> Result<Budgets> {
    Ok(Budgets { budgets: budgets::list(app)? })
}

pub fn delete_budget(app: &App, p: BudgetParams) -> Result<Budget> {
    budgets::delete(app, &p.name)
}

pub fn get_lineage(app: &App, p: AssetParams) -> Result<Lineage> {
    lineage::get(app, &p.asset_id)
}

pub fn save_reference(app: &App, p: SaveReferenceParams) -> Result<Reference> {
    let fields = references::Fields {
        name: &p.name,
        kind: p.kind,
        description: p.description.as_deref(),
        tags: p.tags.as_deref(),
    };
    references::save(app, fields, p.asset_ids.as_deref())
}

pub fn get_reference(app: &App, p: ReferenceParams) -> Result<Reference> {
    references::get(app, &p.reference)
}

pub fn list_references(app: &App, p: ListReferencesParams) -> Result<References> {
    let limit = p.limit.unwrap_or(DEFAULT_LIMIT);
    Ok(References { references: references::list(app, p.kind, p.query.as_deref(), limit)? })
}

pub fn add_to_reference(app: &App, p: ReferenceAssetsParams) -> Result<Reference> {
    references::add(app, &p.reference, &p.asset_ids, p.role.as_deref())
}

pub fn remove_from_reference(app: &App, p: ReferenceAssetsParams) -> Result<Reference> {
    references::remove(app, &p.reference, &p.asset_ids)
}

pub fn delete_reference(app: &App, p: ReferenceParams) -> Result<Reference> {
    references::delete(app, &p.reference)
}

pub fn list_collections(app: &App) -> Result<Collections> {
    Ok(Collections { collections: collections::list(app)? })
}

pub fn add_to_collection(app: &App, p: CollectionAssetsParams) -> Result<Collection> {
    collections::add(app, &p.collection, &p.asset_ids, p.description.as_deref())
}

pub fn remove_from_collection(app: &App, p: CollectionAssetsParams) -> Result<Collection> {
    collections::remove(app, &p.collection, &p.asset_ids)
}

pub async fn import_asset(app: &App, p: ImportAssetParams) -> Result<Imported> {
    let tags = p.tags.unwrap_or_default();
    assets::import_file(app, &PathBuf::from(p.path), &tags, p.prompt.as_deref()).await
}

pub async fn upload_asset(app: &App, p: UploadAssetParams) -> Result<Uploaded> {
    let reference = assets::upload(app, &p.asset_id, &p.provider, &mut Trace::new()).await?;
    let text = reference.as_str().unwrap_or("");
    let kind = if text.starts_with("data:") {
        "data_uri"
    } else if text.contains("://") {
        "url"
    } else {
        "id"
    };
    let spec_usage = app.find_provider(&p.provider)?.spec.upload.as_ref().and_then(|u| u.usage.clone());
    let usage = spec_usage.unwrap_or_else(|| {
        "Put `reference` into the input field the model documents for reference media (e.g. image_url, image_urls).".into()
    });
    Ok(Uploaded { reference, kind: kind.into(), usage })
}

pub fn get_spec(app: &App, p: ProviderParams) -> Result<SpecText> {
    let provider = app.find_provider(&p.provider)?;
    let key = provider.spec.key.clone();
    let builtin = builtin_spec(&key).map(|(_, text)| text.to_owned());
    let (text, builtin_text) = match &provider.origin {
        Origin::Override(path) => {
            let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
            (text, builtin)
        }
        _ => (builtin.unwrap_or_default(), None),
    };
    Ok(SpecText {
        fixtures: fixtures::for_key(&key, Some(&app.home)).into_iter().map(|(name, _)| name).collect(),
        source: provider.origin.kind().to_owned(),
        path: provider.origin.path(),
        key,
        text,
        builtin_text,
    })
}

pub async fn check_spec(app: &App, p: CheckSpecParams) -> Result<SpecCheck> {
    let (parsed, key) = match (&p.spec, &p.provider) {
        (Some(text), _) => {
            let key = toml::from_str::<toml::Table>(text)
                .ok()
                .and_then(|t| t.get("key")?.as_str().map(str::to_owned))
                .or(p.provider.clone());
            (Spec::parse(text), key)
        }
        (None, Some(key)) => (Ok(app.find_provider(key)?.spec.clone()), Some(key.clone())),
        (None, None) => bail!("pass `provider` (check the loaded spec) or `spec` (TOML text of a candidate)"),
    };
    let spec = match parsed {
        Ok(spec) => spec,
        Err(e) => {
            return Ok(SpecCheck {
                key,
                valid: false,
                error: Some(format!("{e:#}")),
                fixtures: Vec::new(),
                requests: Vec::new(),
                hint: "Fix the error and call check_spec again; docs/provider-spec.md describes every field.".into(),
            });
        }
    };
    let results = fixtures::run_all(&spec, &fixtures::for_key(&spec.key, Some(&app.home))).await;
    let requests = match &p.model {
        Some(model) => {
            let provider = Provider::new(spec.clone(), Origin::Candidate, app.env(), app.http.clone(), app.home.clone());
            let input = Value::Object(p.input.clone().unwrap_or_default());
            provider.dry_run(model, &input, &p.vars.clone().unwrap_or_default())?
        }
        None => Vec::new(),
    };
    let failed = results.iter().filter(|r| !r.ok).count();
    let hint = if failed > 0 {
        format!(
            "{failed} fixture(s) fail: the spec no longer matches the recorded responses. If the provider's API \
             changed, update the fixture too (repo providers/fixtures/{0}/ or <home>/providers/fixtures/{0}/).",
            spec.key
        )
    } else if results.is_empty() {
        format!(
            "Valid, but no fixtures exist for {}: add providers/fixtures/{}/<case>.json (a job's debug trace is the \
             same shape) so the next change is checked.",
            spec.key, spec.key
        )
    } else {
        "Valid and every fixture passes. save_spec activates it immediately.".into()
    };
    Ok(SpecCheck { key: Some(spec.key), valid: true, error: None, fixtures: results, requests, hint })
}

pub async fn save_spec(app: &App, p: SaveSpecParams) -> Result<SpecSaved> {
    ensure!(app.spec_writes, "spec writes are disabled for this server (remote HTTP); edit <home>/providers/ directly");
    let spec = Spec::parse(&p.spec).context("spec is invalid (check_spec shows details)")?;
    let builtin = builtin_spec(&spec.key).and_then(|(_, text)| Spec::parse(text).ok());
    let builtin_command = builtin.as_ref().map(|b| b.auth.command.clone()).unwrap_or_default();
    let auths = std::iter::once(&spec.auth).chain(spec.routes.iter().filter_map(|r| r.auth.as_ref()));
    for auth in auths.filter(|a| a.kind == AuthKind::Command) {
        ensure!(
            auth.command == builtin_command,
            "`command` auth runs a local program, so it can only be changed by editing the file by hand"
        );
    }

    let replaced = app.registry().get(&spec.key).map_or("nothing", |p| p.origin.kind()).to_owned();
    let path = match app.registry().get(&spec.key).map(|p| p.origin.clone()) {
        Some(Origin::Override(path)) => path,
        _ => app.providers.overrides_dir().join(format!("{}.toml", spec.key)),
    };
    std::fs::create_dir_all(app.providers.overrides_dir())?;
    std::fs::write(&path, &p.spec).with_context(|| format!("cannot write {}", path.display()))?;
    let registry = app.providers.reload()?;
    if let Some(problem) = registry.problems.iter().find(|pr| pr.file == path.display().to_string()) {
        bail!("saved {} but it did not load: {}", path.display(), problem.error);
    }
    let check = check_spec(app, CheckSpecParams { provider: Some(spec.key.clone()), ..Default::default() }).await?;
    Ok(SpecSaved { key: spec.key, path: path.display().to_string(), replaced, check })
}

pub fn reset_spec(app: &App, p: ProviderParams) -> Result<SpecReset> {
    ensure!(app.spec_writes, "spec writes are disabled for this server (remote HTTP); edit <home>/providers/ directly");
    let provider = app.find_provider(&p.provider)?;
    let Origin::Override(path) = &provider.origin else {
        return Err(NotFound(format!("`{}` has no override; it already uses the built-in spec", p.provider)).into());
    };
    std::fs::remove_file(path).with_context(|| format!("cannot remove {}", path.display()))?;
    let registry = app.providers.reload()?;
    let source = registry.get(&p.provider).map_or("none", |p| p.origin.kind()).to_owned();
    Ok(SpecReset { key: p.provider, removed: path.display().to_string(), source })
}

#[derive(Deserialize, JsonSchema)]
pub struct SaveSnippetParams {
    /// Used as "{{snippet:<name>}}" inside any string of generate input, e.g. "brand-style".
    pub name: String,
    /// The prompt block, e.g. "soft window light, muted teal palette, 35mm". May contain
    /// "{{ref:<name>.description}}" but not other snippets.
    pub text: String,
    pub description: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct NameParams {
    pub name: String,
}

#[derive(Serialize, JsonSchema)]
pub struct Snippets {
    pub snippets: Vec<Snippet>,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct SaveRecipeParams {
    /// Unique name; saving again replaces the recipe.
    pub name: String,
    /// Start from this job's provider, model, input and save_to; other fields given here override them.
    pub from_job: Option<String>,
    /// Like from_job, with the job that made this asset.
    pub from_asset: Option<String>,
    /// With from_job / from_asset: also save the jobs that made its inputs ({{job:…}}, {{asset:…}} made by a
    /// job, asset ids of an edit), back to imported files and references, as earlier steps; their ids become
    /// "{{step:<name>}}". provider, model and input given here then override the last step. Default false.
    pub chain: Option<bool>,
    pub description: Option<String>,
    /// Required unless from_job is given.
    pub provider: Option<String>,
    /// Required unless from_job is given.
    pub model: Option<String>,
    /// Generation input. "{{var:<name>}}" as a whole string becomes the variable's JSON value, inside a
    /// longer string its text. Text and media placeholders ({{snippet:…}}, {{ref:…}}, {{asset:…}}) stay
    /// and resolve at every run.
    pub input: Option<Map<String, Value>>,
    /// With steps: where the final steps' outputs are filed.
    pub save_to: Option<SaveTo>,
    /// Offer from search_prices that prices every run; single-generation recipes only.
    pub offer_id: Option<String>,
    /// Budget every run counts against unless the call names another.
    pub budget: Option<String>,
    /// Declared variables; ones used in input but not listed here are added as required.
    pub variables: Option<BTreeMap<String, Variable>>,
    /// A chain instead of provider/model/input, run in order by one run_recipe: each step is a generation
    /// {name, provider, model, input} or an ffmpeg edit {name, op, input: <that tool's parameters>}. Later steps
    /// use earlier outputs as "{{step:<name>}}", "{{step:<name>:<n>}}" or "{{step:<name>:last_frame}}".
    pub steps: Option<Vec<Step>>,
}

#[derive(Serialize, JsonSchema)]
pub struct RecipeList {
    pub recipes: Vec<Recipe>,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct RunRecipeParams {
    pub name: String,
    /// Values for the recipe's variables, e.g. {"language": "de", "duration": 5}.
    pub vars: Option<Map<String, Value>>,
    /// Replaces the recipe's save_to for this run (with steps: for the final steps).
    pub save_to: Option<SaveTo>,
    /// With steps, the sum of every step's estimate is admitted before anything is submitted.
    pub budget: Option<String>,
    /// With `budget` and no value, it is computed like estimate_cost. Not for a recipe with steps: set it per step.
    pub estimated_cost_usd: Option<f64>,
    /// Offer from search_prices; prices the run from the filled input. Not for a recipe with steps: set it per step.
    pub offer_id: Option<String>,
    /// Resubmit after a retryable failure (rate_limit, provider_error, network, timeout): {attempts: 1-5, delay_secs}.
    /// Off by default, since a provider may bill a failed attempt.
    pub retry: Option<Retry>,
    /// true: when a job with the same provider, model and input (library files compared by content) is done and
    /// its outputs are still in the library, or is still running, return those outputs as a new job at no cost.
    /// Off by default, since a repeat is usually wanted as a new variant. Even without it, an identical request
    /// made while the first one runs (started under 5 minutes ago) returns that job; false always submits.
    pub reuse: Option<bool>,
}

#[derive(Serialize, JsonSchema)]
pub struct PromptList {
    pub prompts: Vec<PromptInfo>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetPromptParams {
    /// Built-in prompt name or "recipe:<recipe name>".
    pub name: String,
    pub arguments: Option<Map<String, Value>>,
}

pub fn save_snippet(app: &App, p: SaveSnippetParams) -> Result<Snippet> {
    recipes::save_snippet(app, &p.name, &p.text, p.description.as_deref())
}

pub fn list_snippets(app: &App) -> Result<Snippets> {
    Ok(Snippets { snippets: recipes::list_snippets(app)? })
}

pub fn delete_snippet(app: &App, p: NameParams) -> Result<Snippet> {
    recipes::delete_snippet(app, &p.name)
}

pub fn save_recipe(app: &App, p: SaveRecipeParams) -> Result<Recipe> {
    let from = match (p.from_job, &p.from_asset) {
        (Some(job), _) => Some(job),
        (None, Some(asset)) => Some(assets::get(app, asset)?.job_id.with_context(|| {
            format!("asset `{asset}` was imported, not made by a job; pass from_job, steps or provider, model and input")
        })?),
        (None, None) => None,
    };
    let mut chain = from.map(|id| recipes::chain(app, &id, p.chain.unwrap_or(false))).transpose()?.unwrap_or_default();
    let mut fields = recipes::RecipeFields {
        name: p.name,
        description: p.description.unwrap_or_default(),
        provider: None,
        model: None,
        input: None,
        steps: Vec::new(),
        save_to: p.save_to,
        offer_id: p.offer_id,
        budget: p.budget,
        variables: p.variables.unwrap_or_default(),
    };
    if let Some(steps) = p.steps {
        ensure!(
            chain.is_empty() && p.provider.is_none() && p.model.is_none() && p.input.is_none(),
            "give `steps` alone, without from_job, from_asset, provider, model or input"
        );
        fields.steps = steps;
    } else if chain.len() > 1 || chain.first().is_some_and(|s| s.op.is_some()) {
        let last = chain.last_mut().expect("the chain has a step");
        last.provider = p.provider.or(last.provider.take());
        last.model = p.model.or(last.model.take());
        if let Some(input) = p.input {
            last.input = input;
        }
        fields.steps = chain;
    } else {
        let job = chain.pop();
        let need = |field: &str| format!("pass `{field}` (or `from_job` to copy a job's provider, model and input)");
        fields.input = Some(match (p.input, &job) {
            (Some(input), _) => Value::Object(input),
            (None, Some(job)) => Value::Object(job.input.clone()),
            (None, None) => bail!(need("input")),
        });
        fields.provider = Some(p.provider.or_else(|| job.as_ref().and_then(|j| j.provider.clone())).with_context(|| need("provider"))?);
        fields.model = Some(p.model.or_else(|| job.as_ref().and_then(|j| j.model.clone())).with_context(|| need("model"))?);
        fields.save_to = fields.save_to.or_else(|| job.and_then(|j| j.save_to));
    }
    recipes::save_recipe(app, fields)
}

pub fn list_recipes(app: &App) -> Result<RecipeList> {
    Ok(RecipeList { recipes: recipes::list_recipes(app)? })
}

pub fn get_recipe(app: &App, p: NameParams) -> Result<Recipe> {
    recipes::get_recipe(app, &p.name)
}

pub fn delete_recipe(app: &App, p: NameParams) -> Result<Recipe> {
    recipes::delete_recipe(app, &p.name)
}

pub async fn run_recipe(app: &App, p: RunRecipeParams) -> Result<Job> {
    let recipe = recipes::get_recipe(app, &p.name)?;
    if recipe.provider.is_none() || recipe.model.is_none() {
        return Box::pin(run_chain(app, &recipe, p)).await;
    }
    generate(app, GenerateParams {
        recipe: Some(p.name),
        vars: p.vars,
        save_to: p.save_to,
        budget: p.budget,
        estimated_cost_usd: p.estimated_cost_usd,
        offer_id: p.offer_id,
        retry: p.retry,
        reuse: p.reuse,
        ..Default::default()
    })
    .await
}

/// Every step's job in one batch, created in order; a step waits for the steps its `{{step:…}}` name.
async fn run_chain(app: &App, recipe: &Recipe, p: RunRecipeParams) -> Result<Job> {
    ensure!(p.estimated_cost_usd.is_none() && p.offer_id.is_none(), "{}", per_step_pricing(recipe));
    let steps = recipes::fill_steps(recipe, &p.vars.unwrap_or_default())?;
    let finals = recipes::finals(&steps)?;
    let budget = p.budget.as_deref();
    let mut estimates = Vec::new();
    for s in &steps {
        let estimate = match (s.op, &s.provider, &s.model) {
            (Some(op), ..) => Edit::parse(op, Value::Object(s.input.clone())).map(|_| Some(0.0)),
            (None, Some(provider), Some(model)) => {
                let input = Value::Object(s.input.clone());
                let pricing =
                    Pricing { provider, model, input: &input, estimated_cost_usd: s.estimated_cost_usd, offer_id: s.offer_id.as_deref(), budget };
                job_estimate(app, pricing).await
            }
            _ => Err(anyhow::anyhow!("it has neither op nor provider and model; save_recipe the recipe again")),
        };
        estimates.push(estimate.with_context(|| format!("recipe `{}` step `{}`", recipe.name, s.name))?);
    }
    if let Some(budget) = budget {
        let total = estimates.iter().copied().sum::<Option<f64>>();
        budgets::admit(app, budget, total)
            .with_context(|| format!("recipe `{}` of {} steps, the sum of their estimates", recipe.name, steps.len()))?;
    }

    let batch = batches::open(app, Some(format!("recipe {}", recipe.name)), Some(&recipe.name))?;
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut last = None;
    for (s, estimate_usd) in steps.iter().zip(estimates) {
        let save_to = match finals.contains(&s.name) {
            true => p.save_to.clone().or(s.save_to.clone()).or(recipe.save_to.clone()),
            false => s.save_to.clone(),
        };
        let started = async {
            let input = recipes::link(&s.input, &ids)?;
            let job = match (s.op, &s.provider, &s.model) {
                (Some(op), ..) => jobs::create_edit(app, op, input, save_to, &batch)?,
                (None, Some(provider), Some(model)) => {
                    let spend = jobs::Spend { budget, estimate_usd, retry: p.retry.unwrap_or_default(), reuse: p.reuse };
                    jobs::create_in(app, provider, model, input, save_to, spend, Some(&batch)).await?
                }
                _ => bail!("it has neither op nor provider and model"),
            };
            jobs::set_step(app, &job.id, &s.name)?;
            match job.status.as_str() {
                "failed" => bail!("submit failed: {}", job.error.unwrap_or_default()),
                _ => Ok(job.id),
            }
        };
        match started.await {
            Ok(id) => {
                ids.insert(s.name.clone(), id.clone());
                last = Some(id);
            }
            Err(e) => {
                let before = match ids.len() {
                    0 => "No earlier step was started.".to_owned(),
                    n => format!(
                        "The {n} earlier step(s) keep running in batch {batch}: get_batch {{batch_id: \"{batch}\"}} shows them, \
                         cancel_job stops one and the steps waiting on it."
                    ),
                };
                bail!("recipe `{}` step `{}`: {e:#}\n{before}", recipe.name, s.name);
            }
        }
    }
    let last = last.context("the recipe has no steps")?;
    let mut job = jobs::get(app, &last, false)?;
    job.hint = Some(format!(
        "Recipe `{}` started {} steps in batch {batch}; this is the last one, `{}`. get_batch {{batch_id: \"{batch}\", \
         wait_secs: 50}} until no step is running or waiting: its outputs are the final results. A failed step fails \
         the steps after it, naming it in their error.",
        recipe.name,
        steps.len(),
        job.step.as_deref().unwrap_or_default()
    ));
    Ok(job)
}

pub fn list_prompts(app: &App) -> Result<PromptList> {
    Ok(PromptList { prompts: prompts::list(app)? })
}

pub fn get_prompt(app: &App, p: GetPromptParams) -> Result<PromptText> {
    prompts::get(app, &p.name, &p.arguments.unwrap_or_default())
}

// The job input is the parameters without save_to, which the job stores separately.

/// A media operation with its parameters: one of the tools below, or a recipe's edit step.
#[derive(Deserialize)]
#[serde(tag = "op", content = "input", rename_all = "snake_case")]
pub enum Edit {
    ExtractFrame(ExtractFrameParams),
    Reframe(ReframeParams),
    Trim(TrimParams),
    Concat(ConcatParams),
    Overlay(OverlayParams),
    MuxAudio(MuxAudioParams),
}

impl Edit {
    pub fn parse(op: Op, input: Value) -> Result<Self> {
        serde_json::from_value(json!({ "op": op, "input": input }))
            .with_context(|| format!("{0} parameters, as the {0} tool takes them", op.as_str()))
    }

    /// `job` is the waiting recipe step this edit finishes; without it a new local job records the edit.
    pub async fn run(self, app: &App, job: Option<&str>, save_to: Option<SaveTo>) -> Result<Edited> {
        match self {
            Edit::ExtractFrame(p) => {
                let at = p.at.unwrap_or(FrameAt::Edge(media::Edge::First));
                media::extract_frame(app, media::Local::new(&p, job)?, &p.asset_id, at, save_to.or(p.save_to)).await
            }
            Edit::Reframe(p) => {
                let (local, mode) = (media::Local::new(&p, job)?, p.mode.unwrap_or_default());
                media::reframe(app, local, &p.asset_id, &p.aspect, mode, save_to.or(p.save_to)).await
            }
            Edit::Trim(p) => {
                media::trim(app, media::Local::new(&p, job)?, &p.asset_id, p.start.unwrap_or(0.0), p.end, save_to.or(p.save_to)).await
            }
            Edit::Concat(p) => media::concat(app, media::Local::new(&p, job)?, &p.asset_ids, save_to.or(p.save_to)).await,
            Edit::Overlay(p) => {
                let at = media::Placement { x: p.x.unwrap_or(0), y: p.y.unwrap_or(0), width: p.width };
                let local = media::Local::new(&p, job)?;
                media::overlay(app, local, &p.base_asset_id, &p.overlay_asset_id, at, save_to.or(p.save_to)).await
            }
            Edit::MuxAudio(p) => {
                let local = media::Local::new(&p, job)?;
                media::mux_audio(app, local, &p.video_asset_id, &p.audio_asset_id, save_to.or(p.save_to)).await
            }
        }
    }
}

pub async fn extract_frame(app: &App, p: ExtractFrameParams) -> Result<Edited> {
    Edit::ExtractFrame(p).run(app, None, None).await
}

pub async fn reframe(app: &App, p: ReframeParams) -> Result<Edited> {
    Edit::Reframe(p).run(app, None, None).await
}

pub async fn trim(app: &App, p: TrimParams) -> Result<Edited> {
    Edit::Trim(p).run(app, None, None).await
}

pub async fn concat(app: &App, p: ConcatParams) -> Result<Edited> {
    Edit::Concat(p).run(app, None, None).await
}

pub async fn overlay(app: &App, p: OverlayParams) -> Result<Edited> {
    Edit::Overlay(p).run(app, None, None).await
}

pub async fn mux_audio(app: &App, p: MuxAudioParams) -> Result<Edited> {
    Edit::MuxAudio(p).run(app, None, None).await
}

#[cfg(test)]
mod tests;
