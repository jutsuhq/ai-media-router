use rmcp::{
    ErrorData, Json, ServerHandler,
    handler::server::{prompt::PromptContext, router::tool::ToolRouter, wrapper::Parameters},
    model::{
        GetPromptResponse, GetPromptResult, Implementation, ListResourcesResult, PaginatedRequestParams, Prompt, PromptArgument,
        PromptMessage, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource, ResourceContents, Role,
        ServerCapabilities, ServerConfig,
    },
    prompt_handler,
    service::RequestContext,
    tool, tool_handler, tool_router, RoleServer,
};

use crate::{
    api::{self, *},
    app::App,
    jobs::{Job, batches::Batch, preflight::{Doctor, DoctorParams, Preflight}},
    library::{assets::Asset, collections::{Collection, Export}, lineage::Lineage, media::Edited, provenance::Imported, references::Reference},
    pricing::{budgets::Budget, costs::Estimate},
    templates::recipes::{Recipe, Snippet},
};

#[derive(Clone)]
pub struct Server {
    app: App,
    tool_router: ToolRouter<Self>,
}

#[tool_handler(router = self.tool_router)]
#[prompt_handler(router = Prompts(&self.app))]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        let capabilities = ServerCapabilities::builder().enable_tools().enable_prompts().enable_resources().build();
        ServerConfig::new(capabilities)
            .with_server_info(Implementation::new("ai-media-router", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS.to_owned())
    }

    async fn list_resources(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(resources(&self.app)))
    }

    async fn read_resource(
        &self,
        r: ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let contents = read_resource(&self.app, &r.uri).map_err(|e| ErrorData::resource_not_found(err(e), None))?;
        Ok(ReadResourceResult::new(vec![contents]).into())
    }
}

const INSTRUCTIONS: &str = "Generate images and video through the user's own provider API keys; every result is downloaded into a local asset library.\n\
        Generate: list_recipes; when a preset fits, generate {recipe, input} (it holds provider, model, offer and budget). Only when \
        choosing a new provider or model: search_prices (the user's configured providers) -> get_provider (model_hint + example show \
        what `model` and `input` mean there) -> generate, then offer save_recipe for the choice. Then get_job with wait_secs (repeat \
        while status is running) -> use assets[].path.\n\
        Missing key: list_keys gives the user a `keys set` command to run in their terminal; never ask for a key in the chat.\n\
        Reuse library files in input as whole string values: \"{{asset:<id>}}\" or \"{{ref:<name>}}\"; they are uploaded through the \
        provider (cached) before submit. In an array a ref expands to all its images, as a single value it is its first one.\n\
        Keep a character/style/product consistent: import_asset or a finished job's assets -> save_reference {name, kind, description, \
        asset_ids} -> generate with \"{{ref:<name>}}\" in input.\n\
        Example, one turn: \"find my character, make three cyberpunk images, keep them with her\": list_references {kind: \"character\"} \
        (or search_assets) -> generate x3 with input image \"{{ref:<name>}}\" and save_to {reference: \"<name>\", role: \"generated\", 
        collection: \"<project>\"} -> get_job with wait_secs. Outputs land in the reference and collection automatically.\n
        Review gate: review_asset {asset_ids, status: approved | rejected, rating 1-5, notes}; rejected assets are skipped by 
        {{ref:…}} expansion, search_assets {status, min_rating} filters, export_collection {only_approved} ships only approved ones. 
        get_stats {provider?, model?, collection?} shows per model what worked: done/failed share, failure kinds, ratings, approvals, 
        cost and seconds; check it before choosing a model.\n
        Failed jobs carry error_kind (moderation, auth, quota, rate_limit, invalid_input, not_found, provider_error, network, timeout, 
        spec, cancelled), retryable and a hint: rewrite the prompt, fix the key, wait, switch provider or fix the spec. 
        list_jobs {status: \"failed\", error_kind} finds them.\n
        Diagnose: doctor checks the setup (keys, ffmpeg, specs, stalled jobs); preflight takes generate's arguments (or {recipe, \
        vars} for every step of a recipe run) and checks them (placeholders, upload support, cost, budget) and renders the \
        requests without sending or spending anything. \
        retry {attempts, delay_secs} on generate resubmits after rate_limit, provider_error, network or timeout. \
        reuse: true on generate, run_recipe or batch defaults returns an identical earlier job's outputs at no cost \
        (reused_from names it); an identical call while the first still runs returns that job unless reuse: false.\n\
        Resources: ai-media-router://guide (this workflow in full), ai-media-router://providers/<key> (spec TOML), \
        ai-media-router://recipes/<name>.\n\
        Spend caps: set_budget {name, limit_usd}, then generate with budget and offer_id (or estimated_cost_usd); list_budgets shows what is left.\n\
        Cost: estimate_cost {provider, model, input, offer_id} prices a job from the offer (price x input duration or image count) \
        and the median of past jobs; generate {offer_id} stores that estimate on the job. estimate_cost {recipe, vars} sums a recipe's steps.\n\
        Many variants: generate_batch {defaults, items, matrix, label} (items x matrix, max 100 jobs, budget admits the total; \
        defaults/items take offer_id too) -> get_batch {batch_id, wait_secs}; a bench is one input over several provider/model items \
        with save_to.collection. generate_batch {recipe, matrix: {<variable>: [values]}} runs a saved recipe once per combination.\n\
        Chains: \"{{job:<id>}}\" / \"{{job:<id>:<n>}}\" in input uses another job's outputs, \"{{job:<id>:last_frame}}\" (or first_frame) \
        a frame of its video (still -> video -> next shot); while that job runs the new one is `waiting` and is submitted \
        automatically when it is done (failed if it fails). cancel_job stops a waiting or running job.\n\
        Memory: list_jobs {reference | collection} is the history, get_lineage {asset_id} shows what an asset was made from and what 
        was made from it; search_assets {query, collection} finds files by prompt, tags and description (update_asset sets them), 
        similar_assets finds them by meaning; export_collection packs a project into a ZIP.\n
        Edit locally with ffmpeg (each result is a new asset made by a `local` job, so lineage and save_to work): extract_frame 
        {asset_id, at: first|last|seconds} (e.g. the last frame of a clip as the start image of the next), reframe {aspect, mode: crop|pad}, 
        trim {start, end}, concat {asset_ids}, overlay {base_asset_id, overlay_asset_id, x, y, width} (a real packshot over a generated 
        plate), mux_audio {video_asset_id, audio_asset_id}. Assets carry width, height, duration_secs and has_audio.\n
        Errors name the provider, the step (submit/poll/result/upload), the spec file and the pointer that failed, plus what to do next.\n
        Fix a provider whose API changed (no rebuild, no restart): get_job with debug=true (raw request/response per step) -> get_spec 
        -> edit the TOML -> check_spec with spec=<text> and model/input (validates, replays recorded fixtures, renders the exact requests) 
        -> save_spec (active immediately) -> generate again. reset_spec returns to the built-in spec.\n\
        Prompt blocks: save_snippet {name, text} once, then \"{{snippet:<name>}}\", \"{{ref:<name>.description}}\" or \
        \"{{ref:<name>.tags}}\" anywhere inside an input string is replaced by that text at submit (the job keeps the placeholder).\n\
        Recipes: save_recipe {name, from_job} (or provider/model/input with \"{{var:<name>}}\") stores a working generation; \
        run_recipe {name, vars} reruns it with new values; list_recipes / get_recipe show them. A recipe can hold a whole chain: \
        save_recipe {name, steps: [{name, provider, model, input} | {name, op: trim|concat|mux_audio|…, input}]} with \
        \"{{step:<name>}}\" (or :<n>, :last_frame) for an earlier step's outputs, or save_recipe {name, from_job: <final job>, chain: true} \
        from its lineage. run_recipe then starts every step at once in one batch (later steps wait, a failed step fails the ones \
        after it) and returns the last step's job; get_batch {batch_id} follows the run. MCP prompts walk through whole \
        workflows (product_shoot, ugc_ad, character_consistency, localization_fanout, fix_provider) and every recipe (recipe:<name>).";

const SCHEME: &str = "ai-media-router://";
const GUIDE: &str = include_str!("../../skills/ai-media-router/SKILL.md");

/// The agent guide, every provider spec and every recipe. Spaces in recipe names are %20.
fn resources(app: &App) -> Vec<Resource> {
    let guide = Resource::new(format!("{SCHEME}guide"), "guide")
        .with_description("How to use AI Media Router: setup, generation loop, rules, placeholders, batches, recipes")
        .with_mime_type("text/markdown");
    let registry = app.registry();
    let specs = registry.iter().map(|p| {
        Resource::new(format!("{SCHEME}providers/{}", p.spec.key), format!("provider {}", p.spec.key))
            .with_description(format!("{} spec ({}, configured: {})", p.spec.name, p.origin.kind(), p.configured()))
            .with_mime_type("application/toml")
    });
    let recipes = api::list_recipes(app).map(|l| l.recipes).unwrap_or_default().into_iter().map(|r| {
        Resource::new(format!("{SCHEME}recipes/{}", r.name.replace(' ', "%20")), format!("recipe {}", r.name))
            .with_description(r.description)
            .with_mime_type("application/json")
    });
    std::iter::once(guide).chain(specs).chain(recipes).collect()
}

fn read_resource(app: &App, uri: &str) -> anyhow::Result<ResourceContents> {
    let path = uri.strip_prefix(SCHEME).unwrap_or_default();
    let (text, mime) = match path.split_once('/') {
        None if path == "guide" => (GUIDE.to_owned(), "text/markdown"),
        Some(("providers", key)) => (api::get_spec(app, ProviderParams { provider: key.into() })?.text, "application/toml"),
        Some(("recipes", name)) => {
            let recipe = api::get_recipe(app, NameParams { name: name.replace("%20", " ") })?;
            (serde_json::to_string_pretty(&recipe)?, "application/json")
        }
        _ => anyhow::bail!("unknown resource `{uri}`; resources/list shows them"),
    };
    Ok(ResourceContents::text(text, uri).with_mime_type(mime))
}

/// Prompt list built per request: the built-in workflows plus the current recipes.
struct Prompts<'a>(&'a App);

impl Prompts<'_> {
    fn list_all(&self) -> Vec<Prompt> {
        let prompts = api::list_prompts(self.0).map(|l| l.prompts).unwrap_or_else(|e| {
            tracing::warn!("cannot list recipes as prompts: {e:#}");
            Vec::new()
        });
        prompts
            .into_iter()
            .map(|p| {
                let args = p
                    .arguments
                    .into_iter()
                    .map(|a| PromptArgument::new(a.name).with_description(a.description).with_required(a.required))
                    .collect();
                Prompt::new(p.name, Some(p.description), Some(args))
            })
            .collect()
    }

    async fn get_prompt(&self, c: PromptContext<'_, Server>) -> Result<GetPromptResponse, ErrorData> {
        let p = GetPromptParams { name: c.name, arguments: c.arguments };
        let text = api::get_prompt(self.0, p).map_err(|e| ErrorData::invalid_params(err(e), None))?;
        let message = PromptMessage::new_text(Role::User, text.text);
        Ok(GetPromptResult::new(vec![message]).with_description(text.description).into())
    }
}

/// Tool failures go back to the agent as readable tool errors, not protocol errors.
fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

/// Each entry becomes a `#[tool]` method that calls the `api` function of the same name.
macro_rules! tools {
    ($($name:ident($($p:ident: $params:ident)?) $(.$aw:tt)? -> $out:ident: $description:tt;)*) => {
        #[tool_router]
        impl Server {
            pub fn new(app: App) -> Self {
                Self { app, tool_router: Self::tool_router() }
            }

            #[tool(description = "List providers, whether their API keys are set, and whether each runs the built-in spec or a local override.")]
            async fn list_providers(&self) -> Json<Providers> {
                Json(api::list_providers(&self.app))
            }

            $(
                #[tool(description = $description)]
                async fn $name(&self $(, Parameters($p): Parameters<$params>)?) -> Result<Json<$out>, String> {
                    api::$name(&self.app $(, $p)?)$(.$aw)?.map(Json).map_err(err)
                }
            )*
        }
    };
}

tools! {
    get_provider(p: ProviderParams) -> ProviderInfo:
        "How to call one provider: model id format, an example generate call, docs link, upload support, known quirks.";
    list_keys() -> Keys:
        "Every provider key any loaded spec uses: whether it is set, where it comes from, and the command that sets it. \
        Never returns key values (only the last 4 characters), so it is safe to show the user; keys are added by the user \
        in a terminal.";
    test_key(p: ProviderParams).await -> KeyTest:
        "Check one provider's key with the spec's free [auth_check] request (account, balance or model list): accepted, \
        refused, not_configured, not_supported or inconclusive. Spends nothing and never returns the key.";
    search_prices(p: SearchPricesParams).await -> Prices:
        "Find video (USD per second) and image (USD per image) generation offers across providers, cheapest first; \
        kind filters one of them. Offers carry provider_key and offer_id for generate.";
    estimate_cost(p: EstimateCostParams).await -> Estimate:
        "Estimate a generation's USD cost: an offer's price (offer_id from search_prices) times the duration or image \
        count read from input, plus the median of past jobs on this provider and model. Says which formula and input fields it used. \
        With recipe (and vars) instead: every step of that recipe and the sum.";
    generate(p: GenerateParams).await -> Job:
        "Start a generation. Returns the job immediately (status running); poll it with get_job. Results are downloaded into the library. \
        Library files go into input as \"{{asset:<id>}}\" / \"{{ref:<name>}}\"; save_to files the outputs into a collection and/or reference. \
        `recipe` names a saved single-generation preset (list_recipes shows the user's presets), so provider and model can be omitted.";
    preflight(p: PreflightParams).await -> Preflight:
        "Dry run of generate with the same arguments: checks the provider and keys, snippets and placeholders (assets, \
        references, parent jobs), upload support, save_to, cost and budget, and renders the exact HTTP requests. Sends nothing, \
        spends nothing; `ready` says whether generate would start. Use it before an expensive or unfamiliar generation. \
        With recipe and vars instead: every step of run_recipe (checks named <step>/<check>), the summed estimate and the budget.";
    doctor(p: DoctorParams).await -> Doctor:
        "Check the installation: data directory, database, ffmpeg/ffprobe, which providers have keys, spec override errors, \
        recorded fixtures of configured providers, jobs no worker is polling, and the last day's failure kinds. online=true \
        also pings every configured provider. Run it first when something does not work.";
    generate_batch(p: GenerateBatchParams).await -> Batch:
        "Start up to 100 jobs at once: items (e.g. one prompt on several provider/model pairs) x matrix \
        (top-level input fields to vary). Item input merges over defaults.input. With defaults.budget the sum of estimates is \
        admitted first. Returns the batch; follow it with get_batch.";
    get_batch(p: GetBatchParams).await -> Batch:
        "A batch's jobs per status, each job's outputs, and every output file. wait_secs blocks until all finish (max 50).";
    cancel_job(p: JobParams) -> Job:
        "Cancel a waiting or running job: it becomes failed with `cancelled`, and jobs waiting on it fail too. \
        The provider is not asked to stop, so a running generation may still bill there.";
    get_job(p: GetJobParams).await -> Job:
        "Job status, provider progress and stored result files. wait_secs blocks until done (max 50); debug=true adds the raw provider exchanges.";
    list_jobs(p: ListJobsParams) -> Jobs:
        "Generation history, newest first, with provider, model, input, cost, input assets and outputs. Filter by reference, collection, status or error_kind.";
    search_assets(p: SearchAssetsParams) -> Assets:
        "Search the local asset library by prompt, tags, description and review notes, optionally within one collection, \
        by review status or minimum rating.";
    import_asset(p: ImportAssetParams).await -> Imported:
        "Copy a local file into the asset library, e.g. a reference image. Files made by ai-media-router carry their provider, model,         input, job and parent assets: import restores prompt, tags and lineage, and returns that provenance.";
    upload_asset(p: UploadAssetParams).await -> Uploaded:
        "Upload a library asset to a provider; returns the reference (URL, file id or data URI) and where to put it in generate input. \
        Usually not needed: \"{{asset:<id>}}\" in generate input does this automatically.";
    get_asset(p: AssetParams).await -> Asset:
        "Metadata and local path of one asset.";
    update_asset(p: UpdateAssetParams) -> Asset:
        "Replace an asset's tags and/or description (both are searched by search_assets).";
    review_asset(p: ReviewAssetParams) -> Assets:
        "Review assets: status approved/rejected/draft, rating 1-5, notes. Rejected assets are skipped when a \
        {{ref:…}} expands; export_collection {only_approved} ships only approved ones; get_stats averages ratings per model.";
    get_stats(p: StatsParams) -> Stats:
        "Per provider and model: jobs, done/failed share, failure kinds, average rating, approved share, average USD cost \
        and seconds to finish. Filter by provider, model or collection. Use it to pick a model that worked before.";
    delete_asset(p: AssetParams) -> Asset:
        "Delete an asset's file and library entry; it leaves every reference and collection. Jobs keep their record of it.";
    similar_assets(p: SimilarAssetsParams).await -> SimilarAssets:
        "Find assets similar in meaning to a text query or to another asset (embeddings of prompt, tags and description). \
        Needs EMBEDDINGS_API_KEY or EMBEDDINGS_URL in config; search_assets is the keyword search.";
    get_lineage(p: AssetParams) -> Lineage:
        "Generation graph of an asset: the assets and references it was made from (ancestors) and everything made from it (descendants), with the job of each step.";
    save_reference(p: SaveReferenceParams) -> Reference:
        "Create or update a reference: a named, typed set of assets (character, style, product, environment, composition, brand) \
        plus a description, reused in generate input as \"{{ref:<name>}}\". asset_ids, when given, replaces its assets.";
    get_reference(p: ReferenceParams) -> Reference:
        "One reference by name or id, with its assets in order and their local paths.";
    list_references(p: ListReferencesParams) -> References:
        "List references, most recently changed first; filter by kind or a substring of name, description or tags.";
    add_to_reference(p: ReferenceAssetsParams) -> Reference:
        "Append assets to a reference, optionally with a role (e.g. \"side\").";
    remove_from_reference(p: ReferenceAssetsParams) -> Reference:
        "Remove assets from a reference; they stay in the library.";
    delete_reference(p: ReferenceParams) -> Reference:
        "Delete a reference; its assets stay in the library.";
    list_collections() -> Collections:
        "List collections (projects) with their asset counts. search_assets {collection} lists one's assets.";
    add_to_collection(p: CollectionAssetsParams) -> Collection:
        "Add assets to a collection, creating it when missing.";
    remove_from_collection(p: CollectionAssetsParams) -> Collection:
        "Remove assets from a collection (not from the library); an emptied collection disappears.";
    export_collection(p: CollectionParams).await -> Export:
        "Export a collection as a ZIP in <home>/exports/: the files, their previews, and collection.json with each \
        asset's metadata (review status, rating, notes), the job that made it (provider, model, input, cost) and its references. \
        only_approved=true ships only assets approved with review_asset.";
    set_budget(p: SetBudgetParams) -> Budget:
        "Create a spending cap in USD, or change its limit. Jobs started with generate {budget, estimated_cost_usd} \
        count against it and are refused when they would exceed it.";
    list_budgets() -> Budgets:
        "Budgets with their limit, spent and remaining USD.";
    delete_budget(p: BudgetParams) -> Budget:
        "Delete a budget; its jobs stay in the history.";
    extract_frame(p: ExtractFrameParams).await -> Edited:
        "Save one frame of a video as a PNG asset: at \"first\", \"last\" or seconds. Use the last frame as the start image of the next clip.";
    reframe(p: ReframeParams).await -> Edited:
        "Change an image's or video's aspect ratio (e.g. 16:9 to 9:16) by centered crop or black padding; returns the new asset.";
    trim(p: TrimParams).await -> Edited:
        "Cut a video or audio asset to start..end seconds; returns the new asset.";
    concat(p: ConcatParams).await -> Edited:
        "Join two or more video assets in order into one MP4 (first clip's size, 30 fps); returns the new asset.";
    overlay(p: OverlayParams).await -> Edited:
        "Draw an image (packshot, logo) over an image or video at x, y pixels, optionally scaled to width; returns the new asset.";
    mux_audio(p: MuxAudioParams).await -> Edited:
        "Put an audio track (audio asset or another video's sound) under a video, replacing its sound; returns the new asset.";
    get_spec(p: ProviderParams) -> SpecText:
        "Read a provider's active spec (TOML), whether it is built-in or a local override, and its fixture names. First step of fixing a provider.";
    check_spec(p: CheckSpecParams).await -> SpecCheck:
        "Check a spec without saving or calling the provider: validates it (pointers, template variables), replays recorded fixtures, and with model/input renders the exact HTTP requests it would send.";
    save_spec(p: SaveSpecParams).await -> SpecSaved:
        "Save a spec as a local override in <home>/providers/ (or add a new provider). Active immediately in every AI Media Router process; returns the check result.";
    save_snippet(p: SaveSnippetParams) -> Snippet:
        "Create or replace a snippet: a reusable prompt block (brand style, series paragraph, voice) used as \
        \"{{snippet:<name>}}\" inside any string of generate input.";
    list_snippets() -> Snippets:
        "List snippets with their text.";
    delete_snippet(p: NameParams) -> Snippet:
        "Delete a snippet; inputs that still use it fail with the list of known snippets.";
    save_recipe(p: SaveRecipeParams) -> Recipe:
        "Save a generation template: from_job copies a working job, or give provider, model and input. \
        \"{{var:<name>}}\" in input marks what changes per run; every recipe also appears as the MCP prompt recipe:<name>. \
        A chain: `steps` (generations and ffmpeg edits linked by \"{{step:<name>}}\"), or from_job / from_asset with chain: true \
        to take the final job's whole lineage.";
    list_recipes() -> RecipeList:
        "List recipes with their provider, model, input or steps, and variables.";
    get_recipe(p: NameParams) -> Recipe:
        "One recipe by name.";
    run_recipe(p: RunRecipeParams).await -> Job:
        "Start a generation from a recipe with values for its variables (defaults fill the rest). Returns the job like generate. \
        A recipe with steps starts every step at once in one batch, admitting the sum of their estimates against `budget`; it \
        returns the last step's job, and get_batch {batch_id} follows the whole run.";
    delete_recipe(p: NameParams) -> Recipe:
        "Delete a recipe; jobs made from it stay in the history.";
    reset_spec(p: ProviderParams) -> SpecReset:
        "Delete a provider's local override so the built-in spec is used again.";
}

#[cfg(test)]
mod tests {
    use rmcp::ServiceExt;
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;

    #[tokio::test]
    async fn prompts_are_listed_and_rendered_over_mcp() {
        let home = std::env::temp_dir().join(format!("ai-media-router-prompts-{}", uuid::Uuid::new_v4().simple()));
        let app = App::init(Some(home.clone())).unwrap();
        let recipe = SaveRecipeParams {
            name: "teaser".into(),
            from_job: None,
            description: Some("5 s teaser".into()),
            provider: Some("kie".into()),
            model: Some("m".into()),
            input: Some(json!({ "prompt": "{{var:subject}} at dusk" }).as_object().unwrap().clone()),
            save_to: None,
            variables: None,
            ..Default::default()
        };
        api::save_recipe(&app, recipe).unwrap();

        let (server_io, client_io) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move { Server::new(app).serve(server_io).await.unwrap().waiting().await });
        let (read, mut write) = tokio::io::split(client_io);
        let mut lines = BufReader::new(read).lines();
        let mut next_id = 0;
        let mut request = async |method: &str, params: Value| -> Value {
            next_id += 1;
            let msg = json!({ "jsonrpc": "2.0", "id": next_id, "method": method, "params": params });
            write.write_all(format!("{msg}\n").as_bytes()).await.unwrap();
            if method == "initialize" {
                let done = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
                write.write_all(format!("{done}\n").as_bytes()).await.unwrap();
            }
            loop {
                let reply: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                if reply["id"] == next_id {
                    return reply;
                }
            }
        };

        let init = json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } });
        let init = request("initialize", init).await;
        assert!(init["result"]["capabilities"]["prompts"].is_object(), "{init}");
        assert!(init["result"]["capabilities"]["tools"].is_object());

        let list = request("prompts/list", json!({})).await;
        let prompts = list["result"]["prompts"].as_array().unwrap();
        let names: Vec<&str> = prompts.iter().map(|p| p["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            ["product_shoot", "ugc_ad", "character_consistency", "localization_fanout", "fix_provider", "recipe:teaser"]
        );
        assert_eq!(prompts[5]["arguments"], json!([{ "name": "subject", "description": "", "required": true }]));

        let shoot = request("prompts/get", json!({ "name": "product_shoot", "arguments": { "product": "bottle" } })).await;
        let text = shoot["result"]["messages"][0]["content"]["text"].as_str().unwrap();
        for step in ["get_reference", "save_reference", "generate", "{{ref:bottle.description}}", "get_job", "save_recipe"] {
            assert!(text.contains(step), "{step} missing: {text}");
        }
        let teaser = request("prompts/get", json!({ "name": "recipe:teaser", "arguments": { "subject": "a fox" } })).await;
        let text = teaser["result"]["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(text.contains("run_recipe {\"name\":\"teaser\",\"vars\":{\"subject\":\"a fox\"}}"), "{text}");
        let missing = request("prompts/get", json!({ "name": "ugc_ad" })).await;
        assert!(missing["error"]["message"].as_str().unwrap().contains("needs product"), "{missing}");
        let unknown = request("prompts/get", json!({ "name": "nope" })).await;
        assert!(unknown["error"]["message"].as_str().unwrap().contains("built-in prompts: product_shoot"), "{unknown}");

        assert!(init["result"]["capabilities"]["resources"].is_object(), "{init}");
        let list = request("resources/list", json!({})).await;
        let uris: Vec<&str> = list["result"]["resources"].as_array().unwrap().iter().map(|r| r["uri"].as_str().unwrap()).collect();
        for uri in ["ai-media-router://guide", "ai-media-router://providers/kie", "ai-media-router://recipes/teaser"] {
            assert!(uris.contains(&uri), "{uri} missing: {uris:?}");
        }
        let mut read = async |uri: &str| request("resources/read", json!({ "uri": uri })).await;
        let guide = read("ai-media-router://guide").await;
        assert!(guide["result"]["contents"][0]["text"].as_str().unwrap().contains("preflight"), "{guide}");
        let spec = read("ai-media-router://providers/kie").await;
        assert!(spec["result"]["contents"][0]["text"].as_str().unwrap().contains("key = \"kie\""), "{spec}");
        let recipe = read("ai-media-router://recipes/teaser").await;
        assert!(recipe["result"]["contents"][0]["text"].as_str().unwrap().contains("{{var:subject}} at dusk"), "{recipe}");
        let missing = read("ai-media-router://recipes/nope").await;
        assert!(missing["error"]["message"].as_str().unwrap().contains("nope"), "{missing}");
        std::fs::remove_dir_all(&home).ok();
    }
}
