//! Turns a stretch of a video into an animated GIF or WebP.
//!
//! GIF goes through ffmpeg's palette pair: one pass builds a palette from the clip, the next maps
//! frames onto it and redraws only the rectangle that changed. WebP frames are written by ffmpeg
//! and assembled by `img2webp`, since Homebrew's ffmpeg is built without libwebp.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::{RenderError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
  Gif,
  Webp,
}

impl Format {
  pub fn extension(self) -> &'static str {
    match self {
      Format::Gif => "gif",
      Format::Webp => "webp",
    }
  }
}

#[derive(Debug, Clone)]
pub struct ExportSpec {
  pub src: PathBuf,
  /// Seconds into `src`.
  pub from: f64,
  pub to: f64,
  /// Playback speed on top of whatever the source already does.
  pub speed: f64,
  pub fps: u32,
  /// Output width in pixels. Height follows the source aspect.
  pub width: u32,
  /// GIF palette size, 2 to 256.
  pub colors: u32,
  /// WebP lossy quality, 0 to 100.
  pub quality: u32,
  pub format: Format,
  pub out: PathBuf,
}

/// Seconds the export plays for.
pub fn output_duration(spec: &ExportSpec) -> f64 {
  ((spec.to - spec.from) / spec.speed.max(0.01)).max(0.0)
}

fn ffmpeg() -> Result<PathBuf> {
  crate::ffmpeg::locate()
}

fn img2webp() -> Result<PathBuf> {
  let ffmpeg = ffmpeg()?;
  let tool = ffmpeg.with_file_name("img2webp");
  if tool.is_file() {
    Ok(tool)
  } else {
    Err(RenderError::Encode(
      "img2webp not found next to ffmpeg; install it with brew install webp".into(),
    ))
  }
}

fn filter(spec: &ExportSpec) -> String {
  format!(
    "setpts=PTS/{speed},fps={fps},scale={w}:-2:flags=lanczos",
    speed = spec.speed.max(0.01),
    fps = spec.fps,
    w = spec.width
  )
}

/// Runs ffmpeg over the trimmed range, reporting progress from 0 to 1.
fn run_ffmpeg(
  spec: &ExportSpec,
  graph: &str,
  tail: &[&str],
  out: &Path,
  progress: &mut dyn FnMut(f64),
) -> Result<()> {
  let total = output_duration(spec).max(0.001);
  let mut child = Command::new(ffmpeg()?)
    .args(["-v", "error", "-y", "-nostdin"])
    .args([
      "-ss",
      &spec.from.to_string(),
      "-to",
      &spec.to.to_string(),
      "-i",
    ])
    .arg(&spec.src)
    .args(["-an", "-filter_complex", graph])
    .args(tail)
    .args(["-progress", "pipe:1"])
    .arg(out)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()?;
  let stdout = child.stdout.take().expect("piped stdout");
  for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
    if let Some(us) = line.strip_prefix("out_time_us=")
      && let Ok(us) = us.trim().parse::<f64>()
    {
      progress((us / 1e6 / total).clamp(0.0, 1.0));
    }
  }
  let output = child.wait_with_output()?;
  if output.status.success() {
    Ok(())
  } else {
    Err(RenderError::Encode(format!(
      "ffmpeg: {}",
      String::from_utf8_lossy(&output.stderr).trim()
    )))
  }
}

/// Writes the export and returns its size in bytes.
pub fn export(spec: &ExportSpec, progress: &mut dyn FnMut(f64)) -> Result<u64> {
  if spec.to <= spec.from {
    return Err(RenderError::Encode("the range is empty".into()));
  }
  match spec.format {
    Format::Gif => {
      let graph = format!(
        "{},split[a][b];[a]palettegen=max_colors={}:stats_mode=diff[p];\
                 [b][p]paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle",
        filter(spec),
        spec.colors.clamp(2, 256)
      );
      run_ffmpeg(spec, &graph, &["-loop", "0"], &spec.out, progress)?;
    }
    Format::Webp => {
      let frames = spec.out.with_extension("frames");
      let _ = fs::remove_dir_all(&frames);
      fs::create_dir_all(&frames)?;
      let pattern = frames.join("f%05d.png");
      let mut half = |p: f64| progress(p * 0.8);
      let result = run_ffmpeg(spec, &filter(spec), &[], &pattern, &mut half)
        .and_then(|()| assemble_webp(spec, &frames));
      let _ = fs::remove_dir_all(&frames);
      result?;
      progress(1.0);
    }
  }
  Ok(fs::metadata(&spec.out)?.len())
}

fn assemble_webp(spec: &ExportSpec, frames: &Path) -> Result<()> {
  let mut files: Vec<PathBuf> = fs::read_dir(frames)?
    .filter_map(|e| e.ok().map(|e| e.path()))
    .filter(|p| p.extension().is_some_and(|x| x == "png"))
    .collect();
  files.sort();
  if files.is_empty() {
    return Err(RenderError::Encode("no frames in the range".into()));
  }
  let frame_ms = (1000.0 / spec.fps as f64).round() as u32;
  let output = Command::new(img2webp()?)
    .args(["-loop", "0", "-mixed", "-lossy", "-q"])
    .arg(spec.quality.min(100).to_string())
    .args(["-d", &frame_ms.to_string()])
    .args(&files)
    .arg("-o")
    .arg(&spec.out)
    .output()?;
  if output.status.success() {
    Ok(())
  } else {
    Err(RenderError::Encode(format!(
      "img2webp: {}",
      String::from_utf8_lossy(&output.stderr).trim()
    )))
  }
}

/// Seconds of source encoded to estimate size.
const SAMPLE: f64 = 2.0;

/// Estimates the export's size by encoding a short sample from the middle of the range and
/// scaling it up. Screen content varies, so this is a guide, not a promise.
pub fn estimate(spec: &ExportSpec, scratch: &Path) -> Result<u64> {
  let span = spec.to - spec.from;
  if span <= 0.0 {
    return Ok(0);
  }
  let sample = span.min(SAMPLE * spec.speed.max(1.0));
  let from = spec.from + (span - sample) / 2.0;
  fs::create_dir_all(scratch)?;
  let probe = ExportSpec {
    from,
    to: from + sample,
    out: scratch.join(format!("estimate.{}", spec.format.extension())),
    ..spec.clone()
  };
  let bytes = export(&probe, &mut |_| {})?;
  let _ = fs::remove_file(&probe.out);
  Ok((bytes as f64 * span / sample) as u64)
}

/// Length of a video in seconds.
pub fn duration(src: &Path) -> Result<f64> {
  let probe = ffmpeg()?.with_file_name("ffprobe");
  let out = Command::new(probe)
    .args([
      "-v",
      "error",
      "-show_entries",
      "format=duration",
      "-of",
      "csv=p=0",
    ])
    .arg(src)
    .output()?;
  String::from_utf8_lossy(&out.stdout)
    .trim()
    .parse()
    .map_err(|_| RenderError::Decode(format!("cannot read the length of {}", src.display())))
}
