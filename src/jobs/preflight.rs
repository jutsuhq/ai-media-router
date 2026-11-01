//! Checks before anything is sent or spent: one generation (`preflight`), every step of a recipe run
//! (`recipe`) and the whole installation (`doctor`).

use std::time::Duration;

use rusqlite::params;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    api::{Edit, GenerateParams},
    app::App,
    db::now,
    jobs::{self, Retry, placeholders, reuse},
    library::{media::{self, Tool}, references},
    pricing::{budgets, catalog, costs},
    providers::{exchange::Exchange, fixtures},
    templates::recipes::{self, Step},
};

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// A running job whose next poll is this overdue has no worker polling it.
const OVERDUE_SECS: i64 = 120;

#[derive(Debug, Serialize, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Serialize, JsonSchema)]
pub struct Check {
    pub name: String,
    pub level: Level,
    pub detail: String,
}

#[derive(Default)]
struct Checks(Vec<Check>);

impl Checks {
    fn add(&mut self, name: &str, level: Level, detail: impl Into<String>) {
        self.0.push(Check { name: name.into(), level, detail: detail.into() });
    }

    fn result<T>(&mut self, name: &str, r: anyhow::Result<T>, ok: impl FnOnce(&T) -> String) -> Option<T> {
        match r {
            Ok(v) => {
                self.add(name, Level::Ok, ok(&v));
                Some(v)
            }
            Err(e) => {
                self.add(name, Level::Fail, format!("{e:#}"));
                None
            }
        }
    }

    fn passed(&self) -> bool {
        self.0.iter().all(|c| c.level != Level::Fail)
    }
}

#[derive(Serialize, JsonSchema)]
pub struct Preflight {
    /// No check failed: generate with the same arguments would be submitted (or wait for its parent jobs).
    pub ready: bool,
    pub checks: Vec<Check>,
    pub estimate_usd: Option<f64>,
    /// The input as the provider would receive it; uploads are shown as `<upload of asset …>`. For a recipe
    /// with steps, each step's input by step name.
    pub input: Option<Value>,
    /// Library assets (or video frames) that would be uploaded to the provider first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub uploads: Vec<String>,
    /// Running jobs named by `{{job:…}}` that the job would wait for.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub waits_on: Vec<String>,
    /// The job whose outputs generate would return at no cost instead of submitting (`reuse`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reuse: Option<reuse::Match>,
    /// The HTTP requests generate would send, secrets masked.
    pub requests: Vec<Exchange>,
    pub hint: String,
}

fn empty() -> Preflight {
    Preflight {
        ready: false,
        checks: Vec::new(),
        estimate_usd: None,
        input: None,
        uploads: Vec::new(),
        waits_on: Vec::new(),
        reuse: None,
        requests: Vec::new(),
        hint: String::new(),
    }
}

pub async fn preflight(app: &App, p: &GenerateParams) -> Preflight {
    let mut checks = Checks::default();
    let mut out = empty();
    let provider = checks.result("provider", app.find_provider(&p.provider), |pr| format!("{} ({})", pr.spec.name, pr.origin.kind()));
    if let Some(provider) = &provider {
        match provider.missing_env().as_slice() {
            [] => checks.add("keys", Level::Ok, "set"),
            missing => checks.add(
                "keys",
                Level::Fail,
                format!("run `ai-media-router keys set {}` in a terminal (list_keys shows the exact command); picked up without restart", missing.join(", ")),
            ),
        }
    }
    if let Some(retry) = p.retry
        && retry.attempts > jobs::MAX_RETRIES
    {
        checks.add("retry", Level::Fail, format!("retry.attempts is at most {}", jobs::MAX_RETRIES));
    }

    let input = Value::Object(p.input.clone());
    let expanded = checks.result("text", recipes::expand_text(app, &input), |v| {
        if *v == input { "no text placeholders".into() } else { "snippets and reference text filled in".into() }
    });
    let plan = expanded.and_then(|e| {
        checks.result("inputs", placeholders::plan(app, &e), |plan| match (plan.uploads.len(), plan.waits_on.len()) {
            (0, 0) => "no library files".into(),
            (n, 0) => format!("{n} file(s) to upload"),
            (n, w) => format!("{n} file(s) to upload; waits for {w} running job(s)"),
        })
    });
    if let (Some(provider), Some(plan)) = (&provider, &plan)
        && !plan.uploads.is_empty()
        && provider.spec.upload.is_none()
    {
        checks.add(
            "upload",
            Level::Fail,
            format!("{} has no [upload]: put public https URLs of the files into `input` instead", provider.spec.key),
        );
    }
    if let Some(reference) = p.save_to.as_ref().and_then(|s| s.reference.as_deref()) {
        checks.result("save_to", references::require_id(&app.db.conn(), reference), |_| format!("reference `{reference}` exists"));
    }

    if provider.is_some() {
        out.reuse = reuse::lookup(app, &p.provider, &p.model, &input, p.reuse).unwrap_or(None);
    }
    out.estimate_usd = match (&out.reuse, p.estimated_cost_usd) {
        (Some(found), _) => {
            checks.add("cost", Level::Ok, format!("$0: returns the outputs of job {} (reuse: false submits anew)", found.job_id()));
            Some(0.0)
        }
        (None, given) => match given {
        Some(usd) => {
            checks.add("cost", Level::Ok, format!("${usd:.4} (given)"));
            Some(usd)
        }
        None if provider.is_some() => match costs::estimate(app, &p.provider, &p.model, &input, p.offer_id.as_deref()).await {
            Ok(est) => {
                let how = est.formula.clone().or(est.note.clone()).unwrap_or_default();
                match est.estimate_usd {
                    Some(usd) => checks.add("cost", Level::Ok, format!("${usd:.4}: {how}")),
                    None => checks.add("cost", Level::Warn, format!("no estimate ({how}); pass offer_id from search_prices")),
                }
                est.estimate_usd
            }
            Err(e) => {
                checks.add("cost", Level::Fail, format!("{e:#}"));
                None
            }
        },
        None => None,
        },
    };
    if let Some(budget) = &p.budget {
        let admitted = budgets::admit(app, budget, out.estimate_usd);
        checks.result("budget", admitted, |name| format!("fits within `{name}`"));
    }

    if let (Some(provider), Some(plan)) = (&provider, plan) {
        out.requests =
            checks.result("request", provider.dry_run(&p.model, &plan.input, &Map::new()), |r| format!("{} step(s) rendered", r.len())).unwrap_or_default();
        out.input = Some(plan.input);
        (out.uploads, out.waits_on) = (plan.uploads, plan.waits_on);
    }
    out.ready = checks.passed();
    out.hint = if out.ready {
        match &out.reuse {
            Some(found) => format!("Nothing was sent or spent. generate with the same arguments returns job {}'s outputs at no cost.", found.job_id()),
            None => "Nothing was sent or spent. generate with the same arguments starts the job.".into(),
        }
    } else {
        "Fix the failed checks; nothing was sent or spent.".into()
    };
    out.checks = checks.0;
    out
}

/// Generation steps as `preflight` and edit steps by their parameters and ffmpeg, checks named
/// `<step>/<check>`; the estimates summed and the budget checked once for the sum.
pub async fn recipe(app: &App, steps: &[Step], budget: Option<&str>, retry: Option<Retry>) -> Preflight {
    let mut checks = Checks::default();
    let mut out = empty();
    let mut inputs = Map::new();
    let (mut total, mut unpriced) = (Some(0.0), Vec::new());
    let ffmpeg = match steps.iter().any(|s| s.op.is_some()) {
        true => Some(media::run(app, Tool::Ffmpeg, ["-version"]).await),
        false => None,
    };
    for s in steps {
        let name = |check: &str| format!("{}/{check}", s.name);
        let Some((input, from_steps)) = checks.result(&name("steps"), recipes::show_steps(&s.input), |(_, refs)| match refs.len() {
            0 => "uses no earlier step".into(),
            n => format!("takes {n} output(s) of earlier steps once they are done"),
        }) else {
            continue;
        };
        match (s.op, &s.provider, &s.model) {
            (Some(op), ..) => {
                let shown = Value::Object(input);
                checks.result(&name("op"), Edit::parse(op, shown.clone()), |_| format!("{} parameters fit; runs locally for $0", op.as_str()));
                if let Some(expanded) = checks.result(&name("text"), recipes::expand_text(app, &shown), |_| "ok".into()) {
                    checks.result(&name("inputs"), placeholders::plan(app, &expanded), |plan| format!("{} library file(s)", plan.uploads.len()));
                }
                match &ffmpeg {
                    Some(Ok(Some(_))) | None => {}
                    Some(Ok(None)) => checks.add(&name("ffmpeg"), Level::Fail, format!("{:#}", media::missing(app, Tool::Ffmpeg))),
                    Some(Err(e)) => checks.add(&name("ffmpeg"), Level::Fail, format!("{e:#}")),
                }
                inputs.insert(s.name.clone(), shown);
            }
            (None, Some(provider), Some(model)) => {
                let p = GenerateParams {
                    provider: provider.clone(),
                    model: model.clone(),
                    input,
                    save_to: s.save_to.clone(),
                    budget: None,
                    estimated_cost_usd: s.estimated_cost_usd,
                    offer_id: s.offer_id.clone(),
                    retry,
                    reuse: None,
                    ..Default::default()
                };
                let one = preflight(app, &p).await;
                for c in one.checks {
                    checks.add(&name(&c.name), c.level, c.detail);
                }
                if !from_steps.is_empty() && app.find_provider(provider).is_ok_and(|pr| pr.spec.upload.is_none()) {
                    checks.add(&name("upload"), Level::Fail, format!("{provider} has no [upload], so it cannot take earlier steps' outputs"));
                }
                match one.estimate_usd {
                    Some(usd) => total = total.map(|t| t + usd),
                    None => {
                        total = None;
                        unpriced.push(s.name.as_str());
                    }
                }
                out.uploads.extend(one.uploads.into_iter().chain(from_steps));
                out.waits_on.extend(one.waits_on);
                out.requests.extend(one.requests);
                inputs.insert(s.name.clone(), one.input.unwrap_or(Value::Object(p.input)));
            }
            _ => checks.add(&name("step"), Level::Fail, "neither op nor provider and model; save_recipe the recipe again"),
        }
    }
    match total {
        Some(usd) => checks.add("cost", Level::Ok, format!("${usd:.4} for {} steps", steps.len())),
        None => checks.add(
            "cost",
            Level::Warn,
            format!("no estimate for step(s) {}: give them estimated_cost_usd or offer_id in the recipe", unpriced.join(", ")),
        ),
    }
    if let Some(budget) = budget {
        checks.result("budget", budgets::admit(app, budget, total), |name| format!("the sum fits within `{name}`"));
    }
    out.estimate_usd = total;
    out.input = Some(Value::Object(inputs));
    out.ready = checks.passed();
    out.hint = if out.ready {
        "Nothing was sent or spent. run_recipe with the same name and vars starts every step.".into()
    } else {
        "Fix the failed checks; nothing was sent or spent.".into()
    };
    out.checks = checks.0;
    out
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct DoctorParams {
    /// Also send one GET to every configured provider's base URL (any HTTP answer counts as reachable).
    pub online: Option<bool>,
}

#[derive(Serialize, JsonSchema)]
pub struct ProviderHealth {
    pub key: String,
    /// `builtin` or `override`.
    pub source: String,
    /// Recorded fixtures that still match the spec, e.g. "3/3".
    pub fixtures: String,
    /// Only with `online`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reachable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct Doctor {
    /// No check failed.
    pub ok: bool,
    pub checks: Vec<Check>,
    /// Configured providers and local overrides.
    pub providers: Vec<ProviderHealth>,
}

pub async fn doctor(app: &App, p: DoctorParams) -> Doctor {
    let mut checks = Checks::default();
    let probe = app.home.join(".doctor");
    let writable = std::fs::write(&probe, b"ok").and_then(|()| std::fs::remove_file(&probe));
    checks.result("home", writable.map_err(Into::into), |()| format!("{} is writable", app.home.display()));
    let integrity = app.db.conn().query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0));
    match integrity {
        Ok(s) if s == "ok" => checks.add("database", Level::Ok, "integrity ok"),
        Ok(s) => checks.add("database", Level::Fail, s),
        Err(e) => checks.add("database", Level::Fail, e.to_string()),
    }
    for (name, tool) in [("ffmpeg", Tool::Ffmpeg), ("ffprobe", Tool::Ffprobe)] {
        match media::run(app, tool, ["-version"]).await {
            Ok(Some(out)) => {
                let version = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or_default().chars().take(60).collect::<String>();
                checks.add(name, Level::Ok, version);
            }
            Ok(None) => checks.add(name, Level::Warn, format!("{:#} (media tools and last_frame chains need it)", media::missing(app, tool))),
            Err(e) => checks.add(name, Level::Warn, format!("{e:#}")),
        }
    }

    let registry = app.registry();
    for problem in &registry.problems {
        checks.add("spec", Level::Fail, format!("{}: {}", problem.file, problem.error));
    }
    let watched: Vec<_> = registry.iter().filter(|pr| pr.configured() || pr.origin.kind() == "override").cloned().collect();
    let configured: Vec<&str> = watched.iter().filter(|pr| pr.configured()).map(|pr| pr.spec.key.as_str()).collect();
    if configured.is_empty() {
        checks.add(
            "providers",
            Level::Fail,
            "no provider has its key set: run `ai-media-router keys set <NAME>` in a terminal (list_keys shows every name and command)".to_owned(),
        );
    } else {
        checks.add("providers", Level::Ok, format!("{} configured: {}", configured.len(), configured.join(", ")));
    }
    let mut providers = Vec::new();
    for pr in &watched {
        let results = fixtures::run_all(&pr.spec, &fixtures::for_key(&pr.spec.key, Some(&app.home))).await;
        let failed: Vec<_> = results.iter().filter(|r| !r.ok).map(|r| r.name.as_str()).collect();
        if !failed.is_empty() {
            checks.add("fixtures", Level::Fail, format!("{}: {} no longer match the spec (check_spec shows why)", pr.spec.key, failed.join(", ")));
        }
        let mut health = ProviderHealth {
            key: pr.spec.key.clone(),
            source: pr.origin.kind().into(),
            fixtures: format!("{}/{}", results.len() - failed.len(), results.len()),
            reachable: None,
            error: None,
        };
        if p.online.unwrap_or(false) && pr.configured() {
            let sent = app.http.get(&pr.spec.base_url).timeout(PROBE_TIMEOUT).send().await;
            health.reachable = Some(sent.is_ok());
            health.error = sent.err().map(|e| e.to_string());
        }
        providers.push(health);
    }
    let unreachable: Vec<&str> = providers.iter().filter(|h| h.reachable == Some(false)).map(|h| h.key.as_str()).collect();
    if !unreachable.is_empty() {
        checks.add("network", Level::Warn, format!("no answer from {}", unreachable.join(", ")));
    }

    for file in [catalog::VIDEO_FILE, catalog::IMAGE_FILE] {
        match std::fs::metadata(app.home.join(file)).and_then(|m| m.modified()) {
            Ok(t) => {
                let hours = t.elapsed().unwrap_or_default().as_secs() / 3600;
                checks.add("prices", Level::Ok, format!("{file} cached {hours} h ago (refreshed daily by search_prices)"));
            }
            Err(_) => checks.add("prices", Level::Ok, format!("{file} not downloaded yet; the first search_prices fetches it")),
        }
    }
    let env = app.env();
    let embeddings = env.get("EMBEDDINGS_API_KEY").or(env.get("EMBEDDINGS_URL")).is_some();
    checks.add("embeddings", Level::Ok, if embeddings { "configured" } else { "off: set EMBEDDINGS_API_KEY or EMBEDDINGS_URL for similar_assets" });

    let (overdue, failures) = {
        let conn = app.db.conn();
        let overdue: rusqlite::Result<i64> = conn.query_row(
            "SELECT COUNT(*) FROM jobs WHERE status = 'running' AND lease_until < ?1",
            params![now() - OVERDUE_SECS],
            |r| r.get(0),
        );
        let failures = conn
            .prepare(
                "SELECT error_kind, COUNT(*) FROM jobs WHERE status = 'failed' AND updated_at > ?1 AND error_kind IS NOT NULL
                 GROUP BY error_kind ORDER BY 2 DESC",
            )
            .and_then(|mut s| s.query_map([now() - 86_400], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.collect::<Result<Vec<_>, _>>());
        (overdue.unwrap_or(0), failures.unwrap_or_default())
    };
    if overdue > 0 {
        checks.add(
            "worker",
            Level::Warn,
            format!("{overdue} running job(s) not polled for over {OVERDUE_SECS}s: start `ai-media-router mcp` or `serve`, which poll them"),
        );
    }
    if !failures.is_empty() {
        let serious = failures.iter().any(|(k, _)| matches!(k.as_str(), "auth" | "quota" | "spec"));
        let summary: Vec<String> = failures.iter().map(|(k, n)| format!("{n} {k}")).collect();
        let level = if serious { Level::Warn } else { Level::Ok };
        checks.add("failures", level, format!("last 24 h: {} (list_jobs {{status: \"failed\", error_kind}})", summary.join(", ")));
    }
    Doctor { ok: checks.passed(), checks: checks.0, providers }
}
