//! HTTP interface: REST under /v1 and MCP (streamable HTTP) under /mcp, sharing one worker.

use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    api::{self, *},
    app::{App, NotFound},
    interfaces::mcp,
    jobs::{Job, Retry, SaveTo, batches::Batch, preflight::{Doctor, DoctorParams, Preflight}},
    library::{assets::{self, Asset}, collections::Collection, lineage::Lineage, media::Edited, provenance::Imported, references::Reference},
    pricing::{budgets::Budget, costs::Estimate},
    templates::{prompts::PromptText, recipes::{Recipe, Snippet}},
};

type ApiResult<T> = Result<Json<T>, ApiError>;

pub fn router(app: App, token: Option<String>) -> Router {
    let mcp_app = app.clone();
    let mcp = StreamableHttpService::new(
        move || Ok(mcp::Server::new(mcp_app.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );
    Router::new()
        .route("/v1/providers", get(list_providers))
        .route("/v1/providers/{key}", get(get_provider))
        .route("/v1/providers/{key}/spec", get(get_spec).delete(reset_spec))
        .route("/v1/providers/{key}/test_key", post(test_key))
        .route("/v1/keys", get(list_keys))
        .route("/v1/specs", put(save_spec))
        .route("/v1/specs/check", post(check_spec))
        .route("/v1/prices", get(search_prices))
        .route("/v1/estimate", post(estimate_cost))
        .route("/v1/jobs", get(list_jobs).post(generate))
        .route("/v1/jobs/preflight", post(preflight))
        .route("/v1/doctor", get(doctor))
        .route("/v1/jobs/{id}", get(get_job))
        .route("/v1/jobs/{id}/cancel", post(cancel_job))
        .route("/v1/batches", post(generate_batch))
        .route("/v1/batches/{id}", get(get_batch))
        .route("/v1/assets", get(search_assets).post(import_asset))
        .route("/v1/assets/{id}", get(get_asset).patch(update_asset).delete(delete_asset))
        .route("/v1/assets/{id}/file", get(asset_file))
        .route("/v1/assets/{id}/preview", get(asset_preview))
        .route("/v1/assets/{id}/upload", post(upload_asset))
        .route("/v1/assets/{id}/lineage", get(get_lineage))
        .route("/v1/references", get(list_references).post(save_reference))
        .route("/v1/references/{ref}", get(get_reference).delete(delete_reference))
        .route("/v1/references/{ref}/assets", post(add_to_reference).delete(remove_from_reference))
        .route("/v1/collections", get(list_collections))
        .route("/v1/collections/{name}/assets", post(add_to_collection).delete(remove_from_collection))
        .route("/v1/collections/{name}/export", get(export_collection))
        .route("/v1/similar", get(similar_assets))
        .route("/v1/review", post(review_asset))
        .route("/v1/stats", get(get_stats))
        .route("/v1/budgets", get(list_budgets).put(set_budget))
        .route("/v1/budgets/{name}", axum::routing::delete(delete_budget))
        .route("/v1/snippets", get(list_snippets).put(save_snippet))
        .route("/v1/snippets/{name}", axum::routing::delete(delete_snippet))
        .route("/v1/recipes", get(list_recipes).put(save_recipe))
        .route("/v1/recipes/{name}", get(get_recipe).delete(delete_recipe))
        .route("/v1/recipes/{name}/run", post(run_recipe))
        .route("/v1/prompts", get(list_prompts))
        .route("/v1/prompts/{name}", get(get_prompt))
        .route("/v1/media/extract_frame", post(extract_frame))
        .route("/v1/media/reframe", post(reframe))
        .route("/v1/media/trim", post(trim))
        .route("/v1/media/concat", post(concat))
        .route("/v1/media/overlay", post(overlay))
        .route("/v1/media/mux_audio", post(mux_audio))
        .with_state(app)
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn(move |req: Request, next: Next| {
            let token = token.clone();
            async move {
                let expected = token.map(|t| format!("Bearer {t}"));
                let given = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
                if expected.is_some_and(|e| given != Some(e.as_str())) {
                    return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "missing or wrong bearer token" })))
                        .into_response();
                }
                next.run(req).await
            }
        }))
}

pub struct ApiError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = if self.0.downcast_ref::<NotFound>().is_some() {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_REQUEST
        };
        (status, Json(json!({ "error": format!("{:#}", self.0) }))).into_response()
    }
}

/// `name(Params { segment })` takes the params from one path segment, `name(Json(p): Params)` from an extractor;
/// both become a handler calling the `api` function of the same name.
macro_rules! handlers {
    () => {};
    ($name:ident($params:ident { $field:ident }) $(.$aw:tt)? -> $out:ident; $($rest:tt)*) => {
        async fn $name(State(app): State<App>, Path($field): Path<String>) -> ApiResult<$out> {
            api::$name(&app, $params { $field })$(.$aw)?.map(Json).map_err(Into::into)
        }
        handlers!($($rest)*);
    };
    ($name:ident($($ex:ident($p:ident): $params:ident)?) $(.$aw:tt)? -> $out:ident; $($rest:tt)*) => {
        async fn $name(State(app): State<App> $(, $ex($p): $ex<$params>)?) -> ApiResult<$out> {
            api::$name(&app $(, $p)?)$(.$aw)?.map(Json).map_err(Into::into)
        }
        handlers!($($rest)*);
    };
}

handlers! {
    get_provider(ProviderParams { provider }) -> ProviderInfo;
    search_prices(Query(p): SearchPricesParams).await -> Prices;
    estimate_cost(Json(p): EstimateCostParams).await -> Estimate;
    generate(Json(p): GenerateParams).await -> Job;
    preflight(Json(p): PreflightParams).await -> Preflight;
    doctor(Query(p): DoctorParams).await -> Doctor;
    cancel_job(JobParams { job_id }) -> Job;
    generate_batch(Json(p): GenerateBatchParams).await -> Batch;
    get_spec(ProviderParams { provider }) -> SpecText;
    reset_spec(ProviderParams { provider }) -> SpecReset;
    list_keys() -> Keys;
    test_key(ProviderParams { provider }).await -> KeyTest;
    check_spec(Json(p): CheckSpecParams).await -> SpecCheck;
    save_spec(Json(p): SaveSpecParams).await -> SpecSaved;
    list_jobs(Query(p): ListJobsParams) -> Jobs;
    search_assets(Query(p): SearchAssetsParams) -> Assets;
    import_asset(Json(p): ImportAssetParams).await -> Imported;
    get_asset(AssetParams { asset_id }).await -> Asset;
    delete_asset(AssetParams { asset_id }) -> Asset;
    get_lineage(AssetParams { asset_id }) -> Lineage;
    list_references(Query(p): ListReferencesParams) -> References;
    save_reference(Json(p): SaveReferenceParams) -> Reference;
    get_reference(ReferenceParams { reference }) -> Reference;
    delete_reference(ReferenceParams { reference }) -> Reference;
    list_collections() -> Collections;
    similar_assets(Query(p): SimilarAssetsParams).await -> SimilarAssets;
    review_asset(Json(p): ReviewAssetParams) -> Assets;
    get_stats(Query(p): StatsParams) -> Stats;
    list_budgets() -> Budgets;
    set_budget(Json(p): SetBudgetParams) -> Budget;
    delete_budget(BudgetParams { name }) -> Budget;
    list_snippets() -> Snippets;
    save_snippet(Json(p): SaveSnippetParams) -> Snippet;
    delete_snippet(NameParams { name }) -> Snippet;
    list_recipes() -> RecipeList;
    save_recipe(Json(p): SaveRecipeParams) -> Recipe;
    get_recipe(NameParams { name }) -> Recipe;
    delete_recipe(NameParams { name }) -> Recipe;
    list_prompts() -> PromptList;
    extract_frame(Json(p): ExtractFrameParams).await -> Edited;
    reframe(Json(p): ReframeParams).await -> Edited;
    trim(Json(p): TrimParams).await -> Edited;
    concat(Json(p): ConcatParams).await -> Edited;
    overlay(Json(p): OverlayParams).await -> Edited;
    mux_audio(Json(p): MuxAudioParams).await -> Edited;
}

async fn list_providers(State(app): State<App>) -> Json<Providers> {
    Json(api::list_providers(&app))
}

#[derive(Deserialize)]
struct WaitQuery {
    wait_secs: Option<u64>,
    debug: Option<bool>,
}

async fn get_job(State(app): State<App>, Path(job_id): Path<String>, Query(q): Query<WaitQuery>) -> ApiResult<Job> {
    Ok(Json(api::get_job(&app, GetJobParams { job_id, wait_secs: q.wait_secs, debug: q.debug }).await?))
}

async fn get_batch(State(app): State<App>, Path(batch_id): Path<String>, Query(q): Query<WaitQuery>) -> ApiResult<Batch> {
    Ok(Json(api::get_batch(&app, GetBatchParams { batch_id, wait_secs: q.wait_secs }).await?))
}

#[derive(Deserialize)]
struct AssetPatch {
    tags: Option<String>,
    description: Option<String>,
}

async fn update_asset(State(app): State<App>, Path(asset_id): Path<String>, Json(b): Json<AssetPatch>) -> ApiResult<Asset> {
    Ok(Json(api::update_asset(&app, UpdateAssetParams { asset_id, tags: b.tags, description: b.description })?))
}

#[derive(Deserialize)]
struct AssetIds {
    asset_ids: Vec<String>,
    role: Option<String>,
    description: Option<String>,
}

async fn add_to_reference(State(app): State<App>, Path(reference): Path<String>, Json(b): Json<AssetIds>) -> ApiResult<Reference> {
    Ok(Json(api::add_to_reference(&app, ReferenceAssetsParams { reference, asset_ids: b.asset_ids, role: b.role })?))
}

async fn remove_from_reference(State(app): State<App>, Path(reference): Path<String>, Json(b): Json<AssetIds>) -> ApiResult<Reference> {
    Ok(Json(api::remove_from_reference(&app, ReferenceAssetsParams { reference, asset_ids: b.asset_ids, role: None })?))
}

async fn add_to_collection(State(app): State<App>, Path(collection): Path<String>, Json(b): Json<AssetIds>) -> ApiResult<Collection> {
    let p = CollectionAssetsParams { collection, asset_ids: b.asset_ids, description: b.description };
    Ok(Json(api::add_to_collection(&app, p)?))
}

async fn remove_from_collection(State(app): State<App>, Path(collection): Path<String>, Json(b): Json<AssetIds>) -> ApiResult<Collection> {
    let p = CollectionAssetsParams { collection, asset_ids: b.asset_ids, description: None };
    Ok(Json(api::remove_from_collection(&app, p)?))
}

#[derive(Deserialize)]
struct ExportQuery {
    only_approved: Option<bool>,
}

/// Builds the ZIP and sends it; it also stays in `<home>/exports/`.
async fn export_collection(
    State(app): State<App>,
    Path(collection): Path<String>,
    Query(q): Query<ExportQuery>,
) -> Result<Response, ApiError> {
    let export = api::export_collection(&app, CollectionParams { collection, only_approved: q.only_approved }).await?;
    let name = std::path::Path::new(&export.path).file_name().and_then(|n| n.to_str()).unwrap_or("export.zip").to_owned();
    let data = tokio::fs::read(&export.path).await?;
    let disposition = format!("attachment; filename=\"{name}\"");
    Ok(([(header::CONTENT_TYPE, "application/zip".to_owned()), (header::CONTENT_DISPOSITION, disposition)], data).into_response())
}

async fn asset_file(State(app): State<App>, Path(asset_id): Path<String>) -> Result<Response, ApiError> {
    let asset = assets::get(&app, &asset_id)?;
    let data = tokio::fs::read(&asset.path).await?;
    let mime = asset.mime.unwrap_or_else(|| "application/octet-stream".into());
    Ok(([(header::CONTENT_TYPE, mime)], data).into_response())
}

async fn asset_preview(State(app): State<App>, Path(asset_id): Path<String>) -> Result<Response, ApiError> {
    let asset = api::get_asset(&app, AssetParams { asset_id }).await?;
    let path = asset.preview.ok_or_else(|| NotFound(format!("no preview for `{}` (audio, or ffmpeg missing)", asset.id)))?;
    Ok(([(header::CONTENT_TYPE, "image/jpeg")], tokio::fs::read(path).await?).into_response())
}

#[derive(Deserialize)]
struct UploadBody {
    provider: String,
}

async fn upload_asset(
    State(app): State<App>,
    Path(asset_id): Path<String>,
    Json(b): Json<UploadBody>,
) -> ApiResult<Uploaded> {
    Ok(Json(api::upload_asset(&app, UploadAssetParams { asset_id, provider: b.provider }).await?))
}

#[derive(Deserialize)]
struct RunBody {
    vars: Option<Map<String, Value>>,
    save_to: Option<SaveTo>,
    budget: Option<String>,
    estimated_cost_usd: Option<f64>,
    offer_id: Option<String>,
    retry: Option<Retry>,
    reuse: Option<bool>,
}

async fn run_recipe(State(app): State<App>, Path(name): Path<String>, Json(b): Json<RunBody>) -> ApiResult<Job> {
    let p = RunRecipeParams { name, vars: b.vars, save_to: b.save_to, budget: b.budget, estimated_cost_usd: b.estimated_cost_usd, offer_id: b.offer_id, retry: b.retry, reuse: b.reuse };
    Ok(Json(api::run_recipe(&app, p).await?))
}

/// Prompt arguments as query parameters, e.g. `/v1/prompts/product_shoot?product=bottle`.
async fn get_prompt(
    State(app): State<App>,
    Path(name): Path<String>,
    Query(args): Query<std::collections::BTreeMap<String, String>>,
) -> ApiResult<PromptText> {
    let arguments = args.into_iter().map(|(k, v)| (k, Value::String(v))).collect();
    Ok(Json(api::get_prompt(&app, GetPromptParams { name, arguments: Some(arguments) })?))
}
