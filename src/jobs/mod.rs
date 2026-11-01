pub mod batches;
pub mod failures;
pub mod placeholders;
pub mod preflight;
pub mod reuse;
pub mod stats;

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, Params, Row, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    app::{App, NotFound},
    db::{new_id, now},
    library::{assets::{self, Asset}, collections, media::Op, references},
    pricing::budgets,
    providers::{TaskState, as_text, exchange::Trace},
    templates::recipes,
};
use failures::ErrorKind;

const COLUMNS: &str = "id, provider, model, input, status, error, cost, created_at, progress, trace, cost_unit, save_to, budget, estimate_usd, \
                       error_kind, waiting_on, batch_id, attempts, reused_from, step";
/// Long enough for one poll plus downloading a large video before another process may retake the job.
const CLAIM_SECS: i64 = 600;
const RETRY_SECS: i64 = 30;
const GIVE_UP_SECS: i64 = 24 * 3600;
/// Consecutive failed polls (HTTP errors, broken spec) before the job is failed: ~10 minutes.
const MAX_POLL_ERRORS: i64 = 20;
/// Jobs claimed per worker tick; a full claim means more are due, so the worker skips its sleep.
const CLAIM_LIMIT: usize = 8;
const TICK: Duration = Duration::from_secs(2);
pub const MAX_RETRIES: u32 = 5;
const MAX_RETRY_DELAY: u64 = 3600;

#[derive(Serialize, JsonSchema)]
pub struct Job {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub input: Value,
    /// `waiting` (for the parent jobs in `waiting_on`), `running`, `done` or `failed`.
    pub status: String,
    pub error: Option<String>,
    /// Why a failed job failed; `hint` says what to do about it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ErrorKind>,
    /// Whether generating the same input again can succeed (rate_limit, provider_error, network, timeout).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    /// While running: the provider's last reported status, or why the last poll failed.
    pub progress: Option<String>,
    /// Reported by the provider, in `cost_unit`.
    pub cost: Option<f64>,
    /// e.g. "USD" or "credits"; null = the provider's own, unnamed unit.
    pub cost_unit: Option<String>,
    pub created_at: i64,
    pub assets: Vec<Asset>,
    /// Library assets resolved from `{{asset:…}}` / `{{ref:…}}` placeholders in `input`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<JobInput>,
    /// Where finished outputs are filed (`reference` is the reference id).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub save_to: Option<SaveTo>,
    /// Budget this job counts against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<String>,
    /// Estimate given at generate; counts while the job runs or when the provider reports no USD cost.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimate_usd: Option<f64>,
    /// Parent jobs named by `{{job:…}}` in `input`; the job is submitted once they are all done.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub waiting_on: Vec<String>,
    /// Batch the job was started in (generate_batch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    /// The job whose outputs this one returns instead of generating (`reuse`); it costs nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reused_from: Option<String>,
    /// Set when an identical request returned this already running job instead of submitting again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Recipe step this job runs (run_recipe of a multi-step recipe).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// Submissions so far, once a retryable failure was retried (`retry`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempts: Option<i64>,
    /// What to do next.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Last request and raw response per step, only with `debug`. Same shape as fixture `exchanges`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<Value>,
}

/// Resubmit after a retryable failure (rate_limit, provider_error, network, timeout).
#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
pub struct Retry {
    /// Extra submissions after the first, at most 5.
    pub attempts: u32,
    /// Seconds before the first retry (default 30); the n-th retry waits n times as long.
    pub delay_secs: Option<u64>,
}

impl Retry {
    fn delay(self) -> i64 {
        self.delay_secs.unwrap_or(RETRY_SECS as u64).min(MAX_RETRY_DELAY) as i64
    }
}

#[derive(Serialize, JsonSchema)]
pub struct JobInput {
    pub asset_id: String,
    /// Name of the reference the asset came through, if any.
    pub reference: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SaveTo {
    /// Collection name; created when missing.
    pub collection: Option<String>,
    /// Reference name or id; must exist (save_reference creates one).
    pub reference: Option<String>,
    /// Role given to the outputs inside the reference, e.g. "generated".
    pub role: Option<String>,
}

/// Narrows job history; filters combine with AND.
#[derive(Default)]
pub struct Filter<'a> {
    /// Jobs that used this reference as input or filed outputs into it.
    pub reference: Option<&'a str>,
    /// Jobs that used an asset of this collection or produced one.
    pub collection: Option<&'a str>,
    pub status: Option<&'a str>,
    pub error_kind: Option<ErrorKind>,
    /// Jobs of this batch.
    pub batch: Option<&'a str>,
}

impl Job {
    pub fn with_hint(mut self) -> Self {
        self.hint = Some(match (self.status.as_str(), &self.reused_from) {
            ("waiting", Some(original)) => format!(
                "An identical request (job {original}) is running: this job returns its outputs at no cost once it is done. \
                 Call get_job with wait_secs; generate with reuse: false submits a new variant instead."
            ),
            ("done", Some(original)) => format!(
                "Reused the outputs of job {original} (same provider, model and input) at no cost; generate with reuse: false \
                 for a new variant. Files are at assets[].path; use one later as \"{{{{asset:<id>}}}}\"."
            ),
            ("running", _) => "Call get_job again with wait_secs (max 50) until status is done.".into(),
            ("waiting", _) => format!(
                "Waits for parent job(s) {}: submitted automatically once they are done, failed if one fails. \
                 Call get_job with wait_secs; cancel_job stops it.",
                self.waiting_on.join(", ")
            ),
            ("done", _) => "Files are stored locally at assets[].path. Use one in a later generate as \"{{asset:<id>}}\"; \
                       get_lineage shows what it was made from."
                .into(),
            _ => format!(
                "{}Inspect the provider's raw responses with get_job {{job_id: \"{}\", debug: true}}. \
                 If the spec is wrong (status/outputs pointers, URL), fix it with get_spec → check_spec → save_spec.",
                self.error_kind.map(|k| format!("{} ", k.hint())).unwrap_or_default(),
                self.id
            ),
        });
        self
    }
}

/// Where a job's spending is counted and how it is retried; every field is optional.
#[derive(Default)]
pub struct Spend<'a> {
    pub budget: Option<&'a str>,
    pub estimate_usd: Option<f64>,
    pub retry: Retry,
    /// true: return a finished or running identical job's outputs; None: only merge into an identical
    /// job started in the last few minutes; false: always submit.
    pub reuse: Option<bool>,
}

pub async fn create(
    app: &App,
    provider: &str,
    model: &str,
    input: Value,
    save_to: Option<SaveTo>,
    spend: Spend<'_>,
) -> Result<Job> {
    let job = create_in(app, provider, model, input, save_to, spend, None).await?;
    if let ("failed", Some(error), Some(kind)) = (job.status.as_str(), job.error.as_deref(), job.error_kind) {
        let (id, retry) = (&job.id, if kind.retryable() { "retryable" } else { "not retryable" });
        return Err(anyhow!(
            "{error}\nError kind: {} ({retry}). {}\nJob {id} recorded as failed: get_job {{job_id: \"{id}\", debug: true}} shows the exact request and \
             raw response; check_spec {{provider: \"{provider}\", model, input}} renders the request without sending it.",
            kind.as_str(),
            kind.hint()
        ));
    }
    Ok(job)
}

/// Like `create`, but a failed submit comes back as a failed job instead of an error.
/// A `{{job:…}}` parent that is still running makes the job wait instead of submitting.
pub async fn create_in(
    app: &App,
    provider: &str,
    model: &str,
    input: Value,
    save_to: Option<SaveTo>,
    spend: Spend<'_>,
    batch: Option<&str>,
) -> Result<Job> {
    let p = app.provider(provider)?;
    if spend.retry.attempts > MAX_RETRIES {
        bail!("retry.attempts is at most {MAX_RETRIES}");
    }
    let expanded = recipes::expand_text(app, &input)?;
    let mut waiting = pending_parents(&app.db.conn(), &placeholders::job_ids(&expanded)?)
        .map_err(|e| anyhow!("{e:#}. Rerun the parent (generate) and use the new job id."))?;
    let fingerprint = reuse::fingerprint(app, &p.spec.key, model, &expanded);
    let found = fingerprint.as_deref().map(|f| reuse::find(&app.db.conn(), f, spend.reuse, batch)).transpose()?.flatten();
    let save_to = resolve_save_to(app, save_to)?;
    let reused_from = match found {
        Some(reuse::Match::Done { job_id }) => {
            let budget = spend.budget.map(|b| budgets::admit(app, b, Some(0.0))).transpose()?;
            let row = Reused { provider, model, input: &input, original: &job_id, save_to: save_to.as_ref(), budget, batch, fingerprint: &fingerprint };
            if let Some(job) = insert_reused(app, row)? {
                return Ok(job);
            }
            None
        }
        Some(reuse::Match::Running { job_id }) => {
            let theirs: Option<String> = app.db.conn().query_row("SELECT save_to FROM jobs WHERE id = ?1", [&job_id], |r| r.get(0))?;
            if batch.is_none() && theirs == save_to.as_ref().map(serde_json::to_string).transpose()? {
                let mut job = get(app, &job_id, false)?;
                job.note = Some(format!(
                    "An identical request started this job {}s ago and it is still running, so nothing was submitted again. \
                     generate with reuse: false submits a new variant.",
                    now() - job.created_at
                ));
                return Ok(job);
            }
            waiting = vec![job_id.clone()];
            Some(job_id)
        }
        None => None,
    };
    let (estimate_usd, admitted) = if reused_from.is_some() { (None, Some(0.0)) } else { (spend.estimate_usd, spend.estimate_usd) };
    let budget = spend.budget.map(|b| budgets::admit(app, b, admitted)).transpose()?;
    let mut trace = Trace::new();
    let (submitted, inputs) = if waiting.is_empty() {
        let (resolved, inputs) = placeholders::resolve(app, provider, &expanded, &mut trace).await?;
        (Some(p.submit(model, &resolved, &mut trace).await), inputs)
    } else {
        (None, Vec::new())
    };
    let id = new_id();
    let t = now();
    let (vars, mut status, mut error) = match &submitted {
        Some(Ok(vars)) => (Value::Object(vars.clone()), "running", None),
        Some(Err(e)) => (Value::Object(Map::new()), "failed", Some(format!("{e:#}"))),
        None => (Value::Object(Map::new()), "running", None),
    };
    let mut kind = error.as_deref().map(|e| failures::classify(e, p.spec.error_kinds()));
    let mut waiting_on = (!waiting.is_empty()).then(|| serde_json::to_string(&waiting)).transpose()?;
    let mut lease_until = if waiting.is_empty() { t + poll_interval(0) } else { t };
    let (mut progress, mut retries_left, mut attempts) = (None, spend.retry.attempts, 1);
    // a retryable submit error goes to the worker's submit queue like a job whose parents are done
    if let (Some(k), Some(e)) = (kind, error.take_if(|_| kind.is_some_and(ErrorKind::retryable) && retries_left > 0)) {
        let wait = spend.retry.delay();
        progress = Some(retry_note(k, 1, wait, &e));
        (status, waiting_on, lease_until, retries_left, attempts, kind) =
            ("running", Some("[]".into()), t + wait, retries_left - 1, 2, None);
    }
    let remote_id = vars.get("id").map(as_text).unwrap_or_default();
    {
        let mut conn = app.db.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO jobs (id, provider, model, input, remote_id, vars, status, error, trace, lease_until, created_at,
                               updated_at, save_to, budget, estimate_usd, error_kind, waiting_on, batch_id, progress,
                               retries_left, retry_delay, attempts, fingerprint, reused_from)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
            params![
                id,
                provider,
                model,
                input.to_string(),
                remote_id,
                vars.to_string(),
                status,
                error,
                serde_json::to_string(&trace)?,
                lease_until,
                t,
                save_to.as_ref().map(serde_json::to_string).transpose()?,
                budget,
                estimate_usd,
                kind.map(ErrorKind::as_str),
                waiting_on,
                batch,
                progress,
                retries_left,
                spend.retry.delay(),
                attempts,
                fingerprint,
                reused_from
            ],
        )?;
        insert_inputs(&tx, &id, &inputs)?;
        tx.commit()?;
    }
    get(app, &id, false)
}

struct Reused<'a> {
    provider: &'a str,
    model: &'a str,
    input: &'a Value,
    original: &'a str,
    save_to: Option<&'a SaveTo>,
    budget: Option<String>,
    batch: Option<&'a str>,
    fingerprint: &'a Option<String>,
}

/// A finished job that returns the outputs of `original`; None when they left the library meanwhile.
fn insert_reused(app: &App, r: Reused) -> Result<Option<Job>> {
    let id = new_id();
    let outputs = {
        let mut conn = app.db.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO jobs (id, provider, model, input, remote_id, status, created_at, updated_at, save_to, budget, batch_id,
                               fingerprint, reused_from)
             VALUES (?1, ?2, ?3, ?4, '', 'running', ?5, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                id,
                r.provider,
                r.model,
                r.input.to_string(),
                now(),
                r.save_to.map(serde_json::to_string).transpose()?,
                r.budget,
                r.batch,
                r.fingerprint,
                r.original
            ],
        )?;
        let Some(outputs) = reuse::adopt(&tx, &id, r.original)? else { return Ok(None) };
        tx.commit()?;
        outputs
    };
    if let Some(s) = r.save_to {
        for asset in &outputs {
            file_output(app, s, asset)?;
        }
    }
    get(app, &id, false).map(Some)
}

/// `save_to` with its reference name replaced by the reference id; an error when it does not exist.
pub fn resolve_save_to(app: &App, save_to: Option<SaveTo>) -> Result<Option<SaveTo>> {
    save_to
        .map(|mut s| -> Result<SaveTo> {
            if let Some(r) = &s.reference {
                s.reference = Some(references::require_id(&app.db.conn(), r)?);
            }
            Ok(s)
        })
        .transpose()
}

/// A recipe's edit step: a waiting `local` job (model = the operation) that the worker runs with
/// ffmpeg once every job named by `{{job:…}}` in `input` is done; `'[]'` queues it at once.
pub fn create_edit(app: &App, op: Op, input: Value, save_to: Option<SaveTo>, batch: &str) -> Result<Job> {
    let parents = placeholders::job_ids(&recipes::expand_text(app, &input)?)?;
    pending_parents(&app.db.conn(), &parents)?;
    let save_to = resolve_save_to(app, save_to)?;
    let (id, t) = (new_id(), now());
    app.db.conn().execute(
        "INSERT INTO jobs (id, provider, model, input, remote_id, status, lease_until, created_at, updated_at, save_to,
                           waiting_on, batch_id)
         VALUES (?1, 'local', ?2, ?3, '', 'running', ?4, ?4, ?4, ?5, ?6, ?7)",
        params![
            id,
            op.as_str(),
            input.to_string(),
            t,
            save_to.as_ref().map(serde_json::to_string).transpose()?,
            serde_json::to_string(&parents)?,
            batch
        ],
    )?;
    get(app, &id, false)
}

/// Names the recipe step a job runs, for get_batch and for the errors of the steps after it.
pub fn set_step(app: &App, id: &str, step: &str) -> Result<()> {
    app.db.conn().execute("UPDATE jobs SET step = ?2 WHERE id = ?1", params![id, step])?;
    Ok(())
}

/// Marks a waiting edit job done once its output is stored.
pub fn finish_local(app: &App, id: &str, inputs: &[&str]) -> Result<()> {
    let mut conn = app.db.conn();
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE jobs SET status = 'done', cost = 0, cost_unit = 'USD', waiting_on = NULL, progress = NULL, updated_at = ?2
         WHERE id = ?1 AND status = 'running'",
        params![id, now()],
    )?;
    let inputs: Vec<_> = inputs.iter().map(|a| placeholders::Input { asset_id: (*a).to_owned(), ref_id: None }).collect();
    insert_inputs(&tx, id, &inputs)?;
    tx.commit()?;
    Ok(())
}

fn insert_inputs(conn: &Connection, job_id: &str, inputs: &[placeholders::Input]) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare_cached("INSERT OR IGNORE INTO job_inputs (job_id, asset_id, ref_id) VALUES (?1, ?2, ?3)")?;
    for i in inputs {
        stmt.execute(params![job_id, i.asset_id, i.ref_id])?;
    }
    Ok(())
}

/// Parents that are not done yet; an error when one failed or does not exist.
fn pending_parents(conn: &Connection, parents: &[String]) -> Result<Vec<String>> {
    let mut pending = Vec::new();
    let mut stmt = conn.prepare_cached("SELECT status, error, step FROM jobs WHERE id = ?1")?;
    for parent in parents {
        let row: Option<(String, Option<String>, Option<String>)> =
            stmt.query_row([parent], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
        match row {
            None => {
                return Err(NotFound(format!("no job `{parent}` (from {{{{job:{parent}}}}}); list_jobs shows job ids")).into());
            }
            Some((status, error, step)) if status == "failed" => {
                bail!("{} failed: {}", parent_name(parent, step.as_deref()), error.unwrap_or_default())
            }
            Some((status, ..)) if status == "running" => pending.push(parent.clone()),
            Some(_) => {}
        }
    }
    Ok(pending)
}

fn parent_name(id: &str, step: Option<&str>) -> String {
    match step {
        Some(step) => format!("step `{step}` (job {id})"),
        None => format!("parent job {id}"),
    }
}

/// Fails a waiting or running job with `cancelled`, and every job waiting on it. The provider is not told.
pub fn cancel(app: &App, id: &str) -> Result<Job> {
    let job = get(app, id, false)?;
    if !matches!(job.status.as_str(), "running" | "waiting") {
        bail!("job `{id}` is already {}; only waiting or running jobs can be cancelled", job.status);
    }
    fail(app, id, "cancelled")?;
    fail_children(app, id, "cancelled")?;
    get(app, id, false)
}

fn fail_children(app: &App, parent: &str, error: &str) -> Result<()> {
    let children: Vec<String> = {
        let conn = app.db.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT id FROM jobs WHERE status = 'running' AND waiting_on IS NOT NULL
               AND EXISTS (SELECT 1 FROM json_each(waiting_on) WHERE value = ?1)",
        )?;
        stmt.query_map([parent], |r| r.get(0))?.collect::<Result<_, _>>()?
    };
    let step: Option<String> = app.db.conn().query_row("SELECT step FROM jobs WHERE id = ?1", [parent], |r| r.get(0))?;
    let error = format!("{} failed: {error}", parent_name(parent, step.as_deref()));
    for child in children {
        fail(app, &child, &error)?;
        fail_children(app, &child, &error)?;
    }
    Ok(())
}

pub fn get(app: &App, id: &str, debug: bool) -> Result<Job> {
    let conn = app.db.conn();
    let mut job = conn
        .prepare_cached(&format!("SELECT {COLUMNS} FROM jobs WHERE id = ?1"))?
        .query_row([id], |r| from_row(r, debug))
        .optional()?
        .ok_or_else(|| NotFound(format!("no job `{id}`")))?;
    job.assets = assets::for_job(&conn, &app.assets_dir, id)?;
    job.inputs = load_inputs(&conn, &serde_json::json!([id]).to_string())?.remove(id).unwrap_or_default();
    Ok(job)
}

pub fn list(app: &App, filter: &Filter, limit: usize) -> Result<Vec<Job>> {
    let conn = app.db.conn();
    let ref_id = filter.reference.map(|r| references::require_id(&conn, r)).transpose()?;
    let sql = format!(
        "SELECT {COLUMNS} FROM jobs
         WHERE (?2 IS NULL OR id IN (SELECT job_id FROM job_inputs WHERE ref_id = ?2)
                OR id IN (SELECT o.job_id FROM ref_assets ra JOIN job_outputs o ON o.asset_id = ra.asset_id WHERE ra.ref_id = ?2))
           AND (?3 IS NULL OR id IN (SELECT i.job_id FROM job_inputs i JOIN collection_assets ca ON ca.asset_id = i.asset_id
                                     WHERE ca.collection = ?3)
                OR id IN (SELECT o.job_id FROM collection_assets ca JOIN job_outputs o ON o.asset_id = ca.asset_id
                          WHERE ca.collection = ?3))
           AND (?4 IS NULL OR status = ?4)
           AND (?5 IS NULL OR error_kind = ?5)
           AND (?6 IS NULL OR batch_id = ?6)
         ORDER BY created_at DESC LIMIT ?1"
    );
    let args =
        params![limit as i64, ref_id, filter.collection, filter.status, filter.error_kind.map(ErrorKind::as_str), filter.batch];
    query(app, &conn, &sql, args)
}

/// Jobs of a batch, in submission order.
pub fn in_batch(app: &App, conn: &Connection, batch_id: &str) -> Result<Vec<Job>> {
    query(app, conn, &format!("SELECT {COLUMNS} FROM jobs WHERE batch_id = ?1 ORDER BY rowid"), [batch_id])
}

/// Jobs selected by `sql`, with outputs and inputs loaded in one query each.
fn query(app: &App, conn: &Connection, sql: &str, args: impl Params) -> Result<Vec<Job>> {
    let mut jobs = conn.prepare_cached(sql)?.query_map(args, |r| from_row(r, false))?.collect::<Result<Vec<_>, _>>()?;
    if jobs.is_empty() {
        return Ok(jobs);
    }
    let ids = serde_json::to_string(&jobs.iter().map(|j| &j.id).collect::<Vec<_>>())?;
    let sql = format!(
        "SELECT {}, o.job_id AS output_of FROM job_outputs o JOIN assets a ON a.id = o.asset_id
         WHERE o.job_id IN (SELECT value FROM json_each(?1)) ORDER BY o.rowid",
        assets::COLUMNS
    );
    let mut outputs: HashMap<String, Vec<Asset>> = HashMap::new();
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut rows = stmt.query([&ids])?;
    while let Some(r) = rows.next()? {
        outputs.entry(r.get("output_of")?).or_default().push(assets::from_row(&app.assets_dir, r)?);
    }
    let mut inputs = load_inputs(conn, &ids)?;
    for job in &mut jobs {
        job.assets = outputs.remove(&job.id).unwrap_or_default();
        job.inputs = inputs.remove(&job.id).unwrap_or_default();
    }
    Ok(jobs)
}

/// Inputs per job id, for a JSON array of job ids.
fn load_inputs(conn: &Connection, ids: &str) -> Result<HashMap<String, Vec<JobInput>>> {
    let mut stmt = conn.prepare_cached(
        "SELECT i.job_id, i.asset_id, r.name FROM job_inputs i LEFT JOIN refs r ON r.id = i.ref_id
         WHERE i.job_id IN (SELECT value FROM json_each(?1)) ORDER BY i.rowid",
    )?;
    let mut rows = stmt.query([ids])?;
    let mut inputs: HashMap<String, Vec<JobInput>> = HashMap::new();
    while let Some(r) = rows.next()? {
        inputs.entry(r.get(0)?).or_default().push(JobInput { asset_id: r.get(1)?, reference: r.get(2)? });
    }
    Ok(inputs)
}

/// Returns once the job is done or failed, or `timeout` passes, whichever comes first.
pub async fn wait(app: &App, id: &str, timeout: Duration, debug: bool) -> Result<Job> {
    let sql = "SELECT status = 'running' FROM jobs WHERE id = ?1";
    wait_while(timeout, || Ok(app.db.conn().query_row(sql, [id], |r| r.get(0)).optional()?.unwrap_or(false))).await?;
    get(app, id, debug)
}

/// Checks `busy` every tick until it is false or `timeout` passes.
pub async fn wait_while(timeout: Duration, mut busy: impl FnMut() -> Result<bool>) -> Result<()> {
    let start = Instant::now();
    loop {
        let left = timeout.saturating_sub(start.elapsed());
        if !busy()? || left.is_zero() {
            return Ok(());
        }
        tokio::time::sleep(left.min(TICK)).await;
    }
}

/// Polls due jobs forever. Safe to run in several processes at once: jobs are claimed by lease.
pub async fn run_worker(app: App) {
    loop {
        let due = claim(&app, "IS NULL", "vars, input, created_at, poll_errors, save_to", |r| {
            Ok(Due {
                id: r.get(0)?,
                provider: r.get(1)?,
                vars: r.get(2)?,
                input: r.get(3)?,
                created_at: r.get(4)?,
                poll_errors: r.get(5)?,
                save_to: r.get(6)?,
            })
        });
        let waiting = claim(&app, "IS NOT NULL", "model, input, waiting_on, reused_from", |r| {
            Ok(Waiting {
                id: r.get(0)?,
                provider: r.get(1)?,
                model: r.get(2)?,
                input: r.get(3)?,
                waiting_on: r.get(4)?,
                reused_from: r.get(5)?,
            })
        });
        let full = due.len() == CLAIM_LIMIT || waiting.len() == CLAIM_LIMIT;
        for job in due {
            tokio::spawn(advance(app.clone(), job));
        }
        for job in waiting {
            tokio::spawn(release(app.clone(), job));
        }
        if !full {
            tokio::time::sleep(TICK).await;
        }
    }
}

struct Due {
    id: String,
    provider: String,
    vars: String,
    input: String,
    created_at: i64,
    poll_errors: i64,
    save_to: Option<String>,
}

struct Waiting {
    id: String,
    provider: String,
    model: String,
    input: String,
    waiting_on: String,
    reused_from: Option<String>,
}

/// Leases up to CLAIM_LIMIT due running jobs whose `waiting_on` matches; rows are `id, provider, <columns>`.
fn claim<T>(app: &App, waiting_on: &str, columns: &str, row: impl FnMut(&Row) -> rusqlite::Result<T>) -> Vec<T> {
    let t = now();
    let sql = format!(
        "UPDATE jobs SET lease_until = ?1
         WHERE id IN (SELECT id FROM jobs WHERE status = 'running' AND waiting_on {waiting_on} AND lease_until <= ?2 LIMIT ?3)
         RETURNING id, provider, {columns}"
    );
    let conn = app.db.conn();
    let claimed = conn
        .prepare_cached(&sql)
        .and_then(|mut stmt| stmt.query_map(params![t + CLAIM_SECS, t, CLAIM_LIMIT as i64], row)?.collect());
    claimed.unwrap_or_else(|e| {
        tracing::warn!("claiming jobs failed: {e:#}");
        Vec::new()
    })
}

/// Submits a waiting job once all its parents are done, or fails it when one failed.
async fn release(app: App, job: Waiting) {
    let pending = serde_json::from_str::<Vec<String>>(&job.waiting_on)
        .map_err(Into::into)
        .and_then(|parents| pending_parents(&app.db.conn(), &parents));
    let outcome = match pending {
        Ok(pending) if !pending.is_empty() => app
            .db
            .conn()
            .execute("UPDATE jobs SET lease_until = ?2 WHERE id = ?1", params![job.id, now() + TICK.as_secs() as i64])
            .map(drop)
            .map_err(Into::into),
        Ok(_) => match &job.reused_from {
            Some(original) => finish_reused(&app, &job, original).await,
            None => submit_waiting(&app, &job).await,
        },
        Err(e) => fail(&app, &job.id, &format!("{e:#}")),
    };
    if let Err(e) = outcome {
        tracing::warn!(job = %job.id, "cannot release waiting job: {e:#}");
    }
}

/// Finishes a job that waited for an identical one; submits it after all if those outputs are gone.
async fn finish_reused(app: &App, job: &Waiting, original: &str) -> Result<()> {
    let outputs = {
        let mut conn = app.db.conn();
        let tx = conn.transaction()?;
        let outputs = reuse::adopt(&tx, &job.id, original)?;
        tx.commit()?;
        outputs
    };
    let Some(outputs) = outputs else {
        app.db.conn().execute("UPDATE jobs SET reused_from = NULL WHERE id = ?1", [&job.id])?;
        return submit_waiting(app, job).await;
    };
    let save_to: Option<String> = app.db.conn().query_row("SELECT save_to FROM jobs WHERE id = ?1", [&job.id], |r| r.get(0))?;
    if let Some(s) = save_to.as_deref().map(serde_json::from_str::<SaveTo>).transpose()? {
        for asset in &outputs {
            file_output(app, &s, asset)?;
        }
    }
    tracing::info!(job = %job.id, original, "reused");
    Ok(())
}

/// Submits a job whose parents are done, or a job due for a retry.
async fn submit_waiting(app: &App, job: &Waiting) -> Result<()> {
    if job.provider == "local" {
        return Box::pin(run_edit(app, job)).await;
    }
    let mut trace = Trace::new();
    let submitted = async {
        let p = app.provider(&job.provider)?;
        let input = recipes::expand_text(app, &serde_json::from_str(&job.input)?)?;
        let fingerprint = reuse::fingerprint(app, &p.spec.key, &job.model, &input);
        let (resolved, inputs) = placeholders::resolve(app, &job.provider, &input, &mut trace).await?;
        Ok::<_, anyhow::Error>((p.submit(&job.model, &resolved, &mut trace).await?, inputs, fingerprint))
    }
    .await;
    save_trace(app, &job.id, trace)?;
    let (vars, inputs, fingerprint) = match submitted {
        Ok(ok) => ok,
        Err(e) => return retry_or_fail(app, &job.id, &format!("{e:#}")),
    };
    let remote_id = vars.get("id").map(as_text).unwrap_or_default();
    let mut conn = app.db.conn();
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE jobs SET vars = ?2, remote_id = ?3, waiting_on = NULL, lease_until = ?4, updated_at = ?5,
                         fingerprint = COALESCE(?6, fingerprint)
         WHERE id = ?1 AND status = 'running'",
        params![job.id, Value::Object(vars).to_string(), remote_id, now() + poll_interval(0), now(), fingerprint],
    )?;
    insert_inputs(&tx, &job.id, &inputs)?;
    tx.commit()?;
    tracing::info!(job = %job.id, "submitted");
    Ok(())
}

/// Runs a waiting edit step with its placeholders replaced by asset ids; produce marks it done.
async fn run_edit(app: &App, job: &Waiting) -> Result<()> {
    let edited = async {
        let input = placeholders::resolve_ids(app, &recipes::expand_text(app, &serde_json::from_str(&job.input)?)?).await?;
        let save_to: Option<String> = app.db.conn().query_row("SELECT save_to FROM jobs WHERE id = ?1", [&job.id], |r| r.get(0))?;
        let save_to = save_to.as_deref().map(serde_json::from_str).transpose()?;
        let op: Op = serde_json::from_value(Value::String(job.model.clone()))?;
        crate::api::Edit::parse(op, input)?.run(app, Some(&job.id), save_to).await
    }
    .await;
    match edited {
        Ok(_) => {
            tracing::info!(job = %job.id, status = "done", "edit finished");
            Ok(())
        }
        Err(e) => fail(app, &job.id, &format!("{e:#}")),
    }
}

async fn advance(app: App, job: Due) {
    let mut trace = Trace::new();
    let result = step(&app, &job, &mut trace).await;
    if let Err(e) = save_trace(&app, &job.id, trace) {
        tracing::warn!(job = %job.id, "cannot store trace: {e:#}");
    }
    let Err(e) = result else { return };
    let errors = job.poll_errors + 1;
    let msg = format!("{e:#}");
    let outcome = if errors >= MAX_POLL_ERRORS {
        fail(&app, &job.id, &format!("gave up after {errors} failed polls: {msg}"))
    } else {
        tracing::warn!(job = %job.id, "poll failed, retrying in {RETRY_SECS}s: {msg}");
        let progress = format!("poll attempt {errors}/{MAX_POLL_ERRORS} failed, retrying: {msg}");
        app.db
            .conn()
            .prepare_cached("UPDATE jobs SET lease_until = ?2, progress = ?3, poll_errors = ?4, updated_at = ?5 WHERE id = ?1")
            .and_then(|mut stmt| stmt.execute(params![job.id, now() + RETRY_SECS, progress, errors, now()]))
            .map(drop)
            .map_err(Into::into)
    };
    if let Err(e) = outcome {
        tracing::warn!(job = %job.id, "cannot reschedule: {e:#}");
    }
}

async fn step(app: &App, job: &Due, trace: &mut Trace) -> Result<()> {
    let age = now() - job.created_at;
    if age > GIVE_UP_SECS {
        return fail(app, &job.id, "gave up after 24h without a result");
    }
    let provider = app.provider(&job.provider)?;
    let vars: Map<String, Value> = serde_json::from_str(&job.vars)?;
    match provider.poll(&vars, trace).await? {
        TaskState::Pending(progress) => {
            app.db
                .conn()
                .prepare_cached("UPDATE jobs SET lease_until = ?2, progress = ?3, poll_errors = 0, updated_at = ?4 WHERE id = ?1")?
                .execute(params![job.id, now() + poll_interval(age), progress, now()])?;
            Ok(())
        }
        TaskState::Done { outputs, cost } => {
            let stored: Value = serde_json::from_str(&job.input)?;
            let input = recipes::expand_text(app, &stored).unwrap_or(stored);
            let prompt = input.get("prompt").and_then(Value::as_str);
            let save_to: Option<SaveTo> = job.save_to.as_deref().map(serde_json::from_str).transpose()?;
            for output in outputs {
                let asset = assets::store_output(app, &provider, output, &job.id, prompt).await?;
                if let Some(s) = &save_to {
                    file_output(app, s, &asset.id)?;
                }
            }
            // a cancelled job stays failed when this poll was in flight
            app.db.conn().execute(
                "UPDATE jobs SET status = 'done', error = NULL, error_kind = NULL, cost = ?2, cost_unit = ?3, progress = NULL,
                                 updated_at = ?4
                 WHERE id = ?1 AND status = 'running'",
                params![job.id, cost, provider.spec.cost_unit, now()],
            )?;
            tracing::info!(job = %job.id, status = "done", "job finished");
            Ok(())
        }
        TaskState::Failed(msg) => retry_or_fail(app, &job.id, &msg),
    }
}

/// Records work done in this process (media operations) as a finished job: provider `local`,
/// `model` = the operation, cost 0 USD. `save_to.reference` must already be a reference id.
pub fn record_local(app: &App, model: &str, input: &Value, inputs: &[&str], save_to: Option<&SaveTo>) -> Result<String> {
    let id = new_id();
    let mut conn = app.db.conn();
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO jobs (id, provider, model, input, remote_id, status, cost, cost_unit, created_at, updated_at, save_to)
         VALUES (?1, 'local', ?2, ?3, '', 'done', 0, 'USD', ?4, ?4, ?5)",
        params![id, model, input.to_string(), now(), save_to.map(serde_json::to_string).transpose()?],
    )?;
    let inputs: Vec<_> = inputs.iter().map(|a| placeholders::Input { asset_id: (*a).to_owned(), ref_id: None }).collect();
    insert_inputs(&tx, &id, &inputs)?;
    tx.commit()?;
    Ok(id)
}

/// A reference deleted while the job ran is skipped; a collection is recreated.
pub fn file_output(app: &App, s: &SaveTo, asset_id: &str) -> Result<()> {
    let conn = app.db.conn();
    if let Some(name) = &s.collection {
        collections::ensure(&conn, name)?;
        collections::add_with(&conn, name, asset_id)?;
    }
    if let Some(ref_id) = &s.reference
        && references::find_id(&conn, ref_id)?.is_some()
    {
        references::add_with(&conn, ref_id, asset_id, s.role.as_deref())?;
    }
    Ok(())
}

/// Keeps the latest exchange per step, so the trace shows submit plus the most recent poll/result.
fn save_trace(app: &App, id: &str, fresh: Trace) -> Result<()> {
    if fresh.is_empty() {
        return Ok(());
    }
    let conn = app.db.conn();
    let old: String = conn.prepare_cached("SELECT trace FROM jobs WHERE id = ?1")?.query_row([id], |r| r.get(0))?;
    let mut trace: Trace = serde_json::from_str(&old).unwrap_or_default();
    for exchange in fresh {
        trace.retain(|e| e.step != exchange.step);
        trace.push(exchange);
    }
    conn.prepare_cached("UPDATE jobs SET trace = ?2 WHERE id = ?1")?.execute(params![id, serde_json::to_string(&trace)?])?;
    Ok(())
}

fn poll_interval(age_secs: i64) -> i64 {
    match age_secs {
        ..60 => 3,
        60..300 => 10,
        _ => 30,
    }
}

fn classify(app: &App, conn: &Connection, id: &str, error: &str) -> Result<ErrorKind> {
    let provider: String = conn.prepare_cached("SELECT provider FROM jobs WHERE id = ?1")?.query_row([id], |r| r.get(0))?;
    let spec = app.provider(&provider).ok();
    Ok(failures::classify(error, spec.as_ref().and_then(|p| p.spec.error_kinds())))
}

fn retry_note(kind: ErrorKind, attempt: i64, wait: i64, error: &str) -> String {
    format!("attempt {attempt} failed ({}), retrying in {wait}s: {error}", kind.as_str())
}

/// Puts a job with retries left back in the submit queue after a retryable failure; otherwise fails it.
fn retry_or_fail(app: &App, id: &str, error: &str) -> Result<()> {
    let conn = app.db.conn();
    let kind = classify(app, &conn, id, error)?;
    let (left, delay, attempts): (i64, i64, i64) = conn
        .prepare_cached("SELECT retries_left, retry_delay, attempts FROM jobs WHERE id = ?1")?
        .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    if !kind.retryable() || left <= 0 {
        drop(conn);
        return fail(app, id, error);
    }
    let wait = delay * attempts;
    conn.prepare_cached(
        "UPDATE jobs SET waiting_on = '[]', vars = '{}', remote_id = '', poll_errors = 0, retries_left = retries_left - 1,
                         attempts = attempts + 1, lease_until = ?2, progress = ?3, updated_at = ?4
         WHERE id = ?1 AND status = 'running'",
    )?
    .execute(params![id, now() + wait, retry_note(kind, attempts, wait, error), now()])?;
    tracing::info!(job = %id, "retryable failure, resubmitting in {wait}s");
    Ok(())
}

/// Only a running job fails, so a cancelled job keeps its error when a poll in flight completes.
fn fail(app: &App, id: &str, error: &str) -> Result<()> {
    let conn = app.db.conn();
    let kind = classify(app, &conn, id, error)?;
    conn.prepare_cached(
        "UPDATE jobs SET status = 'failed', error = ?2, cost = NULL, progress = NULL, updated_at = ?3, error_kind = ?4
         WHERE id = ?1 AND status = 'running'",
    )?
    .execute(params![id, error, now(), kind.as_str()])?;
    tracing::info!(job = %id, status = "failed", "job finished");
    Ok(())
}

fn from_row(r: &Row, debug: bool) -> rusqlite::Result<Job> {
    let input: String = r.get(3)?;
    let waiting_on: Vec<String> =
        r.get::<_, Option<String>>(15)?.and_then(|w| serde_json::from_str(&w).ok()).unwrap_or_default();
    let status: String = r.get(4)?;
    let status = if status == "running" && !waiting_on.is_empty() { "waiting".to_owned() } else { status };
    let trace = if debug {
        let raw: String = r.get(9)?;
        Some(serde_json::from_str(&raw).unwrap_or(Value::String(raw)))
    } else {
        None
    };
    let error: Option<String> = r.get(5)?;
    let stored: Option<String> = r.get(14)?;
    let error_kind = stored.as_deref().and_then(ErrorKind::parse).or_else(|| error.as_deref().map(|e| failures::classify(e, None)));
    Ok(Job {
        id: r.get(0)?,
        provider: r.get(1)?,
        model: r.get(2)?,
        input: serde_json::from_str(&input).unwrap_or(Value::String(input)),
        status,
        retryable: error_kind.map(ErrorKind::retryable),
        error_kind,
        error,
        progress: r.get(8)?,
        cost: r.get(6)?,
        cost_unit: r.get(10)?,
        created_at: r.get(7)?,
        assets: Vec::new(),
        inputs: Vec::new(),
        save_to: r.get::<_, Option<String>>(11)?.and_then(|s| serde_json::from_str(&s).ok()),
        budget: r.get(12)?,
        estimate_usd: r.get(13)?,
        waiting_on,
        batch_id: r.get(16)?,
        step: r.get(19)?,
        attempts: Some(r.get::<_, i64>(17)?).filter(|n| *n > 1),
        reused_from: r.get(18)?,
        note: None,
        hint: None,
        trace,
    })
}
