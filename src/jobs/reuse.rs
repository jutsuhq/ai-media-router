//! Not paying twice: a fingerprint hashes provider, model and expanded input with library files named
//! by asset id (a content hash), not by upload URL. A match becomes a new job row marked `reused_from`
//! that links the original's outputs at cost 0; stats and price history skip such rows.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{app::App, db::now, jobs::placeholders, templates::recipes};

/// Without `reuse`, an identical request is merged into a running one only this soon after it started:
/// long enough for an agent re-calling after a client timeout, short enough not to swallow a later variant.
pub const IN_FLIGHT_SECS: i64 = 300;

/// The fingerprint of an already expanded input; None when a placeholder cannot be planned
/// (the submit reports that error itself).
pub fn fingerprint(app: &App, provider: &str, model: &str, expanded: &Value) -> Option<String> {
    let plan = placeholders::plan(app, expanded).ok()?;
    // serde_json maps are sorted by key, so this is canonical JSON
    let canonical = json!([provider, model, plan.input]).to_string();
    Some(hex::encode(Sha256::digest(canonical.as_bytes())))
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Match {
    /// A finished job whose outputs are all still in the library and none rejected.
    Done { job_id: String },
    /// The same request, still waiting or running.
    Running { job_id: String },
}

impl Match {
    pub fn job_id(&self) -> &str {
        match self {
            Match::Done { job_id } | Match::Running { job_id } => job_id,
        }
    }
}

/// What generate would reuse for this request, for preflight and estimate_cost.
pub fn lookup(app: &App, provider: &str, model: &str, input: &Value, reuse: Option<bool>) -> Result<Option<Match>> {
    let key = app.find_provider(provider)?.spec.key.clone();
    let expanded = recipes::expand_text(app, input)?;
    let Some(fingerprint) = fingerprint(app, &key, model, &expanded) else { return Ok(None) };
    find(&app.db.conn(), &fingerprint, reuse, None)
}

/// `reuse`: true matches finished and running jobs of any age, None only running jobs started in the
/// last IN_FLIGHT_SECS outside `batch` (a batch's identical items are variants), false nothing.
pub fn find(conn: &Connection, fingerprint: &str, reuse: Option<bool>, batch: Option<&str>) -> Result<Option<Match>> {
    if reuse == Some(false) {
        return Ok(None);
    }
    if reuse == Some(true) {
        let done: Option<String> = conn
            .prepare_cached(
                "SELECT j.id FROM jobs j
                 WHERE j.fingerprint = ?1 AND j.reused_from IS NULL AND j.status = 'done'
                   AND EXISTS (SELECT 1 FROM job_outputs o WHERE o.job_id = j.id)
                   AND NOT EXISTS (SELECT 1 FROM job_outputs o LEFT JOIN assets a ON a.id = o.asset_id
                                   WHERE o.job_id = j.id AND (a.id IS NULL OR a.status = 'rejected'))
                 ORDER BY j.created_at DESC LIMIT 1",
            )?
            .query_row([fingerprint], |r| r.get(0))
            .optional()?;
        if let Some(job_id) = done {
            return Ok(Some(Match::Done { job_id }));
        }
    }
    let (since, batch) = if reuse == Some(true) { (0, None) } else { (now() - IN_FLIGHT_SECS, batch) };
    let running: Option<String> = conn
        .prepare_cached(
            "SELECT id FROM jobs
             WHERE fingerprint = ?1 AND reused_from IS NULL AND status = 'running' AND created_at >= ?2
               AND (?3 IS NULL OR batch_id IS NOT ?3)
             ORDER BY created_at DESC LIMIT 1",
        )?
        .query_row(params![fingerprint, since, batch], |r| r.get(0))
        .optional()?;
    Ok(running.map(|job_id| Match::Running { job_id }))
}

/// Finishes `job_id` with the outputs and inputs of the done job `original`, at cost 0 USD.
/// Returns the output asset ids, or None (nothing changed) when none of them is left in the library.
pub fn adopt(tx: &Transaction, job_id: &str, original: &str) -> Result<Option<Vec<String>>> {
    let outputs: Vec<String> = tx
        .prepare_cached("SELECT o.asset_id FROM job_outputs o JOIN assets a ON a.id = o.asset_id WHERE o.job_id = ?1 ORDER BY o.rowid")?
        .query_map([original], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if outputs.is_empty() {
        return Ok(None);
    }
    let mut link = tx.prepare_cached("INSERT OR IGNORE INTO job_outputs (job_id, asset_id) VALUES (?1, ?2)")?;
    for asset in &outputs {
        link.execute([job_id, asset])?;
    }
    tx.execute(
        "INSERT OR IGNORE INTO job_inputs (job_id, asset_id, ref_id)
         SELECT ?1, asset_id, ref_id FROM job_inputs WHERE job_id = ?2 ORDER BY rowid",
        [job_id, original],
    )?;
    tx.execute(
        "UPDATE jobs SET status = 'done', cost = 0, cost_unit = 'USD', estimate_usd = NULL, waiting_on = NULL,
                         progress = NULL, error = NULL, error_kind = NULL, updated_at = ?2
         WHERE id = ?1",
        params![job_id, now()],
    )?;
    Ok(Some(outputs))
}
