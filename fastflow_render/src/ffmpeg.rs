use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use fastflow_core::recording::SegmentInfo;

use crate::{Frame, FrameSink, FrameSource, RenderError, Result};

const SEARCH_DIRS: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];

pub fn locate() -> Result<PathBuf> {
  let path_dirs = std::env::var_os("PATH")
    .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
    .unwrap_or_default();
  path_dirs
    .into_iter()
    .chain(SEARCH_DIRS.iter().map(PathBuf::from))
    .map(|d| d.join("ffmpeg"))
    .find(|p| p.is_file())
    .ok_or_else(|| RenderError::Decode("ffmpeg not found".into()))
}

/// How decoded frames are sized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decode {
  /// The capture's own pixels, for the camera to crop.
  Native,
  /// The whole frame scaled and padded into this size.
  Fit(u32, u32),
}

/// Decodes one segment to constant-rate RGBA. The constant rate makes frame `n` sit at
/// `n / fps`, whatever the capture's own timing was.
pub struct FfmpegSource {
  bin: PathBuf,
  path: PathBuf,
  decode: Decode,
  fps: f64,
  segment: SegmentInfo,
  child: Child,
  stdout: ChildStdout,
  current: Frame,
  spare: Frame,
  /// Index of `current`. -1 before the first read.
  index: i64,
  eof: bool,
}

impl FfmpegSource {
  pub fn open(path: &Path, segment: SegmentInfo, decode: Decode, fps: f64) -> Result<Self> {
    let bin = locate()?;
    let size = match decode {
      Decode::Native => (segment.surface_px[0], segment.surface_px[1]),
      Decode::Fit(w, h) => (w, h),
    };
    let (child, stdout) = spawn_decoder(&bin, path, decode, fps, 0.0)?;
    Ok(FfmpegSource {
      bin,
      path: path.to_owned(),
      decode,
      fps,
      segment,
      child,
      stdout,
      current: Frame::black(size.0, size.1),
      spare: Frame::black(size.0, size.1),
      index: -1,
      eof: false,
    })
  }

  /// Returns false at the end of the stream.
  fn read_next(&mut self) -> Result<bool> {
    match self.stdout.read_exact(&mut self.spare.data) {
      Ok(()) => {
        std::mem::swap(&mut self.current, &mut self.spare);
        self.index += 1;
        Ok(true)
      }
      Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(false),
      Err(e) => Err(e.into()),
    }
  }
}

fn spawn_decoder(
  bin: &Path,
  path: &Path,
  decode: Decode,
  fps: f64,
  start: f64,
) -> Result<(Child, ChildStdout)> {
  let filter = match decode {
    Decode::Native => format!("fps={fps}"),
    Decode::Fit(w, h) => format!(
      "scale={w}:{h}:force_original_aspect_ratio=decrease:flags=lanczos,\
             pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,fps={fps}"
    ),
  };
  let mut child = Command::new(bin)
    .args(["-v", "error", "-hwaccel", "videotoolbox"])
    .args(["-ss", &start.to_string()])
    .arg("-i")
    .arg(path)
    .args([
      "-vf", &filter, "-pix_fmt", "rgba", "-f", "rawvideo", "pipe:1",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit())
    .spawn()?;
  let stdout = child.stdout.take().expect("piped stdout");
  Ok((child, stdout))
}

impl FrameSource for FfmpegSource {
  fn segment_at(&self, _src_t: f64) -> &SegmentInfo {
    &self.segment
  }

  fn fps(&self) -> f64 {
    self.fps
  }

  fn advance_to(&mut self, src_t: f64) -> Result<&Frame> {
    if self.index < 0 && !self.read_next()? {
      return Err(RenderError::Decode(format!(
        "{} has no frames",
        self.path.display()
      )));
    }
    while !self.eof && (self.index + 1) as f64 / self.fps <= src_t + 1e-9 {
      if !self.read_next()? {
        self.eof = true;
      }
    }
    Ok(&self.current)
  }

  fn seek(&mut self, src_t: f64) -> Result<()> {
    let _ = self.child.kill();
    let _ = self.child.wait();
    let (child, stdout) = spawn_decoder(&self.bin, &self.path, self.decode, self.fps, src_t)?;
    self.child = child;
    self.stdout = stdout;
    self.index = (src_t * self.fps).floor() as i64 - 1;
    self.eof = false;
    Ok(())
  }
}

impl Drop for FfmpegSource {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
  }
}

/// Scales a raw segment down for fast previews. Keyframes every second keep seeking cheap.
pub fn transcode_proxy(src: &Path, out: &Path, (w, h): (u32, u32), fps: u32) -> Result<()> {
  let status = Command::new(locate()?)
    .args(["-v", "error", "-y", "-hwaccel", "videotoolbox", "-i"])
    .arg(src)
    .args(["-vf", &format!("scale={w}:{h},fps={fps}")])
    .args([
      "-c:v",
      "h264_videotoolbox",
      "-b:v",
      "3M",
      "-pix_fmt",
      "yuv420p",
    ])
    .args(["-force_key_frames", "expr:gte(t,n_forced*1)"])
    .arg(out)
    .stdin(Stdio::null())
    .status()?;
  if status.success() {
    Ok(())
  } else {
    Err(RenderError::Encode(format!(
      "proxy: ffmpeg exited with {status}"
    )))
  }
}

pub struct FfmpegSink {
  child: Child,
  stdin: Option<ChildStdin>,
  size: (u32, u32),
}

impl FfmpegSink {
  /// `chapters` is an ffmetadata file whose chapters are copied into the output.
  pub fn create(path: &Path, size: (u32, u32), fps: u32, chapters: Option<&Path>) -> Result<Self> {
    let bin = locate()?;
    let mut cmd = Command::new(bin);
    cmd
      .args(["-v", "error", "-y"])
      .args(["-f", "rawvideo", "-pix_fmt", "rgba"])
      .args(["-s", &format!("{}x{}", size.0, size.1)])
      .args(["-r", &fps.to_string(), "-i", "pipe:0"]);
    if let Some(meta) = chapters {
      cmd
        .args(["-f", "ffmetadata", "-i"])
        .arg(meta)
        .args(["-map", "0:v", "-map_chapters", "1"]);
    }
    let mut child = cmd
      .args([
        "-c:v",
        "h264_videotoolbox",
        "-b:v",
        "12M",
        "-pix_fmt",
        "yuv420p",
      ])
      .args(["-movflags", "+faststart"])
      .arg(path)
      .stdin(Stdio::piped())
      .stdout(Stdio::null())
      .stderr(Stdio::inherit())
      .spawn()?;
    let stdin = child.stdin.take();
    Ok(FfmpegSink { child, stdin, size })
  }
}

impl FrameSink for FfmpegSink {
  fn write(&mut self, frame: &Frame) -> Result<()> {
    if (frame.width, frame.height) != self.size {
      return Err(RenderError::Encode(format!(
        "frame is {}x{}, sink expects {}x{}",
        frame.width, frame.height, self.size.0, self.size.1
      )));
    }
    let stdin = self.stdin.as_mut().expect("stdin open until finish");
    stdin
      .write_all(&frame.data)
      .map_err(|e| RenderError::Encode(format!("encoder closed its input: {e}")))
  }

  fn finish(mut self: Box<Self>) -> Result<()> {
    drop(self.stdin.take());
    let status = self.child.wait()?;
    if status.success() {
      Ok(())
    } else {
      Err(RenderError::Encode(format!("ffmpeg exited with {status}")))
    }
  }
}
