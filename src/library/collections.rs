//! Collections (projects): named groups of assets. A collection exists while it holds assets.

use std::collections::HashMap;

use anyhow::{Result, bail};
use futures_util::{StreamExt, stream};
use rusqlite::{Connection, params};
use schemars::JsonSchema;
use serde::Serialize;

use crate::{
    app::{App, NotFound},
    db::now,
    jobs,
    library::{assets, previews},
};

/// ffmpeg processes making missing previews during an export.
const PREVIEW_JOBS: usize = 4;

#[derive(Serialize, JsonSchema)]
pub struct Collection {
    pub name: String,
    pub description: String,
    pub assets: i64,
    pub created_at: i64,
}

pub fn list(app: &App) -> Result<Vec<Collection>> {
    query(&app.db.conn(), None)
}

/// All collections, or the one named `name` (case-insensitive).
fn query(conn: &Connection, name: Option<&str>) -> Result<Vec<Collection>> {
    assets::rows(
        conn,
        "SELECT c.name, c.description, COUNT(ca.asset_id), c.created_at FROM collections c
         LEFT JOIN collection_assets ca ON ca.collection = c.name WHERE ?1 IS NULL OR c.name = ?1 COLLATE NOCASE
         GROUP BY c.name ORDER BY c.created_at DESC",
        [name],
        |r| Ok(Collection { name: r.get(0)?, description: r.get(1)?, assets: r.get(2)?, created_at: r.get(3)? }),
    )
}

/// Creates the collection when missing; `description` replaces the current one when given.
pub fn add(app: &App, name: &str, asset_ids: &[String], description: Option<&str>) -> Result<Collection> {
    let conn = app.db.conn();
    assets::require(&conn, asset_ids)?;
    ensure(&conn, name)?;
    if let Some(d) = description {
        conn.execute("UPDATE collections SET description = ?2 WHERE name = ?1", params![name, d])?;
    }
    for id in asset_ids {
        add_with(&conn, name, id)?;
    }
    find(&conn, name)
}

pub fn ensure(conn: &Connection, name: &str) -> Result<()> {
    conn.execute("INSERT OR IGNORE INTO collections (name, created_at) VALUES (?1, ?2)", params![name, now()])?;
    Ok(())
}

pub fn add_with(conn: &Connection, name: &str, asset_id: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO collection_assets (collection, asset_id, added_at) VALUES (?1, ?2, ?3)",
        params![name, asset_id, now()],
    )?;
    Ok(())
}

/// Removes assets from the collection (not from the library); an emptied collection is deleted.
pub fn remove(app: &App, name: &str, asset_ids: &[String]) -> Result<Collection> {
    let conn = app.db.conn();
    let before = find(&conn, name)?;
    for id in asset_ids {
        conn.execute("DELETE FROM collection_assets WHERE collection = ?1 AND asset_id = ?2", params![name, id])?;
    }
    conn.execute(
        "DELETE FROM collections WHERE name = ?1 AND NOT EXISTS (SELECT 1 FROM collection_assets WHERE collection = ?1)",
        [name],
    )?;
    find(&conn, name).or(Ok(Collection { assets: 0, ..before }))
}

fn find(conn: &Connection, name: &str) -> Result<Collection> {
    query(conn, Some(name))?
        .into_iter()
        .next()
        .ok_or_else(|| NotFound(format!("no collection `{name}`; list_collections shows existing ones")).into())
}

#[derive(Serialize, JsonSchema)]
pub struct Export {
    /// The ZIP file: `files/` (assets), `previews/`, and `collection.json` with metadata (review status,
    /// rating, notes), the job behind each asset (provider, model, input, cost) and its references.
    pub path: String,
    pub assets: usize,
    pub bytes: u64,
}

#[derive(Serialize)]
struct Manifest {
    collection: Collection,
    exported_at: i64,
    assets: Vec<ManifestAsset>,
}

#[derive(Serialize)]
struct ManifestAsset {
    /// Path inside the ZIP.
    file: String,
    preview: Option<String>,
    #[serde(flatten)]
    asset: assets::Asset,
    job: Option<jobs::Job>,
    references: Vec<String>,
}

/// Writes `<home>/exports/<collection>-<time>.zip`; media are stored uncompressed (already compressed).
pub async fn export(app: &App, name: &str, only_approved: bool) -> Result<Export> {
    let collection = find(&app.db.conn(), name)?;
    let review = assets::ReviewFilter { status: only_approved.then_some(assets::ReviewStatus::Approved), min_rating: None };
    let members = assets::search(app, None, Some(&collection.name), &review, usize::MAX >> 1)?;
    if only_approved && members.is_empty() {
        bail!("`{name}` has no approved assets; review_asset {{status: \"approved\"}} some, or export without only_approved");
    }
    let members: Vec<_> = stream::iter(members).map(|a| previews::attach(app, a)).buffered(PREVIEW_JOBS).collect().await;
    let mut held_by: HashMap<String, Vec<String>> = HashMap::new();
    let held = assets::rows(
        &app.db.conn(),
        "SELECT ra.asset_id, r.name FROM ref_assets ra JOIN refs r ON r.id = ra.ref_id
         JOIN collection_assets ca ON ca.asset_id = ra.asset_id WHERE ca.collection = ?1 ORDER BY r.name",
        [&collection.name],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    for (asset, name) in held {
        held_by.entry(asset).or_default().push(name);
    }
    let mut entries = Vec::with_capacity(members.len());
    for asset in members {
        let job = asset.job_id.as_deref().map(|id| jobs::get(app, id, false)).transpose()?;
        let references = held_by.remove(&asset.id).unwrap_or_default();
        let file_name = std::path::Path::new(&asset.path).file_name().and_then(|n| n.to_str()).unwrap_or(&asset.id);
        entries.push(ManifestAsset {
            file: format!("files/{file_name}"),
            preview: asset.preview.as_ref().map(|_| format!("previews/{}.jpg", asset.id)),
            asset,
            job,
            references,
        });
    }
    let safe: String = collection.name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    let dir = app.home.join("exports");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{safe}-{}.zip", now()));
    let manifest = Manifest { collection, exported_at: now(), assets: entries };
    let count = manifest.assets.len();
    let out = path.clone();
    tokio::task::spawn_blocking(move || write_zip(&out, &manifest)).await??;
    let bytes = std::fs::metadata(&path)?.len();
    Ok(Export { path: path.to_string_lossy().into_owned(), assets: count, bytes })
}

fn write_zip(path: &std::path::Path, manifest: &Manifest) -> Result<()> {
    use zip::{CompressionMethod, write::SimpleFileOptions};
    let tmp = path.with_extension("part");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&tmp)?);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored).large_file(true);
    for a in &manifest.assets {
        let mut sources = vec![(&a.file, &a.asset.path)];
        if let (Some(inner), Some(src)) = (&a.preview, &a.asset.preview) {
            sources.push((inner, src));
        }
        for (inner, src) in sources {
            zip.start_file(inner.as_str(), stored)?;
            std::io::copy(&mut std::fs::File::open(src)?, &mut zip)?;
        }
    }
    zip.start_file("collection.json", SimpleFileOptions::default())?;
    serde_json::to_writer_pretty(&mut zip, manifest)?;
    zip.finish()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
