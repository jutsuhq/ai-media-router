//! Similarity search: embeddings of each asset's prompt, tags and description through any
//! OpenAI-compatible `/embeddings` endpoint, ranked by cosine similarity.
// note: text only, brute force over all vectors (fine up to ~100k assets); visual similarity
// needs a multimodal embedding endpoint, a large library needs sqlite-vec.

use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, params};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    app::App,
    library::assets::{self, Asset},
};

const DEFAULT_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "text-embedding-3-small";
const BATCH: usize = 64;

#[derive(Serialize, JsonSchema)]
pub struct Similar {
    /// Cosine similarity, 1 = same meaning.
    pub score: f32,
    #[serde(flatten)]
    pub asset: Asset,
}

struct Config {
    url: String,
    model: String,
    key: Option<String>,
}

impl Config {
    fn load(app: &App) -> Result<Self> {
        let env = app.env();
        let url = env.get("EMBEDDINGS_URL");
        let key = env.get("EMBEDDINGS_API_KEY").map(str::to_owned);
        if url.is_none() && key.is_none() {
            bail!(
                "similarity search needs an embeddings endpoint: set EMBEDDINGS_API_KEY (OpenAI), or EMBEDDINGS_URL for any \
                 OpenAI-compatible API (e.g. http://localhost:11434/v1 for Ollama) plus EMBEDDINGS_MODEL, in {} [env]",
                app.home.join("config.toml").display()
            );
        }
        Ok(Self {
            url: url.unwrap_or(DEFAULT_URL).trim_end_matches('/').to_owned(),
            model: env.get("EMBEDDINGS_MODEL").unwrap_or(DEFAULT_MODEL).to_owned(),
            key,
        })
    }
}

/// Assets ranked by similarity to `query` text or to another asset, optionally within a collection.
pub async fn similar(
    app: &App,
    query: Option<&str>,
    asset_id: Option<&str>,
    collection: Option<&str>,
    limit: usize,
) -> Result<Vec<Similar>> {
    let config = Config::load(app)?;
    refresh(app, &config).await?;
    let mut target = match (query.filter(|q| !q.trim().is_empty()), asset_id) {
        (Some(q), None) => embed(app, &config, &[q]).await?.remove(0),
        (None, Some(id)) => {
            assets::get(app, id)?;
            let vector: Option<Vec<u8>> = app
                .db
                .conn()
                .query_row("SELECT vector FROM embeddings WHERE model = ?1 AND asset_id = ?2", params![config.model, id], |r| r.get(0))
                .optional()?;
            let vector = vector.with_context(|| {
                format!("asset `{id}` has no prompt, tags or description to compare; set them with update_asset")
            })?;
            floats(&vector).collect()
        }
        _ => bail!("pass exactly one of `query` or `asset_id`"),
    };
    let norm = target.iter().map(|x| x * x).sum::<f32>().sqrt();
    target.iter_mut().for_each(|x| *x /= norm);
    let top = rank(app, &config.model, &target, collection, asset_id, limit)?;
    top.into_iter().map(|(score, id)| Ok(Similar { score, asset: assets::get(app, &id)? })).collect()
}

/// The `limit` best cosine scores against a unit-length `target`, best first, ties in row order.
fn rank(app: &App, model: &str, target: &[f32], collection: Option<&str>, skip: Option<&str>, limit: usize) -> Result<Vec<(f32, String)>> {
    let conn = app.db.conn();
    let mut stmt = conn.prepare_cached(
        "SELECT asset_id, vector FROM embeddings WHERE model = ?1
         AND (?2 IS NULL OR asset_id IN (SELECT asset_id FROM collection_assets WHERE collection = ?2))",
    )?;
    let mut rows = stmt.query(params![model, collection])?;
    let mut top: Vec<(f32, String)> = Vec::new();
    while let Some(row) = rows.next()? {
        let id = row.get_ref(0)?.as_str()?;
        if Some(id) == skip {
            continue;
        }
        let (dot, sq) = floats(row.get_ref(1)?.as_blob()?).zip(target).fold((0.0f32, 0.0f32), |(d, n), (x, t)| (d + x * t, n + x * x));
        // a zero target (NaN after normalizing) or zero vector scores 0, like an undefined cosine
        let score = if sq == 0.0 || dot.is_nan() { 0.0 } else { dot / sq.sqrt() };
        let at = top.partition_point(|(s, _)| s.total_cmp(&score).is_ge());
        if at < limit {
            top.insert(at, (score, id.to_owned()));
            top.truncate(limit);
        }
    }
    Ok(top)
}

fn floats(blob: &[u8]) -> impl Iterator<Item = f32> + '_ {
    blob.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b))
}

/// Embeds assets that are new, edited, or embedded with another model.
async fn refresh(app: &App, config: &Config) -> Result<()> {
    let stale: Vec<(String, String, String)> = {
        let conn = app.db.conn();
        let mut stmt = conn.prepare(
            "SELECT a.id, COALESCE(a.prompt, ''), a.tags, a.description, e.model, e.hash
             FROM assets a LEFT JOIN embeddings e ON e.asset_id = a.id",
        )?;
        let rows = stmt.query_map([], |r| {
            let parts: [String; 3] = [r.get(1)?, r.get(2)?, r.get(3)?];
            Ok((r.get::<_, String>(0)?, parts, r.get::<_, Option<String>>(4)?, r.get::<_, Option<String>>(5)?))
        })?;
        let mut stale = Vec::new();
        for row in rows {
            let (id, parts, model, hash) = row?;
            let text = parts.iter().map(|p| p.trim()).filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n");
            let fresh = hex::encode(&Sha256::digest(&text)[..8]);
            if !text.is_empty() && (model.as_deref() != Some(&config.model) || hash.as_deref() != Some(&fresh)) {
                stale.push((id, text, fresh));
            }
        }
        stale
    };
    for chunk in stale.chunks(BATCH) {
        let texts: Vec<&str> = chunk.iter().map(|(_, t, _)| t.as_str()).collect();
        let vectors = embed(app, config, &texts).await?;
        let conn = app.db.conn();
        for ((id, _, hash), v) in chunk.iter().zip(vectors) {
            let blob: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
            conn.execute(
                "INSERT OR REPLACE INTO embeddings (asset_id, model, hash, vector) VALUES (?1, ?2, ?3, ?4)",
                params![id, config.model, hash, blob],
            )?;
        }
    }
    Ok(())
}

async fn embed(app: &App, config: &Config, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
    let url = format!("{}/embeddings", config.url);
    let mut req = app.http.post(&url).json(&json!({ "model": config.model, "input": texts }));
    if let Some(key) = &config.key {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await.with_context(|| format!("embeddings request to {url} failed"))?;
    let status = resp.status();
    let body: Value = resp.json().await.with_context(|| format!("embeddings response from {url} is not JSON"))?;
    if !status.is_success() {
        bail!("embeddings endpoint {url} returned {status}: {body}");
    }
    let mut data: Vec<(u64, Vec<f32>)> = body["data"]
        .as_array()
        .with_context(|| format!("embeddings response has no `data` array: {body}"))?
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let vector = d["embedding"].as_array().map(|v| v.iter().filter_map(|x| x.as_f64()).map(|x| x as f32).collect());
            (d["index"].as_u64().unwrap_or(i as u64), vector.unwrap_or_default())
        })
        .collect();
    data.sort_by_key(|(i, _)| *i);
    if data.len() != texts.len() || data.iter().any(|(_, v)| v.is_empty()) {
        bail!("embeddings endpoint returned {} vectors for {} texts", data.len(), texts.len());
    }
    Ok(data.into_iter().map(|(_, v)| v).collect())
}
