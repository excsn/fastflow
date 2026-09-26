//! `config.toml`. Every field has a default, so a file only needs the settings it changes.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
  pub capture: CaptureConfig,
  pub pacing: PacingConfig,
  pub camera: CameraConfig,
  pub focus: FocusConfig,
  pub render: RenderConfig,
  pub output: OutputConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
  /// "auto", "ffmpeg" or "sck".
  pub backend: String,
  /// Widest capture in pixels. A wider display is scaled down while it is captured. 0 captures
  /// every display at its native size.
  pub max_width: u32,
}

impl CaptureConfig {
  /// The size a display of `native` pixels is captured at. Both sides stay even for the encoder.
  pub fn capture_size(&self, native: (u32, u32)) -> (u32, u32) {
    if self.max_width == 0 || native.0 <= self.max_width {
      return native;
    }
    let even = |v: f64| ((v / 2.0).round() as u32 * 2).max(2);
    let w = even(self.max_width as f64);
    (w, even(native.1 as f64 * w as f64 / native.0 as f64))
  }
}

/// Durations in seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PacingConfig {
  pub pad_before: f64,
  pub pad_after: f64,
  pub human_speed: f64,
  pub idle_speed: f64,
  pub max_dead: f64,
  pub hold: f64,
  /// Output seconds each ramp takes to ease between human speed and `ramp_speed`.
  pub ramp: f64,
  pub ramp_speed: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CameraConfig {
  pub enabled: bool,
  pub commit_ms: u32,
  pub transition_ms: u32,
  /// Points.
  pub window_pad: f64,
  pub pull_back: f64,
  pub max_zoom: f64,
  /// Points.
  pub min_window_size: [f64; 2],
  pub overlap_hold: f64,
  /// Draw the camera's framing on screen while recording, when the capture backend can keep
  /// it out of the footage.
  pub live_overlay: bool,
}

/// Blurs and dims everything outside the window the camera has chosen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FocusConfig {
  pub enabled: bool,
  /// Output pixels.
  pub blur: f64,
  pub dim: f64,
  /// Output pixels.
  pub feather: f64,
  /// Points, matching the window's own corners.
  pub corner_radius: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RenderConfig {
  pub switch_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
  /// Pixels.
  pub size: [u32; 2],
  pub fps: u32,
}

impl Default for CaptureConfig {
  fn default() -> Self {
    CaptureConfig {
      backend: "auto".into(),
      max_width: 4096,
    }
  }
}

impl Default for PacingConfig {
  fn default() -> Self {
    PacingConfig {
      pad_before: 0.3,
      pad_after: 1.2,
      human_speed: 1.0,
      idle_speed: 8.0,
      max_dead: 0.5,
      hold: 0.25,
      ramp: 0.5,
      ramp_speed: 3.0,
    }
  }
}

impl Default for CameraConfig {
  fn default() -> Self {
    CameraConfig {
      enabled: true,
      commit_ms: 400,
      transition_ms: 700,
      window_pad: 24.0,
      pull_back: 1.15,
      max_zoom: 2.0,
      min_window_size: [300.0, 200.0],
      overlap_hold: 0.85,
      live_overlay: true,
    }
  }
}

impl Default for FocusConfig {
  fn default() -> Self {
    FocusConfig {
      enabled: true,
      blur: 24.0,
      dim: 0.25,
      feather: 6.0,
      corner_radius: 10.0,
    }
  }
}

impl Default for RenderConfig {
  fn default() -> Self {
    RenderConfig { switch_ms: 400 }
  }
}

impl Default for OutputConfig {
  fn default() -> Self {
    OutputConfig {
      size: [1920, 1080],
      fps: 60,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn capture(max_width: u32) -> CaptureConfig {
    CaptureConfig {
      max_width,
      ..CaptureConfig::default()
    }
  }

  #[test]
  fn narrow_displays_are_captured_native() {
    assert_eq!(capture(4096).capture_size((3024, 1964)), (3024, 1964));
    assert_eq!(capture(4096).capture_size((4096, 2304)), (4096, 2304));
  }

  #[test]
  fn wide_displays_are_scaled_to_the_cap() {
    assert_eq!(capture(4096).capture_size((6400, 3600)), (4096, 2304));
    assert_eq!(capture(4096).capture_size((5120, 2160)), (4096, 1728));
  }

  #[test]
  fn scaled_sides_stay_even() {
    let (w, h) = capture(3001).capture_size((6400, 3600));
    assert_eq!((w % 2, h % 2), (0, 0));
  }

  #[test]
  fn zero_means_native() {
    assert_eq!(capture(0).capture_size((6400, 3600)), (6400, 3600));
  }
}
