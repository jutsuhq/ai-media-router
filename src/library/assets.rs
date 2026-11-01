use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use reqwest::header::CONTENT_TYPE;
use rusqlite::{Connection, OptionalExtension, Params, Row, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{
    app::{App, NotFound},
    db::now,
    library::{media, previews, provenance::{self, Imported}},
    providers::{Output, Provider, exchange::Trace, spec::UploadKind},
};

pub const COLUMNS: &str = "a.id, a.file, a.mime, a.bytes, a.job_id, a.prompt, a.tags, a.created_at, a.description, a.status, a.rating, a.review_notes, a.width, a.height, a.duration_secs, a.has_audio";
/// Reuse a cached upload only if it stays valid this long, so the provider can still fetch it.
const UPLOAD_MARGIN_SECS: i64 = 3600;
/// Upload lifetime when the spec does not declare `ttl_secs`.
const UPLOAD_DEFAULT_TTL_SECS: i64 = 24 * 3600;

const MIME: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("webp", "image/webp"),
    ("gif", "image/gif"),
    ("mp4", "video/mp4"),
    ("webm", "video/webm"),
    ("mov", "video/quicktime"),
    ("mp3", "audio/mpeg"),
    ("wav", "audio/wav"),
];

#[derive(Serialize, JsonSchema)]
pub struct Asset {
    /// Content hash prefix; identical files share one id.
    pub id: String,
    /// Absolute path of the stored file.
    pub path: String,
    pub mime: Option<String>,
    pub bytes: i64,
    /// First job that produced this file; get_lineage shows every job around it.
    pub job_id: Option<String>,
    pub prompt: Option<String>,
    pub tags: String,
    /// Free text set with update_asset; searched together with prompt and tags.
    pub description: String,
    pub created_at: i64,
    /// Review gate: `draft` (default), `approved` or `rejected`; set with review_asset.
    pub status: String,
    /// 1-5, set with review_asset.
    pub rating: Option<i64>,
    /// Reviewer's notes (what is wrong or right); searched like prompt, tags and description.
    pub review_notes: String,
    /// JPEG preview (scaled image or first video frame); null without ffmpeg or for audio.
    pub preview: Option<String>,
    /// Pixels, from ffprobe; null for audio, or until ffprobe has read the file.
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// Length of a video or audio file.
    pub duration_secs: Option<f64>,
    /// Whether the file has an audio track; null until ffprobe has read it (needs ffmpeg installed).
    pub has_audio: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReviewStatus {
    Draft,
    Approved,
    Rejected,
}

impl ReviewStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }
}

/// Narrows a search by review state; both fields are optional.
#[derive(Default)]
pub struct ReviewFilter {
    pub status: Option<ReviewStatus>,
    pub min_rating: Option<u8>,
}

struct Meta<'a> {
    ext: &'a str,
    mime: Option<String>,
    job_id: Option<&'a str>,
    source_url: Option<&'a str>,
    prompt: Option<&'a str>,
    tags: &'a str,
}

/// Saves a provider result into the library; provider URLs expire, the local copy does not.
pub async fn store_output(
    app: &App,
    provider: &Provider,
    output: Output,
    job_id: &str,
    prompt: Option<&str>,
) -> Result<Asset> {
    let tmp = temp_path(app);
    let (hashed, mime, ext, source_url) = match output {
        Output::Inline { data, mime } => {
            tokio::fs::write(&tmp, &data).await?;
            let ext = mime.as_deref().and_then(ext_for_mime);
            ((Sha256::digest(&data).to_vec(), data.len() as i64), mime, ext, None)
        }
        Output::Url { url, auth } => {
            let (hashed, mime) = match download(provider, &url, auth, &tmp).await {
                Ok(d) => d,
                Err(e) => {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Err(e);
                }
            };
            let url_path = url.split(['?', '#']).next().unwrap_or(&url);
            let ext = ext_of(url_path)
                .and_then(|e| MIME.iter().find(|(known, _)| known.eq_ignore_ascii_case(e)))
                .map(|(e, _)| *e)
                .or_else(|| mime.as_deref().and_then(ext_for_mime));
            (hashed, mime, ext, Some(url))
        }
    };
    let meta = Meta { ext: ext.unwrap_or("bin"), mime, job_id: Some(job_id), source_url: source_url.as_deref(), prompt, tags: "" };
    finalize(app, &tmp, hashed, meta).await
}

async fn download(provider: &Provider, url: &str, auth: bool, tmp: &Path) -> Result<((Vec<u8>, i64), Option<String>)> {
    let resp = provider.download(url, auth).await?;
    let mime = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(|v| v.trim().to_owned());

    let mut file = tokio::fs::File::create(tmp).await?;
    let mut hasher = Sha256::new();
    let mut bytes = 0i64;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        bytes += chunk.len() as i64;
    }
    file.flush().await?;
    Ok(((hasher.finalize().to_vec(), bytes), mime))
}

/// SHA-256 and size of a file, read in chunks.
async fn hash_file(path: &Path) -> Result<(Vec<u8>, i64)> {
    let mut file = tokio::fs::File::open(path).await.with_context(|| format!("cannot read {}", path.display()))?;
    let (mut hasher, mut bytes, mut buf) = (Sha256::new(), 0i64, vec![0u8; 1 << 16]);
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            return Ok((hasher.finalize().to_vec(), bytes));
        }
        hasher.update(&buf[..n]);
        bytes += n as i64;
    }
}

/// Copies a local file (e.g. a reference image) into the library; provenance written by
/// ai-media-router restores its prompt, job and lineage.
pub async fn import_file(app: &App, src: &Path, tags: &str, prompt: Option<&str>) -> Result<Imported> {
    let tmp = temp_path(app);
    if let Err(e) = tokio::fs::copy(src, &tmp).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e).with_context(|| format!("cannot read {}", src.display()));
    }
    let ext = src.to_str().and_then(ext_of).unwrap_or("bin");
    let found = provenance::read(app, &tmp, ext).await;
    let prompt = prompt.or_else(|| found.as_ref().and_then(provenance::prompt));
    let meta = Meta { ext, mime: mime_of(ext), job_id: None, source_url: None, prompt, tags };
    let asset = finalize(app, &tmp, hash_file(&tmp).await?, meta).await?;
    let Some(p) = found else { return Ok(Imported { asset, provenance: None, missing_parents: Vec::new(), hint: None }) };
    let missing_parents = provenance::link(app, &asset.id, &p)?;
    let asset = Asset { job_id: Some(p.job_id.clone()), ..get(app, &asset.id)? };
    Ok(Imported { hint: Some(provenance::hint(&p, &missing_parents)), asset, provenance: Some(p), missing_parents })
}

/// Moves a file made in the library folder (media operations) into the library as an output of `job_id`.
pub async fn store_local(app: &App, src: &Path, ext: &str, job_id: &str, prompt: Option<&str>) -> Result<Asset> {
    let meta = Meta { ext, mime: mime_of(ext), job_id: Some(job_id), source_url: None, prompt, tags: "" };
    finalize(app, src, hash_file(src).await?, meta).await
}

/// Moves `tmp` to its content-addressed name, records the asset (and the job output), then adds
/// preview and metadata. A job's output first gets its provenance written into the file.
async fn finalize(app: &App, tmp: &Path, hashed: (Vec<u8>, i64), m: Meta<'_>) -> Result<Asset> {
    let embedded = match m.job_id {
        Some(job_id) => provenance::embed(app, job_id, m.prompt, tmp, m.ext).await.unwrap_or_else(|e| {
            tracing::warn!(job = %job_id, "cannot write provenance into the file: {e:#}");
            false
        }),
        None => false,
    };
    let (digest, bytes) = if embedded { hash_file(tmp).await? } else { hashed };
    let id = hex::encode(&digest[..8]);
    let file = format!("{id}.{}", m.ext);
    let dest = app.assets_dir.join(&file);
    if tokio::fs::try_exists(&dest).await? {
        tokio::fs::remove_file(tmp).await?;
    } else {
        tokio::fs::rename(tmp, &dest).await?;
    }

    let asset = {
        let conn = app.db.conn();
        conn.execute(
            "INSERT OR IGNORE INTO assets (id, file, mime, bytes, job_id, source_url, prompt, tags, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![id, file, m.mime, bytes, m.job_id, m.source_url, m.prompt, m.tags, now()],
        )?;
        if let Some(job_id) = m.job_id {
            conn.execute("INSERT OR IGNORE INTO job_outputs (job_id, asset_id) VALUES (?1, ?2)", params![job_id, id])?;
        }
        get_with(&conn, &app.assets_dir, &id)?.context("asset vanished after insert")?
    };
    Ok(media::enrich(app, asset).await)
}

pub fn get(app: &App, id: &str) -> Result<Asset> {
    get_with(&app.db.conn(), &app.assets_dir, id)?.ok_or_else(|| not_found(id))
}

fn get_with(conn: &Connection, dir: &Path, id: &str) -> Result<Option<Asset>> {
    Ok(select(conn, dir, "FROM assets a WHERE a.id = ?1", [id])?.pop())
}

pub fn for_job(conn: &Connection, dir: &Path, job_id: &str) -> Result<Vec<Asset>> {
    select(conn, dir, "FROM job_outputs o JOIN assets a ON a.id = o.asset_id WHERE o.job_id = ?1 ORDER BY o.rowid", [job_id])
}

/// `SELECT {COLUMNS} {tail}`: assets in the order the query returns them.
fn select(conn: &Connection, dir: &Path, tail: &str, params: impl Params) -> Result<Vec<Asset>> {
    rows(conn, &format!("SELECT {COLUMNS} {tail}"), params, |r| from_row(dir, r))
}

/// Runs a (cached) query and collects every row.
pub fn rows<T>(conn: &Connection, sql: &str, params: impl Params, f: impl FnMut(&Row) -> rusqlite::Result<T>) -> Result<Vec<T>> {
    let mut stmt = conn.prepare_cached(sql)?;
    Ok(stmt.query_map(params, f)?.collect::<Result<_, _>>()?)
}

/// Fails with NotFound on the first id that is not in the library.
pub fn require(conn: &Connection, ids: &[String]) -> Result<()> {
    let mut stmt = conn.prepare_cached("SELECT 1 FROM assets WHERE id = ?1")?;
    for id in ids {
        if !stmt.exists([id])? {
            return Err(not_found(id));
        }
    }
    Ok(())
}

fn not_found(id: &str) -> anyhow::Error {
    NotFound(format!("no asset `{id}`")).into()
}

/// Full-text search over prompt, tags, description and review notes; an empty query lists the newest assets.
pub fn search(
    app: &App,
    query: Option<&str>,
    collection: Option<&str>,
    review: &ReviewFilter,
    limit: usize,
) -> Result<Vec<Asset>> {
    // Quote every token so agent input can never be parsed as FTS5 syntax.
    let fts = query
        .unwrap_or("")
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ");
    let filters = "(?3 IS NULL OR a.id IN (SELECT asset_id FROM collection_assets WHERE collection = ?3))
        AND (?4 IS NULL OR a.status = ?4) AND (?5 IS NULL OR a.rating >= ?5)";
    let tail = if fts.is_empty() {
        format!("FROM assets a WHERE ?2 = '' AND {filters} ORDER BY a.created_at DESC LIMIT ?1")
    } else {
        format!("FROM assets_fts f JOIN assets a ON a.rowid = f.rowid WHERE assets_fts MATCH ?2 AND {filters} ORDER BY f.rank LIMIT ?1")
    };
    let status = review.status.map(ReviewStatus::as_str);
    select(&app.db.conn(), &app.assets_dir, &tail, params![limit as i64, fts, collection, status, review.min_rating])
}

/// Uploads an asset to a provider; returns what the provider wants in `input` (URL, file id or data URI).
/// Hosted uploads are reused per (asset, provider) until shortly before they expire.
pub async fn upload(app: &App, id: &str, provider: &str, trace: &mut Trace) -> Result<Value> {
    let asset = get(app, id)?;
    let p = app.provider(provider)?;
    let hosted = p.spec.upload.as_ref().filter(|u| u.kind != UploadKind::DataUri);
    if hosted.is_some() {
        let cached: Option<String> = app
            .db
            .conn()
            .query_row(
                "SELECT reference FROM uploads WHERE asset_id = ?1 AND provider = ?2 AND expires_at > ?3",
                params![id, provider, now() + UPLOAD_MARGIN_SECS],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(value) = cached.and_then(|c| serde_json::from_str(&c).ok()) {
            return Ok(value);
        }
    }
    let path = PathBuf::from(&asset.path);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or(id).to_owned();
    let bytes = tokio::fs::read(&path).await.with_context(|| format!("cannot read {}", path.display()))?;
    let value = p.upload(bytes, &name, asset.mime.as_deref(), trace).await?;
    if let Some(u) = hosted {
        let ttl = u.ttl_secs.map_or(UPLOAD_DEFAULT_TTL_SECS, |t| t as i64);
        app.db.conn().execute(
            "INSERT OR REPLACE INTO uploads (asset_id, provider, reference, expires_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, provider, value.to_string(), now() + ttl],
        )?;
    }
    Ok(value)
}

/// Replaces tags and/or description; `None` keeps the current value.
pub fn update(app: &App, id: &str, tags: Option<&str>, description: Option<&str>) -> Result<Asset> {
    let changed = app.db.conn().execute(
        "UPDATE assets SET tags = COALESCE(?2, tags), description = COALESCE(?3, description) WHERE id = ?1",
        params![id, tags, description],
    )?;
    if changed == 0 {
        return Err(not_found(id));
    }
    get(app, id)
}

/// Sets the review state of several assets at once; `None` keeps the current value.
pub fn review(
    app: &App,
    ids: &[String],
    status: Option<ReviewStatus>,
    rating: Option<u8>,
    notes: Option<&str>,
) -> Result<Vec<Asset>> {
    let mut conn = app.db.conn();
    let tx = conn.transaction()?;
    for id in ids {
        let changed = tx.execute(
            "UPDATE assets SET status = COALESCE(?2, status), rating = COALESCE(?3, rating),
                               review_notes = COALESCE(?4, review_notes) WHERE id = ?1",
            params![id, status.map(ReviewStatus::as_str), rating, notes],
        )?;
        if changed == 0 {
            return Err(not_found(id));
        }
    }
    tx.commit()?;
    ids.iter().map(|id| get_with(&conn, &app.assets_dir, id)?.ok_or_else(|| not_found(id))).collect()
}

/// Removes the file and its row; reference, collection and upload-cache entries go with it,
/// lineage edges stay so jobs still show what they were made from.
pub fn delete(app: &App, id: &str) -> Result<Asset> {
    let asset = get(app, id)?;
    app.db.conn().execute("DELETE FROM assets WHERE id = ?1", [id])?;
    let _ = std::fs::remove_file(previews::path(&app.assets_dir, id));
    match std::fs::remove_file(&asset.path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).with_context(|| format!("cannot remove {}", asset.path)),
        _ => Ok(asset),
    }
}

pub fn from_row(dir: &Path, r: &Row) -> rusqlite::Result<Asset> {
    let file: String = r.get(1)?;
    let id: String = r.get(0)?;
    let preview = previews::path(dir, &id);
    Ok(Asset {
        preview: preview.exists().then(|| preview.to_string_lossy().into_owned()),
        id,
        path: dir.join(file).to_string_lossy().into_owned(),
        mime: r.get(2)?,
        bytes: r.get(3)?,
        job_id: r.get(4)?,
        prompt: r.get(5)?,
        tags: r.get(6)?,
        created_at: r.get(7)?,
        description: r.get(8)?,
        status: r.get("status")?,
        rating: r.get("rating")?,
        review_notes: r.get("review_notes")?,
        width: r.get("width")?,
        height: r.get("height")?,
        duration_secs: r.get("duration_secs")?,
        has_audio: r.get("has_audio")?,
    })
}

fn temp_path(app: &App) -> PathBuf {
    app.assets_dir.join(format!(".{}.part", uuid::Uuid::new_v4().simple()))
}

fn ext_of(path: &str) -> Option<&str> {
    let ext = path.rsplit_once('.')?.1;
    (!ext.is_empty() && ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric()))
        .then_some(ext)
}

fn ext_for_mime(mime: &str) -> Option<&'static str> {
    MIME.iter().find(|(_, m)| *m == mime).map(|(e, _)| *e)
}

fn mime_of(ext: &str) -> Option<String> {
    MIME.iter().find(|(e, _)| *e == ext).map(|(_, m)| (*m).to_owned())
}
