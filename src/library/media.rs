//! ffmpeg / ffprobe: file metadata and local edits (frames, reframing, trims, concat, overlays, audio).
//! Every edit is recorded as a finished job of provider `local`, so lineage, history and save_to apply.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Output,
};

use anyhow::{Context, Result, bail, ensure};
use rusqlite::params;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;

use crate::{
    app::App,
    jobs::{self, SaveTo},
    library::{assets::{self, Asset}, previews},
};

/// Frame rate of concat output; sources with other rates are converted.
const CONCAT_FPS: u32 = 30;
const VIDEO_OUT: &[&str] = &["-pix_fmt", "yuv420p"];
/// One still image out of a filter graph.
const IMAGE_OUT: &[&str] = &["-frames:v", "1", "-update", "1"];

#[derive(Clone, Copy)]
pub enum Tool {
    Ffmpeg,
    Ffprobe,
}

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Tool::Ffmpeg => "ffmpeg",
            Tool::Ffprobe => "ffprobe",
        }
    }

    /// `FFMPEG` / `FFPROBE` from config; ffprobe defaults to the file next to a configured ffmpeg.
    fn program(self, app: &App) -> PathBuf {
        let env = app.env();
        match self {
            Tool::Ffmpeg => PathBuf::from(env.get("FFMPEG").unwrap_or("ffmpeg")),
            Tool::Ffprobe => match (env.get("FFPROBE"), env.get("FFMPEG")) {
                (Some(p), _) => PathBuf::from(p),
                (None, Some(ff)) => Path::new(ff).with_file_name(format!("ffprobe{}", std::env::consts::EXE_SUFFIX)),
                (None, None) => PathBuf::from("ffprobe"),
            },
        }
    }
}

/// Runs ffmpeg or ffprobe; None when the program is not installed.
pub async fn run(app: &App, tool: Tool, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Result<Option<Output>> {
    let result = Command::new(tool.program(app)).args(args).kill_on_drop(true).output().await;
    match result {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        r => Ok(Some(r.with_context(|| format!("cannot run {}", tool.name()))?)),
    }
}

pub fn missing(app: &App, tool: Tool) -> anyhow::Error {
    anyhow::anyhow!(
        "{0} is not installed: install ffmpeg (https://ffmpeg.org/download.html, it includes ffprobe) and put it on PATH, \
         or set FFMPEG and FFPROBE to the full paths in {1} [env] (picked up without restart)",
        tool.name(),
        app.home.join("config.toml").display()
    )
}

/// What ffprobe reports about a file.
pub struct Probe {
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration_secs: Option<f64>,
    pub has_audio: bool,
}

/// None when ffprobe is not installed.
pub async fn probe(app: &App, path: &Path) -> Result<Option<Probe>> {
    let args = ["-v", "error", "-print_format", "json", "-show_streams", "-show_format"].map(OsStr::new);
    let Some(out) = run(app, Tool::Ffprobe, args.iter().copied().chain([path.as_os_str()])).await? else { return Ok(None) };
    ensure!(out.status.success(), "ffprobe cannot read {}: {}", path.display(), String::from_utf8_lossy(&out.stderr).trim());
    let info: Value = serde_json::from_slice(&out.stdout).context("ffprobe printed invalid JSON")?;
    let streams = info["streams"].as_array().map(Vec::as_slice).unwrap_or_default();
    let video = streams.iter().find(|s| s["codec_type"] == "video");
    let seconds = |v: &Value| v.as_str().and_then(|s| s.parse::<f64>().ok()).filter(|d| *d > 0.0);
    Ok(Some(Probe {
        width: video.and_then(|v| v["width"].as_i64()),
        height: video.and_then(|v| v["height"].as_i64()),
        duration_secs: seconds(&info["format"]["duration"]).or_else(|| video.and_then(|v| seconds(&v["duration"]))),
        has_audio: streams.iter().any(|s| s["codec_type"] == "audio"),
    }))
}

fn is_media(asset: &Asset) -> bool {
    asset.mime.as_deref().is_some_and(|m| ["image/", "video/", "audio/"].iter().any(|k| m.starts_with(k)))
}

fn kind(asset: &Asset) -> &str {
    asset.mime.as_deref().and_then(|m| m.split('/').next()).unwrap_or("unknown")
}

/// Adds the preview and, once per file, the ffprobe metadata (also backfills assets stored before
/// ffmpeg was installed). Best effort: a missing ffmpeg never fails storing or reading an asset.
pub async fn enrich(app: &App, mut asset: Asset) -> Asset {
    let needs_probe = asset.has_audio.is_none() && is_media(&asset);
    // boxed: every store and read awaits this, and two inline process futures overflow the debug main stack
    let (preview, probed) = tokio::join!(Box::pin(previews::ensure(app, &asset)), Box::pin(async {
        if needs_probe { probe(app, Path::new(&asset.path)).await } else { Ok(None) }
    }));
    previews::set(&mut asset, preview);
    match probed {
        Ok(Some(p)) => {
            // still images report no duration worth keeping (an image stream of one frame)
            let duration = p.duration_secs.filter(|_| kind(&asset) != "image");
            let saved = app.db.conn().execute(
                "UPDATE assets SET width = ?2, height = ?3, duration_secs = ?4, has_audio = ?5 WHERE id = ?1",
                params![asset.id, p.width, p.height, duration, p.has_audio],
            );
            if let Err(e) = saved {
                tracing::warn!(asset = %asset.id, "cannot store metadata: {e:#}");
            }
            (asset.width, asset.height, asset.duration_secs, asset.has_audio) = (p.width, p.height, duration, Some(p.has_audio));
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(asset = %asset.id, "{e:#}"),
    }
    asset
}

/// An ffmpeg edit a recipe step can run.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    ExtractFrame,
    Reframe,
    Trim,
    Concat,
    Overlay,
    MuxAudio,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::ExtractFrame => "extract_frame",
            Op::Reframe => "reframe",
            Op::Trim => "trim",
            Op::Concat => "concat",
            Op::Overlay => "overlay",
            Op::MuxAudio => "mux_audio",
        }
    }
}

/// How an edit is recorded: a new local job with these parameters as its input, or `job`, a waiting
/// recipe step that this edit finishes.
pub struct Local {
    pub params: Value,
    pub job: Option<String>,
}

impl Local {
    pub fn new(params: &impl Serialize, job: Option<&str>) -> Result<Self> {
        Ok(Self { params: serde_json::to_value(params)?, job: job.map(str::to_owned) })
    }
}

impl From<Value> for Local {
    fn from(params: Value) -> Self {
        Self { params, job: None }
    }
}

#[derive(Serialize, JsonSchema)]
pub struct Edited {
    /// The local job that recorded this operation (provider `local`, model = operation, cost 0).
    pub job_id: String,
    pub asset: Asset,
    pub hint: String,
}

/// Where in a video to take the frame.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum FrameAt {
    /// Seconds from the start, e.g. 1.5.
    Seconds(f64),
    Edge(Edge),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    First,
    Last,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReframeMode {
    /// Cut the sides (or top and bottom) away, centered.
    #[default]
    Crop,
    /// Add black bars.
    Pad,
}

/// A source asset with its metadata; `kinds` are the accepted MIME types (`video`, `image`, `audio`).
async fn source(app: &App, op: &str, id: &str, kinds: &[&str]) -> Result<Asset> {
    let asset = enrich(app, assets::get(app, id)?).await;
    if !kinds.contains(&kind(&asset)) {
        bail!(
            "{op} needs {} input; asset `{id}` is {}",
            kinds.join(" or "),
            asset.mime.as_deref().unwrap_or("of unknown type")
        );
    }
    if asset.has_audio.is_none() {
        return Err(missing(app, Tool::Ffprobe));
    }
    Ok(asset)
}

fn output_ext(asset: &Asset) -> &str {
    match kind(asset) {
        "video" => "mp4",
        _ => Path::new(&asset.path).extension().and_then(OsStr::to_str).unwrap_or("png"),
    }
}

fn duration(asset: &Asset) -> Result<f64> {
    asset.duration_secs.with_context(|| format!("ffprobe reports no duration for asset `{}`", asset.id))
}

fn even_down(v: f64) -> i64 {
    ((v / 2.0).floor() as i64 * 2).max(2)
}

fn even_up(v: f64) -> i64 {
    (v / 2.0).ceil() as i64 * 2
}

/// Runs ffmpeg with `args` + the output file, then stores the file and records the local job.
async fn produce(
    app: &App,
    op: &str,
    local: Local,
    sources: &[&Asset],
    ext: &str,
    args: Vec<String>,
    save_to: Option<SaveTo>,
) -> Result<Edited> {
    let save_to = jobs::resolve_save_to(app, save_to)?;
    let tmp = app.assets_dir.join(format!(".{}.{ext}", uuid::Uuid::new_v4().simple()));
    let full = ["-v", "error", "-nostdin", "-y"].iter().map(OsStr::new).chain(args.iter().map(OsStr::new));
    let Some(out) = run(app, Tool::Ffmpeg, full.chain([tmp.as_os_str()])).await? else { return Err(missing(app, Tool::Ffmpeg)) };
    if !out.status.success() || !tmp.exists() {
        let _ = tokio::fs::remove_file(&tmp).await;
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<_> = stderr.trim().lines().rev().take(5).collect();
        bail!("ffmpeg failed in {op}: {}", tail.into_iter().rev().collect::<Vec<_>>().join("\n"));
    }
    let ids: Vec<&str> = sources.iter().map(|a| a.id.as_str()).collect();
    let job_id = match &local.job {
        Some(id) => id.clone(),
        None => jobs::record_local(app, op, &local.params, &ids, save_to.as_ref())?,
    };
    let prompt = sources.iter().find_map(|a| a.prompt.as_deref());
    let asset = match assets::store_local(app, &tmp, ext, &job_id, prompt).await {
        Ok(a) => a,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
    };
    if let Some(s) = &save_to {
        jobs::file_output(app, s, &asset.id)?;
    }
    if local.job.is_some() {
        jobs::finish_local(app, &job_id, &ids)?;
    }
    let hint = format!(
        "New asset {0}: use it as \"{{{{asset:{0}}}}}\" in generate or in another media operation; \
         get_lineage {{asset_id: \"{0}\"}} links it to its source, get_job {{job_id: \"{job_id}\"}} shows the parameters.",
        asset.id
    );
    Ok(Edited { job_id, asset, hint })
}

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

pub async fn extract_frame(app: &App, local: Local, id: &str, at: FrameAt, save_to: Option<SaveTo>) -> Result<Edited> {
    let video = source(app, "extract_frame", id, &["video"]).await?;
    let input = video.path.as_str();
    let a = match at {
        FrameAt::Edge(Edge::First) => args(&["-i", input, "-frames:v", "1"]),
        // -update rewrites the one output image with every decoded frame, so the last one remains
        FrameAt::Edge(Edge::Last) => args(&["-sseof", "-3", "-i", input, "-update", "1"]),
        FrameAt::Seconds(t) => {
            let length = duration(&video)?;
            ensure!((0.0..length).contains(&t), "`at` {t} is outside the video (0 to {length:.3} s); use \"last\" for its end");
            args(&["-ss", &t.to_string(), "-i", input, "-frames:v", "1"])
        }
    };
    produce(app, "extract_frame", local, &[&video], "png", a, save_to).await
}

/// `aspect` is "W:H" (e.g. "9:16") or a ratio (e.g. "1.91").
fn parse_aspect(aspect: &str) -> Result<f64> {
    let ratio = match aspect.split_once([':', '/', 'x']) {
        Some((w, h)) => w.trim().parse::<f64>().ok().zip(h.trim().parse::<f64>().ok()).map(|(w, h)| w / h),
        None => aspect.trim().parse().ok(),
    };
    ratio
        .filter(|r| r.is_finite() && *r > 0.0)
        .with_context(|| format!("aspect `{aspect}` is not a ratio; write it like \"9:16\", \"1:1\", \"16:9\" or \"1.91\""))
}

pub async fn reframe(app: &App, local: Local, id: &str, aspect: &str, mode: ReframeMode, save_to: Option<SaveTo>) -> Result<Edited> {
    let target = parse_aspect(aspect)?;
    let asset = source(app, "reframe", id, &["video", "image"]).await?;
    let (Some(w), Some(h)) = (asset.width, asset.height) else { bail!("ffprobe reports no size for asset `{id}`") };
    let (w, h) = (w as f64, h as f64);
    let wider = w / h > target;
    let filter = match mode {
        ReframeMode::Crop if wider => format!("crop={}:{}", even_down(h * target), even_down(h)),
        ReframeMode::Crop => format!("crop={}:{}", even_down(w), even_down(w / target)),
        ReframeMode::Pad if wider => format!("pad={}:{}:(ow-iw)/2:(oh-ih)/2:color=black", even_up(w), even_up(w / target)),
        ReframeMode::Pad => format!("pad={}:{}:(ow-iw)/2:(oh-ih)/2:color=black", even_up(h * target), even_up(h)),
    };
    let mut a = args(&["-i", &asset.path, "-vf", &filter]);
    a.extend(args(if kind(&asset) == "video" { VIDEO_OUT } else { IMAGE_OUT }));
    produce(app, "reframe", local, &[&asset], output_ext(&asset), a, save_to).await
}

pub async fn trim(app: &App, local: Local, id: &str, start: f64, end: Option<f64>, save_to: Option<SaveTo>) -> Result<Edited> {
    let asset = source(app, "trim", id, &["video", "audio"]).await?;
    let length = duration(&asset)?;
    let end = end.unwrap_or(length).min(length);
    ensure!(start >= 0.0 && start < end, "trim needs 0 <= start < end within the {length:.3} s of asset `{id}` (got start {start}, end {end})");
    let mut a = args(&["-ss", &start.to_string(), "-i", &asset.path, "-t", &(end - start).to_string()]);
    if kind(&asset) == "video" {
        a.extend(args(VIDEO_OUT));
    }
    produce(app, "trim", local, &[&asset], output_ext(&asset), a, save_to).await
}

pub async fn concat(app: &App, local: Local, ids: &[String], save_to: Option<SaveTo>) -> Result<Edited> {
    ensure!(ids.len() >= 2, "concat needs at least two video asset_ids, in playback order");
    let looked_up = futures_util::future::join_all(ids.iter().map(|id| source(app, "concat", id, &["video"]))).await;
    let videos = looked_up.into_iter().collect::<Result<Vec<_>>>()?;
    let (Some(w), Some(h)) = (videos[0].width, videos[0].height) else { bail!("ffprobe reports no size for asset `{}`", ids[0]) };
    let (w, h) = (even_down(w as f64), even_down(h as f64));
    let audio = videos.iter().any(|v| v.has_audio == Some(true));
    let mut a = Vec::new();
    let mut graph = String::new();
    let mut pads = String::new();
    for (i, v) in videos.iter().enumerate() {
        a.extend(args(&["-i", &v.path]));
        graph += &format!(
            "[{i}:v]scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,\
             fps={CONCAT_FPS},format=yuv420p[v{i}];"
        );
        pads += &format!("[v{i}]");
        if audio {
            // silence for clips without sound, and every track padded or cut to its clip's length
            let d = duration(v)?;
            let track = if v.has_audio == Some(true) { format!("[{i}:a]") } else { "anullsrc=r=48000:cl=stereo,".into() };
            graph += &format!("{track}aformat=sample_rates=48000:channel_layouts=stereo,apad,atrim=duration={d}[a{i}];");
            pads += &format!("[a{i}]");
        }
    }
    graph += &format!("{pads}concat=n={}:v=1:a={}[v]{}", videos.len(), u8::from(audio), if audio { "[a]" } else { "" });
    a.extend(args(&["-filter_complex", &graph, "-map", "[v]"]));
    if audio {
        a.extend(args(&["-map", "[a]"]));
    }
    let refs: Vec<&Asset> = videos.iter().collect();
    produce(app, "concat", local, &refs, "mp4", a, save_to).await
}

pub struct Placement {
    pub x: i64,
    pub y: i64,
    pub width: Option<i64>,
}

pub async fn overlay(app: &App, local: Local, base_id: &str, overlay_id: &str, at: Placement, save_to: Option<SaveTo>) -> Result<Edited> {
    let (base, top) = tokio::join!(source(app, "overlay", base_id, &["video", "image"]), source(app, "overlay", overlay_id, &["image"]));
    let (base, top) = (base?, top?);
    let (Some(bw), Some(bh)) = (base.width, base.height) else { bail!("ffprobe reports no size for asset `{base_id}`") };
    ensure!(
        (0..bw).contains(&at.x) && (0..bh).contains(&at.y),
        "x and y are pixels of the base, which is {bw}x{bh}: 0 <= x < {bw}, 0 <= y < {bh}"
    );
    ensure!(at.width.is_none_or(|w| w > 0), "width must be positive (pixels; height follows the overlay's aspect)");
    let scale = at.width.map_or_else(|| "null".to_owned(), |w| format!("scale={w}:-1"));
    let video = kind(&base) == "video";
    let fmt = if video { ",format=yuv420p" } else { "" };
    let graph = format!("[1:v]{scale}[o];[0:v][o]overlay={}:{}{fmt}[v]", at.x, at.y);
    let mut a = args(&["-i", &base.path, "-i", &top.path, "-filter_complex", &graph, "-map", "[v]"]);
    a.extend(args(if video { &["-map", "0:a?"] } else { IMAGE_OUT }));
    produce(app, "overlay", local, &[&base, &top], output_ext(&base), a, save_to).await
}

pub async fn mux_audio(app: &App, local: Local, video_id: &str, audio_id: &str, save_to: Option<SaveTo>) -> Result<Edited> {
    let (video, audio) = tokio::join!(source(app, "mux_audio", video_id, &["video"]), source(app, "mux_audio", audio_id, &["audio", "video"]));
    let (video, audio) = (video?, audio?);
    ensure!(audio.has_audio == Some(true), "asset `{audio_id}` has no audio track");
    // the soundtrack is padded with silence or cut, so the video keeps its full length
    let a = args(&["-i", &video.path, "-i", &audio.path, "-map", "0:v:0", "-map", "1:a:0", "-c:v", "copy", "-af", "apad", "-shortest"]);
    produce(app, "mux_audio", local, &[&video, &audio], "mp4", a, save_to).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aspect_ratios_parse() {
        assert_eq!(parse_aspect("16:9").unwrap(), 16.0 / 9.0);
        assert_eq!(parse_aspect("1.91").unwrap(), 1.91);
        assert_eq!(parse_aspect("9/16").unwrap(), 9.0 / 16.0);
        assert!(format!("{:#}", parse_aspect("tall").unwrap_err()).contains("\"9:16\""));
        assert!(parse_aspect("0:1").is_err());
    }
}
