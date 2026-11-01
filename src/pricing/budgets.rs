//! Opt-in spend caps. A job started with `budget` needs an estimate (given or from costs.rs) and is refused
//! when it would push the budget over its limit; failed jobs cost nothing.

use anyhow::{Result, bail};
use rusqlite::{Connection, params};
use schemars::JsonSchema;
use serde::Serialize;

use crate::{
    app::{App, NotFound},
    db::now,
};

#[derive(Serialize, JsonSchema)]
pub struct Budget {
    pub name: String,
    pub limit_usd: f64,
    /// Reported USD cost of finished jobs, plus the estimate of running jobs and of jobs whose
    /// provider reports no USD cost.
    pub spent_usd: f64,
    pub remaining_usd: f64,
    pub jobs: i64,
    pub created_at: i64,
}

const SELECT: &str = "SELECT b.name, b.limit_usd, b.created_at,
    COALESCE(SUM(CASE WHEN j.status = 'failed' THEN 0
                      WHEN j.cost IS NOT NULL AND j.cost_unit = 'USD' THEN j.cost
                      ELSE COALESCE(j.estimate_usd, 0) END), 0),
    COUNT(j.id)
    FROM budgets b LEFT JOIN jobs j ON j.budget = b.name";

fn query(conn: &Connection, name: Option<&str>) -> Result<Vec<Budget>> {
    let sql = format!("{SELECT} WHERE ?1 IS NULL OR b.name = ?1 GROUP BY b.name ORDER BY b.created_at DESC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([name], |r| {
        let limit_usd: f64 = r.get(1)?;
        let spent_usd: f64 = r.get(3)?;
        Ok(Budget {
            name: r.get(0)?,
            limit_usd,
            created_at: r.get(2)?,
            spent_usd,
            remaining_usd: limit_usd - spent_usd,
            jobs: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn list(app: &App) -> Result<Vec<Budget>> {
    query(&app.db.conn(), None)
}

fn find(conn: &Connection, name: &str) -> Result<Budget> {
    query(conn, Some(name))?
        .pop()
        .ok_or_else(|| NotFound(format!("no budget `{name}`; set_budget creates one")).into())
}

/// Creates the budget or changes its limit; spending so far stays.
pub fn set(app: &App, name: &str, limit_usd: f64) -> Result<Budget> {
    if !(limit_usd.is_finite() && limit_usd >= 0.0) {
        bail!("limit_usd must be a non-negative number");
    }
    let conn = app.db.conn();
    conn.execute(
        "INSERT INTO budgets (name, limit_usd, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (name) DO UPDATE SET limit_usd = excluded.limit_usd",
        params![name, limit_usd, now()],
    )?;
    find(&conn, name)
}

/// Jobs keep their history; they just stop counting against a budget.
pub fn delete(app: &App, name: &str) -> Result<Budget> {
    let conn = app.db.conn();
    let budget = find(&conn, name)?;
    conn.execute("DELETE FROM budgets WHERE name = ?1", [name])?;
    Ok(budget)
}

/// Canonical budget name, or why this job may not start.
// note: checked before submit and recorded after it, so two processes submitting at the same
// moment can overshoot by one job; a reservation row would close that.
pub fn admit(app: &App, name: &str, estimate: Option<f64>) -> Result<String> {
    let budget = find(&app.db.conn(), name)?;
    let Some(estimate) = estimate.filter(|e| e.is_finite() && *e >= 0.0) else {
        bail!(
            "budget `{}` needs estimated_cost_usd on generate: take the per-second or per-image price from search_prices \
             and multiply by the duration or image count you request",
            budget.name
        );
    };
    if budget.spent_usd + estimate > budget.limit_usd + 1e-9 {
        bail!(
            "budget `{}` would be exceeded: spent ${:.4} of ${:.4}, this job is estimated at ${estimate:.4}. \
             Pick a cheaper offer (search_prices) or raise the limit with set_budget if the user agrees.",
            budget.name,
            budget.spent_usd,
            budget.limit_usd
        );
    }
    Ok(budget.name)
}
