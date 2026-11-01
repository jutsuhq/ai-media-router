//! What worked, per provider and model: outcomes, failure kinds, review results, cost and speed.

use std::collections::BTreeMap;

use anyhow::Result;
use rusqlite::params;
use schemars::JsonSchema;
use serde::Serialize;

use crate::{
    app::App,
    jobs::failures::{self, ErrorKind},
};

#[derive(Serialize, JsonSchema)]
pub struct ModelStats {
    pub provider: String,
    pub model: String,
    pub jobs: i64,
    pub done: i64,
    pub failed: i64,
    /// Share of finished jobs (done + failed) that succeeded, 0-1.
    pub done_share: Option<f64>,
    pub failed_share: Option<f64>,
    /// Failed jobs per error kind.
    pub failure_kinds: BTreeMap<ErrorKind, i64>,
    /// Files produced by these jobs.
    pub outputs: i64,
    /// Mean review rating of rated outputs, 1-5.
    pub average_rating: Option<f64>,
    /// Share of outputs approved with review_asset, 0-1.
    pub approved_share: Option<f64>,
    pub rejected: i64,
    /// Mean cost of done jobs: reported USD cost, else the estimate given at generate.
    pub average_cost_usd: Option<f64>,
    /// Mean seconds from generate until the job was done.
    pub average_seconds: Option<f64>,
}

#[derive(Default)]
pub struct Filter<'a> {
    pub provider: Option<&'a str>,
    pub model: Option<&'a str>,
    /// Jobs that read from or wrote into this collection.
    pub collection: Option<&'a str>,
}

const JOB_FILTER: &str = "j.reused_from IS NULL AND (?1 IS NULL OR j.provider = ?1) AND (?2 IS NULL OR j.model = ?2)
    AND (?3 IS NULL OR j.id IN (SELECT i.job_id FROM job_inputs i JOIN collection_assets ca ON ca.asset_id = i.asset_id
                                WHERE ca.collection = ?3)
         OR j.id IN (SELECT o.job_id FROM collection_assets ca JOIN job_outputs o ON o.asset_id = ca.asset_id
                     WHERE ca.collection = ?3))";

#[derive(Default)]
struct Acc {
    jobs: i64,
    done: i64,
    failed: i64,
    kinds: BTreeMap<ErrorKind, i64>,
    costs: Vec<f64>,
    seconds: Vec<f64>,
    outputs: i64,
    ratings: Vec<f64>,
    approved: i64,
    rejected: i64,
}

/// Most used provider+model first.
pub fn get(app: &App, filter: &Filter) -> Result<Vec<ModelStats>> {
    let conn = app.db.conn();
    let args = params![filter.provider, filter.model, filter.collection];
    let mut groups: BTreeMap<(String, String), Acc> = BTreeMap::new();

    let sql = format!(
        "SELECT j.provider, j.model, j.status, j.error, j.error_kind,
                CASE WHEN j.cost IS NOT NULL AND j.cost_unit = 'USD' THEN j.cost ELSE j.estimate_usd END,
                j.updated_at - j.created_at
         FROM jobs j WHERE {JOB_FILTER}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(args)?;
    while let Some(r) = rows.next()? {
        let acc = groups.entry((r.get(0)?, r.get(1)?)).or_default();
        acc.jobs += 1;
        match r.get_ref(2)?.as_str()? {
            "done" => {
                acc.done += 1;
                acc.costs.extend(r.get::<_, Option<f64>>(5)?);
                acc.seconds.push(r.get::<_, i64>(6)? as f64);
            }
            "failed" => {
                acc.failed += 1;
                let error: Option<String> = r.get(3)?;
                let stored: Option<String> = r.get(4)?;
                let kind = stored
                    .as_deref()
                    .and_then(ErrorKind::parse)
                    .unwrap_or_else(|| failures::classify(error.as_deref().unwrap_or(""), None));
                *acc.kinds.entry(kind).or_default() += 1;
            }
            _ => {}
        }
    }

    let sql = format!(
        "SELECT j.provider, j.model, a.status, a.rating FROM jobs j
         JOIN job_outputs o ON o.job_id = j.id JOIN assets a ON a.id = o.asset_id WHERE {JOB_FILTER}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(args)?;
    while let Some(r) = rows.next()? {
        let acc = groups.entry((r.get(0)?, r.get(1)?)).or_default();
        acc.outputs += 1;
        match r.get_ref(2)?.as_str()? {
            "approved" => acc.approved += 1,
            "rejected" => acc.rejected += 1,
            _ => {}
        }
        acc.ratings.extend(r.get::<_, Option<i64>>(3)?.map(|v| v as f64));
    }

    let mean = |v: &[f64]| (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64);
    let share = |part: i64, whole: i64| (whole > 0).then(|| part as f64 / whole as f64);
    let mut stats: Vec<ModelStats> = groups
        .into_iter()
        .map(|((provider, model), a)| ModelStats {
            provider,
            model,
            jobs: a.jobs,
            done: a.done,
            failed: a.failed,
            done_share: share(a.done, a.done + a.failed),
            failed_share: share(a.failed, a.done + a.failed),
            failure_kinds: a.kinds,
            outputs: a.outputs,
            average_rating: mean(&a.ratings),
            approved_share: share(a.approved, a.outputs),
            rejected: a.rejected,
            average_cost_usd: mean(&a.costs),
            average_seconds: mean(&a.seconds),
        })
        .collect();
    stats.sort_by_key(|s| std::cmp::Reverse(s.jobs));
    Ok(stats)
}
