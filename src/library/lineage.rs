//! The generation graph: a job's inputs (job_inputs) lead to its outputs (job_outputs).

use anyhow::Result;
use rusqlite::params;
use schemars::JsonSchema;
use serde::Serialize;

use crate::{
    app::App,
    library::assets::{self, Asset},
};

/// Guards against cycles: content hashing can make an output identical to one of its ancestors.
const MAX_DEPTH: i64 = 20;

#[derive(Serialize, JsonSchema)]
pub struct Lineage {
    pub asset: Asset,
    /// References and collections that hold this asset.
    pub references: Vec<String>,
    pub collections: Vec<String>,
    /// What the asset was made from, nearest first.
    pub ancestors: Vec<Link>,
    /// What was made from it, nearest first.
    pub descendants: Vec<Link>,
    pub hint: String,
}

#[derive(Serialize, JsonSchema)]
pub struct Link {
    /// Steps away from the asset: 1 = direct input / direct output.
    pub depth: i64,
    /// The job that consumed the ancestor or produced the descendant.
    pub job_id: String,
    /// The reference this input came through, if any.
    pub reference: Option<String>,
    pub asset: Asset,
}

pub fn get(app: &App, asset_id: &str) -> Result<Lineage> {
    let asset = assets::get(app, asset_id)?;
    let conn = app.db.conn();
    let references = assets::rows(
        &conn,
        "SELECT r.name FROM ref_assets ra JOIN refs r ON r.id = ra.ref_id WHERE ra.asset_id = ?1",
        [asset_id],
        |r| r.get(0),
    )?;
    let collections = assets::rows(&conn, "SELECT collection FROM collection_assets WHERE asset_id = ?1", [asset_id], |r| r.get(0))?;
    // up: the inputs of the job that made each asset; down: the outputs of jobs that read it.
    let ancestors = walk(&conn, app, asset_id, "o", "i")?;
    let descendants = walk(&conn, app, asset_id, "i", "o")?;
    let hint = "Regenerate a result: get_job {job_id} shows its provider, model and input (placeholders kept), \
                pass them to generate again."
        .into();
    Ok(Lineage { asset, references, collections, ancestors, descendants, hint })
}

/// Follows job edges from the side `from` (`i` = job_inputs, `o` = job_outputs) to the side `to`.
fn walk(conn: &rusqlite::Connection, app: &App, asset_id: &str, from: &str, to: &str) -> Result<Vec<Link>> {
    let edge = "job_inputs i JOIN job_outputs o ON o.job_id = i.job_id";
    // SQLite takes the bare columns of a MIN() aggregate from the row holding the minimum.
    let sql = format!(
        "WITH RECURSIVE g (asset_id, job_id, ref_id, depth) AS (
           SELECT {to}.asset_id, i.job_id, i.ref_id, 1 FROM {edge} WHERE {from}.asset_id = ?1
           UNION
           SELECT {to}.asset_id, i.job_id, i.ref_id, g.depth + 1 FROM g, {edge} WHERE {from}.asset_id = g.asset_id AND g.depth < ?2)
         SELECT {}, g.job_id AS link_job, r.name AS link_ref, MIN(g.depth) AS link_depth FROM g JOIN assets a ON a.id = g.asset_id
         LEFT JOIN refs r ON r.id = g.ref_id WHERE g.asset_id != ?1 GROUP BY g.asset_id ORDER BY MIN(g.depth), a.created_at",
        assets::COLUMNS
    );
    assets::rows(conn, &sql, params![asset_id, MAX_DEPTH], |r| {
        Ok(Link { asset: assets::from_row(&app.assets_dir, r)?, job_id: r.get("link_job")?, reference: r.get("link_ref")?, depth: r.get("link_depth")? })
    })
}
