//! Provenance inside the file: provider, model, input, job and parent assets travel with every
//! generated file (PNG iTXt chunk, JPEG COM segment, MP4/MOV/WebM `comment` tag), so import_asset
//! can restore prompt and lineage in another library. `EMBED_METADATA = "false"` turns writing off.

use std::{ffi::OsStr, path::Path};

use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    app::App,
    db::now,
    library::{assets::{self, Asset}, media::{self, Tool}},
    templates::recipes,
};

/// PNG text keyword; also the start of `generator`.
pub const KEY: &str = "ai-media-router";
const GENERATOR: &str = concat!("ai-media-router/", env!("CARGO_PKG_VERSION"));
/// Serialized provenance starts with this, which tells our JPEG comments from others.
const MARK: &[u8] = b"{\"generator\":\"ai-media-router";
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
const VIDEO: &[&str] = &["mp4", "mov", "webm"];
/// JPEG segment length limit; Windows limits the ffmpeg command line to 32767 characters.
const JPEG_MAX: usize = 65533;
const ARG_MAX: usize = 30000;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Provenance {
    /// `ai-media-router/<version>` that wrote the file.
    pub generator: String,
    pub job_id: String,
    pub provider: String,
    pub model: String,
    /// Generation input with snippets expanded; media placeholders stay, URLs lose query string and
    /// credentials (or become `{{asset:<id>}}` when the library knows them), API keys are redacted.
    pub input: Value,
    /// The asset's prompt (for media operations, the source's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Assets the job read, in order.
    #[serde(default)]
    pub parents: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct Imported {
    #[serde(flatten)]
    pub asset: Asset,
    /// Provenance found in the file (written by ai-media-router); null for other files.
    pub provenance: Option<Provenance>,
    /// Parents named in the provenance that are not in this library; importing them links them up.
    pub missing_parents: Vec<String>,
    pub hint: Option<String>,
}

fn enabled(app: &App) -> bool {
    app.env().get("EMBED_METADATA").is_none_or(|v| !matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"))
}

/// Writes the provenance of `job_id` into the file at `path`; false when nothing was written
/// (turned off, unsupported format, malformed file, ffmpeg missing). The file is replaced atomically.
pub async fn embed(app: &App, job_id: &str, prompt: Option<&str>, path: &Path, ext: &str) -> Result<bool> {
    let ext = ext.to_ascii_lowercase();
    let image = matches!(ext.as_str(), "png" | "jpg" | "jpeg");
    if !enabled(app) || !(image || VIDEO.contains(&ext.as_str())) {
        return Ok(false);
    }
    let Some(p) = for_job(app, job_id, prompt)? else { return Ok(false) };
    let json = serde_json::to_string(&p)?;
    let out = path.with_extension(format!("meta.{ext}"));
    let written = if image {
        let data = tokio::fs::read(path).await?;
        let with = if ext == "png" { png_with(&data, &json) } else { jpeg_with(&data, &json) };
        match with {
            Some(bytes) => tokio::fs::write(&out, bytes).await.map(|()| true)?,
            None => false,
        }
    } else {
        json.len() <= ARG_MAX && {
            let comment = format!("comment={json}");
            let args = ["-v", "error", "-nostdin", "-y", "-i"].map(OsStr::new).into_iter().chain([path.as_os_str()]);
            let copy = ["-map", "0", "-c", "copy", "-fflags", "+bitexact", "-metadata", &comment].map(OsStr::new);
            let ran = media::run(app, Tool::Ffmpeg, args.chain(copy).chain([out.as_os_str()])).await?;
            ran.is_some_and(|o| o.status.success()) && out.exists()
        }
    };
    if !written {
        let _ = tokio::fs::remove_file(&out).await;
        return Ok(false);
    }
    tokio::fs::rename(&out, path).await?;
    Ok(true)
}

fn for_job(app: &App, job_id: &str, prompt: Option<&str>) -> Result<Option<Provenance>> {
    let (row, parents) = {
        let conn = app.db.conn();
        let row: Option<(String, String, String)> = conn
            .query_row("SELECT provider, model, input FROM jobs WHERE id = ?1", [job_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        let parents = assets::rows(&conn, "SELECT asset_id FROM job_inputs WHERE job_id = ?1 ORDER BY rowid", [job_id], |r| r.get(0))?;
        (row, parents)
    };
    let Some((provider, model, input)) = row else { return Ok(None) };
    let stored: Value = serde_json::from_str(&input)?;
    let input = recipes::expand_text(app, &stored).unwrap_or(stored);
    let scrubbed = scrub(app, &provider, json!([input, prompt]))?;
    let (input, prompt) = (scrubbed[0].clone(), scrubbed[1].as_str().map(str::to_owned));
    Ok(Some(Provenance { generator: GENERATOR.into(), job_id: job_id.into(), provider, model, input, prompt, parents }))
}

/// Removes what must not leave the library: API keys, signed URL parameters, inline data.
fn scrub(app: &App, provider: &str, input: Value) -> Result<Value> {
    let secrets = app.find_provider(provider).map(|p| p.secrets()).unwrap_or_default();
    let conn = app.db.conn();
    let mut known = conn.prepare_cached(
        "SELECT id FROM assets WHERE source_url = ?1 UNION ALL SELECT asset_id FROM uploads WHERE reference = ?2 LIMIT 1",
    )?;
    map_strings(input, &mut |mut s| {
        for secret in &secrets {
            s = s.replace(secret.as_str(), "<redacted>");
        }
        if s.starts_with("data:") {
            return Ok("<data URI omitted>".into());
        }
        if !(s.starts_with("http://") || s.starts_with("https://")) {
            return Ok(s);
        }
        let id: Option<String> = known.query_row(params![s, Value::String(s.clone()).to_string()], |r| r.get(0)).optional()?;
        Ok(id.map_or_else(|| strip_url(&s), |id| format!("{{{{asset:{id}}}}}")))
    })
}

fn map_strings(v: Value, f: &mut impl FnMut(String) -> Result<String>) -> Result<Value> {
    Ok(match v {
        Value::String(s) => Value::String(f(s)?),
        Value::Array(items) => Value::Array(items.into_iter().map(|i| map_strings(i, f)).collect::<Result<_>>()?),
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, i)| Ok((k, map_strings(i, f)?))).collect::<Result<_>>()?),
        other => other,
    })
}

/// `https://user:pass@host/path?sig=…#x` -> `https://host/path`.
fn strip_url(url: &str) -> String {
    let base = url.split(['?', '#']).next().unwrap_or(url);
    let Some((scheme, rest)) = base.split_once("://") else { return base.to_owned() };
    let host_end = rest.find('/').unwrap_or(rest.len());
    let host_start = rest[..host_end].rfind('@').map_or(0, |i| i + 1);
    format!("{scheme}://{}", &rest[host_start..])
}

/// Provenance written by ai-media-router, if the file carries one.
pub async fn read(app: &App, path: &Path, ext: &str) -> Option<Provenance> {
    let ext = ext.to_ascii_lowercase();
    let text = match ext.as_str() {
        "png" => png_text(&tokio::fs::read(path).await.ok()?)?,
        "jpg" | "jpeg" => jpeg_text(&tokio::fs::read(path).await.ok()?)?,
        e if VIDEO.contains(&e) => video_comment(app, path).await?,
        _ => return None,
    };
    serde_json::from_str::<Provenance>(&text).ok().filter(|p| p.generator.starts_with(KEY))
}

async fn video_comment(app: &App, path: &Path) -> Option<String> {
    let args = ["-v", "error", "-print_format", "json", "-show_entries", "format_tags"].map(OsStr::new);
    let out = media::run(app, Tool::Ffprobe, args.into_iter().chain([path.as_os_str()])).await.ok()??;
    let info: Value = serde_json::from_slice(&out.stdout).ok()?;
    let tags = info["format"]["tags"].as_object()?;
    tags.iter().find(|(k, _)| k.eq_ignore_ascii_case("comment")).and_then(|(_, v)| v.as_str()).map(str::to_owned)
}

/// Links an imported asset to the job its provenance names, creating that job (status done) when
/// this library does not have it. Returns the parents that are not in the library.
pub fn link(app: &App, asset_id: &str, p: &Provenance) -> Result<Vec<String>> {
    let mut conn = app.db.conn();
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT OR IGNORE INTO jobs (id, provider, model, input, remote_id, status, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, '', 'done', ?5, ?5)",
        params![p.job_id, p.provider, p.model, p.input.to_string(), now()],
    )?;
    let mut missing = Vec::new();
    for parent in &p.parents {
        tx.execute("INSERT OR IGNORE INTO job_inputs (job_id, asset_id) VALUES (?1, ?2)", params![p.job_id, parent])?;
        if !tx.prepare_cached("SELECT 1 FROM assets WHERE id = ?1")?.exists([parent])? {
            missing.push(parent.clone());
        }
    }
    tx.execute("INSERT OR IGNORE INTO job_outputs (job_id, asset_id) VALUES (?1, ?2)", params![p.job_id, asset_id])?;
    let tags: String = tx.query_row("SELECT tags FROM assets WHERE id = ?1", [asset_id], |r| r.get(0))?;
    let mut words: Vec<&str> = tags.split_whitespace().collect();
    for word in [p.provider.as_str(), p.model.as_str()] {
        if !words.contains(&word) {
            words.push(word);
        }
    }
    tx.execute(
        "UPDATE assets SET job_id = COALESCE(job_id, ?2), prompt = COALESCE(prompt, ?3), tags = ?4 WHERE id = ?1",
        params![asset_id, p.job_id, prompt(p), words.join(" ")],
    )?;
    tx.commit()?;
    Ok(missing)
}

pub fn prompt(p: &Provenance) -> Option<&str> {
    p.prompt.as_deref().or_else(|| p.input.get("prompt").and_then(Value::as_str))
}

pub fn hint(p: &Provenance, missing: &[String]) -> String {
    let mut hint = format!(
        "Restored from the file: job {0} ({1} / {2}). get_job {{job_id: \"{0}\"}} shows the input; \
         save_recipe {{name, from_job: \"{0}\"}} makes it reusable.",
        p.job_id, p.provider, p.model
    );
    if !missing.is_empty() {
        hint.push_str(&format!(" Parents not in this library: {}; import their files to link them.", missing.join(", ")));
    }
    hint
}

struct Chunk<'a> {
    kind: &'a [u8],
    body: &'a [u8],
    /// length, type, body and CRC, as in the file
    raw: &'a [u8],
}

/// Every chunk up to IEND; None unless the file is a well-formed PNG.
fn png_chunks(data: &[u8]) -> Option<Vec<Chunk<'_>>> {
    let mut rest = data.strip_prefix(PNG_SIGNATURE)?;
    let mut chunks = Vec::new();
    loop {
        let len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let end = len.checked_add(8)?;
        let raw = rest.get(..end.checked_add(4)?)?;
        let chunk = Chunk { kind: &raw[4..8], body: &raw[8..end], raw };
        rest = &rest[raw.len()..];
        let last = chunk.kind == b"IEND";
        chunks.push(chunk);
        if last {
            return Some(chunks);
        }
    }
}

/// Text of an uncompressed tEXt / iTXt chunk with our keyword.
fn png_ours<'a>(c: &Chunk<'a>) -> Option<&'a [u8]> {
    let (key, rest) = split_nul(c.body)?;
    if key != KEY.as_bytes() {
        return None;
    }
    match c.kind {
        b"tEXt" => Some(rest),
        b"iTXt" => {
            let [0, _, rest @ ..] = rest else { return None };
            let (_language, rest) = split_nul(rest)?;
            Some(split_nul(rest)?.1)
        }
        _ => None,
    }
}

fn split_nul(b: &[u8]) -> Option<(&[u8], &[u8])> {
    let i = b.iter().position(|&c| c == 0)?;
    Some((&b[..i], &b[i + 1..]))
}

/// The PNG with one iTXt chunk `ai-media-router` = `json` right after IHDR, replacing older ones.
pub fn png_with(data: &[u8], json: &str) -> Option<Vec<u8>> {
    let chunks = png_chunks(data)?;
    let (ihdr, rest) = chunks.split_first().filter(|(c, _)| c.kind == b"IHDR")?;
    // keyword, NUL, compression flag 0, method 0, empty language tag and translated keyword
    let body = [KEY.as_bytes(), &[0, 0, 0, 0, 0], json.as_bytes()].concat();
    let mut out = [PNG_SIGNATURE, ihdr.raw].concat();
    out.extend((body.len() as u32).to_be_bytes());
    let typed = [b"iTXt".as_slice(), &body].concat();
    out.extend(&typed);
    out.extend(crc32(&typed).to_be_bytes());
    rest.iter().filter(|c| png_ours(c).is_none()).for_each(|c| out.extend(c.raw));
    Some(out)
}

pub fn png_text(data: &[u8]) -> Option<String> {
    png_chunks(data)?.iter().find_map(png_ours).and_then(|t| String::from_utf8(t.to_vec()).ok())
}

/// CRC-32 (IEEE), as PNG chunks need it.
fn crc32(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0u32, |crc, &b| {
        (0..8).fold(crc ^ u32::from(b), |c, _| if c & 1 == 1 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 })
    })
}

struct Segment {
    marker: u8,
    start: usize,
    body: usize,
    end: usize,
}

/// Header segments between SOI and SOS; None unless they parse.
fn jpeg_segments(data: &[u8]) -> Option<Vec<Segment>> {
    if !data.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let (mut segments, mut i) = (Vec::new(), 2);
    loop {
        let start = i;
        if *data.get(i)? != 0xFF {
            return None;
        }
        while *data.get(i + 1)? == 0xFF {
            i += 1;
        }
        let marker = data[i + 1];
        if marker == 0xDA {
            return Some(segments);
        }
        let len = usize::from(u16::from_be_bytes([*data.get(i + 2)?, *data.get(i + 3)?]));
        if len < 2 || i + 2 + len > data.len() {
            return None;
        }
        segments.push(Segment { marker, start, body: i + 4, end: i + 2 + len });
        i += 2 + len;
    }
}

fn jpeg_ours<'a>(data: &'a [u8], s: &Segment) -> Option<&'a [u8]> {
    let body = &data[s.body..s.end];
    (s.marker == 0xFE && body.starts_with(MARK)).then_some(body)
}

/// The JPEG with a COM segment holding `json` after its APPn segments, replacing older ones.
pub fn jpeg_with(data: &[u8], json: &str) -> Option<Vec<u8>> {
    if json.len() > JPEG_MAX {
        return None;
    }
    let segments = jpeg_segments(data)?;
    let at = segments.iter().take_while(|s| (0xE0..=0xEF).contains(&s.marker)).last().map_or(2, |s| s.end);
    let mut out = data[..at].to_vec();
    out.extend([0xFF, 0xFE]);
    out.extend(((json.len() + 2) as u16).to_be_bytes());
    out.extend(json.as_bytes());
    let mut pos = at;
    for s in segments.iter().filter(|s| s.start >= at && jpeg_ours(data, s).is_some()) {
        out.extend(&data[pos..s.start]);
        pos = s.end;
    }
    out.extend(&data[pos..]);
    Some(out)
}

pub fn jpeg_text(data: &[u8]) -> Option<String> {
    let segments = jpeg_segments(data)?;
    segments.iter().find_map(|s| jpeg_ours(data, s)).and_then(|t| String::from_utf8(t.to_vec()).ok())
}
