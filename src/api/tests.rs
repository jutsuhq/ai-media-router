use std::{
    future::Future,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};

use super::*;

/// A scratch home with a config.toml, removed when dropped.
struct Home(PathBuf);

impl Home {
    fn new(env: &str) -> Self {
        let home = std::env::temp_dir().join(format!("ai-media-router-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&home).unwrap();
        let home = Self(home);
        home.config(env);
        home
    }

    fn config(&self, env: &str) {
        std::fs::write(self.0.join("config.toml"), format!("[env]\n{env}")).unwrap();
    }
}

impl std::ops::Deref for Home {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn start(home: &Home) -> App {
    let app = App::init(Some(home.to_path_buf())).unwrap();
    tokio::spawn(jobs::run_worker(app.clone()));
    app
}

/// Serves the router built for its own base URL; returns that URL.
async fn serve(router: impl FnOnce(&str) -> Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = router(&base);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    base
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

fn into_collection(collection: &str) -> SaveTo {
    SaveTo { collection: Some(collection.into()), reference: None, role: None }
}

fn in_collection(collection: &str) -> SearchAssetsParams {
    SearchAssetsParams { collection: Some(collection.into()), ..Default::default() }
}

fn error_of<T>(r: Result<T>) -> String {
    match r {
        Ok(_) => panic!("expected an error"),
        Err(e) => format!("{e:#}"),
    }
}

/// Polls `check` until it returns Some, failing after 20 s.
async fn until<T, F: Future<Output = Option<T>>>(mut check: impl FnMut() -> F) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = check().await {
            return v;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "timed out");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The job once it left `running` and `waiting`.
async fn settled(app: &App, id: &str) -> Job {
    until(|| async {
        let job = get_job(app, GetJobParams { job_id: id.into(), ..Default::default() }).await.unwrap();
        (!matches!(job.status.as_str(), "running" | "waiting")).then_some(job)
    })
    .await
}

async fn finished(app: &App, job: Job) -> Job {
    let job = settled(app, &job.id).await;
    assert_eq!(job.status, "done", "{:?}", job.error);
    job
}

/// A kie-like API whose submit response renamed `taskId` to `task`.
async fn changed_api() -> String {
    serve(|base| {
        let result = json!({ "resultUrls": [format!("{base}/file.mp4")] }).to_string();
        let info = json!({ "code": 200, "data": { "state": "success", "resultJson": result } });
        Router::new()
            .route("/api/v1/jobs/createTask", post(|| async { Json(json!({ "code": 200, "data": { "task": "t1" } })) }))
            .route("/api/v1/jobs/recordInfo", get(move || async move { Json(info) }))
            .route("/file.mp4", get(|| async { ([("content-type", "video/mp4")], b"mp4".to_vec()) }))
    })
    .await
}

#[tokio::test]
async fn agent_fixes_a_provider_without_restart() {
    let home = Home::new("KIE_API_KEY = \"k\"\n");
    let app = start(&home);

    let base = changed_api().await;
    let spec = builtin_spec("kie").unwrap().1.replace("https://api.kie.ai", &base);
    let saved = save_spec(&app, SaveSpecParams { spec: spec.clone() }).await.unwrap();
    assert_eq!(saved.replaced, "builtin");
    assert_eq!(get_provider(&app, ProviderParams { provider: "kie".into() }).unwrap().source, "override");

    let generate_params = || GenerateParams { provider: "kie".into(), model: "m".into(), input: obj(json!({ "prompt": "cat" })), ..Default::default() };
    let err = error_of(generate(&app, generate_params()).await);
    assert!(err.contains("kie submit (override") && err.contains("`/data/taskId` matched nothing"), "{err}");
    let failed = list_jobs(&app, ListJobsParams { limit: Some(1), ..Default::default() }).unwrap().jobs.remove(0);
    let job = get_job(&app, GetJobParams { job_id: failed.id, debug: Some(true), ..Default::default() }).await.unwrap();
    assert_eq!(job.trace.unwrap()[0]["response"]["body"]["data"]["task"], "t1", "trace shows the raw response");

    let fixed = spec.replace("\"/data/taskId\"", "\"/data/task\"");
    let check = CheckSpecParams { spec: Some(fixed.clone()), model: Some("m".into()), ..Default::default() };
    let check = check_spec(&app, check).await.unwrap();
    assert!(check.valid, "{:?}", check.error);
    assert_eq!(check.requests[0].request.url, format!("{base}/api/v1/jobs/createTask"));
    save_spec(&app, SaveSpecParams { spec: fixed }).await.unwrap();

    let job = finished(&app, generate(&app, generate_params()).await.unwrap()).await;
    assert_eq!(job.assets[0].bytes, 3);

    assert_eq!(reset_spec(&app, ProviderParams { provider: "kie".into() }).unwrap().source, "builtin");
}

const MOCK_SPEC: &str = r#"
key = "mock"
name = "Mock"
catalog = ["mockcloud"]
docs = "-"
model_hint = "-"
base_url = "BASE"

[auth]
type = "bearer"
env = ["MOCK_KEY"]

[submit]
url = "/submit"
body = { model = "{{model}}", input = "{{input}}" }
outputs = ["/out"]

[upload]
type = "multipart"
url = "/upload"
result = "/url"
ttl_secs = 86400
"#;

/// MOCK_SPEC under another key, served from `base`.
fn mock_spec(key: &str, base: &str) -> String {
    MOCK_SPEC.replace("key = \"mock\"", &format!("key = \"{key}\"")).replace("BASE", base)
}

#[derive(Clone, Default)]
struct Seen {
    uploads: Arc<AtomicUsize>,
    submits: Arc<Mutex<Vec<Value>>>,
}

impl Seen {
    fn uploads(&self) -> usize {
        self.uploads.load(Ordering::SeqCst)
    }

    fn input(&self, n: usize) -> Value {
        self.submits.lock().unwrap()[n]["input"].clone()
    }
}

/// Sync image API: every submit returns a different PNG; every upload a new hosted URL.
async fn mock_api(seen: Seen) -> String {
    let router = Router::new()
        .route(
            "/submit",
            post(|State(s): State<Seen>, Json(body): Json<Value>| async move {
                let mut submits = s.submits.lock().unwrap();
                submits.push(body);
                let png = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("img-{}", submits.len()));
                Json(json!({ "out": format!("data:image/png;base64,{png}") }))
            }),
        )
        .route(
            "/upload",
            post(|State(s): State<Seen>| async move {
                let n = s.uploads.fetch_add(1, Ordering::SeqCst) + 1;
                Json(json!({ "url": format!("https://cdn.test/up-{n}.png") }))
            }),
        )
        .with_state(seen);
    serve(|_| router).await
}

async fn mock_app() -> (App, Seen, Home) {
    let home = Home::new("MOCK_KEY = \"k\"\n");
    let app = start(&home);
    let seen = Seen::default();
    let base = mock_api(seen.clone()).await;
    save_spec(&app, SaveSpecParams { spec: MOCK_SPEC.replace("BASE", &base) }).await.unwrap();
    (app, seen, home)
}

async fn import(app: &App, home: &Path, name: &str) -> Asset {
    let path = home.join(name);
    std::fs::write(&path, name).unwrap();
    import_asset(app, ImportAssetParams { path: path.to_string_lossy().into(), ..Default::default() }).await.unwrap().asset
}

fn params(input: Value, save_to: Option<SaveTo>) -> GenerateParams {
    GenerateParams { provider: "mock".into(), model: "m".into(), input: obj(input), save_to, ..Default::default() }
}

fn hero(asset_ids: Vec<String>) -> SaveReferenceParams {
    SaveReferenceParams { name: "hero".into(), kind: Some(Kind::Character), asset_ids: Some(asset_ids), ..Default::default() }
}

fn placeholder(kind: &str, id: &str) -> String {
    format!("{{{{{kind}:{id}}}}}")
}

async fn fails(app: &App, p: GenerateParams) -> String {
    error_of(generate(app, p).await)
}

#[tokio::test]
async fn placeholders_upload_once_and_expand_by_context() {
    let (app, seen, home) = mock_app().await;
    let (a, b) = (import(&app, &home, "a.png").await, import(&app, &home, "b.png").await);
    save_reference(&app, SaveReferenceParams { description: Some("red-haired woman".into()), ..hero(vec![a.id.clone(), b.id.clone()]) })
        .unwrap();

    let input = json!({
        "prompt": "cat",
        "image_urls": ["{{ref:hero}}"],
        "image_url": "{{ref:hero}}",
        "nested": { "style": [{ "src": placeholder("asset", &b.id) }] },
    });
    let job = generate(&app, params(input.clone(), None)).await.unwrap();
    assert_eq!(seen.uploads(), 2);
    let sent = seen.input(0);
    let (up_a, up_b) = ("https://cdn.test/up-1.png", "https://cdn.test/up-2.png");
    assert_eq!(sent["image_urls"], json!([up_a, up_b]), "array element expands to every asset");
    assert_eq!(sent["image_url"], up_a, "single value is the first asset");
    assert_eq!(sent["nested"]["style"][0]["src"], up_b);
    assert_eq!(job.input, input, "the job keeps the placeholders, so it can be rerun");
    let inputs: Vec<_> = job.inputs.iter().map(|i| (i.asset_id.as_str(), i.reference.as_deref())).collect();
    assert_eq!(inputs, [(a.id.as_str(), Some("hero")), (b.id.as_str(), Some("hero"))]);

    let again = || GenerateParams { reuse: Some(false), ..params(input.clone(), None) };
    generate(&app, again()).await.unwrap();
    assert_eq!(seen.uploads(), 2, "second job reuses cached uploads");
    app.db.conn().execute("UPDATE uploads SET expires_at = 0", []).unwrap();
    generate(&app, again()).await.unwrap();
    assert_eq!(seen.uploads(), 4, "expired uploads are redone");

    let embedded = json!({ "prompt": format!("like {}", placeholder("asset", &a.id)) });
    let err = fails(&app, params(embedded, None)).await;
    assert!(err.contains("whole string"), "{err}");
    let err = fails(&app, params(json!({ "image": "{{ref:nobody}}" }), None)).await;
    assert!(err.contains("no reference `nobody`") && err.contains("save_reference"), "{err}");

    let no_upload = mock_spec("plain", "http://127.0.0.1:9");
    let no_upload = &no_upload[..no_upload.find("[upload]").unwrap()];
    save_spec(&app, SaveSpecParams { spec: no_upload.into() }).await.unwrap();
    let p = GenerateParams { provider: "plain".into(), ..params(json!({ "image": "{{ref:hero}}" }), None) };
    let err = fails(&app, p).await;
    assert!(err.contains("plain has no [upload]") && err.contains("public https URL"), "{err}");
}

#[tokio::test]
async fn save_to_files_outputs_and_lineage_links_them() {
    let (app, _, home) = mock_app().await;
    let a = import(&app, &home, "a.png").await;
    save_reference(&app, hero(vec![a.id.clone()])).unwrap();

    let save_to = SaveTo { collection: Some("proj".into()), reference: Some("hero".into()), role: Some("generated".into()) };
    let first = generate(&app, params(json!({ "image": "{{ref:hero}}" }), Some(save_to))).await.unwrap();
    let first = finished(&app, first).await;
    let b = &first.assets[0];
    let held = get_reference(&app, ReferenceParams { reference: "hero".into() }).unwrap();
    let held: Vec<_> = held.assets.iter().map(|r| (r.asset.id.as_str(), r.role.as_deref())).collect();
    assert_eq!(held, [(a.id.as_str(), None), (b.id.as_str(), Some("generated"))]);
    let in_proj = search_assets(&app, in_collection("proj")).unwrap();
    assert_eq!(in_proj.assets.iter().map(|x| &x.id).collect::<Vec<_>>(), [b.id.as_str()]);

    let second = generate(&app, params(json!({ "image": placeholder("asset", &b.id) }), None)).await.unwrap();
    let c = finished(&app, second).await.assets.remove(0);

    let links = |l: Vec<lineage::Link>| l.into_iter().map(|l| (l.asset.id, l.depth, l.reference)).collect::<Vec<_>>();
    let of_b = get_lineage(&app, AssetParams { asset_id: b.id.clone() }).unwrap();
    assert_eq!(links(of_b.ancestors), [(a.id.clone(), 1, Some("hero".into()))]);
    assert_eq!(links(of_b.descendants), [(c.id.clone(), 1, None)]);
    assert_eq!((of_b.references, of_b.collections), (vec!["hero".to_owned()], vec!["proj".to_owned()]));
    let of_c = get_lineage(&app, AssetParams { asset_id: c.id.clone() }).unwrap();
    assert_eq!(links(of_c.ancestors), [(b.id.clone(), 1, None), (a.id.clone(), 2, Some("hero".into()))]);
    let of_a = get_lineage(&app, AssetParams { asset_id: a.id.clone() }).unwrap();
    assert_eq!(links(of_a.descendants), [(b.id.clone(), 1, Some("hero".into())), (c.id.clone(), 2, None)]);

    let history = |reference: Option<&str>, collection: Option<&str>| {
        let p = ListJobsParams { reference: reference.map(Into::into), collection: collection.map(Into::into), ..Default::default() };
        list_jobs(&app, p).unwrap().jobs.into_iter().map(|j| j.id).collect::<Vec<_>>()
    };
    assert_eq!(history(Some("hero"), None), [first.id]);
    assert_eq!(history(None, Some("proj")).len(), 2, "one job wrote into proj, the other read from it");

    update_asset(&app, UpdateAssetParams { asset_id: c.id.clone(), tags: None, description: Some("neon alley".into()) }).unwrap();
    let found = search_assets(&app, SearchAssetsParams { query: Some("neon".into()), ..Default::default() }).unwrap();
    assert_eq!(found.assets[0].id, c.id);
    delete_asset(&app, AssetParams { asset_id: b.id.clone() }).unwrap();
    assert_eq!(get_reference(&app, ReferenceParams { reference: "hero".into() }).unwrap().assets.len(), 1);
    assert!(list_collections(&app).unwrap().collections.iter().all(|c| c.assets == 0));
}

#[tokio::test]
async fn budgets_refuse_jobs_that_would_overspend() {
    let (app, _, _home) = mock_app().await;
    let budgeted = |estimate: Option<f64>| GenerateParams {
        budget: Some("session".into()),
        estimated_cost_usd: estimate,
        ..params(json!({ "prompt": "cat" }), None)
    };
    assert!(fails(&app, budgeted(Some(0.1))).await.contains("no budget `session`"));
    set_budget(&app, SetBudgetParams { name: "session".into(), limit_usd: 1.0 }).unwrap();
    assert!(fails(&app, budgeted(None)).await.contains("needs estimated_cost_usd"));
    let job = generate(&app, budgeted(Some(0.6))).await.unwrap();
    assert_eq!(finished(&app, job).await.budget.as_deref(), Some("session"));
    assert!(fails(&app, budgeted(Some(0.5))).await.contains("would be exceeded: spent $0.6000 of $1.0000"));
    generate(&app, budgeted(Some(0.4))).await.unwrap();
    let b = list_budgets(&app).unwrap().budgets.remove(0);
    assert_eq!((b.jobs, (b.remaining_usd * 100.0).round()), (2, 0.0));
}

fn manifest(path: &str) -> Value {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    serde_json::from_reader(zip.by_name("collection.json").unwrap()).unwrap()
}

#[tokio::test]
async fn export_packs_files_and_job_metadata() {
    let (app, _, _home) = mock_app().await;
    let job = generate(&app, params(json!({ "prompt": "neon cat" }), Some(into_collection("my proj")))).await.unwrap();
    let job = finished(&app, job).await;
    let export = export_collection(&app, CollectionParams { collection: "my proj".into(), only_approved: None }).await.unwrap();
    assert_eq!(export.assets, 1);
    assert!(export.path.contains("my_proj-"), "{}", export.path);

    let mut zip = zip::ZipArchive::new(std::fs::File::open(&export.path).unwrap()).unwrap();
    let file = format!("files/{}", Path::new(&job.assets[0].path).file_name().unwrap().to_str().unwrap());
    assert!(zip.by_name(&file).is_ok(), "{file} missing");
    let manifest = manifest(&export.path);
    assert_eq!(manifest["collection"]["name"], "my proj");
    assert_eq!(manifest["assets"][0]["file"], file);
    assert_eq!(manifest["assets"][0]["job"]["input"]["prompt"], "neon cat");
    assert_eq!(manifest["assets"][0]["job"]["provider"], "mock");
}

/// Embeddings by word counts over a tiny vocabulary: texts sharing words point the same way.
async fn mock_embeddings() -> String {
    serve(|_| Router::new().route(
        "/embeddings",
        post(|Json(body): Json<Value>| async move {
            let data: Vec<Value> = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let t = t.as_str().unwrap().to_lowercase();
                    let v: Vec<f32> = ["cat", "dog", "neon", "forest"].iter().map(|w| t.matches(w).count() as f32 + 0.01).collect();
                    json!({ "index": i, "embedding": v })
                })
                .collect();
            Json(json!({ "data": data }))
        }),
    ))
    .await
}

#[tokio::test]
async fn similar_assets_rank_by_meaning() {
    let (app, _, home) = mock_app().await;
    let by_text = |query: &str, limit| SimilarAssetsParams { query: Some(query.into()), limit, ..Default::default() };
    assert!(error_of(similar_assets(&app, by_text("cat", None)).await).contains("EMBEDDINGS_API_KEY"));

    let base = mock_embeddings().await;
    home.config(&format!("MOCK_KEY = \"k\"\nEMBEDDINGS_URL = \"{base}\"\n"));
    app.providers.reload().unwrap();
    let mut ids = Vec::new();
    for (name, prompt) in [("1.png", "a cat in neon light"), ("2.png", "a dog in a forest"), ("3.png", "neon city")] {
        std::fs::write(home.join(name), name).unwrap();
        let p = ImportAssetParams { path: home.join(name).to_string_lossy().into(), tags: None, prompt: Some(prompt.into()) };
        ids.push(import_asset(&app, p).await.unwrap().asset.id);
    }
    let ranked = |r: SimilarAssets| r.assets.into_iter().map(|s| s.asset.id).collect::<Vec<_>>();
    assert_eq!(ranked(similar_assets(&app, by_text("forest dog", Some(1))).await.unwrap()), [ids[1].clone()]);
    let by_asset = SimilarAssetsParams { asset_id: Some(ids[2].clone()), ..Default::default() };
    let by_asset = ranked(similar_assets(&app, by_asset).await.unwrap());
    assert_eq!(by_asset.first(), Some(&ids[0]), "neon city is closest to the neon cat");
    assert!(!by_asset.contains(&ids[2]), "the asset itself is excluded");

    update_asset(&app, UpdateAssetParams { asset_id: ids[1].clone(), tags: None, description: Some("cat cat cat".into()) }).unwrap();
    let edited = ranked(similar_assets(&app, by_text("cat", Some(1))).await.unwrap());
    assert_eq!(edited, [ids[1].clone()], "edited text is re-embedded");
}

#[tokio::test]
async fn previews_need_ffmpeg_and_are_made_on_import() {
    if !ffmpeg_installed() {
        return;
    }
    let (app, _, home) = mock_app().await;
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, png).unwrap();
    std::fs::write(home.join("dot.png"), bytes).unwrap();
    let p = ImportAssetParams { path: home.join("dot.png").to_string_lossy().into(), ..Default::default() };
    let asset = import_asset(&app, p).await.unwrap().asset;
    let preview = asset.preview.expect("preview made on import");
    assert!(std::fs::read(&preview).unwrap().starts_with(&[0xFF, 0xD8]), "JPEG");
}

#[tokio::test]
async fn text_placeholders_resolve_at_submit_and_stay_in_the_job() {
    let (app, seen, home) = mock_app().await;
    let a = import(&app, &home, "a.png").await;
    let described = SaveReferenceParams {
        description: Some("red-haired woman".into()),
        tags: Some("neon cyberpunk".into()),
        ..hero(vec![a.id.clone()])
    };
    save_reference(&app, described).unwrap();
    let snippet = |name: &str, text: &str| SaveSnippetParams { name: name.into(), text: text.into(), description: None };
    save_snippet(&app, snippet("style", "teal light, {{ref:hero.tags}}")).unwrap();

    let input = json!({
        "prompt": "{{ref:hero.description}} in rain, {{snippet:style}}",
        "image": "{{ref:hero}}",
        "extra": ["{{ref:hero.tags}}"],
    });
    let job = finished(&app, generate(&app, params(input.clone(), None)).await.unwrap()).await;
    let sent = seen.input(0);
    assert_eq!(sent["prompt"], "red-haired woman in rain, teal light, neon cyberpunk");
    assert_eq!(sent["image"], "https://cdn.test/up-1.png", "whole-string ref stays a media placeholder");
    assert_eq!(sent["extra"], json!(["neon cyberpunk"]));
    assert_eq!(job.input, input, "the job keeps the placeholders");
    assert_eq!(job.assets[0].prompt.as_deref(), Some("red-haired woman in rain, teal light, neon cyberpunk"));

    let err = fails(&app, params(json!({ "prompt": "{{snippet:nope}}" }), None)).await;
    assert!(err.contains("no snippet `nope`; known snippets: style"), "{err}");
    let err = fails(&app, params(json!({ "prompt": "a {{ref:villain.description}}" }), None)).await;
    assert!(err.contains("no reference `villain`") && err.contains("known references: hero"), "{err}");
    assert!(error_of(save_snippet(&app, snippet("x", "{{snippet:style}}"))).contains("cannot include another snippet"));

    assert_eq!(list_snippets(&app).unwrap().snippets.len(), 1);
    delete_snippet(&app, NameParams { name: "style".into() }).unwrap();
    let err = fails(&app, params(json!({ "prompt": "{{snippet:style}}" }), None)).await;
    assert!(err.contains("no snippets exist yet"), "{err}");
}

/// Recipe "ad" on mock/m, filed into "ads".
fn ad_recipe(input: Value) -> SaveRecipeParams {
    SaveRecipeParams {
        name: "ad".into(),
        provider: Some("mock".into()),
        model: Some("m".into()),
        input: Some(obj(input)),
        save_to: Some(into_collection("ads")),
        ..Default::default()
    }
}

#[tokio::test]
async fn recipes_save_run_and_copy_jobs() {
    let (app, seen, _home) = mock_app().await;
    let variables = serde_json::from_value(json!({ "duration": { "description": "seconds", "default": 5 } })).unwrap();
    let save = SaveRecipeParams {
        description: Some("vertical ad".into()),
        variables: Some(variables),
        ..ad_recipe(json!({ "prompt": "cat says {{var:line}}", "duration": "{{var:duration}}" }))
    };
    let recipe = save_recipe(&app, save).unwrap();
    assert!(recipe.variables["line"].default.is_none(), "undeclared variable is added as required");

    let run = |vars: Value| RunRecipeParams { name: "ad".into(), vars: Some(obj(vars)), ..Default::default() };
    let job = finished(&app, run_recipe(&app, run(json!({ "line": "hallo" }))).await.unwrap()).await;
    assert_eq!(seen.input(0), json!({ "prompt": "cat says hallo", "duration": 5 }), "whole-string var keeps the JSON type");
    assert_eq!(job.save_to.unwrap().collection.as_deref(), Some("ads"));
    run_recipe(&app, run(json!({ "line": "salut", "duration": 10 }))).await.unwrap();
    assert_eq!(seen.input(1)["duration"], 10);

    let err = error_of(run_recipe(&app, run(json!({}))).await);
    assert!(err.contains("needs `line`") && err.contains("`duration` (seconds) default 5"), "{err}");
    assert!(error_of(run_recipe(&app, run(json!({ "line": "x", "lang": "de" }))).await).contains("has no variable `lang`"));
    let unknown = RunRecipeParams { name: "nope".into(), ..run(json!({})) };
    assert!(error_of(run_recipe(&app, unknown).await).contains("no recipe `nope`; known recipes: ad"));

    let copy = save_recipe(&app, SaveRecipeParams { name: "again".into(), from_job: Some(job.id.clone()), ..Default::default() }).unwrap();
    assert_eq!((copy.provider.as_deref(), copy.model.as_deref()), (Some("mock"), Some("m")));
    assert_eq!(copy.input, Some(json!({ "prompt": "cat says hallo", "duration": 5 })));
    assert!(copy.variables.is_empty());
    let incomplete = SaveRecipeParams { name: "x".into(), provider: None, ..ad_recipe(json!({})) };
    assert!(error_of(save_recipe(&app, incomplete)).contains("pass `provider`"));

    let names: Vec<_> = list_prompts(&app).unwrap().prompts.into_iter().map(|p| p.name).collect();
    assert!(names.contains(&"product_shoot".to_owned()) && names.contains(&"recipe:ad".to_owned()), "{names:?}");
    let prompt = |name: &str, arguments: Option<Value>| get_prompt(&app, GetPromptParams { name: name.into(), arguments: arguments.map(obj) });
    let text = prompt("recipe:ad", Some(json!({ "line": "hej" }))).unwrap().text;
    assert!(text.contains("run_recipe {\"name\":\"ad\",\"vars\":{\"line\":\"hej\"}}"), "{text}");
    assert!(error_of(prompt("recipe:ad", None)).contains("needs `line`"));
    assert!(error_of(prompt("fix_provider", None)).contains("needs provider"));

    assert_eq!(list_recipes(&app).unwrap().recipes.len(), 2);
    delete_recipe(&app, NameParams { name: "again".into() }).unwrap();
    assert_eq!(get_recipe(&app, NameParams { name: "ad".into() }).unwrap().description, "vertical ad");
}

#[tokio::test]
async fn generate_from_a_preset_recipe_defaults_and_charges_the_budget() {
    let (app, seen, home) = mock_app().await;
    write_prices(&home, json!([{ "family": "Mock", "model": "Mock Image", "provider": "mockcloud", "mode": "generate",
                                 "usdPerImage": 0.5, "source": "-" }]));
    let offer = offers(&app, "mock").await.remove(0).offer_id;
    set_budget(&app, SetBudgetParams { name: "session".into(), limit_usd: 2.0 }).unwrap();

    let recipe = save_recipe(&app, SaveRecipeParams {
        name: "draft".into(),
        provider: Some("mock".into()),
        model: Some("m".into()),
        input: Some(obj(json!({ "prompt": "cat", "style": "plain", "num_images": 2 }))),
        save_to: Some(into_collection("presets")),
        offer_id: Some(offer),
        budget: Some("session".into()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!((recipe.offer_id.is_some(), recipe.budget.as_deref()), (true, Some("session")));

    // provider and model come from the recipe; the given prompt wins over the recipe's.
    let job = generate(&app, GenerateParams { recipe: Some("draft".into()), input: obj(json!({ "prompt": "a fox in snow" })), ..Default::default() })
        .await
        .unwrap();
    assert_eq!((job.provider.as_str(), job.model.as_str()), ("mock", "m"));
    assert_eq!(job.save_to.as_ref().and_then(|s| s.collection.as_deref()), Some("presets"));
    assert_eq!(job.budget.as_deref(), Some("session"));
    assert_eq!(job.estimate_usd, Some(1.0), "offer price x 2 images");

    let job = finished(&app, job).await;
    assert_eq!(seen.input(0), json!({ "prompt": "a fox in snow", "style": "plain", "num_images": 2 }), "given prompt wins, recipe fields stay");
    assert_eq!(job.assets[0].mime.as_deref(), Some("image/png"));
    let b = list_budgets(&app).unwrap().budgets.remove(0);
    assert_eq!((b.jobs, (b.spent_usd * 100.0).round()), (1, 100.0));
}

#[tokio::test]
async fn generate_requires_provider_model_or_a_single_generation_recipe() {
    let (app, _, _home) = mock_app().await;

    let err = error_of(generate(&app, GenerateParams { input: obj(json!({ "prompt": "x" })), ..Default::default() }).await);
    assert!(err.contains("pass provider and model, or recipe"), "{err}");

    save_recipe(&app, chain_recipe("story", story_steps())).unwrap();
    let err = error_of(generate(&app, GenerateParams { recipe: Some("story".into()), input: obj(json!({ "prompt": "x" })), ..Default::default() }).await);
    assert!(err.contains("has 2 steps"), "{err}");
}

#[tokio::test]
async fn search_prices_defaults_to_the_configured_providers() {
    let (app, _, home) = mock_app().await;
    write_prices(&home, json!([
        { "family": "Mock", "model": "Mock Video", "provider": "mockcloud", "mode": "generate", "usdPerSec": 0.1, "source": "-" },
        { "family": "Other", "model": "Other Video", "provider": "nobody", "mode": "generate", "usdPerSec": 0.01, "source": "-" },
    ]));

    let scoped = search_prices(&app, SearchPricesParams::default()).await.unwrap();
    assert_eq!(scoped.total, 1, "only the configured mockcloud offer");
    assert!(scoped.offers.iter().all(|o| o.configured));
    assert!(scoped.hint.contains("configured providers"), "{}", scoped.hint);

    let all = search_prices(&app, SearchPricesParams { configured_only: Some(false), ..Default::default() }).await.unwrap();
    assert_eq!(all.total, 2, "configured_only: false lists every provider");

    let none = search_prices(&app, SearchPricesParams { query: Some("other".into()), ..Default::default() }).await.unwrap();
    assert_eq!(none.total, 0, "the configured-only default hides the unconfigured match");
    assert!(none.hint.contains("None of your configured providers offers this"), "{}", none.hint);
}

#[tokio::test]
async fn identical_outputs_of_two_jobs_show_on_both() {
    let (app, _, _home) = mock_app().await;
    let first = finished(&app, generate(&app, params(json!({ "prompt": "a" }), None)).await.unwrap()).await;
    let second = finished(&app, generate(&app, params(json!({ "prompt": "b" }), None)).await.unwrap()).await;
    let provider = app.provider("mock").unwrap();
    for job in [&first, &second] {
        let same = crate::providers::Output::Inline { data: b"same bytes".to_vec(), mime: Some("image/png".into()) };
        assets::store_output(&app, &provider, same, &job.id, None).await.unwrap();
    }
    for job in [first, second] {
        let job = get_job(&app, GetJobParams { job_id: job.id, ..Default::default() }).await.unwrap();
        assert_eq!(job.assets.len(), 2, "own output plus the shared file");
    }
}

#[tokio::test]
async fn review_gates_refs_search_and_export() {
    let (app, seen, home) = mock_app().await;
    let (a, b, c) = (import(&app, &home, "a.png").await, import(&app, &home, "b.png").await, import(&app, &home, "c.png").await);
    assert_eq!((a.status.as_str(), a.rating), ("draft", None));
    save_reference(&app, hero(vec![a.id.clone(), b.id.clone(), c.id.clone()])).unwrap();
    let proj = CollectionAssetsParams { collection: "proj".into(), asset_ids: vec![a.id.clone(), b.id.clone()], description: None };
    add_to_collection(&app, proj).unwrap();

    let review = |ids: &[&str], status: Option<ReviewStatus>, rating: Option<u8>, notes: Option<&str>| {
        let p = ReviewAssetParams { asset_ids: ids.iter().map(|s| s.to_string()).collect(), status, rating, notes: notes.map(Into::into) };
        review_asset(&app, p)
    };
    let reviewed = review(&[b.id.as_str()], Some(ReviewStatus::Rejected), Some(1), Some("extra fingers")).unwrap().assets.remove(0);
    assert_eq!((reviewed.status.as_str(), reviewed.rating, reviewed.review_notes.as_str()), ("rejected", Some(1), "extra fingers"));
    review(&[&a.id, &c.id], Some(ReviewStatus::Approved), Some(4), None).unwrap();
    review(&[c.id.as_str()], None, Some(5), None).unwrap();
    assert!(error_of(review(&[&a.id], None, Some(6), None)).contains("rating must be 1-5"));
    assert!(error_of(review(&[&a.id], None, None, None)).contains("pass status"));
    assert!(error_of(review(&["nope"], Some(ReviewStatus::Approved), None, None)).contains("no asset `nope`"));

    let ids = |p: SearchAssetsParams| {
        let mut ids = search_assets(&app, p).unwrap().assets.into_iter().map(|x| x.id).collect::<Vec<_>>();
        ids.sort();
        ids
    };
    let mut approved = vec![a.id.clone(), c.id.clone()];
    approved.sort();
    assert_eq!(ids(SearchAssetsParams { query: Some("fingers".into()), ..Default::default() }), [b.id.as_str()], "review notes are searched");
    assert_eq!(ids(SearchAssetsParams { status: Some(ReviewStatus::Rejected), ..Default::default() }), [b.id.as_str()]);
    assert_eq!(ids(SearchAssetsParams { status: Some(ReviewStatus::Approved), ..Default::default() }), approved);
    assert_eq!(ids(SearchAssetsParams { min_rating: Some(5), ..Default::default() }), [c.id.as_str()]);

    generate(&app, params(json!({ "image_urls": ["{{ref:hero}}"], "image": "{{ref:hero}}" }), None)).await.unwrap();
    let sent = seen.input(0);
    assert_eq!(sent["image_urls"].as_array().unwrap().len(), 2, "the rejected asset is skipped: {sent}");
    assert_eq!(seen.uploads(), 2);
    review(&[&a.id, &c.id], Some(ReviewStatus::Rejected), None, None).unwrap();
    let err = fails(&app, params(json!({ "image": "{{ref:hero}}" }), None)).await;
    assert!(err.contains("no usable assets") && err.contains("review_asset"), "{err}");
    review(&[&a.id], Some(ReviewStatus::Approved), None, None).unwrap();

    let export = |only_approved| export_collection(&app, CollectionParams { collection: "proj".into(), only_approved: Some(only_approved) });
    assert_eq!(export(false).await.unwrap().assets, 2);
    let only = export(true).await.unwrap();
    assert_eq!(only.assets, 1);
    let m = manifest(&only.path);
    assert_eq!((m["assets"][0]["id"].as_str(), m["assets"][0]["status"].as_str()), (Some(a.id.as_str()), Some("approved")));
    assert_eq!(m["assets"][0]["rating"], 4);
    review(&[&a.id], Some(ReviewStatus::Draft), None, None).unwrap();
    assert!(error_of(export(true).await).contains("no approved assets"));
}

fn item(model: &str, input: Value) -> BatchItem {
    BatchItem { model: Some(model.into()), input: Some(obj(input)), ..Default::default() }
}

fn mock_defaults(input: Value) -> BatchDefaults {
    BatchDefaults { provider: Some("mock".into()), input: Some(obj(input)), ..Default::default() }
}

/// The batch once none of its jobs is running or waiting.
async fn settled_batch(app: &App, id: &str) -> Batch {
    until(|| async {
        let batch = get_batch(app, GetBatchParams { batch_id: id.into(), wait_secs: None }).await.unwrap();
        batch.jobs.iter().all(|j| !matches!(j.status.as_str(), "running" | "waiting")).then_some(batch)
    })
    .await
}

#[tokio::test]
async fn batch_runs_items_times_matrix_and_goes_on_past_refusals() {
    let (app, seen, _home) = mock_app().await;
    let p = GenerateBatchParams {
        defaults: Some(BatchDefaults { save_to: Some(into_collection("bench")), ..mock_defaults(json!({ "prompt": "cat", "style": "plain" })) }),
        items: Some(vec![item("a", json!({})), item("b", json!({ "style": "noir" })), item("c", json!({ "image": "{{ref:nobody}}" }))]),
        matrix: Some([("seed".to_owned(), vec![json!(1), json!(2)])].into()),
        label: Some("bench".into()),
        recipe: None,
    };
    let batch = generate_batch(&app, p).await.unwrap();
    assert_eq!(batch.jobs.len(), 4);
    assert_eq!(batch.refused.iter().map(|r| r.index).collect::<Vec<_>>(), [4, 5], "item c x 2 seeds");
    assert!(batch.refused[0].error.contains("no reference `nobody`"), "{}", batch.refused[0].error);

    let batch = settled_batch(&app, &batch.id).await;
    assert_eq!(batch.counts.get("done"), Some(&4), "{:?}", batch.counts);
    assert_eq!(batch.outputs.len(), 4);
    let mut sent: Vec<_> = seen
        .submits
        .lock()
        .unwrap()
        .iter()
        .map(|s| (s["model"].as_str().unwrap().to_owned(), s["input"]["style"].clone(), s["input"]["seed"].clone()))
        .collect();
    sent.sort_by_key(|s| format!("{s:?}"));
    let expected = [("a", "plain", 1), ("a", "plain", 2), ("b", "noir", 1), ("b", "noir", 2)].map(|(m, s, n)| (m.to_owned(), json!(s), json!(n)));
    assert_eq!(sent, expected);

    let in_batch = list_jobs(&app, ListJobsParams { batch: Some(batch.id.clone()), ..Default::default() });
    assert!(in_batch.unwrap().jobs.iter().all(|j| j.batch_id.as_deref() == Some(batch.id.as_str())));
    assert_eq!(search_assets(&app, in_collection("bench")).unwrap().assets.len(), 4, "save_to keeps the variants side by side");

    let too_big = GenerateBatchParams {
        defaults: Some(BatchDefaults { model: Some("m".into()), ..mock_defaults(json!({})) }),
        matrix: Some([("a".to_owned(), vec![json!(0); 11]), ("b".to_owned(), vec![json!(0); 10])].into()),
        ..Default::default()
    };
    let err = error_of(generate_batch(&app, too_big).await);
    assert!(err.contains("110 combinations"), "{err}");
}

#[tokio::test]
async fn stats_sum_up_outcomes_reviews_cost_and_speed() {
    let (app, _, _home) = mock_app().await;
    let mut outputs = Vec::new();
    for (i, estimate) in [0.1, 0.2, 0.3].into_iter().enumerate() {
        let save_to = (i == 0).then(|| into_collection("proj"));
        let p = GenerateParams { estimated_cost_usd: Some(estimate), ..params(json!({ "prompt": format!("cat {i}") }), save_to) };
        outputs.push(finished(&app, generate(&app, p).await.unwrap()).await.assets.remove(0).id);
    }
    let mock = app.provider("mock").unwrap().spec.base_url.clone();
    let broken = mock_spec("broken", &mock).replace("[\"/out\"]", "[\"/missing\"]");
    save_spec(&app, SaveSpecParams { spec: broken }).await.unwrap();
    let p = GenerateParams { provider: "broken".into(), ..params(json!({ "prompt": "dog" }), None) };
    assert!(fails(&app, p).await.contains("Error kind: spec"));

    let review = |id: &str, status, rating| {
        review_asset(&app, ReviewAssetParams { asset_ids: vec![id.into()], status: Some(status), rating: Some(rating), notes: None }).unwrap()
    };
    review(&outputs[0], ReviewStatus::Approved, 5);
    review(&outputs[1], ReviewStatus::Approved, 4);
    review(&outputs[2], ReviewStatus::Rejected, 1);

    let stats = |provider: Option<&str>, collection: Option<&str>| {
        get_stats(&app, StatsParams { provider: provider.map(Into::into), model: None, collection: collection.map(Into::into) }).unwrap().models
    };
    let all = stats(None, None);
    assert_eq!(all.len(), 2);
    let m = &all[0];
    assert_eq!((m.provider.as_str(), m.jobs, m.done, m.failed, m.outputs, m.rejected), ("mock", 3, 3, 0, 3, 1));
    assert_eq!(m.done_share, Some(1.0));
    assert_eq!(m.average_rating.map(|r| (r * 100.0).round()), Some(333.0));
    assert_eq!(m.approved_share.map(|r| (r * 100.0).round()), Some(67.0));
    assert_eq!(m.average_cost_usd.map(|c| (c * 100.0).round()), Some(20.0));
    assert!(m.average_seconds.is_some_and(|s| s >= 0.0));
    let b = &all[1];
    assert_eq!((b.provider.as_str(), b.failed, b.failed_share, b.failure_kinds.get(&ErrorKind::Spec)), ("broken", 1, Some(1.0), Some(&1)));
    assert_eq!(stats(Some("broken"), None).len(), 1);
    let in_proj = stats(None, Some("proj"));
    assert_eq!((in_proj.len(), in_proj[0].jobs, in_proj[0].average_rating), (1, 1, Some(5.0)));
}

/// Answers like a provider that refused the job; the prompt picks the refusal.
async fn refusing_api() -> String {
    let router = Router::new()
        .route(
            "/submit",
            post(|Json(body): Json<Value>| async move {
                use axum::http::StatusCode as S;
                if body["model"] == "async" {
                    return (S::OK, Json(json!({ "id": "t1" })));
                }
                let (status, reply) = match body["input"]["prompt"].as_str().unwrap_or("") {
                    "moderation" => (S::BAD_REQUEST, json!({ "error": "Prompt violates our content policy" })),
                    "auth" => (S::UNAUTHORIZED, json!({ "error": "bad key" })),
                    "quota" => (S::PAYMENT_REQUIRED, json!({ "error": "pay up" })),
                    "quota-text" => (S::OK, json!({ "error": "Insufficient credits for this request" })),
                    "rate" => (S::TOO_MANY_REQUESTS, json!({})),
                    "invalid" => (S::BAD_REQUEST, json!({ "error": "duration must be 5 or 10" })),
                    "invalid-422" => (S::UNPROCESSABLE_ENTITY, json!({ "detail": "bad field" })),
                    "missing" => (S::NOT_FOUND, json!({ "error": "no such model" })),
                    "down" => (S::BAD_GATEWAY, json!({})),
                    "custom" => (S::BAD_REQUEST, json!({ "error": "prompt has a banned phrase" })),
                    _ => (S::OK, json!({ "unexpected": true })),
                };
                (status, Json(reply))
            }),
        )
        .route("/poll/t1", get(|| async { Json(json!({ "status": "failed", "error": "NSFW content detected" })) }));
    serve(|_| router).await
}

#[tokio::test]
async fn failed_jobs_carry_a_kind_retryable_and_a_hint() {
    let (app, _, _home) = mock_app().await;
    let base = refusing_api().await;
    let spec = mock_spec("refuse", &base)
        + "\n[errors.kinds]\nmoderation = [\"banned phrase\"]\n[poll]\nurl = \"/poll/{{id}}\"\nstatus = \"/status\"\n\
           success = [\"done\"]\nfailure = [\"failed\"]\nerror = \"/error\"\noutputs = [\"/out\"]\n";
    let spec = spec.replace("outputs = [\"/out\"]\n\n[upload]", "capture = { id = \"/id\" }\noutputs = [\"/out\"]\n\n[upload]");
    save_spec(&app, SaveSpecParams { spec }).await.unwrap();
    save_spec(&app, SaveSpecParams { spec: mock_spec("dead", "http://127.0.0.1:9") }).await.unwrap();

    let refuse = |provider: &str, model: &str, prompt: &str| GenerateParams {
        provider: provider.into(),
        model: model.into(),
        ..params(json!({ "prompt": prompt }), None)
    };
    let cases = [
        ("moderation", "moderation", false),
        ("auth", "auth", false),
        ("quota", "quota", false),
        ("quota-text", "quota", false),
        ("rate", "rate_limit", true),
        ("invalid", "invalid_input", false),
        ("invalid-422", "invalid_input", false),
        ("missing", "not_found", false),
        ("down", "provider_error", true),
        ("custom", "moderation", false),
        ("spec", "spec", false),
    ];
    for (prompt, kind, retryable) in cases {
        let err = fails(&app, refuse("refuse", "m", prompt)).await;
        let retry = if retryable { "(retryable)" } else { "(not retryable)" };
        assert!(err.contains(&format!("Error kind: {kind} {retry}")), "{prompt}: {err}");
    }
    let err = fails(&app, refuse("dead", "m", "x")).await;
    assert!(err.contains("Error kind: network (retryable)"), "{err}");

    let job = generate(&app, refuse("refuse", "async", "a fox")).await.unwrap();
    let job = settled(&app, &job.id).await;
    assert_eq!((job.status.as_str(), job.error_kind, job.retryable), ("failed", Some(ErrorKind::Moderation), Some(false)));
    assert!(job.hint.unwrap().contains("rewrite the prompt"));

    let listed = |status: Option<&str>, error_kind| {
        let p = ListJobsParams { limit: Some(100), status: status.map(Into::into), error_kind, ..Default::default() };
        list_jobs(&app, p).unwrap().jobs
    };
    assert_eq!(listed(None, Some(ErrorKind::Moderation)).len(), 3);
    assert_eq!(listed(None, Some(ErrorKind::Quota)).len(), 2);
    assert!(listed(Some("failed"), Some(ErrorKind::RateLimit)).iter().all(|j| j.retryable == Some(true)));
    assert_eq!(listed(Some("failed"), None).len(), cases.len() + 2);
    assert!(listed(Some("done"), None).is_empty());

    app.db.conn().execute("UPDATE jobs SET error_kind = NULL", []).unwrap();
    let polled = listed(Some("failed"), None).into_iter().find(|j| j.model == "async").unwrap();
    assert_eq!(polled.error_kind, Some(ErrorKind::Moderation), "older rows are classified when read");
}

#[tokio::test]
async fn batch_budget_admits_the_total_up_front() {
    let (app, _, _home) = mock_app().await;
    set_budget(&app, SetBudgetParams { name: "session".into(), limit_usd: 1.0 }).unwrap();
    let batch = |n: usize, estimate: Option<f64>| GenerateBatchParams {
        defaults: Some(BatchDefaults {
            model: Some("m".into()),
            budget: Some("session".into()),
            estimated_cost_usd: estimate,
            ..mock_defaults(json!({ "prompt": "cat" }))
        }),
        matrix: Some([("seed".to_owned(), (0..n).map(|i| json!(i)).collect())].into()),
        ..Default::default()
    };
    let refuse = |p| async { error_of(generate_batch(&app, p).await) };
    let err = refuse(batch(3, Some(0.4))).await;
    assert!(err.contains("batch of 3 jobs") && err.contains("estimated at $1.2000"), "{err}");
    assert!(refuse(batch(2, None)).await.contains("needs estimated_cost_usd"));
    assert!(list_jobs(&app, ListJobsParams::default()).unwrap().jobs.is_empty());

    let ok = generate_batch(&app, batch(2, Some(0.4))).await.unwrap();
    assert_eq!(ok.jobs.len(), 2);
    let b = list_budgets(&app).unwrap().budgets.remove(0);
    assert_eq!((b.jobs, (b.spent_usd * 100.0).round()), (2, 80.0));
    assert!(refuse(batch(1, Some(0.4))).await.contains("would be exceeded"));
}

/// Keeps a just-submitted job running until the test finishes or fails it.
fn hold(app: &App, job: &Job) {
    set_lease(app, &job.id, i64::MAX);
}

fn set_lease(app: &App, id: &str, until: i64) {
    app.db.conn().execute("UPDATE jobs SET lease_until = ?2 WHERE id = ?1", rusqlite::params![id, until]).unwrap();
}

fn lease(app: &App, id: &str) -> i64 {
    app.db.conn().query_row("SELECT lease_until FROM jobs WHERE id = ?1", [id], |r| r.get(0)).unwrap()
}

fn job_input(id: &str, n: Option<usize>) -> String {
    match n {
        Some(n) => placeholder("job", &format!("{id}:{n}")),
        None => placeholder("job", id),
    }
}

#[tokio::test]
async fn chained_job_waits_then_submits_with_parent_output() {
    let (app, seen, _home) = mock_app().await;
    let parent = generate(&app, params(json!({ "prompt": "still" }), None)).await.unwrap();
    hold(&app, &parent);
    let input = json!({ "prompt": "animate", "image": job_input(&parent.id, None), "frames": [job_input(&parent.id, Some(0))] });
    let child = generate(&app, params(input.clone(), None)).await.unwrap();
    assert_eq!((child.status.as_str(), child.waiting_on.clone()), ("waiting", vec![parent.id.clone()]));
    assert!(child.hint.as_deref().unwrap().contains(&parent.id));
    let created = lease(&app, &child.id);
    until(|| async { (lease(&app, &child.id) > created).then_some(()) }).await;
    assert_eq!(seen.submits.lock().unwrap().len(), 1, "the worker saw the child and kept it waiting while the parent runs");

    set_lease(&app, &parent.id, 0);
    let parent = finished(&app, parent).await;
    let child = finished(&app, child).await;
    assert!(child.waiting_on.is_empty());
    assert_eq!(child.input, input, "the job keeps the placeholders");
    let sent = seen.input(1);
    assert_eq!(sent["image"], "https://cdn.test/up-1.png", "parent output uploaded once");
    assert_eq!(sent["frames"], json!(["https://cdn.test/up-1.png"]));
    let parent_out = &parent.assets[0].id;
    assert_eq!(child.inputs.iter().map(|i| &i.asset_id).collect::<Vec<_>>(), [parent_out]);
    let lineage = get_lineage(&app, AssetParams { asset_id: child.assets[0].id.clone() }).unwrap();
    assert_eq!(lineage.ancestors[0].asset.id, *parent_out);

    let again = generate(&app, params(json!({ "image": job_input(&parent.id, None) }), None)).await.unwrap();
    assert_eq!(again.status, "running", "a done parent resolves at once");
    let err = fails(&app, params(json!({ "image": job_input(&parent.id, Some(3)) }), None)).await;
    assert!(err.contains("1 output(s), numbered from 0"), "{err}");
    let err = fails(&app, params(json!({ "image": "{{job:nope}}" }), None)).await;
    assert!(err.contains("no job `nope`"), "{err}");
}

#[tokio::test]
async fn failed_or_cancelled_parent_fails_its_children() {
    let (app, _, _home) = mock_app().await;
    let child_of = |parent: &Job| params(json!({ "image": job_input(&parent.id, None) }), None);
    let held_parent = || async {
        let parent = generate(&app, params(json!({ "prompt": "p" }), None)).await.unwrap();
        hold(&app, &parent);
        parent
    };

    let parent = held_parent().await;
    let child = generate(&app, child_of(&parent)).await.unwrap();
    app.db.conn().execute("UPDATE jobs SET status = 'failed', error = 'boom' WHERE id = ?1", [&parent.id]).unwrap();
    let child = settled(&app, &child.id).await;
    assert_eq!(child.status, "failed");
    assert_eq!(child.error.unwrap(), format!("parent job {} failed: boom", parent.id));
    let err = fails(&app, child_of(&parent)).await;
    assert!(err.contains("failed: boom") && err.contains("Rerun the parent"), "{err}");

    let parent = held_parent().await;
    let child = generate(&app, child_of(&parent)).await.unwrap();
    let grandchild = generate(&app, child_of(&child)).await.unwrap();
    assert_eq!(grandchild.status, "waiting");
    let cancelled = cancel_job(&app, JobParams { job_id: parent.id.clone() }).unwrap();
    assert_eq!((cancelled.status.as_str(), cancelled.error.as_deref()), ("failed", Some("cancelled")));
    assert!(cancelled.hint.unwrap().contains("not asked to stop"));
    for (job, by) in [(&child, &parent), (&grandchild, &child)] {
        let job = get_job(&app, GetJobParams { job_id: job.id.clone(), ..Default::default() }).await.unwrap();
        assert_eq!(job.status, "failed");
        assert!(job.error.as_deref().unwrap().starts_with(&format!("parent job {} failed:", by.id)), "{:?}", job.error);
    }
    assert!(error_of(cancel_job(&app, JobParams { job_id: parent.id.clone() })).contains("already failed"));

    let parent = held_parent().await;
    let waiting = generate(&app, child_of(&parent)).await.unwrap();
    assert_eq!(cancel_job(&app, JobParams { job_id: waiting.id }).unwrap().status, "failed");
    set_lease(&app, &parent.id, 0);
    finished(&app, parent).await;
}

/// ffmpeg from FFMPEG or PATH, like the app.
fn ffmpeg() -> std::process::Command {
    std::process::Command::new(std::env::var("FFMPEG").unwrap_or_else(|_| "ffmpeg".into()))
}

/// Tests that need ffmpeg skip without it.
fn ffmpeg_installed() -> bool {
    let found = ffmpeg().arg("-version").output().is_ok();
    if !found {
        eprintln!("ffmpeg not installed, skipping");
    }
    found
}

/// Writes a test file made from ffmpeg's lavfi sources and imports it.
async fn synth(app: &App, home: &Path, name: &str, args: &[&str]) -> Asset {
    let path = home.join(name);
    let out = ffmpeg().args(["-v", "error", "-y"]).args(args).arg(&path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let p = ImportAssetParams { path: path.to_string_lossy().into(), tags: None, prompt: Some(format!("test {name}")) };
    import_asset(app, p).await.unwrap().asset
}

fn near(actual: Option<f64>, expected: f64) -> bool {
    actual.is_some_and(|a| (a - expected).abs() < 0.2)
}

#[tokio::test]
async fn media_operations_make_new_assets_with_lineage() {
    if !ffmpeg_installed() {
        return;
    }
    let (app, _, home) = mock_app().await;
    let clip = synth(&app, &home, "clip.mp4", &[
        "-f", "lavfi", "-i", "testsrc=size=320x240:rate=10:duration=2",
        "-f", "lavfi", "-i", "sine=frequency=440:duration=2",
        "-pix_fmt", "yuv420p", "-shortest",
    ])
    .await;
    let silent = synth(&app, &home, "silent.mp4", &["-f", "lavfi", "-i", "testsrc2=size=160x120:rate=10:duration=1", "-pix_fmt", "yuv420p"]).await;
    let tone = synth(&app, &home, "tone.wav", &["-f", "lavfi", "-i", "sine=frequency=220:duration=1"]).await;
    let logo = synth(&app, &home, "logo.png", &["-f", "lavfi", "-i", "color=c=red:size=40x40", "-frames:v", "1"]).await;

    assert_eq!((clip.width, clip.height, clip.has_audio), (Some(320), Some(240), Some(true)));
    assert!(near(clip.duration_secs, 2.0), "{:?}", clip.duration_secs);
    assert_eq!(silent.has_audio, Some(false));
    assert_eq!((tone.width, tone.has_audio), (None, Some(true)));
    assert!(near(tone.duration_secs, 1.0));
    assert_eq!((logo.width, logo.height, logo.duration_secs), (Some(40), Some(40), None));
    app.db.conn().execute("UPDATE assets SET has_audio = NULL, width = NULL WHERE id = ?1", [&clip.id]).unwrap();
    let again = get_asset(&app, AssetParams { asset_id: clip.id.clone() }).await.unwrap();
    assert_eq!((again.width, again.has_audio), (Some(320), Some(true)), "get_asset backfills metadata");

    let frame_of = |asset: &Asset, at: Option<FrameAt>| ExtractFrameParams { asset_id: asset.id.clone(), at, save_to: None };
    let last = ExtractFrameParams { save_to: Some(into_collection("cuts")), ..frame_of(&clip, Some(FrameAt::Edge(media::Edge::Last))) };
    let frame = extract_frame(&app, last).await.unwrap();
    assert_eq!((frame.asset.mime.as_deref(), frame.asset.width, frame.asset.height), (Some("image/png"), Some(320), Some(240)));
    let job = get_job(&app, GetJobParams { job_id: frame.job_id.clone(), ..Default::default() }).await.unwrap();
    assert_eq!((job.provider.as_str(), job.model.as_str(), job.status.as_str(), job.cost), ("local", "extract_frame", "done", Some(0.0)));
    assert_eq!(job.input, json!({ "asset_id": clip.id, "at": "last" }));
    assert_eq!(job.assets[0].id, frame.asset.id);
    let cuts = search_assets(&app, in_collection("cuts")).unwrap();
    assert_eq!(cuts.assets[0].id, frame.asset.id, "save_to files the result");
    let first = frame_of(&clip, Some(FrameAt::Seconds(0.5)));
    assert_ne!(extract_frame(&app, first).await.unwrap().asset.id, frame.asset.id);

    assert!(error_of(extract_frame(&app, frame_of(&clip, Some(FrameAt::Seconds(5.0)))).await).contains("outside the video"));
    assert!(error_of(extract_frame(&app, frame_of(&logo, None)).await).contains("needs video input"));

    let reframed = |asset: &Asset, aspect: &str, mode| ReframeParams { asset_id: asset.id.clone(), aspect: aspect.into(), mode, save_to: None };
    let tall = reframe(&app, reframed(&clip, "9:16", None)).await.unwrap().asset;
    assert_eq!((tall.width, tall.height, tall.has_audio), (Some(134), Some(240), Some(true)));
    assert_eq!(reframe(&app, reframed(&logo, "2:1", Some(ReframeMode::Pad))).await.unwrap().asset.width, Some(80));
    assert!(error_of(reframe(&app, reframed(&logo, "wide", None)).await).contains("\"9:16\""));

    let trimmed = |asset: &Asset, start, end| TrimParams { asset_id: asset.id.clone(), start, end, save_to: None };
    let cut = trim(&app, trimmed(&clip, Some(0.5), Some(1.5))).await.unwrap();
    assert!(near(cut.asset.duration_secs, 1.0), "{:?}", cut.asset.duration_secs);
    let short = trim(&app, trimmed(&tone, None, Some(0.5))).await.unwrap();
    assert_eq!(short.asset.mime.as_deref(), Some("audio/wav"));
    assert!(near(short.asset.duration_secs, 0.5));
    assert!(error_of(trim(&app, trimmed(&clip, Some(3.0), None)).await).contains("start < end"));

    let joined = concat(&app, ConcatParams { asset_ids: vec![clip.id.clone(), silent.id.clone()], save_to: None }).await.unwrap();
    let j = &joined.asset;
    assert_eq!((j.width, j.height, j.has_audio), (Some(320), Some(240), Some(true)));
    assert!(near(j.duration_secs, 3.0), "{:?}", j.duration_secs);
    assert!(error_of(concat(&app, ConcatParams { asset_ids: vec![clip.id.clone()], save_to: None }).await).contains("at least two"));

    let logo_on = |base: &str| OverlayParams { base_asset_id: base.into(), overlay_asset_id: logo.id.clone(), ..Default::default() };
    let packshot = OverlayParams { x: Some(10), y: Some(10), width: Some(20), ..logo_on(&clip.id) };
    let packshot = overlay(&app, packshot).await.unwrap().asset;
    assert_eq!((packshot.mime.as_deref(), packshot.width, packshot.has_audio), (Some("video/mp4"), Some(320), Some(true)));
    assert_eq!(overlay(&app, logo_on(&frame.asset.id)).await.unwrap().asset.mime.as_deref(), Some("image/png"));
    let off = OverlayParams { x: Some(400), ..logo_on(&clip.id) };
    assert!(error_of(overlay(&app, off).await).contains("320x240"));

    let mux = |video: &Asset, audio: &Asset| MuxAudioParams { video_asset_id: video.id.clone(), audio_asset_id: audio.id.clone(), save_to: None };
    let scored = mux_audio(&app, mux(&silent, &tone)).await.unwrap();
    assert_eq!(scored.asset.has_audio, Some(true));
    assert!(near(scored.asset.duration_secs, 1.0), "{:?}", scored.asset.duration_secs);
    assert!(error_of(mux_audio(&app, mux(&clip, &silent)).await).contains("has no audio track"));

    let ids = |l: Vec<lineage::Link>| l.into_iter().map(|l| (l.asset.id, l.depth)).collect::<Vec<_>>();
    let of_mux = get_lineage(&app, AssetParams { asset_id: scored.asset.id.clone() }).unwrap();
    let mut ancestors = ids(of_mux.ancestors);
    ancestors.sort();
    let mut expected = vec![(silent.id.clone(), 1), (tone.id.clone(), 1)];
    expected.sort();
    assert_eq!(ancestors, expected);
    let chained = extract_frame(&app, frame_of(&joined.asset, None)).await.unwrap();
    let of_chained = ids(get_lineage(&app, AssetParams { asset_id: chained.asset.id.clone() }).unwrap().ancestors);
    assert!(of_chained.contains(&(joined.asset.id.clone(), 1)) && of_chained.contains(&(clip.id.clone(), 2)), "{of_chained:?}");
    let made_from_clip = ids(get_lineage(&app, AssetParams { asset_id: clip.id.clone() }).unwrap().descendants);
    assert!(made_from_clip.contains(&(frame.asset.id.clone(), 1)) && made_from_clip.contains(&(chained.asset.id.clone(), 2)));
}

fn write_prices(home: &Path, rows: Value) {
    std::fs::write(home.join(catalog::VIDEO_FILE), json!({ "rows": rows }).to_string()).unwrap();
    std::fs::write(home.join(catalog::IMAGE_FILE), json!({ "rows": [] }).to_string()).unwrap();
}

async fn offers(app: &App, query: &str) -> Vec<Offer> {
    search_prices(app, SearchPricesParams { query: Some(query.into()), ..Default::default() }).await.unwrap().offers
}

#[tokio::test]
async fn recipe_batch_fills_variables_from_the_matrix_and_prices_each_job() {
    let (app, seen, home) = mock_app().await;
    write_prices(&home, json!([{ "family": "Mock", "model": "Mock Video", "provider": "mockcloud", "mode": "generate",
                                 "res": "720p", "usdPerSec": 0.1, "source": "-" }]));
    let offer = offers(&app, "mock").await.remove(0).offer_id;
    save_recipe(&app, ad_recipe(json!({ "prompt": "fox says {{var:line}}", "duration": "{{var:duration}}" }))).unwrap();
    set_budget(&app, SetBudgetParams { name: "s".into(), limit_usd: 2.0 }).unwrap();
    let batch = |durations: Vec<Value>| GenerateBatchParams {
        recipe: Some("ad".into()),
        defaults: Some(BatchDefaults {
            budget: Some("s".into()),
            offer_id: Some(offer.clone()),
            vars: Some(obj(json!({ "line": "hi" }))),
            ..Default::default()
        }),
        items: Some(vec![BatchItem { vars: Some(obj(json!({ "line": "hallo" }))), ..Default::default() }, BatchItem::default()]),
        matrix: Some([("duration".to_owned(), durations)].into()),
        label: None,
    };
    let err = error_of(generate_batch(&app, batch(vec![json!(5), json!(10)])).await);
    assert!(err.contains("batch of 4 jobs") && err.contains("estimated at $3.0000"), "{err}");

    let started = generate_batch(&app, batch(vec![json!(2), json!(3)])).await.unwrap();
    let done = settled_batch(&app, &started.id).await;
    assert_eq!(done.counts.get("done"), Some(&4), "{:?}", done.counts);
    let mut sent: Vec<_> = seen.submits.lock().unwrap().iter().map(|s| (s["model"].clone(), s["input"].clone())).collect();
    sent.sort_by_key(|s| s.1.to_string());
    let mut want = [("hallo", 2), ("hallo", 3), ("hi", 2), ("hi", 3)]
        .map(|(line, d)| (json!("m"), json!({ "prompt": format!("fox says {line}"), "duration": d })));
    want.sort_by_key(|s| s.1.to_string());
    assert_eq!(sent, want, "matrix keys fill variables, the recipe's whole-string var keeps the number");
    let b = list_budgets(&app).unwrap().budgets.remove(0);
    assert_eq!((b.jobs, (b.spent_usd * 100.0).round()), (4, 100.0), "offer price x duration per job");
    assert_eq!(search_assets(&app, in_collection("ads")).unwrap().assets.len(), 4, "the recipe's save_to applies");

    let missing = GenerateBatchParams { defaults: None, items: None, ..batch(vec![json!(1)]) };
    let err = error_of(generate_batch(&app, missing).await);
    assert!(err.contains("item 0") && err.contains("`line`"), "{err}");
}

#[tokio::test]
async fn last_frame_of_a_job_feeds_the_next_one() {
    if !ffmpeg_installed() {
        return;
    }
    let (app, seen, home) = mock_app().await;
    let clip = synth(&app, &home, "clip.mp4", &["-f", "lavfi", "-i", "testsrc=size=160x120:rate=10:duration=2", "-pix_fmt", "yuv420p"]).await;
    let cut = trim(&app, TrimParams { asset_id: clip.id.clone(), start: Some(0.5), ..Default::default() }).await.unwrap();
    let frame_of = |job: &str, frame: &str| placeholder("job", &format!("{job}:{frame}"));

    let next = params(json!({ "prompt": "continue", "image": frame_of(&cut.job_id, "last_frame") }), None);
    let job = finished(&app, generate(&app, next).await.unwrap()).await;
    let frame = get_asset(&app, AssetParams { asset_id: job.inputs[0].asset_id.clone() }).await.unwrap();
    assert_eq!((frame.mime.as_deref(), frame.width), (Some("image/png"), Some(160)));
    assert_eq!(seen.input(0)["image"], "https://cdn.test/up-1.png");
    let lineage = get_lineage(&app, AssetParams { asset_id: job.assets[0].id.clone() }).unwrap();
    let chain: Vec<_> = lineage.ancestors.iter().map(|l| l.asset.id.clone()).collect();
    assert!(chain.contains(&frame.id) && chain.contains(&cut.asset.id) && chain.contains(&clip.id), "{chain:?}");

    let still = finished(&app, generate(&app, params(json!({ "prompt": "still" }), None)).await.unwrap()).await;
    let err = fails(&app, params(json!({ "image": frame_of(&still.id, "last_frame") }), None)).await;
    assert!(err.contains("no video output"), "{err}");
    let err = fails(&app, params(json!({ "image": frame_of(&cut.job_id, "middle_frame") }), None)).await;
    assert!(err.contains("use last_frame or first_frame"), "{err}");
}

#[tokio::test]
async fn media_operations_say_how_to_install_ffmpeg() {
    if std::env::var_os("FFMPEG").is_some() {
        eprintln!("FFMPEG is set in the environment, which config.toml cannot override; skipping");
        return;
    }
    let (app, _, home) = mock_app().await;
    let nowhere = home.join("no-ffmpeg").join("ffmpeg");
    home.config(&format!("MOCK_KEY = \"k\"\nFFMPEG = {:?}\n", nowhere.to_string_lossy()));
    app.providers.reload().unwrap();
    let video = import(&app, &home, "clip.mp4").await;
    assert_eq!((video.has_audio, video.preview.as_deref()), (None, None), "import still works without ffmpeg");
    let e = error_of(extract_frame(&app, ExtractFrameParams { asset_id: video.id, ..Default::default() }).await);
    assert!(e.contains("ffprobe is not installed") && e.contains("FFMPEG") && e.contains("config.toml"), "{e}");
    assert!(list_jobs(&app, ListJobsParams::default()).unwrap().jobs.is_empty(), "nothing recorded");
}

#[tokio::test]
async fn cost_estimates_price_offers_history_and_budgets() {
    let (app, _, home) = mock_app().await;
    write_prices(&home, json!([
        { "family": "Mock", "model": "Mock Video 2.0", "provider": "mockcloud", "mode": "generate", "res": "720p",
          "audio": true, "usdPerSec": 0.05, "source": "-" },
        { "family": "Mock", "model": "Mock Image", "provider": "mockcloud", "mode": "generate", "usdPerImage": 0.04, "source": "-" },
        { "family": "Other", "model": "Other Video", "provider": "nobody", "mode": "generate", "usdPerSec": 0.01, "source": "-" },
    ]));

    let mocks = offers(&app, "mock").await;
    let video = mocks.iter().find(|o| o.row.usd_per_sec.is_some()).unwrap();
    assert_eq!(video.offer_id, "mockcloud:mock-video-2.0::generate:720p:audio");
    let mut repriced = video.row.clone();
    repriced.usd_per_sec = Some(0.07);
    assert_eq!(repriced.offer_id(), video.offer_id, "a price change keeps the id");
    let image_offer = mocks.iter().find(|o| o.row.usd_per_image.is_some()).unwrap().offer_id.clone();
    let video_offer = video.offer_id.clone();

    let estimate = |model: &str, input: Value, offer: Option<&str>| {
        let p = EstimateCostParams { provider: Some("mock".into()), model: Some(model.into()), input: obj(input), offer_id: offer.map(Into::into), ..Default::default() };
        estimate_cost(&app, p)
    };
    let e = estimate("m", json!({ "prompt": "x", "duration": "5s" }), Some(&video_offer)).await.unwrap();
    assert_eq!(e.estimate_usd.map(|u| (u * 1e4).round()), Some(2500.0));
    assert!(e.formula.as_deref().unwrap().contains("$0.05/s × 5 s (input.duration)"), "{:?}", e.formula);
    assert_eq!(Value::Object(e.read), json!({ "duration": "5s" }));
    let e = estimate("m", json!({ "settings": { "duration_seconds": 4 }, "n": 2 }), Some(&video_offer)).await.unwrap();
    assert_eq!(e.estimate_usd.map(|u| (u * 1e4).round()), Some(4000.0), "{:?}", e.formula);
    assert_eq!(Value::Object(e.read), json!({ "settings.duration_seconds": 4, "n": 2 }));
    let e = estimate("m", json!({ "num_images": 3 }), Some(&image_offer)).await.unwrap();
    assert_eq!(e.estimate_usd.map(|u| (u * 1e4).round()), Some(1200.0));
    let e = estimate("m", json!({ "prompt": "x" }), Some(&video_offer)).await.unwrap();
    assert!(e.estimate_usd.is_none() && e.note.as_deref().unwrap().contains("no duration field"), "{:?}", e.note);
    let err = error_of(estimate("m", json!({}), Some("nope")).await);
    assert!(err.contains("search_prices"), "{err}");
    let other = search_prices(&app, SearchPricesParams { query: Some("other".into()), configured_only: Some(false), ..Default::default() })
        .await
        .unwrap()
        .offers
        .remove(0)
        .offer_id;
    let err = error_of(estimate("m", json!({}), Some(&other)).await);
    assert!(err.contains("no provider spec serves `nobody`"), "{err}");

    for (prompt, usd) in [("a", 0.1), ("b", 0.3), ("c", 0.2)] {
        let job = finished(&app, generate(&app, params(json!({ "prompt": prompt }), None)).await.unwrap()).await;
        let sql = "UPDATE jobs SET cost = ?2, cost_unit = 'USD' WHERE id = ?1";
        app.db.conn().execute(sql, rusqlite::params![job.id, usd]).unwrap();
    }
    let e = estimate("m", json!({}), None).await.unwrap();
    let h = e.history.unwrap();
    assert_eq!((h.median_usd, h.jobs), (0.2, 3));
    assert_eq!(e.estimate_usd, Some(0.2), "history is the fallback estimate");
    assert!(e.formula.unwrap().starts_with("median of 3 past mock/m jobs"));

    set_budget(&app, SetBudgetParams { name: "s".into(), limit_usd: 1.0 }).unwrap();
    let budgeted = |model: &str, input: Value, offer: Option<&str>| GenerateParams {
        model: model.into(),
        budget: Some("s".into()),
        offer_id: offer.map(Into::into),
        ..params(input, None)
    };
    let job = generate(&app, budgeted("m", json!({ "duration": 5 }), Some(&video_offer))).await.unwrap();
    assert_eq!(job.estimate_usd.map(|u| (u * 1e4).round()), Some(2500.0), "offer estimate stored on the job");
    let job = generate(&app, budgeted("m", json!({ "prompt": "x" }), None)).await.unwrap();
    assert_eq!(job.estimate_usd, Some(0.2), "history median admitted");
    let err = fails(&app, budgeted("fresh", json!({ "prompt": "x" }), None)).await;
    assert!(err.contains("needs estimated_cost_usd") && err.contains("no past mock/fresh jobs reported a USD cost"), "{err}");
    let err = fails(&app, budgeted("fresh", json!({ "prompt": "x" }), Some(&video_offer))).await;
    assert!(err.contains("no duration field"), "{err}");
    let job = generate(&app, GenerateParams { budget: None, ..budgeted("fresh", json!({ "seconds": 2 }), Some(&video_offer)) });
    assert_eq!(job.await.unwrap().estimate_usd.map(|u| (u * 1e4).round()), Some(1000.0), "estimate kept without a budget");
}

fn level(p: &Preflight, name: &str) -> Option<preflight::Level> {
    p.checks.iter().find(|c| c.name == name).map(|c| c.level)
}

#[tokio::test]
async fn preflight_checks_everything_and_sends_nothing() {
    use preflight::Level::{Fail, Ok as Pass};
    let (app, seen, home) = mock_app().await;
    let a = import(&app, &home, "a.png").await;
    write_prices(&home, json!([{ "family": "Mock", "model": "Mock Image", "provider": "mockcloud", "mode": "generate",
                                 "usdPerImage": 0.5, "source": "-" }]));
    let offer = offers(&app, "mock").await.remove(0).offer_id;
    save_snippet(&app, SaveSnippetParams { name: "look".into(), text: "teal and orange".into(), description: None }).unwrap();
    set_budget(&app, SetBudgetParams { name: "small".into(), limit_usd: 1.0 }).unwrap();

    let input = json!({ "prompt": "fox, {{snippet:look}}", "image_urls": [placeholder("asset", &a.id)] });
    let p = GenerateParams { offer_id: Some(offer.clone()), budget: Some("small".into()), ..params(input.clone(), None) };
    let checked = preflight(&app, p.into()).await.unwrap();
    assert!(checked.ready, "{:?}", checked.checks.iter().map(|c| (&c.name, &c.detail)).collect::<Vec<_>>());
    assert_eq!(checked.estimate_usd, Some(0.5));
    assert_eq!(checked.uploads, std::slice::from_ref(&a.id));
    let shown = checked.input.unwrap();
    assert_eq!(shown["prompt"], "fox, teal and orange");
    assert_eq!(shown["image_urls"], json!([format!("<upload of asset {}>", a.id)]));
    let body = checked.requests[0].request.body.as_ref().map(ToString::to_string).unwrap_or_default();
    assert!(body.contains("teal and orange") && body.contains("<upload of asset"), "{body}");
    assert_eq!((seen.uploads(), seen.submits.lock().unwrap().len()), (0, 0), "nothing uploaded or submitted");
    assert!(list_jobs(&app, ListJobsParams::default()).unwrap().jobs.is_empty(), "no job recorded");

    let costly = GenerateParams { budget: Some("small".into()), estimated_cost_usd: Some(5.0), ..params(input.clone(), None) };
    let checked = preflight(&app, costly.into()).await.unwrap();
    assert!(!checked.ready);
    assert_eq!((level(&checked, "cost"), level(&checked, "budget")), (Some(Pass), Some(Fail)));

    let broken = params(json!({ "prompt": "{{snippet:nope}}", "image": "{{asset:missing}}" }), Some(SaveTo { reference: Some("ghost".into()), ..Default::default() }));
    let checked = preflight(&app, broken.into()).await.unwrap();
    assert_eq!((level(&checked, "text"), level(&checked, "save_to")), (Some(Fail), Some(Fail)));
    let checked = preflight(&app, params(json!({ "image": "{{asset:missing}}" }), None).into()).await.unwrap();
    assert_eq!(level(&checked, "inputs"), Some(Fail));
    assert!(checked.requests.is_empty());

    let running = generate(&app, params(json!({ "prompt": "parent" }), None)).await.unwrap();
    app.db.conn().execute("UPDATE jobs SET status = 'running' WHERE id = ?1", [&running.id]).unwrap();
    let checked = preflight(&app, params(json!({ "image": placeholder("job", &running.id) }), None).into()).await.unwrap();
    assert!(checked.ready);
    assert_eq!(checked.waits_on, std::slice::from_ref(&running.id));

    let unknown = GenerateParams { provider: "nope".into(), ..params(json!({}), None) };
    let checked = preflight(&app, unknown.into()).await.unwrap();
    assert_eq!((checked.ready, level(&checked, "provider"), level(&checked, "request")), (false, Some(Fail), None));
}

#[tokio::test]
async fn doctor_reports_setup_problems() {
    let home = Home::new("");
    std::fs::create_dir_all(home.join("providers")).unwrap();
    std::fs::write(home.join("providers/broken.toml"), "key = \"broken\"").unwrap();
    let app = start(&home);
    let report = doctor(&app, DoctorParams::default()).await.unwrap();
    let find = |name: &str| report.checks.iter().find(|c| c.name == name).map(|c| (c.level, c.detail.clone()));
    assert!(!report.ok);
    if !list_providers(&app).providers.iter().any(|p| p.configured) {
        assert!(matches!(find("providers"), Some((preflight::Level::Fail, d)) if d.contains("no provider has its key set")), "{:?}", find("providers"));
    }
    assert!(matches!(find("spec"), Some((preflight::Level::Fail, d)) if d.contains("broken.toml")), "{:?}", find("spec"));
    assert_eq!(find("database").map(|c| c.0), Some(preflight::Level::Ok));
    assert_eq!(find("home").map(|c| c.0), Some(preflight::Level::Ok));

    home.config("KIE_API_KEY = \"k\"\n");
    std::fs::remove_file(home.join("providers/broken.toml")).unwrap();
    app.providers.reload().unwrap();
    let report = doctor(&app, DoctorParams::default()).await.unwrap();
    let kie = report.providers.iter().find(|p| p.key == "kie").expect("configured provider listed");
    let (passed, total) = kie.fixtures.split_once('/').unwrap();
    assert!(passed == total && total != "0", "fixtures pass: {}", kie.fixtures);
    assert!(report.ok, "{:?}", report.checks.iter().filter(|c| c.level == preflight::Level::Fail).map(|c| &c.detail).collect::<Vec<_>>());
}

/// Sync API whose first `failures` submits answer `status`.
async fn flaky_app(failures: usize, status: u16) -> (App, Arc<AtomicUsize>, Home) {
    let home = Home::new("MOCK_KEY = \"k\"\n");
    let app = start(&home);
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let base = serve(move |_| {
        Router::new().route(
            "/submit",
            post(move || async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let png = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "img");
                let code = if n < failures { status } else { 200 };
                let body = if n < failures { json!({ "error": "busy" }) } else { json!({ "out": format!("data:image/png;base64,{png}") }) };
                (axum::http::StatusCode::from_u16(code).unwrap(), Json(body))
            }),
        )
    })
    .await;
    save_spec(&app, SaveSpecParams { spec: MOCK_SPEC.replace("BASE", &base) }).await.unwrap();
    (app, calls, home)
}

fn retrying(attempts: u32) -> GenerateParams {
    GenerateParams { retry: Some(Retry { attempts, delay_secs: Some(0) }), ..params(json!({ "prompt": "x" }), None) }
}

#[tokio::test]
async fn retryable_failures_are_resubmitted_when_asked() {
    let (app, calls, _home) = flaky_app(2, 429).await;
    let err = fails(&app, params(json!({ "prompt": "x" }), None)).await;
    assert!(err.contains("rate_limit (retryable)"), "without retry the error comes back at once: {err}");

    let job = generate(&app, retrying(2)).await.unwrap();
    assert_eq!(job.status, "running");
    assert!(job.progress.as_deref().unwrap_or_default().contains("attempt 1 failed (rate_limit)"), "{:?}", job.progress);
    let job = finished(&app, job).await;
    assert_eq!((job.attempts, job.assets.len(), calls.load(Ordering::SeqCst)), (Some(2), 1, 3));

    let (app, calls, _home) = flaky_app(10, 503).await;
    let job = settled(&app, &generate(&app, retrying(2)).await.unwrap().id).await;
    assert_eq!((job.status.as_str(), job.error_kind, job.attempts), ("failed", Some(ErrorKind::ProviderError), Some(3)));
    assert_eq!(calls.load(Ordering::SeqCst), 3, "first submit plus two retries");

    let (app, calls, _home) = flaky_app(10, 400).await;
    let err = error_of(generate(&app, retrying(2)).await);
    assert!(err.contains("invalid_input"), "{err}");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "a non-retryable failure is not resubmitted");
    assert!(error_of(generate(&app, retrying(6)).await).contains("at most 5"));
}

#[derive(Clone, Default)]
struct Comfy {
    prompts: Arc<Mutex<Vec<Value>>>,
    polls: Arc<AtomicUsize>,
    views: Arc<Mutex<Vec<String>>>,
    authorized: Arc<AtomicUsize>,
}

/// ComfyUI's /prompt, /history (empty until the second poll), /upload/image and /view.
async fn mock_comfyui(c: Comfy) -> String {
    let router = Router::new()
        .route(
            "/prompt",
            post(|State(c): State<Comfy>, headers: axum::http::HeaderMap, Json(body): Json<Value>| async move {
                c.authorized.fetch_add(usize::from(headers.contains_key("authorization")), Ordering::SeqCst);
                c.prompts.lock().unwrap().push(body);
                Json(json!({ "prompt_id": "p1", "number": 1, "node_errors": {} }))
            }),
        )
        .route(
            "/history/{id}",
            get(|State(c): State<Comfy>, axum::extract::Path(id): axum::extract::Path<String>| async move {
                if c.polls.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Json(json!({}));
                }
                let images = json!([
                    { "filename": "out 1.png", "subfolder": "", "type": "output" },
                    { "filename": "preview.png", "subfolder": "", "type": "temp" },
                ]);
                Json(json!({ id: { "outputs": { "9": { "images": images } }, "status": { "status_str": "success", "completed": true } } }))
            }),
        )
        .route("/upload/image", post(|| async { Json(json!({ "name": "a.png", "subfolder": "", "type": "input" })) }))
        .route(
            "/view",
            get(|State(c): State<Comfy>, axum::extract::RawQuery(q): axum::extract::RawQuery| async move {
                c.views.lock().unwrap().push(q.unwrap_or_default());
                ([("content-type", "image/png")], "png-bytes")
            }),
        )
        .with_state(c);
    serve(|_| router).await
}

#[tokio::test]
async fn comfyui_runs_a_workflow_with_an_uploaded_image_for_free() {
    let home = Home::new("");
    let app = start(&home);
    let info = get_provider(&app, ProviderParams { provider: "comfyui".into() }).unwrap();
    assert_eq!((info.configured, info.missing_env.clone()), (false, vec!["COMFYUI_URL".to_owned()]), "the token is optional");

    let comfy = Comfy::default();
    home.config(&format!("COMFYUI_URL = \"{}\"\n", mock_comfyui(comfy.clone()).await));
    let app = start(&home);
    assert!(get_provider(&app, ProviderParams { provider: "comfyui".into() }).unwrap().configured);
    let a = import(&app, &home, "a.png").await;
    set_budget(&app, SetBudgetParams { name: "gpu".into(), limit_usd: 0.0 }).unwrap();

    let workflow = json!({
        "10": { "class_type": "LoadImage", "inputs": { "image": placeholder("asset", &a.id) } },
        "9": { "class_type": "SaveImage", "inputs": { "images": ["8", 0] } },
    });
    let p = GenerateParams { provider: "comfyui".into(), model: "i2i".into(), budget: Some("gpu".into()), ..params(workflow, None) };
    let job = finished(&app, generate(&app, p).await.unwrap()).await;

    let prompt = comfy.prompts.lock().unwrap()[0].clone();
    assert_eq!(prompt["prompt"]["10"]["inputs"]["image"], "a.png", "the upload's file name lands in LoadImage");
    assert_eq!(comfy.authorized.load(Ordering::SeqCst), 0, "no token configured, no Authorization header");
    assert_eq!(*comfy.views.lock().unwrap(), ["filename=out%201.png&subfolder=&type=output"], "temp previews are skipped");
    assert_eq!((job.cost, job.cost_unit.as_deref(), job.assets.len()), (Some(0.0), Some("USD"), 1));
    assert_eq!(job.inputs[0].asset_id, a.id);
}

/// A 1x1 PNG.
const PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

/// MOCK_SPEC as `pixel`, answering every submit with PIXEL_PNG.
async fn pixel_app(home: &Home) -> App {
    let app = start(home);
    let base = serve(|_| {
        Router::new()
            .route("/submit", post(|| async { Json(json!({ "out": format!("data:image/png;base64,{PIXEL_PNG}") })) }))
            .route("/upload", post(|| async { Json(json!({ "url": "https://cdn.test/up.png?X-Amz-Signature=abc" })) }))
    })
    .await;
    save_spec(&app, SaveSpecParams { spec: mock_spec("pixel", &base) }).await.unwrap();
    app
}

#[tokio::test]
async fn files_carry_provenance_back_into_a_library() {
    use crate::library::provenance;
    let home = Home::new("MOCK_KEY = \"secret-key-123\"\n");
    let app = pixel_app(&home).await;
    let parent = import(&app, &home, "parent.png").await;
    let input = json!({
        "prompt": "red fox",
        "image_url": placeholder("asset", &parent.id),
        "style_url": "https://user:pw@cdn.test/style.png?X-Amz-Signature=abc",
        "note": "key secret-key-123",
    });
    let job = generate(&app, GenerateParams { provider: "pixel".into(), ..params(input, None) }).await.unwrap();
    let out = finished(&app, job).await.assets.remove(0);

    let bytes = std::fs::read(&out.path).unwrap();
    assert_eq!(out.bytes, bytes.len() as i64, "size is that of the file with metadata");
    let stored: provenance::Provenance = serde_json::from_str(&provenance::png_text(&bytes).unwrap()).unwrap();
    assert_eq!((stored.provider.as_str(), stored.model.as_str(), stored.job_id.as_str()), ("pixel", "m", out.job_id.as_deref().unwrap()));
    assert_eq!(stored.parents, [parent.id.as_str()]);
    assert_eq!(stored.input["image_url"], placeholder("asset", &parent.id));
    assert_eq!(stored.input["style_url"], "https://cdn.test/style.png", "no credentials or signature");
    assert_eq!(stored.input["note"], "key <redacted>");
    assert!(!String::from_utf8_lossy(&bytes).contains("secret-key-123"));

    // same library: delete, import the copy, the same id comes back with prompt and lineage
    let copy = home.join("copy.png");
    std::fs::copy(&out.path, &copy).unwrap();
    delete_asset(&app, AssetParams { asset_id: out.id.clone() }).unwrap();
    let reimport = || ImportAssetParams { path: copy.to_string_lossy().into(), ..Default::default() };
    let back = import_asset(&app, reimport()).await.unwrap();
    assert_eq!(back.asset.id, out.id);
    assert_eq!(back.asset.prompt.as_deref(), Some("red fox"));
    assert_eq!(back.asset.tags, "pixel m");
    assert!(back.missing_parents.is_empty());
    let ancestors = get_lineage(&app, AssetParams { asset_id: out.id.clone() }).unwrap().ancestors;
    assert_eq!(ancestors.iter().map(|l| l.asset.id.as_str()).collect::<Vec<_>>(), [parent.id.as_str()]);

    // another library: the job is recreated, the parent links up once imported
    let other_home = Home::new("");
    let other = pixel_app(&other_home).await;
    let moved = import_asset(&other, reimport()).await.unwrap();
    assert_eq!((moved.asset.id.as_str(), moved.asset.job_id.as_deref()), (out.id.as_str(), out.job_id.as_deref()));
    assert_eq!(moved.missing_parents, [parent.id.as_str()]);
    assert!(moved.hint.unwrap().contains("import their files"));
    std::fs::copy(&parent.path, other_home.join("parent.png")).unwrap();
    import(&other, &other_home, "parent.png").await;
    let ancestors = get_lineage(&other, AssetParams { asset_id: out.id.clone() }).unwrap().ancestors;
    assert_eq!(ancestors.len(), 1);
    let job_id = moved.asset.job_id.unwrap();
    let recipe = save_recipe(&other, SaveRecipeParams { name: "fox".into(), from_job: Some(job_id), ..Default::default() }).unwrap();
    assert_eq!((recipe.provider.as_deref(), recipe.input.as_ref().unwrap()["prompt"].as_str()), (Some("pixel"), Some("red fox")));

    // turned off: the stored file is the provider's
    home.config("MOCK_KEY = \"k\"\nEMBED_METADATA = \"false\"\n");
    until(|| async { (app.env().get("EMBED_METADATA") == Some("false")).then_some(()) }).await;
    let job = generate(&app, GenerateParams { provider: "pixel".into(), ..params(json!({ "prompt": "plain" }), None) }).await.unwrap();
    let plain = finished(&app, job).await.assets.remove(0);
    assert!(provenance::png_text(&std::fs::read(&plain.path).unwrap()).is_none());
}

#[tokio::test]
async fn jpeg_and_video_carry_provenance() {
    use crate::library::provenance;
    if !ffmpeg_installed() {
        return;
    }
    let (app, _, home) = mock_app().await;
    let jpg = home.join("x.jpg");
    let out = ffmpeg().args(["-v", "error", "-y", "-f", "lavfi", "-i", "color=red:size=16x16", "-frames:v", "1"]).arg(&jpg).output().unwrap();
    assert!(out.status.success());
    let data = std::fs::read(&jpg).unwrap();
    let json = r#"{"generator":"ai-media-router/0","job_id":"j","provider":"p","model":"m","input":{}}"#;
    let once = provenance::jpeg_with(&data, json).unwrap();
    let twice = provenance::jpeg_with(&once, json).unwrap();
    assert_eq!(once, twice, "an older comment is replaced");
    assert_eq!(provenance::jpeg_text(&twice).as_deref(), Some(json));
    std::fs::write(&jpg, &twice).unwrap();
    let decoded = ffmpeg().args(["-v", "error", "-i"]).arg(&jpg).args(["-f", "null", "-"]).output().unwrap();
    assert!(decoded.status.success() && decoded.stderr.is_empty(), "{}", String::from_utf8_lossy(&decoded.stderr));

    let clip = synth(&app, &home, "clip.mp4", &["-f", "lavfi", "-i", "testsrc=size=64x48:rate=10:duration=2", "-pix_fmt", "yuv420p"]).await;
    let cut = trim(&app, TrimParams { asset_id: clip.id.clone(), start: Some(0.5), end: Some(1.5), save_to: None }).await.unwrap();
    let copy = home.join("cut.mp4");
    std::fs::copy(&cut.asset.path, &copy).unwrap();
    delete_asset(&app, AssetParams { asset_id: cut.asset.id.clone() }).unwrap();
    let back = import_asset(&app, ImportAssetParams { path: copy.to_string_lossy().into(), ..Default::default() }).await.unwrap();
    let p = back.provenance.unwrap();
    assert_eq!((p.provider.as_str(), p.model.as_str(), p.job_id.as_str()), ("local", "trim", cut.job_id.as_str()));
    assert_eq!((back.asset.id, p.parents), (cut.asset.id, vec![clip.id]));
    assert!(near(back.asset.duration_secs, 1.0));
}

fn submits(seen: &Seen) -> usize {
    seen.submits.lock().unwrap().len()
}

fn reusing(input: Value, reuse: Option<bool>, save_to: Option<SaveTo>) -> GenerateParams {
    GenerateParams { reuse, ..params(input, save_to) }
}

#[tokio::test]
async fn reuse_returns_finished_outputs_at_no_cost() {
    let (app, seen, home) = mock_app().await;
    let a = import(&app, &home, "a.png").await;
    save_reference(&app, hero(vec![a.id.clone()])).unwrap();
    let by_asset = json!({ "prompt": "cat", "image": placeholder("asset", &a.id) });
    let original = finished(&app, generate(&app, params(by_asset, None)).await.unwrap()).await;
    assert_eq!((submits(&seen), seen.uploads()), (1, 1));

    // the same file through a reference, after its upload expired, is the same request
    app.db.conn().execute("UPDATE uploads SET expires_at = 0", []).unwrap();
    let by_ref = json!({ "image": "{{ref:hero}}", "prompt": "cat" });
    let pre = preflight(&app, reusing(by_ref.clone(), Some(true), None).into()).await.unwrap();
    assert_eq!(pre.reuse, Some(reuse::Match::Done { job_id: original.id.clone() }));
    assert_eq!(pre.estimate_usd, Some(0.0));
    let est = EstimateCostParams { provider: Some("mock".into()), model: Some("m".into()), input: obj(by_ref.clone()), reuse: Some(true), ..Default::default() };
    let est = estimate_cost(&app, est).await.unwrap();
    assert_eq!((est.estimate_usd, est.reuse.map(|m| m.job_id().to_owned())), (Some(0.0), Some(original.id.clone())));

    let reused = generate(&app, reusing(by_ref.clone(), Some(true), Some(into_collection("picks")))).await.unwrap();
    assert_ne!(reused.id, original.id, "a new job row keeps this request in history");
    assert_eq!((reused.status.as_str(), reused.reused_from.as_deref()), ("done", Some(original.id.as_str())));
    assert_eq!((reused.cost, reused.cost_unit.as_deref()), (Some(0.0), Some("USD")));
    let ids = |job: &Job| job.assets.iter().map(|x| x.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&reused), ids(&original));
    assert_eq!(reused.inputs.iter().map(|i| &i.asset_id).collect::<Vec<_>>(), [&a.id]);
    assert!(reused.hint.as_deref().unwrap().contains("Reused the outputs"));
    assert_eq!((submits(&seen), seen.uploads()), (1, 1), "nothing reached the provider");
    let filed = search_assets(&app, in_collection("picks")).unwrap().assets;
    assert_eq!(filed.iter().map(|x| x.id.clone()).collect::<Vec<_>>(), ids(&original));
    let chained = generate(&app, reusing(json!({ "image": job_input(&reused.id, None) }), Some(false), None)).await.unwrap();
    assert_eq!(chained.status, "running", "a reused job chains like its original");

    let stats = get_stats(&app, StatsParams { provider: Some("mock".into()), model: Some("m".into()), collection: None }).unwrap();
    assert_eq!(stats.models[0].jobs, 2, "the reused row is not a generation: original + chained");

    let fresh = generate(&app, params(json!({ "prompt": "cat", "image": placeholder("asset", &a.id) }), None)).await.unwrap();
    assert!(fresh.reused_from.is_none(), "reuse is off by default once the original finished");
    finished(&app, fresh).await;
    let n = submits(&seen);
    let outputs: Vec<String> = app
        .db
        .conn()
        .prepare("SELECT asset_id FROM job_outputs o JOIN jobs j ON j.id = o.job_id WHERE j.model = 'm' AND j.reused_from IS NULL")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let all: Vec<&str> = outputs.iter().map(String::as_str).collect();
    review_asset(&app, ReviewAssetParams { asset_ids: all.iter().map(|s| s.to_string()).collect(), status: Some(ReviewStatus::Rejected), rating: None, notes: None })
        .unwrap();
    let anew = generate(&app, reusing(by_ref, Some(true), None)).await.unwrap();
    assert!(anew.reused_from.is_none(), "rejected outputs are not reused");
    assert_eq!(submits(&seen), n + 1);
}

#[tokio::test]
async fn identical_request_while_running_is_not_submitted_twice() {
    let (app, seen, _home) = mock_app().await;
    let input = json!({ "prompt": "slow" });
    let first = generate(&app, params(input.clone(), None)).await.unwrap();
    hold(&app, &first);

    let again = generate(&app, params(input.clone(), None)).await.unwrap();
    assert_eq!(again.id, first.id, "a re-call after a client timeout gets the running job");
    assert!(again.note.as_deref().unwrap().contains("reuse: false"));
    let pre = preflight(&app, params(input.clone(), None).into()).await.unwrap();
    assert_eq!(pre.reuse, Some(reuse::Match::Running { job_id: first.id.clone() }));

    let follower = generate(&app, params(input.clone(), Some(into_collection("c")))).await.unwrap();
    assert_eq!((follower.status.as_str(), follower.reused_from.as_deref()), ("waiting", Some(first.id.as_str())));
    assert!(follower.hint.as_deref().unwrap().contains("identical request"));
    assert_eq!(submits(&seen), 1);

    set_lease(&app, &first.id, 0);
    let first = finished(&app, first).await;
    let follower = finished(&app, follower).await;
    assert_eq!((follower.cost, follower.assets[0].id.clone()), (Some(0.0), first.assets[0].id.clone()));
    assert_eq!(search_assets(&app, in_collection("c")).unwrap().assets.len(), 1, "the follower filed the outputs");
    assert_eq!(submits(&seen), 1);

    let variant = generate(&app, params(input.clone(), None)).await.unwrap();
    assert!(variant.reused_from.is_none() && variant.note.is_none(), "a finished job is reused only with reuse: true");
    hold(&app, &variant);
    let forced = generate(&app, reusing(input.clone(), Some(false), None)).await.unwrap();
    assert_ne!(forced.id, variant.id, "reuse: false always submits");
    hold(&app, &forced);
    assert_eq!(submits(&seen), 3);

    let old = generate(&app, params(json!({ "prompt": "old" }), None)).await.unwrap();
    hold(&app, &old);
    app.db.conn().execute("UPDATE jobs SET created_at = created_at - 600 WHERE id = ?1", [&old.id]).unwrap();
    let late = generate(&app, params(json!({ "prompt": "old" }), None)).await.unwrap();
    assert_ne!(late.id, old.id, "past the in-flight window an identical request is a new variant");
    hold(&app, &late);

    let n = submits(&seen);
    let twins = GenerateBatchParams {
        defaults: Some(BatchDefaults { model: Some("m".into()), ..mock_defaults(json!({ "prompt": "twin" })) }),
        items: Some(vec![BatchItem::default(), BatchItem::default()]),
        ..Default::default()
    };
    let batch = generate_batch(&app, twins).await.unwrap();
    assert_eq!(submits(&seen), n + 2, "identical items of one batch are variants");
    for job in &batch.jobs {
        set_lease(&app, &job.id, i64::MAX);
    }
    let rerun = GenerateBatchParams {
        defaults: Some(BatchDefaults { model: Some("m".into()), ..mock_defaults(json!({ "prompt": "twin" })) }),
        ..Default::default()
    };
    let rerun = generate_batch(&app, rerun).await.unwrap();
    assert_eq!(submits(&seen), n + 2, "a re-sent batch follows the running jobs");
    let job = get_job(&app, GetJobParams { job_id: rerun.jobs[0].id.clone(), ..Default::default() }).await.unwrap();
    assert!(batch.jobs.iter().any(|j| Some(&j.id) == job.reused_from.as_ref()), "{:?}", job.reused_from);
}

fn steps(v: Value) -> Vec<Step> {
    serde_json::from_value(v).unwrap()
}

fn chain_recipe(name: &str, steps: Vec<Step>) -> SaveRecipeParams {
    SaveRecipeParams { name: name.into(), steps: Some(steps), save_to: Some(into_collection("story")), ..Default::default() }
}

/// still -> video on mock, each step $0.4.
fn story_steps() -> Vec<Step> {
    steps(json!([
        { "name": "still", "provider": "mock", "model": "m", "input": { "prompt": "a {{var:subject}}" }, "estimated_cost_usd": 0.4 },
        { "name": "video", "provider": "mock", "model": "m", "estimated_cost_usd": 0.4,
          "input": { "prompt": "animate", "image": "{{step:still}}", "frames": ["{{step:still:0}}"] } },
    ]))
}

fn run_story(vars: Value) -> RunRecipeParams {
    RunRecipeParams { name: "story".into(), vars: Some(obj(vars)), ..Default::default() }
}

#[tokio::test]
async fn chain_recipe_runs_every_step_in_one_batch() {
    let (app, seen, _home) = mock_app().await;
    let recipe = save_recipe(&app, chain_recipe("story", story_steps())).unwrap();
    assert_eq!((recipe.provider.as_deref(), recipe.steps.len()), (None, 2));
    assert!(recipe.variables["subject"].default.is_none(), "a variable used in a step is declared");
    let shown = serde_json::to_value(&recipe).unwrap();
    assert!(shown.get("provider").is_none() && shown["steps"][1]["input"]["image"] == "{{step:still}}", "{shown}");

    let refuse = |s: Value| error_of(save_recipe(&app, chain_recipe("bad", steps(s))));
    let step_of = |name: &str, input: Value| json!({ "name": name, "provider": "mock", "model": "m", "input": input });
    assert!(refuse(json!([step_of("a", json!({ "image": "{{step:b}}" })), step_of("b", json!({}))])).contains("`{{step:b}}` names no earlier step; it is the first step"));
    assert!(refuse(json!([step_of("a", json!({})), step_of("a", json!({}))])).contains("two steps are named `a`"));
    assert!(refuse(json!([step_of("a", json!({})), step_of("b", json!({ "prompt": "x {{step:a}}" }))])).contains("whole string value"));
    assert!(refuse(json!([step_of("a", json!({})), step_of("b", json!({ "image": "{{step:a:middle}}" }))])).contains("last_frame / first_frame"));
    assert!(refuse(json!([{ "name": "a", "op": "trim", "provider": "mock", "input": {} }])).contains("not both"));
    assert!(refuse(json!([{ "name": "a", "provider": "local", "model": "trim", "input": {} }])).contains("write the edit as"));
    assert!(refuse(json!([{ "name": "a", "input": {} }])).contains("needs op (extract_frame"));
    let mixed = SaveRecipeParams { provider: Some("mock".into()), ..chain_recipe("bad", story_steps()) };
    assert!(error_of(save_recipe(&app, mixed)).contains("give `steps` alone"));

    set_budget(&app, SetBudgetParams { name: "b".into(), limit_usd: 0.7 }).unwrap();
    let over = RunRecipeParams { budget: Some("b".into()), ..run_story(json!({ "subject": "fox" })) };
    let err = error_of(run_recipe(&app, over).await);
    assert!(err.contains("recipe `story` of 2 steps") && err.contains("would be exceeded"), "{err}");
    assert!(list_jobs(&app, ListJobsParams::default()).unwrap().jobs.is_empty(), "nothing started over budget");
    let per_run = RunRecipeParams { estimated_cost_usd: Some(1.0), ..run_story(json!({ "subject": "fox" })) };
    assert!(error_of(run_recipe(&app, per_run).await).contains("to each generation step"));
    assert!(error_of(run_recipe(&app, run_story(json!({}))).await).contains("needs `subject`"));

    let check = |limit: f64| {
        set_budget(&app, SetBudgetParams { name: "b".into(), limit_usd: limit }).unwrap();
        PreflightParams { recipe: Some("story".into()), vars: Some(obj(json!({ "subject": "fox" }))), budget: Some("b".into()), ..Default::default() }
    };
    let checked = preflight(&app, check(0.7)).await.unwrap();
    assert!(!checked.ready && level(&checked, "budget") == Some(preflight::Level::Fail));
    let checked = preflight(&app, check(1.0)).await.unwrap();
    assert!(checked.ready, "{:?}", checked.checks.iter().map(|c| (&c.name, &c.detail)).collect::<Vec<_>>());
    assert_eq!(checked.estimate_usd.map(|u| (u * 10.0).round()), Some(8.0));
    assert_eq!((level(&checked, "still/provider"), level(&checked, "video/steps")), (Some(preflight::Level::Ok), Some(preflight::Level::Ok)));
    let input = checked.input.unwrap();
    assert_eq!((input["still"]["prompt"].as_str(), input["video"]["image"].as_str()), (Some("a fox"), Some("<output of step still>")));
    assert_eq!(checked.requests.len(), 2);
    assert!(seen.submits.lock().unwrap().is_empty(), "preflight sends nothing");

    let estimate = estimate_cost(&app, EstimateCostParams { recipe: Some("story".into()), vars: Some(obj(json!({ "subject": "x" }))), ..Default::default() });
    let estimate = estimate.await.unwrap();
    assert_eq!((estimate.steps.len(), estimate.estimate_usd.map(|u| (u * 10.0).round())), (2, Some(8.0)));
    assert_eq!(estimate.steps[0].how, "given");

    let started = run_recipe(&app, RunRecipeParams { budget: Some("b".into()), ..run_story(json!({ "subject": "fox" })) }).await.unwrap();
    assert_eq!((started.step.as_deref(), started.status.as_str()), (Some("video"), "waiting"));
    assert!(started.hint.as_deref().unwrap().contains("get_batch"), "{:?}", started.hint);
    let batch = settled_batch(&app, started.batch_id.as_deref().unwrap()).await;
    assert_eq!((batch.recipe.as_deref(), batch.counts.get("done")), (Some("story"), Some(&2)), "{:?}", batch.jobs.iter().map(|j| &j.error).collect::<Vec<_>>());
    assert_eq!(batch.jobs.iter().map(|j| j.step.as_deref().unwrap()).collect::<Vec<_>>(), ["still", "video"]);
    let video = &batch.jobs[1];
    assert_eq!(batch.outputs.iter().map(|a| &a.id).collect::<Vec<_>>(), video.outputs.iter().collect::<Vec<_>>(), "only the final step's outputs");
    assert_eq!(seen.input(0)["prompt"], "a fox");
    assert_eq!((seen.input(1)["image"].as_str(), seen.input(1)["frames"].clone()), (Some("https://cdn.test/up-1.png"), json!(["https://cdn.test/up-1.png"])));
    let filed: Vec<_> = search_assets(&app, in_collection("story")).unwrap().assets.into_iter().map(|a| a.id).collect();
    assert_eq!(filed, video.outputs, "the recipe's save_to files the final step only");
    assert_eq!(list_budgets(&app).unwrap().budgets[0].jobs, 2);

    let copy = |chain: bool| SaveRecipeParams { name: "copy".into(), from_job: Some(video.id.clone()), chain: Some(chain), ..Default::default() };
    let single = save_recipe(&app, copy(false)).unwrap();
    assert!(single.steps.is_empty() && single.input.unwrap()["image"].as_str().unwrap().starts_with("{{job:"));
    let walked = save_recipe(&app, copy(true)).unwrap();
    assert_eq!(walked.steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["image", "image-2"]);
    assert_eq!((walked.steps[0].input["prompt"].as_str(), walked.steps[1].input["image"].as_str()), (Some("a fox"), Some("{{step:image}}")));
    assert_eq!(walked.steps[1].input["frames"], json!(["{{step:image:0}}"]));
    let from_asset = SaveRecipeParams { from_job: None, from_asset: Some(video.outputs[0].clone()), ..copy(true) };
    assert_eq!(save_recipe(&app, from_asset).unwrap().steps.len(), 2);

    let resource = serde_json::to_string(&get_recipe(&app, NameParams { name: "story".into() }).unwrap()).unwrap();
    assert!(resource.contains("{{step:still:0}}"), "{resource}");
    let text = get_prompt(&app, GetPromptParams { name: "recipe:story".into(), arguments: Some(obj(json!({ "subject": "owl" }))) }).unwrap().text;
    for part in ["2 steps: still → video", "1. `still`: generate mock / m", "- `subject` (required)", "preflight {recipe: \"story\"", "get_batch"] {
        assert!(text.contains(part), "{part} missing in {text}");
    }
    let batch_of_chain = GenerateBatchParams { recipe: Some("story".into()), ..Default::default() };
    assert!(error_of(generate_batch(&app, batch_of_chain).await).contains("has 2 steps"));
}

#[tokio::test]
async fn failed_step_fails_the_steps_after_it() {
    let (app, _, _home) = mock_app().await;
    let mut chain = story_steps();
    chain.push(steps(json!([{ "name": "end", "provider": "mock", "model": "m", "input": { "image": "{{step:video}}" } }])).remove(0));
    save_recipe(&app, chain_recipe("story", chain)).unwrap();
    let last = run_recipe(&app, run_story(json!({ "subject": "fox" }))).await.unwrap();
    let batch = get_batch(&app, GetBatchParams { batch_id: last.batch_id.clone().unwrap(), wait_secs: None }).await.unwrap();
    let still = &batch.jobs[0].id;
    hold(&app, &jobs::get(&app, still, false).unwrap());
    app.db.conn().execute("UPDATE jobs SET status = 'failed', error = 'boom' WHERE id = ?1", [still]).unwrap();
    let video = settled(&app, &batch.jobs[1].id).await;
    assert_eq!(video.error.as_deref(), Some(format!("step `still` (job {still}) failed: boom").as_str()));
    let end = settled(&app, &last.id).await;
    assert!(end.error.as_deref().unwrap().starts_with(&format!("step `video` (job {}) failed: step `still`", video.id)), "{:?}", end.error);
    let batch = settled_batch(&app, &batch.id).await;
    assert_eq!((batch.counts.get("failed"), batch.outputs.len()), (Some(&3), 0));
}

#[tokio::test]
async fn chain_recipe_runs_edits_between_generations() {
    if !ffmpeg_installed() {
        return;
    }
    let (app, seen, home) = mock_app().await;
    let clip = synth(&app, &home, "clip.mp4", &["-f", "lavfi", "-i", "testsrc=size=160x120:rate=10:duration=2", "-pix_fmt", "yuv420p"]).await;
    let tone = synth(&app, &home, "tone.wav", &["-f", "lavfi", "-i", "sine=frequency=220:duration=1"]).await;
    let cut_steps = steps(json!([
        { "name": "cut", "op": "trim", "input": { "asset_id": placeholder("asset", &clip.id), "start": "{{var:start}}" } },
        { "name": "scored", "op": "mux_audio", "input": { "video_asset_id": "{{step:cut}}", "audio_asset_id": tone.id }, "save_to": into_collection("scored") },
        { "name": "next", "provider": "mock", "model": "m", "input": { "prompt": "continue", "image": "{{step:scored:last_frame}}" } },
    ]));
    save_recipe(&app, chain_recipe("cut", cut_steps)).unwrap();
    let vars = || Some(obj(json!({ "start": 0.5 })));
    let checked = preflight(&app, PreflightParams { recipe: Some("cut".into()), vars: vars(), ..Default::default() }).await.unwrap();
    assert!(checked.ready, "{:?}", checked.checks.iter().map(|c| (&c.name, &c.detail)).collect::<Vec<_>>());
    assert_eq!(checked.estimate_usd, None, "the generation step has no estimate");
    assert_eq!(level(&checked, "cut/op"), Some(preflight::Level::Ok));
    let bad = RunRecipeParams { name: "cut".into(), vars: Some(obj(json!({ "start": "soon" }))), ..Default::default() };
    let err = error_of(run_recipe(&app, bad).await);
    assert!(err.contains("step `cut`") && err.contains("trim parameters"), "{err}");

    let last = run_recipe(&app, RunRecipeParams { name: "cut".into(), vars: vars(), ..Default::default() }).await.unwrap();
    let batch = settled_batch(&app, last.batch_id.as_deref().unwrap()).await;
    assert_eq!(batch.counts.get("done"), Some(&3), "{:?}", batch.jobs.iter().map(|j| &j.error).collect::<Vec<_>>());
    let cut = jobs::get(&app, &batch.jobs[0].id, false).unwrap();
    assert_eq!((cut.provider.as_str(), cut.model.as_str(), cut.cost, cut.step.as_deref()), ("local", "trim", Some(0.0), Some("cut")));
    let cut_out = asset_info(&app, &cut.assets[0].id).await;
    assert!(near(cut_out.duration_secs, 1.5), "{:?}", cut_out.duration_secs);
    let scored = asset_info(&app, &batch.jobs[1].outputs[0]).await;
    assert_eq!(scored.has_audio, Some(true));
    assert_eq!(search_assets(&app, in_collection("scored")).unwrap().assets[0].id, scored.id, "a step's own save_to");
    assert_eq!(seen.input(0)["image"], "https://cdn.test/up-1.png");
    let ancestors: Vec<_> = get_lineage(&app, AssetParams { asset_id: batch.outputs[0].id.clone() }).unwrap().ancestors.into_iter().map(|l| l.asset.id).collect();
    assert!([&scored.id, &cut_out.id, &clip.id, &tone.id].iter().all(|id| ancestors.contains(id)), "{ancestors:?}");

    let walked = save_recipe(&app, SaveRecipeParams { name: "again".into(), from_job: Some(last.id.clone()), chain: Some(true), ..Default::default() }).unwrap();
    let ops: Vec<_> = walked.steps.iter().map(|s| (s.name.as_str(), s.op)).collect();
    assert_eq!(ops, [("trim", Some(Op::Trim)), ("mux_audio", Some(Op::MuxAudio)), ("image", None)]);
    assert_eq!(walked.steps[1].input["video_asset_id"], "{{step:trim}}");
    assert_eq!(walked.steps[1].input["audio_asset_id"], tone.id.as_str(), "an imported file stays as it is");
    assert_eq!(walked.steps[2].input["image"], "{{step:mux_audio:last_frame}}");

    let direct = trim(&app, TrimParams { asset_id: clip.id.clone(), start: Some(1.0), ..Default::default() }).await.unwrap();
    let mux = MuxAudioParams { video_asset_id: direct.asset.id.clone(), audio_asset_id: tone.id.clone(), save_to: None };
    let muxed = mux_audio(&app, mux).await.unwrap();
    let walked = save_recipe(&app, SaveRecipeParams { name: "direct".into(), from_job: Some(muxed.job_id), chain: Some(true), ..Default::default() }).unwrap();
    assert_eq!(walked.steps[1].input["video_asset_id"], "{{step:trim}}", "plain asset ids of an edit become step references");

    let doomed = steps(json!([
        { "name": "still", "provider": "mock", "model": "m", "input": { "prompt": "x" } },
        { "name": "framed", "op": "reframe", "input": { "asset_id": "{{step:still}}", "aspect": "9:16" } },
        { "name": "after", "provider": "mock", "model": "m", "input": { "image": "{{step:framed}}" } },
    ]));
    save_recipe(&app, chain_recipe("doomed", doomed)).unwrap();
    let last = run_recipe(&app, RunRecipeParams { name: "doomed".into(), ..Default::default() }).await.unwrap();
    let after = settled(&app, &last.id).await;
    let framed = &settled_batch(&app, last.batch_id.as_deref().unwrap()).await.jobs[1];
    assert_eq!(framed.status, "failed", "the mock's bytes are no image ffmpeg reads");
    assert!(after.error.as_deref().unwrap().starts_with(&format!("step `framed` (job {}) failed:", framed.id)), "{:?}", after.error);
}

async fn asset_info(app: &App, id: &str) -> Asset {
    get_asset(app, AssetParams { asset_id: id.into() }).await.unwrap()
}

/// A spec for key checks; `check` is the `[auth_check]` path, and `env` its only credential.
fn key_spec(key: &str, base: &str, env: &str, check: Option<&str>) -> String {
    let mut spec = format!(
        r#"
key = "{key}"
name = "Key Mock"
catalog = []
docs = "https://example.test/{key}"
model_hint = "-"
base_url = "{base}"

[auth]
type = "bearer"
env = ["{env}"]

[submit]
url = "/submit"
outputs = ["/out"]
"#
    );
    if let Some(url) = check {
        spec.push_str(&format!("\n[auth_check]\nurl = \"{url}\"\n"));
    }
    spec
}

async fn key_api() -> String {
    serve(|_| {
        Router::new()
            .route("/check/ok", get(|| async { Json(json!({})) }))
            .route("/check/401", get(|| async { (StatusCode::UNAUTHORIZED, Json(json!({ "error": "invalid api key" }))) }))
    })
    .await
}

#[tokio::test]
async fn list_keys_masks_values_and_points_at_the_set_command() {
    let home = Home::new("KEYONE_API_KEY = \"sk-live-1234567890abcd\"\n");
    let app = start(&home);
    let base = key_api().await;
    save_spec(&app, SaveSpecParams { spec: key_spec("keyone", &base, "KEYONE_API_KEY", Some("/check/ok")) }).await.unwrap();
    save_spec(&app, SaveSpecParams { spec: key_spec("keytwo", &base, "KEYTWO_API_KEY", None) }).await.unwrap();

    let keys = list_keys(&app).unwrap();
    let json = serde_json::to_string(&keys).unwrap();
    assert!(!json.contains("sk-live-1234567890abcd"), "a key leaked: {json}");
    let one = keys.keys.iter().find(|k| k.name == "KEYONE_API_KEY").unwrap();
    assert!(one.set, "{json}");
    assert_eq!(one.source.as_deref(), Some("config_file"));
    assert_eq!(one.masked.as_deref(), Some("…abcd"));
    assert_eq!(one.providers, ["keyone"]);
    assert!(one.set_command.contains("keys set KEYONE_API_KEY"), "{}", one.set_command);
    assert!(one.set_command.contains("--home"), "a scratch home is not the default: {}", one.set_command);

    let two = keys.keys.iter().find(|k| k.name == "KEYTWO_API_KEY").unwrap();
    assert!(!two.set && two.masked.is_none() && two.source.is_none(), "{json}");
    assert!(two.set_command.contains("keys set KEYTWO_API_KEY"), "{}", two.set_command);
    assert_eq!(keys.config_file, home.join("config.toml").display().to_string());
}

#[tokio::test]
async fn test_key_checks_without_spending_or_leaking() {
    let base = key_api().await;
    let home = Home::new(
        "KEYOK_API_KEY = \"sk-ok-123456789\"\nKEYREF_API_KEY = \"sk-ref-987654321\"\nKEYPLAIN_API_KEY = \"sk-plain-123456789\"\n",
    );
    let app = start(&home);
    for (key, env, check) in [
        ("keyok", "KEYOK_API_KEY", Some("/check/ok")),
        ("keyref", "KEYREF_API_KEY", Some("/check/401")),
        ("keyplain", "KEYPLAIN_API_KEY", None),
        ("keymissing", "KEYMISSING_API_KEY", Some("/check/ok")),
    ] {
        save_spec(&app, SaveSpecParams { spec: key_spec(key, &base, env, check) }).await.unwrap();
    }

    let test = |provider: &str| test_key(&app, ProviderParams { provider: provider.into() });
    let accepted = test("keyok").await.unwrap();
    assert_eq!(accepted.result, "accepted", "{}", accepted.hint);
    let refused = test("keyref").await.unwrap();
    assert_eq!(refused.result, "refused", "{}", refused.hint);
    assert!(refused.hint.contains("create a new one at https://example.test/keyref"), "{}", refused.hint);
    assert!(refused.hint.contains("keys set KEYREF_API_KEY"), "{}", refused.hint);
    let unsupported = test("keyplain").await.unwrap();
    assert_eq!(unsupported.result, "not_supported");
    assert!(unsupported.hint.contains("no free check"), "{}", unsupported.hint);
    let missing = test("keymissing").await.unwrap();
    assert_eq!(missing.result, "not_configured");
    assert!(missing.hint.contains("KEYMISSING_API_KEY") && missing.hint.contains("keys set KEYMISSING_API_KEY"), "{}", missing.hint);

    let json = serde_json::to_string(&[&accepted, &refused, &unsupported, &missing]).unwrap();
    for secret in ["sk-ok-123456789", "sk-ref-987654321"] {
        assert!(!json.contains(secret), "a key leaked: {json}");
    }
}
