//! References: named, typed sets of assets that keep a character, style or product consistent.

use std::{collections::HashMap, path::Path};

use anyhow::{Result, anyhow};
use rusqlite::{Connection, OptionalExtension, Row, ToSql, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    app::{App, NotFound},
    db::now,
    library::assets::{self, Asset},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Character,
    Style,
    Product,
    Environment,
    Composition,
    Brand,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Character => "character",
            Self::Style => "style",
            Self::Product => "product",
            Self::Environment => "environment",
            Self::Composition => "composition",
            Self::Brand => "brand",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        [Self::Character, Self::Style, Self::Product, Self::Environment, Self::Composition, Self::Brand]
            .into_iter()
            .find(|k| k.as_str() == s)
    }
}

#[derive(Serialize, JsonSchema)]
pub struct Reference {
    pub id: String,
    pub name: String,
    pub kind: Kind,
    pub description: String,
    pub tags: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// In order; `{{ref:<name>}}` in a string field uses the first one.
    pub assets: Vec<RefAsset>,
}

#[derive(Serialize, JsonSchema)]
pub struct RefAsset {
    /// e.g. "front", "side", "palette"; free text.
    pub role: Option<String>,
    #[serde(flatten)]
    pub asset: Asset,
}

pub struct Fields<'a> {
    pub name: &'a str,
    pub kind: Option<Kind>,
    pub description: Option<&'a str>,
    pub tags: Option<&'a str>,
}

/// Creates the reference or updates the given fields of an existing one (matched by name or id).
/// With `asset_ids`, the reference then holds exactly those assets in that order.
pub fn save(app: &App, f: Fields, asset_ids: Option<&[String]>) -> Result<Reference> {
    let mut conn = app.db.conn();
    assets::require(&conn, asset_ids.unwrap_or_default())?;
    let tx = conn.transaction()?;
    let id = match find_id(&tx, f.name)? {
        Some(id) => {
            tx.execute(
                "UPDATE refs SET kind = COALESCE(?2, kind), description = COALESCE(?3, description),
                 tags = COALESCE(?4, tags), updated_at = ?5 WHERE id = ?1",
                params![id, f.kind.map(Kind::as_str), f.description, f.tags, now()],
            )?;
            id
        }
        None => {
            let kind = f.kind.ok_or_else(|| {
                anyhow!("reference `{}` does not exist yet; pass `kind` (character, style, product, environment, composition, brand) to create it", f.name)
            })?;
            let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_owned();
            tx.execute(
                "INSERT INTO refs (id, name, kind, description, tags, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                params![id, f.name, kind.as_str(), f.description.unwrap_or(""), f.tags.unwrap_or(""), now()],
            )?;
            id
        }
    };
    if let Some(ids) = asset_ids {
        let roles: Vec<(String, Option<String>)> =
            assets::rows(&tx, "SELECT asset_id, role FROM ref_assets WHERE ref_id = ?1", [&id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        tx.execute("DELETE FROM ref_assets WHERE ref_id = ?1", [&id])?;
        for (position, asset) in ids.iter().enumerate() {
            let role = roles.iter().find(|(a, _)| a == asset).and_then(|(_, r)| r.clone());
            tx.execute(
                "INSERT OR IGNORE INTO ref_assets (ref_id, asset_id, role, position) VALUES (?1, ?2, ?3, ?4)",
                params![id, asset, role, position as i64],
            )?;
        }
    }
    tx.commit()?;
    load(&conn, &app.assets_dir, &id)
}

pub fn get(app: &App, name_or_id: &str) -> Result<Reference> {
    let conn = app.db.conn();
    let id = require_id(&conn, name_or_id)?;
    load(&conn, &app.assets_dir, &id)
}

/// Newest first; `query` matches name, description and tags as a substring.
pub fn list(app: &App, kind: Option<Kind>, query: Option<&str>, limit: usize) -> Result<Vec<Reference>> {
    let pattern = query.map(|q| format!("%{q}%"));
    query_refs(
        &app.db.conn(),
        &app.assets_dir,
        "WHERE (?1 IS NULL OR kind = ?1) AND (?2 IS NULL OR name LIKE ?2 OR description LIKE ?2 OR tags LIKE ?2)
         ORDER BY updated_at DESC LIMIT ?3",
        params![kind.map(Kind::as_str), pattern, limit as i64],
    )
}

/// Appends assets (already present ones only get the new role, if given).
pub fn add(app: &App, name_or_id: &str, asset_ids: &[String], role: Option<&str>) -> Result<Reference> {
    let conn = app.db.conn();
    assets::require(&conn, asset_ids)?;
    let id = require_id(&conn, name_or_id)?;
    for asset in asset_ids {
        add_with(&conn, &id, asset, role)?;
    }
    conn.execute("UPDATE refs SET updated_at = ?2 WHERE id = ?1", params![id, now()])?;
    load(&conn, &app.assets_dir, &id)
}

pub fn add_with(conn: &Connection, ref_id: &str, asset_id: &str, role: Option<&str>) -> Result<()> {
    conn.execute(
        "INSERT INTO ref_assets (ref_id, asset_id, role, position)
         SELECT ?1, ?2, ?3, COALESCE(MAX(position) + 1, 0) FROM ref_assets WHERE ref_id = ?1
         ON CONFLICT (ref_id, asset_id) DO UPDATE SET role = COALESCE(excluded.role, role)",
        params![ref_id, asset_id, role],
    )?;
    Ok(())
}

pub fn remove(app: &App, name_or_id: &str, asset_ids: &[String]) -> Result<Reference> {
    let conn = app.db.conn();
    let id = require_id(&conn, name_or_id)?;
    for asset in asset_ids {
        conn.execute("DELETE FROM ref_assets WHERE ref_id = ?1 AND asset_id = ?2", params![id, asset])?;
    }
    conn.execute("UPDATE refs SET updated_at = ?2 WHERE id = ?1", params![id, now()])?;
    load(&conn, &app.assets_dir, &id)
}

/// Deletes the reference only; its assets stay in the library.
pub fn delete(app: &App, name_or_id: &str) -> Result<Reference> {
    let conn = app.db.conn();
    let id = require_id(&conn, name_or_id)?;
    let reference = load(&conn, &app.assets_dir, &id)?;
    conn.execute("DELETE FROM refs WHERE id = ?1", [&id])?;
    Ok(reference)
}

pub fn find_id(conn: &Connection, name_or_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT id FROM refs WHERE id = ?1 OR name = ?1", [name_or_id], |r| r.get(0))
        .optional()?)
}

pub fn require_id(conn: &Connection, name_or_id: &str) -> Result<String> {
    find_id(conn, name_or_id)?.ok_or_else(|| {
        NotFound(format!("no reference `{name_or_id}`; list_references shows existing ones, save_reference creates one")).into()
    })
}

/// Asset ids a `{{ref:…}}` placeholder expands to: in order, without rejected assets.
pub fn usable_asset_ids(conn: &Connection, ref_id: &str) -> Result<Vec<String>> {
    assets::rows(
        conn,
        "SELECT ra.asset_id FROM ref_assets ra JOIN assets a ON a.id = ra.asset_id
         WHERE ra.ref_id = ?1 AND a.status != 'rejected' ORDER BY ra.position",
        [ref_id],
        |r| r.get(0),
    )
}

fn load(conn: &Connection, dir: &Path, id: &str) -> Result<Reference> {
    query_refs(conn, dir, "WHERE id = ?1", params![id])?.pop().ok_or_else(|| NotFound(format!("no reference `{id}`")).into())
}

/// References matching `filter` (a WHERE/ORDER/LIMIT tail on `refs`), with their assets in two queries.
fn query_refs(conn: &Connection, dir: &Path, filter: &str, params: &[&dyn ToSql]) -> Result<Vec<Reference>> {
    let sql = format!("SELECT id, name, kind, description, tags, created_at, updated_at FROM refs {filter}");
    let mut refs = assets::rows(conn, &sql, params, from_row)?;
    let sql = format!(
        "SELECT {}, ra.role AS ref_role, ra.ref_id AS ref_owner FROM ref_assets ra JOIN assets a ON a.id = ra.asset_id
         WHERE ra.ref_id IN (SELECT id FROM refs {filter}) ORDER BY ra.position",
        assets::COLUMNS
    );
    let held = assets::rows(conn, &sql, params, |r| {
        Ok((r.get::<_, String>("ref_owner")?, RefAsset { asset: assets::from_row(dir, r)?, role: r.get("ref_role")? }))
    })?;
    let mut by_ref: HashMap<String, Vec<RefAsset>> = HashMap::new();
    for (owner, asset) in held {
        by_ref.entry(owner).or_default().push(asset);
    }
    for r in &mut refs {
        r.assets = by_ref.remove(&r.id).unwrap_or_default();
    }
    Ok(refs)
}

fn from_row(r: &Row) -> rusqlite::Result<Reference> {
    let kind: String = r.get(2)?;
    Ok(Reference {
        id: r.get(0)?,
        name: r.get(1)?,
        kind: Kind::parse(&kind).ok_or_else(|| rusqlite::Error::InvalidColumnType(2, kind, rusqlite::types::Type::Text))?,
        description: r.get(3)?,
        tags: r.get(4)?,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
        assets: Vec::new(),
    })
}
