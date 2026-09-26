pub mod permissions;

#[cfg(target_os = "macos")]
pub mod macos;

use fibre::mpsc::UnboundedSyncSender as Sender;
use std::fmt;
use std::time::Instant;

use fastflow_core::geom::Point;
use fastflow_core::recording::{InputKind, WindowInfo};

#[derive(Debug)]
pub struct DesktopError(pub String);

impl fmt::Display for DesktopError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.0)
  }
}

impl std::error::Error for DesktopError {}

pub type Result<T> = std::result::Result<T, DesktopError>;

#[derive(Debug, Clone, Copy)]
pub struct WindowCaps {
  pub can_read_other_app_geometry: bool,
}

/// Samples window geometry and the cursor, normalized to one captured surface.
pub trait WindowSource: Send {
  fn caps(&self) -> WindowCaps;
  fn sample(&mut self) -> Result<Vec<WindowInfo>>;
  fn cursor(&mut self) -> Result<Point>;
}

/// An input event stamped on arrival. The recorder converts `at` to the recording clock.
#[derive(Debug, Clone, Copy)]
pub struct RawInput {
  pub at: Instant,
  pub kind: InputKind,
}

/// Not `Send`: the macOS tap is bound to the run loop of the thread that started it.
pub trait InputMonitor {
  fn start(&mut self, sink: Sender<RawInput>) -> Result<()>;
  /// Call periodically. Re-arms the source if the OS disabled it.
  fn maintain(&mut self);
  fn stop(&mut self);
}
