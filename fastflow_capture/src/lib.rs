pub mod ffmpeg;
#[cfg(target_os = "macos")]
pub mod sck;

use std::fmt;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Instant;

use fastflow_core::recording::Confidence;

#[derive(Debug, Clone, Copy)]
pub struct CaptureCaps {
  pub can_exclude_windows: bool,
  pub can_deliver_frames: bool,
  pub reports_frame_timestamps: bool,
  pub can_follow_displays: bool,
  pub max_fps: u32,
}

#[derive(Debug, Clone)]
pub struct CaptureSpec {
  /// Position of the display in `CGGetActiveDisplayList` order.
  pub display_index: usize,
  pub display_id: u32,
  pub size_px: (u32, u32),
  pub fps: u32,
  pub out: PathBuf,
}

#[derive(Debug)]
pub struct CaptureArtifact {
  pub path: PathBuf,
  pub status: Option<ExitStatus>,
  /// Frames written and frames dropped, when the backend counts them.
  pub frames: Option<(u64, u64)>,
}

#[derive(Debug)]
pub enum CaptureError {
  BackendMissing(String),
  NoSuchDisplay(usize),
  Io(std::io::Error),
  Exited(String),
}

impl fmt::Display for CaptureError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      CaptureError::BackendMissing(what) => write!(f, "capture backend unavailable: {what}"),
      CaptureError::NoSuchDisplay(i) => write!(f, "no capture device for display {i}"),
      CaptureError::Io(e) => write!(f, "capture i/o: {e}"),
      CaptureError::Exited(why) => write!(f, "capture exited: {why}"),
    }
  }
}

impl std::error::Error for CaptureError {}

impl From<std::io::Error> for CaptureError {
  fn from(e: std::io::Error) -> Self {
    CaptureError::Io(e)
  }
}

pub type Result<T> = std::result::Result<T, CaptureError>;

pub trait ScreenCapture: Send {
  fn name(&self) -> &'static str;
  fn caps(&self) -> CaptureCaps;
  fn start(&mut self, spec: &CaptureSpec) -> Result<Box<dyn CaptureSession>>;
}

pub trait CaptureSession: Send {
  fn first_frame_at(&self) -> Option<Instant>;
  fn confidence(&self) -> Confidence;
  /// `Some` once the capture has ended on its own, which is always a failure.
  fn exited(&mut self) -> Option<String>;
  fn pid(&self) -> Option<u32>;
  fn stop(self: Box<Self>) -> Result<CaptureArtifact>;
}

/// `backend` is `capture.backend` from the settings: "auto" prefers the native backend.
pub fn detect(backend: &str) -> Result<Box<dyn ScreenCapture>> {
  match backend {
    "ffmpeg" => Ok(Box::new(ffmpeg::FfmpegCapture::locate()?)),
    #[cfg(target_os = "macos")]
    "auto" | "sck" => Ok(Box::new(sck::SckCapture)),
    other => Err(CaptureError::BackendMissing(format!(
      "unknown backend {other:?}"
    ))),
  }
}

/// Longest side, in pixels, the VideoToolbox H.264 encoder accepts.
pub const H264_MAX_SIDE: u32 = 4096;

/// Whether a capture of `size` pixels has to be encoded as HEVC.
pub fn needs_hevc(size: (u32, u32)) -> bool {
  size.0 > H264_MAX_SIDE || size.1 > H264_MAX_SIDE
}

/// The file name a backend writes a segment to.
pub fn segment_file(backend: &str, index: u32) -> String {
  match backend {
    "ffmpeg" => format!("raw.{index}.mp4"),
    _ => format!("raw.{index}.mov"),
  }
}
