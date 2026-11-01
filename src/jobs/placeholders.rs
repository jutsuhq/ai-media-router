//! `{{asset:<id>}}`, `{{ref:<name or id>}}` and `{{job:<id>}}` / `{{job:<id>:<n>}}` inside generation
//! input, replaced by what the provider's `[upload]` returns before the input is submitted.
//!
//! A placeholder must be a whole JSON string. As an array element a ref or job expands to all its
//! assets in order; anywhere else (an object field, the top of a string) it becomes its first asset.
//! `<n>` picks one output of a job, counting from 0; `last_frame` / `first_frame` take that frame of
//! the job's first video output (extract_frame, recorded as a local job). Assets rejected with
//! review_asset are skipped.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};

use crate::{
    app::App,
    library::{assets, media::{self, Edge, FrameAt}, references},
    providers::exchange::Trace,
};

/// An asset the job read, and the reference it came through.
#[derive(Debug, Clone, PartialEq)]
pub struct Input {
    pub asset_id: String,
    pub ref_id: Option<String>,
}

enum Placeholder<'a> {
    Asset(&'a str),
    Ref(&'a str),
    Job(&'a str, Option<&'a str>),
}

fn parse(s: &str) -> Result<Option<Placeholder<'_>>> {
    let inner = s.trim().strip_prefix("{{").and_then(|t| t.strip_suffix("}}"));
    let parsed = inner.and_then(|t| {
        t.strip_prefix("asset:")
            .map(|id| Placeholder::Asset(id.trim()))
            .or_else(|| t.strip_prefix("ref:").map(|r| Placeholder::Ref(r.trim())))
            .or_else(|| {
                let rest = t.strip_prefix("job:")?;
                Some(match rest.split_once(':') {
                    Some((id, n)) => Placeholder::Job(id.trim(), Some(n.trim())),
                    None => Placeholder::Job(rest.trim(), None),
                })
            })
    });
    if parsed.is_none() && (s.contains("{{asset:") || s.contains("{{ref:") || s.contains("{{job:")) {
        bail!("`{s}`: a placeholder must be the whole string value, e.g. \"image_url\": \"{{{{asset:<id>}}}}\"");
    }
    Ok(parsed)
}

/// Returns the input as the provider must receive it, plus the assets it used (for lineage).
pub async fn resolve(app: &App, provider: &str, input: &Value, trace: &mut Trace) -> Result<(Value, Vec<Input>)> {
    let Some((refs, inputs)) = locate(app, input).await? else { return Ok((input.clone(), Vec::new())) };
    let mut uploaded = HashMap::new();
    for i in &inputs {
        let value = assets::upload(app, &i.asset_id, provider, trace)
            .await
            .with_context(|| format!("resolving {{{{asset:{}}}}} for {provider}", i.asset_id))?;
        uploaded.insert(i.asset_id.clone(), value);
    }
    let lookup = |text: &str| -> Vec<Value> { refs[text].iter().filter_map(|id| uploaded.get(id).cloned()).collect() };
    Ok((substitute(input, &uploaded, &lookup)?, inputs))
}

/// The input with every placeholder replaced by asset ids, as the media operations take them.
pub async fn resolve_ids(app: &App, input: &Value) -> Result<Value> {
    let Some((refs, inputs)) = locate(app, input).await? else { return Ok(input.clone()) };
    let ids: HashMap<String, Value> = inputs.into_iter().map(|i| (i.asset_id.clone(), Value::String(i.asset_id))).collect();
    let lookup = |text: &str| -> Vec<Value> { refs[text].iter().map(|id| Value::String(id.clone())).collect() };
    substitute(input, &ids, &lookup)
}

/// Ref and job placeholders, by their text, to every asset they stand for, and every asset used
/// (taking video frames first); None without placeholders.
async fn locate(app: &App, input: &Value) -> Result<Option<(HashMap<String, Vec<String>>, Vec<Input>)>> {
    let mut found = Vec::new();
    collect(input, &mut found)?;
    if found.is_empty() {
        return Ok(None);
    }
    let mut refs: HashMap<String, Vec<String>> = HashMap::new();
    let mut inputs: Vec<Input> = Vec::new();
    let mut frames: Vec<(String, String, Edge)> = Vec::new();
    {
        let conn = app.db.conn();
        for (text, in_array) in &found {
            let (asset_ids, ref_id) = match parse(text)? {
                Some(Placeholder::Asset(id)) => (vec![id.to_owned()], None),
                Some(Placeholder::Ref(name)) => {
                    let (id, ids) = ref_assets(&conn, name, *in_array)?;
                    let used = ids.iter().take(if *in_array { usize::MAX } else { 1 }).cloned().collect();
                    refs.insert(text.clone(), ids);
                    (used, Some(id))
                }
                Some(Placeholder::Job(job_id, Some(edge @ ("last_frame" | "first_frame")))) => {
                    let edge = if edge == "last_frame" { Edge::Last } else { Edge::First };
                    frames.push((text.clone(), first_video(&conn, job_id)?, edge));
                    continue;
                }
                Some(Placeholder::Job(job_id, n)) => {
                    let ids = job_outputs(&conn, job_id, n)?;
                    let used = ids.iter().take(if *in_array { usize::MAX } else { 1 }).cloned().collect();
                    refs.insert(text.clone(), ids);
                    (used, None)
                }
                None => continue,
            };
            for asset_id in asset_ids {
                if !inputs.iter().any(|i| i.asset_id == asset_id) {
                    inputs.push(Input { asset_id, ref_id: ref_id.clone() });
                }
            }
        }
    }

    for (text, video, edge) in frames {
        let params = serde_json::json!({ "asset_id": video, "at": edge });
        let frame = media::extract_frame(app, params.into(), &video, FrameAt::Edge(edge), None)
            .await
            .with_context(|| format!("{text}: taking the frame"))?
            .asset
            .id;
        if !inputs.iter().any(|i| i.asset_id == frame) {
            inputs.push(Input { asset_id: frame.clone(), ref_id: None });
        }
        refs.insert(text, vec![frame]);
    }
    Ok(Some((refs, inputs)))
}

fn ref_assets(conn: &Connection, name: &str, in_array: bool) -> Result<(String, Vec<String>)> {
    let id = references::require_id(conn, name)?;
    let ids = references::usable_asset_ids(conn, &id)?;
    if ids.is_empty() && !in_array {
        bail!(
            "reference `{name}` has no usable assets (rejected ones are skipped); add_to_reference, or \
             review_asset {{status: \"approved\"}} one of its assets"
        );
    }
    Ok((id, ids))
}

/// What `resolve` would do, checked against the library without uploading anything.
pub struct Plan {
    /// The input with every placeholder shown as `<upload of asset …>`.
    pub input: Value,
    /// Assets (or frames of video outputs) that would be uploaded to the provider.
    pub uploads: Vec<String>,
    /// Jobs still running that the new job would wait for.
    pub waits_on: Vec<String>,
}

pub fn plan(app: &App, input: &Value) -> Result<Plan> {
    let mut found = Vec::new();
    collect(input, &mut found)?;
    let conn = app.db.conn();
    let mut refs: HashMap<String, Vec<String>> = HashMap::new();
    let mut shown: HashMap<String, Value> = HashMap::new();
    let (mut uploads, mut waits_on) = (Vec::new(), Vec::new());
    for (text, in_array) in &found {
        let ids = match parse(text)? {
            Some(Placeholder::Asset(id)) => {
                assets::require(&conn, &[id.to_owned()])?;
                vec![id.to_owned()]
            }
            Some(Placeholder::Ref(name)) => ref_assets(&conn, name, *in_array)?.1,
            Some(Placeholder::Job(job_id, n)) => {
                let status: Option<String> =
                    conn.query_row("SELECT status FROM jobs WHERE id = ?1", [job_id], |r| r.get(0)).optional()?;
                match (status.as_deref(), n) {
                    (Some("failed"), _) => bail!("job `{job_id}` (from {text}) failed; rerun it and use the new job id"),
                    (Some("running"), _) => {
                        if !waits_on.iter().any(|w| w == job_id) {
                            waits_on.push(job_id.to_owned());
                        }
                        shown.insert(text.clone(), json!(format!("<{text} once job {job_id} is done>")));
                        vec![text.clone()]
                    }
                    (_, Some(edge @ ("last_frame" | "first_frame"))) => {
                        let frame = format!("{edge} of asset {}", first_video(&conn, job_id)?);
                        shown.insert(frame.clone(), json!(format!("<upload of the {frame}>")));
                        uploads.push(frame.clone());
                        vec![frame]
                    }
                    _ => job_outputs(&conn, job_id, n)?,
                }
            }
            None => continue,
        };
        for id in ids.iter().take(if *in_array { usize::MAX } else { 1 }) {
            if !shown.contains_key(id) {
                shown.insert(id.clone(), json!(format!("<upload of asset {id}>")));
                uploads.push(id.clone());
            }
        }
        refs.insert(text.clone(), ids);
    }
    let lookup = |text: &str| -> Vec<Value> { refs[text].iter().filter_map(|id| shown.get(id).cloned()).collect() };
    Ok(Plan { input: substitute(input, &shown, &lookup)?, uploads, waits_on })
}

/// Every placeholder-looking string, and whether it sits directly in an array.
fn collect(v: &Value, out: &mut Vec<(String, bool)>) -> Result<()> {
    match v {
        Value::String(s) if parse(s)?.is_some() => out.push((s.clone(), false)),
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::String(s) if parse(s)?.is_some() => out.push((s.clone(), true)),
                    other => collect(other, out)?,
                }
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                collect(item, out)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn substitute(v: &Value, assets: &HashMap<String, Value>, refs: &dyn Fn(&str) -> Vec<Value>) -> Result<Value> {
    let one = |s: &str| -> Result<Option<Vec<Value>>> {
        Ok(match parse(s)? {
            Some(Placeholder::Asset(id)) => Some(vec![assets[id].clone()]),
            Some(Placeholder::Ref(_) | Placeholder::Job(..)) => Some(refs(s)),
            None => None,
        })
    };
    Ok(match v {
        Value::String(s) => match one(s)? {
            Some(values) => values.into_iter().next().ok_or_else(|| anyhow!("`{s}` resolved to nothing"))?,
            None => v.clone(),
        },
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str().map(one).transpose()?.flatten() {
                    Some(values) => out.extend(values),
                    None => out.push(substitute(item, assets, refs)?),
                }
            }
            Value::Array(out)
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, item)| Ok((k.clone(), substitute(item, assets, refs)?)))
                .collect::<Result<_>>()?,
        ),
        other => other.clone(),
    })
}

/// Jobs named by `{{job:…}}` placeholders, each once, in order of appearance.
pub fn job_ids(input: &Value) -> Result<Vec<String>> {
    let mut found = Vec::new();
    collect(input, &mut found)?;
    let mut ids: Vec<String> = Vec::new();
    for (text, _) in &found {
        if let Some(Placeholder::Job(id, _)) = parse(text)?
            && !ids.iter().any(|i| i == id)
        {
            ids.push(id.to_owned());
        }
    }
    Ok(ids)
}

/// Outputs of a finished job still in the library, or the one at index `n`.
fn job_outputs(conn: &Connection, job_id: &str, n: Option<&str>) -> Result<Vec<String>> {
    let status: Option<String> = conn.query_row("SELECT status FROM jobs WHERE id = ?1", [job_id], |r| r.get(0)).optional()?;
    match status.as_deref() {
        None => bail!("no job `{job_id}` (from {{{{job:{job_id}}}}}); list_jobs shows job ids"),
        Some("done") => {}
        Some(s) => bail!("job `{job_id}` is {s}, not done; its outputs exist only once get_job reports done"),
    }
    let mut stmt = conn.prepare(
        "SELECT o.asset_id FROM job_outputs o JOIN assets a ON a.id = o.asset_id WHERE o.job_id = ?1 ORDER BY o.rowid",
    )?;
    let ids: Vec<String> = stmt.query_map([job_id], |r| r.get(0))?.collect::<Result<_, _>>()?;
    if ids.is_empty() {
        bail!("job `{job_id}` has no outputs left in the library");
    }
    let Some(n) = n else { return Ok(ids) };
    if n.ends_with("_frame") {
        bail!("{{{{job:{job_id}:{n}}}}}: use last_frame or first_frame");
    }
    let picked = n.parse::<usize>().ok().and_then(|i| ids.get(i));
    match picked {
        Some(id) => Ok(vec![id.clone()]),
        None => bail!("{{{{job:{job_id}:{n}}}}}: the job has {} output(s), numbered from 0", ids.len()),
    }
}

/// The first video among a finished job's outputs, for `last_frame` / `first_frame`.
fn first_video(conn: &Connection, job_id: &str) -> Result<String> {
    let ids = job_outputs(conn, job_id, None)?;
    let mut stmt = conn.prepare("SELECT mime FROM assets WHERE id = ?1")?;
    for id in &ids {
        let mime: String = stmt.query_row([id], |r| r.get(0))?;
        if mime.starts_with("video/") {
            return Ok(id.clone());
        }
    }
    bail!("{{{{job:{job_id}:last_frame}}}}: the job has no video output; use {{{{job:{job_id}}}}} for its images")
}
