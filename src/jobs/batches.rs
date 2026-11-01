//! Many jobs started together: a list of items times a matrix of input values, admitted against a
//! budget as one sum, submitted a few at a time, and followed as one unit. One run of a recipe with
//! steps is a batch too, its jobs named by step.

use std::{
    collections::{BTreeMap, HashSet},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, stream};
use rusqlite::{OptionalExtension, params};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::{
    app::{App, NotFound},
    db::{new_id, now},
    jobs::{self, SaveTo, placeholders},
    library::assets::Asset,
    pricing::budgets,
};

const MAX_JOBS: usize = 100;
const CONCURRENCY: usize = 4;

/// One job of a batch, fully merged.
pub struct Planned {
    pub provider: String,
    pub model: String,
    pub input: Map<String, Value>,
    pub save_to: Option<SaveTo>,
    pub estimate_usd: Option<f64>,
    pub retry: jobs::Retry,
    pub reuse: Option<bool>,
}

#[derive(Serialize, JsonSchema)]
pub struct Batch {
    pub id: String,
    pub label: Option<String>,
    /// The recipe this batch is one run of.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipe: Option<String>,
    pub created_at: i64,
    /// Jobs per status (`waiting`, `running`, `done`, `failed`).
    pub counts: BTreeMap<String, usize>,
    /// In submission order.
    pub jobs: Vec<BatchJob>,
    /// Every output of the batch's finished jobs, in job order; for a recipe run only the final steps' outputs.
    pub outputs: Vec<Asset>,
    /// Jobs that were never created (bad placeholder, unknown reference), by their position in the batch.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<Refused>,
    pub hint: String,
}

#[derive(Serialize, JsonSchema)]
pub struct BatchJob {
    pub id: String,
    /// Recipe step name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    pub provider: String,
    pub model: String,
    pub status: String,
    pub error: Option<String>,
    /// Asset ids of its outputs.
    pub outputs: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct Refused {
    /// Position in the expanded batch (items x matrix), from 0.
    pub index: usize,
    pub error: String,
}

/// Every combination of the matrix values, keys in order; one empty combination without a matrix.
pub fn combinations(matrix: &BTreeMap<String, Vec<Value>>) -> Result<Vec<Map<String, Value>>> {
    let total = matrix.values().try_fold(1usize, |n, v| n.checked_mul(v.len())).unwrap_or(usize::MAX);
    if total > MAX_JOBS {
        bail!("the matrix alone makes {total} combinations, more than {MAX_JOBS}; split it into several generate_batch calls");
    }
    let mut combos = vec![Map::new()];
    for (field, values) in matrix {
        if values.is_empty() {
            bail!("matrix field `{field}` has no values; give at least one or drop the field");
        }
        combos = combos
            .iter()
            .flat_map(|c| {
                values.iter().map(move |v| {
                    let mut c = c.clone();
                    c.insert(field.clone(), v.clone());
                    c
                })
            })
            .collect();
    }
    Ok(combos)
}

/// Admits the whole batch against `budget`, then submits every job; a failed submit is a failed job.
pub async fn run(app: &App, label: Option<String>, budget: Option<&str>, planned: Vec<Planned>) -> Result<Batch> {
    if planned.is_empty() {
        bail!("the batch is empty: give `items`, a `matrix`, or both");
    }
    if planned.len() > MAX_JOBS {
        bail!("the batch expands to {} jobs, more than {MAX_JOBS}; split it into several generate_batch calls", planned.len());
    }
    for p in &planned {
        app.provider(&p.provider)?;
    }
    if let Some(budget) = budget {
        let missing = planned.iter().any(|p| p.estimate_usd.is_none());
        let total = (!missing).then(|| planned.iter().filter_map(|p| p.estimate_usd).sum::<f64>());
        budgets::admit(app, budget, total)
            .with_context(|| format!("batch of {} jobs, estimated_cost_usd per job (item or defaults)", planned.len()))?;
    }

    let id = open(app, label, None)?;
    let results: Vec<(usize, Result<jobs::Job>)> = stream::iter(planned.into_iter().enumerate())
        .map(|(index, p)| {
            let id = &id;
            async move {
                let spend = jobs::Spend { budget, estimate_usd: p.estimate_usd, retry: p.retry, reuse: p.reuse };
                let job = jobs::create_in(app, &p.provider, &p.model, Value::Object(p.input), p.save_to, spend, Some(id));
                (index, job.await)
            }
        })
        .buffered(CONCURRENCY)
        .collect()
        .await;
    let refused = results
        .into_iter()
        .filter_map(|(index, r)| r.err().map(|e| Refused { index, error: format!("{e:#}") }))
        .collect();
    let mut batch = get(app, &id)?;
    batch.refused = refused;
    Ok(batch)
}

/// Records an empty batch; `recipe` marks one run of a recipe with steps.
pub fn open(app: &App, label: Option<String>, recipe: Option<&str>) -> Result<String> {
    let id = new_id();
    app.db.conn().execute(
        "INSERT INTO batches (id, label, recipe, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![id, label, recipe, now()],
    )?;
    Ok(id)
}

pub fn get(app: &App, id: &str) -> Result<Batch> {
    let conn = app.db.conn();
    let (label, recipe, created_at): (Option<String>, Option<String>, i64) = conn
        .query_row("SELECT label, recipe, created_at FROM batches WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?
        .ok_or_else(|| NotFound(format!("no batch `{id}`; generate_batch or run_recipe returns its id")))?;
    let mut counts = BTreeMap::new();
    let (mut jobs, mut outputs) = (Vec::new(), Vec::new());
    let all = jobs::in_batch(app, &conn, id)?;
    // a recipe step whose outputs another step takes is an intermediate result
    let used: HashSet<String> = match recipe {
        Some(_) => all.iter().flat_map(|j| placeholders::job_ids(&j.input).unwrap_or_default()).collect(),
        None => HashSet::new(),
    };
    for job in all {
        *counts.entry(job.status.clone()).or_insert(0) += 1;
        let intermediate = used.contains(&job.id);
        jobs.push(BatchJob {
            outputs: job.assets.iter().map(|a| a.id.clone()).collect(),
            step: job.step,
            id: job.id,
            provider: job.provider,
            model: job.model,
            status: job.status,
            error: job.error,
        });
        if !intermediate {
            outputs.extend(job.assets);
        }
    }
    let unfinished = counts.get("running").copied().unwrap_or(0) + counts.get("waiting").copied().unwrap_or(0);
    let hint = if unfinished > 0 {
        format!("{unfinished} job(s) unfinished: call get_batch again with wait_secs (max 50).")
    } else if recipe.is_some() {
        "Finished. outputs[].path are the final files, jobs[].outputs every step's; for a failed step, get_job \
         {job_id, debug: true} shows why, and the steps after it name it in their error."
            .into()
    } else {
        "Finished. outputs[].path are the files; for a failed job, get_job {job_id, debug: true} shows why.".into()
    };
    Ok(Batch { id: id.to_owned(), label, recipe, created_at, counts, jobs, outputs, refused: Vec::new(), hint })
}

/// Returns once no job of the batch is waiting or running, or `timeout` passes.
pub async fn wait(app: &App, id: &str, timeout: Duration) -> Result<Batch> {
    let sql = "SELECT EXISTS (SELECT 1 FROM jobs WHERE batch_id = ?1 AND status = 'running')";
    jobs::wait_while(timeout, || Ok(app.db.conn().query_row(sql, [id], |r| r.get(0))?)).await?;
    get(app, id)
}
