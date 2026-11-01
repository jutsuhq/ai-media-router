//! Preview images: a scaled JPEG of an image, or the first frame of a video, made with ffmpeg.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};

use crate::{
    app::App,
    library::{assets::Asset, media::{self, Tool}},
};

const WIDTH: u32 = 512;

pub fn path(assets_dir: &Path, id: &str) -> PathBuf {
    assets_dir.join("previews").join(format!("{id}.jpg"))
}

/// Makes the preview when missing. Returns None for audio and other files, and when ffmpeg is
/// not installed (`FFMPEG` in config.toml [env] points to it when it is not on PATH).
pub async fn ensure(app: &App, asset: &Asset) -> Result<Option<PathBuf>> {
    let out = path(&app.assets_dir, &asset.id);
    if out.exists() {
        return Ok(Some(out));
    }
    let visual = asset.mime.as_deref().is_some_and(|m| m.starts_with("image/") || m.starts_with("video/"));
    if !visual {
        return Ok(None);
    }
    tokio::fs::create_dir_all(out.parent().unwrap_or(&app.assets_dir)).await?;
    let tmp = out.with_extension("part.jpg");
    let scale = format!("scale='min({WIDTH},iw)':-2");
    let args = ["-v", "error", "-nostdin", "-y", "-i", &asset.path, "-frames:v", "1", "-vf", &scale, "-q:v", "4"].map(OsStr::new);
    let Some(output) = media::run(app, Tool::Ffmpeg, args.into_iter().chain([tmp.as_os_str()])).await? else { return Ok(None) };
    if !output.status.success() {
        let _ = tokio::fs::remove_file(&tmp).await;
        bail!("ffmpeg could not make a preview of {}: {}", asset.path, String::from_utf8_lossy(&output.stderr).trim());
    }
    tokio::fs::rename(&tmp, &out).await?;
    Ok(Some(out))
}

/// Best effort: a missing preview never fails storing or reading an asset.
pub async fn attach(app: &App, mut asset: Asset) -> Asset {
    let made = ensure(app, &asset).await;
    set(&mut asset, made);
    asset
}

pub fn set(asset: &mut Asset, made: Result<Option<PathBuf>>) {
    match made {
        Ok(p) => asset.preview = p.map(|p| p.to_string_lossy().into_owned()),
        Err(e) => tracing::warn!(asset = %asset.id, "{e:#}"),
    }
}
