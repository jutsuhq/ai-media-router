//! USD estimates for a generation: a catalog offer's price times the duration or image count read
//! from `input`, and the median USD cost of past jobs on the same provider and model.

use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::{
    app::{App, NotFound},
    jobs::reuse::Match,
    pricing::catalog::PriceRow,
};

/// Input fields read as seconds, first match wins (also inside nested objects).
const DURATION_FIELDS: &[&str] = &["duration", "duration_seconds", "duration_secs", "seconds", "video_length", "length"];
/// Input fields read as the number of outputs.
const COUNT_FIELDS: &[&str] =
    &["num_images", "n", "num_outputs", "image_count", "number_of_images", "sample_count", "batch_size", "num_videos", "count"];

#[derive(Serialize, JsonSchema, Default)]
pub struct Estimate {
    /// Expected cost in USD; null when none could be computed (`note` says why).
    pub estimate_usd: Option<f64>,
    /// How `estimate_usd` was computed.
    pub formula: Option<String>,
    /// Input fields the formula read, by path, with the values found.
    pub read: Map<String, Value>,
    /// The offer priced, when `offer_id` was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offer: Option<PriceRow>,
    /// Median USD cost of finished jobs on this provider and model that reported USD.
    pub history: Option<History>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The job whose outputs generate would return instead of submitting (`reuse`); the estimate is then 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reuse: Option<Match>,
    /// For a recipe with steps: each step's share of `estimate_usd`, in order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<StepEstimate>,
}

#[derive(Serialize, JsonSchema)]
pub struct StepEstimate {
    pub step: String,
    pub estimate_usd: Option<f64>,
    /// How it was computed, or why it could not be.
    pub how: String,
}

#[derive(Serialize, JsonSchema)]
pub struct History {
    pub median_usd: f64,
    pub jobs: usize,
}

/// The offer's price wins; the history median is the fallback.
pub async fn estimate(app: &App, provider: &str, model: &str, input: &Value, offer_id: Option<&str>) -> Result<Estimate> {
    let spec = app.find_provider(provider)?.spec.clone();
    let key = spec.key.clone();
    let history = history(app, &key, model)?;
    let mut est = Estimate { history, ..Default::default() };
    if let (Some(cost), None, Some("USD")) = (spec.fixed_cost, offer_id, spec.cost_unit.as_deref()) {
        est.estimate_usd = Some(cost);
        est.formula = Some(format!("fixed ${cost} per job (providers/{key}.toml fixed_cost)"));
        return Ok(est);
    }
    let mut notes = Vec::new();

    match offer_id {
        Some(id) => {
            let row = find_offer(app, id).await?;
            match app.registry().for_catalog(&row.provider).map(|p| p.spec.key.clone()) {
                Some(k) if k == key => {}
                Some(k) => bail!("offer `{id}` is served by provider `{k}`, not `{key}`; pass provider \"{k}\" or pick another offer"),
                None => bail!("no provider spec serves `{}` yet, so offer `{id}` cannot be generated", row.provider),
            }
            match price(&row, input, &mut est.read) {
                Ok((usd, formula)) => (est.estimate_usd, est.formula) = (Some(usd), Some(formula)),
                Err(why) => notes.push(why),
            }
            est.offer = Some(row);
        }
        None => notes.push("no offer_id given (search_prices lists offers with their offer_id)".into()),
    }

    if est.estimate_usd.is_none() {
        match &est.history {
            Some(h) => {
                est.estimate_usd = Some(h.median_usd);
                est.formula = Some(format!("median of {} past {key}/{model} jobs that reported USD = ${:.4}", h.jobs, h.median_usd));
            }
            None => notes.push(format!("no past {key}/{model} jobs reported a USD cost")),
        }
    }
    if !notes.is_empty() {
        est.note = Some(notes.join("; "));
    }
    Ok(est)
}

async fn find_offer(app: &App, id: &str) -> Result<PriceRow> {
    app.catalog().await?.iter().find(|r| r.offer_id() == id).cloned().ok_or_else(|| {
        NotFound(format!("no offer `{id}` in the price catalog; search_prices lists current offers and their offer_id")).into()
    })
}

/// USD and the formula, or why the price cannot be applied to this input.
fn price(row: &PriceRow, input: &Value, read: &mut Map<String, Value>) -> Result<(f64, String), String> {
    let count = find(input, COUNT_FIELDS, "");
    if let Some((path, raw, n)) = &count {
        read.insert(path.clone(), raw.clone());
        if *n < 1.0 {
            return Err(format!("`{path}` must be at least 1"));
        }
    }
    if let Some(per_image) = row.usd_per_image {
        let (n, from) = count.as_ref().map_or((1.0, "default".to_owned()), |(path, _, n)| (*n, format!("input.{path}")));
        return Ok((per_image * n, format!("${per_image}/image × {n} image(s) ({from}) = ${:.4}", per_image * n)));
    }
    let Some(per_sec) = row.usd_per_sec else {
        return Err("the offer has no per-second or per-image price".into());
    };
    let Some((path, raw, secs)) = find(input, DURATION_FIELDS, "") else {
        return Err(format!(
            "input has no duration field ({}); add the duration you request",
            DURATION_FIELDS.join(", ")
        ));
    };
    read.insert(path.clone(), raw);
    if secs <= 0.0 {
        return Err(format!("`{path}` must be a positive number of seconds"));
    }
    let mut usd = per_sec * secs;
    let mut formula = format!("${per_sec}/s × {secs} s (input.{path})");
    if let Some((count_path, _, n)) = count {
        usd *= n;
        formula.push_str(&format!(" × {n} (input.{count_path})"));
    }
    Ok((usd, format!("{formula} = ${usd:.4}")))
}

/// First of `names` holding a number (or "5", "5s"), on this level before nested objects.
fn find(v: &Value, names: &[&str], prefix: &str) -> Option<(String, Value, f64)> {
    let obj = v.as_object()?;
    let hit = names.iter().find_map(|name| {
        let raw = obj.get(*name)?;
        let n = match raw {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().trim_end_matches(|c: char| c.is_ascii_alphabetic()).trim().parse().ok(),
            _ => None,
        }?;
        n.is_finite().then(|| (format!("{prefix}{name}"), raw.clone(), n))
    });
    hit.or_else(|| obj.iter().find_map(|(k, child)| find(child, names, &format!("{prefix}{k}."))))
}

fn history(app: &App, provider: &str, model: &str) -> Result<Option<History>> {
    let conn = app.db.conn();
    let mut stmt = conn.prepare(
        "SELECT cost FROM jobs WHERE provider = ?1 AND model = ?2 AND status = 'done'
         AND cost IS NOT NULL AND cost_unit = 'USD' AND reused_from IS NULL ORDER BY cost",
    )?;
    let costs: Vec<f64> = stmt.query_map([provider, model], |r| r.get(0))?.collect::<Result<_, _>>()?;
    let n = costs.len();
    Ok((n > 0).then(|| History {
        median_usd: if n % 2 == 1 { costs[n / 2] } else { (costs[n / 2 - 1] + costs[n / 2]) / 2.0 },
        jobs: n,
    }))
}
