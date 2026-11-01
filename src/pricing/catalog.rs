use std::{
    collections::HashMap,
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const REPO: &str = "https://raw.githubusercontent.com/jutsuhq/ai-media-api-prices/main";
/// Video offers, USD per second; required.
pub const VIDEO_FILE: &str = "prices.json";
/// Image offers, USD per image; optional, so a missing file only hides image offers.
pub const IMAGE_FILE: &str = "image-prices.json";
const MAX_AGE: Duration = Duration::from_secs(24 * 3600);

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PriceRow {
    pub family: String,
    pub model: String,
    pub variant: Option<String>,
    pub provider: String,
    #[serde(default)]
    pub official: bool,
    /// Video: `generate`, `video input` or `motion / avatar`. Image: `generate`, `edit` or `generate + edit`.
    pub mode: String,
    pub res: Option<String>,
    /// Image quality tier the price applies to (`low`, `medium`, `high`, `turbo`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd_per_sec: Option<f64>,
    #[serde(default, rename = "usdPer10s", skip_serializing_if = "Option::is_none")]
    pub usd_per_10s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd_per_image: Option<f64>,
    /// Image rows: about one megapixel, or one price for any size (the rows to compare first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<bool>,
    /// The price as the provider states it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listed: Option<String>,
    /// Surcharges and conditions, e.g. extra megapixels or reference images.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip_note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promo: Option<bool>,
    pub source: String,
    #[serde(default, skip_serializing)]
    pub gone: bool,
    /// Tells apart offers whose other id parts are equal (two pages or tiers of one model).
    #[serde(skip)]
    #[schemars(skip)]
    dup: Option<String>,
}

impl PriceRow {
    /// Stable across price changes: provider, model, variant, mode, resolution, audio (`image` for images) and quality.
    pub fn offer_id(&self) -> String {
        let slug = |v: &str| {
            let s: String = v.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '-' }).collect();
            s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-")
        };
        let audio = match (self.audio, self.kind()) {
            (Some(true), _) => "audio",
            (Some(false), _) => "silent",
            (None, "image") => "image",
            (None, _) => "",
        };
        let parts = [&self.provider, &self.model, self.variant.as_deref().unwrap_or(""), &self.mode, self.res.as_deref().unwrap_or(""), audio];
        let mut id = parts.map(slug).join(":");
        for extra in [&self.quality, &self.dup].into_iter().flatten() {
            id = format!("{id}:{}", slug(extra));
        }
        id
    }

    /// `image` for per-image offers, else `video`.
    pub fn kind(&self) -> &'static str {
        if self.usd_per_sec.is_none() && self.usd_per_image.is_some() { "image" } else { "video" }
    }

    fn price(&self) -> f64 {
        self.usd_per_sec.or(self.usd_per_image).unwrap_or(f64::INFINITY)
    }
}

#[derive(Deserialize)]
struct PricesFile {
    rows: Vec<PriceRow>,
}

#[derive(Default)]
pub struct Filter<'a> {
    pub query: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub res: Option<&'a str>,
    pub mode: Option<&'a str>,
    pub quality: Option<&'a str>,
    /// `video` or `image`.
    pub kind: Option<&'a str>,
}

/// Live video and image offers from the cached catalog files in `home`, each refreshed when older than a day.
pub async fn load(http: &reqwest::Client, home: &Path) -> Result<Vec<PriceRow>> {
    let mut rows = load_file(http, home, VIDEO_FILE).await?;
    match load_file(http, home, IMAGE_FILE).await {
        Ok(images) => rows.extend(images),
        Err(e) => tracing::warn!("image prices unavailable: {e:#}"),
    }
    disambiguate(&mut rows);
    Ok(rows)
}

/// Equal ids get a hash of source and notes, then, for price tiers of one page, their rank by price.
fn disambiguate(rows: &mut [PriceRow]) {
    let collisions = |rows: &[PriceRow]| {
        let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, row) in rows.iter().enumerate() {
            groups.entry(row.offer_id()).or_default().push(i);
        }
        groups.into_values().filter(|g| g.len() > 1).collect::<Vec<_>>()
    };
    for i in collisions(rows).into_iter().flatten() {
        let digest = Sha256::digest(format!("{}|{}", rows[i].source, rows[i].notes.as_deref().unwrap_or("")));
        rows[i].dup = Some(hex::encode(&digest[..3]));
    }
    for mut group in collisions(rows) {
        group.sort_by(|a, b| rows[*a].price().total_cmp(&rows[*b].price()));
        for (rank, i) in group.into_iter().enumerate() {
            let hash = rows[i].dup.take().unwrap_or_default();
            rows[i].dup = Some(format!("{hash}-{}", rank + 1));
        }
    }
}

/// A failed refresh falls back to the stale copy.
async fn load_file(http: &reqwest::Client, home: &Path, file: &str) -> Result<Vec<PriceRow>> {
    let cache = home.join(file);
    let fresh = tokio::fs::metadata(&cache)
        .await
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().unwrap_or(MAX_AGE) < MAX_AGE);

    if !fresh {
        match fetch(http, file).await {
            Ok(body) => tokio::fs::write(&cache, &body).await?,
            Err(e) if tokio::fs::try_exists(&cache).await? => {
                tracing::warn!("catalog refresh of {file} failed, using cached copy: {e:#}");
            }
            Err(e) => return Err(e.context(format!("{file}: download failed and no cache exists"))),
        }
    }

    let body = tokio::fs::read(&cache).await?;
    let parsed: PricesFile = serde_json::from_slice(&body).with_context(|| format!("unreadable {file}"))?;
    Ok(parsed.rows.into_iter().filter(|r| !r.gone).collect())
}

async fn fetch(http: &reqwest::Client, file: &str) -> Result<Vec<u8>> {
    let resp = http.get(format!("{REPO}/{file}")).send().await?.error_for_status()?;
    Ok(resp.bytes().await?.to_vec())
}

/// Case-insensitive substring match on every given field; videos then images, each cheapest first.
pub fn search(rows: &[PriceRow], f: &Filter) -> Vec<PriceRow> {
    let [query, provider, res, mode, quality] = [f.query, f.provider, f.res, f.mode, f.quality].map(|n| n.map(str::to_lowercase));
    let has = |hay: &str, needle: &Option<String>| needle.as_ref().is_none_or(|n| hay.to_lowercase().contains(n));
    let mut hits: Vec<PriceRow> = rows
        .iter()
        .filter(|r| {
            f.kind.is_none_or(|k| k.eq_ignore_ascii_case(r.kind()))
                && has(&r.provider, &provider)
                && has(r.res.as_deref().unwrap_or(""), &res)
                && has(&r.mode, &mode)
                && has(r.quality.as_deref().unwrap_or(""), &quality)
                && has(&format!("{} {} {}", r.family, r.model, r.variant.as_deref().unwrap_or("")), &query)
        })
        .cloned()
        .collect();
    hits.sort_by(|a, b| (a.kind() == "image").cmp(&(b.kind() == "image")).then(a.price().total_cmp(&b.price())));
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(v: serde_json::Value) -> PriceRow {
        let mut base = serde_json::json!({ "family": "F", "model": "M", "variant": null, "provider": "P", "mode": "generate",
                                           "res": null, "source": "https://p/m" });
        base.as_object_mut().unwrap().extend(v.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    #[test]
    fn image_offers_get_unique_ids_and_sort_after_videos() {
        let mut rows = vec![
            row(serde_json::json!({ "usdPerSec": 0.1 })),
            row(serde_json::json!({ "usdPerImage": 0.03, "quality": "high" })),
            row(serde_json::json!({ "usdPerImage": 0.02, "notes": "tier: 1 input image" })),
            row(serde_json::json!({ "usdPerImage": 0.01, "notes": "tier: 1 input image" })),
        ];
        disambiguate(&mut rows);
        let ids: Vec<String> = rows.iter().map(PriceRow::offer_id).collect();
        assert_eq!(ids[0], "p:m::generate::");
        assert_eq!(ids[1], "p:m::generate::image:high");
        assert!(ids[2].ends_with("-2") && ids[3].ends_with("-1") && ids[2][..ids[2].len() - 2] == ids[3][..ids[3].len() - 2], "{ids:?}");
        let kinds: Vec<_> = search(&rows, &Filter::default()).iter().map(|r| (r.kind(), r.price())).collect();
        assert_eq!(kinds, [("video", 0.1), ("image", 0.01), ("image", 0.02), ("image", 0.03)]);
        let high = search(&rows, &Filter { kind: Some("image"), quality: Some("HIGH"), ..Default::default() });
        assert_eq!(high.len(), 1);
    }
}
