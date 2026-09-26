//! Debug drawing on output frames.

use fastflow_core::geom::Rect;

use crate::Frame;

pub type Rgb = [u8; 3];

/// Maps a normalized surface rect to output pixels for a whole frame fitted into the output,
/// as `FfmpegSource` letterboxes it.
#[derive(Debug, Clone, Copy)]
pub struct Letterbox {
  scale: f64,
  off: (f64, f64),
  surface: (f64, f64),
}

impl Letterbox {
  pub fn new(surface_px: (u32, u32), output: (u32, u32)) -> Letterbox {
    let (sw, sh) = (surface_px.0 as f64, surface_px.1 as f64);
    let (ow, oh) = (output.0 as f64, output.1 as f64);
    let scale = (ow / sw).min(oh / sh);
    Letterbox {
      scale,
      off: ((ow - sw * scale) / 2.0, (oh - sh * scale) / 2.0),
      surface: (sw, sh),
    }
  }

  pub fn rect(&self, r: &Rect) -> Rect {
    Rect {
      x: self.off.0 + r.x * self.surface.0 * self.scale,
      y: self.off.1 + r.y * self.surface.1 * self.scale,
      w: r.w * self.surface.0 * self.scale,
      h: r.h * self.surface.1 * self.scale,
    }
  }
}

fn put(frame: &mut Frame, x: i64, y: i64, c: Rgb) {
  if x < 0 || y < 0 || x >= frame.width as i64 || y >= frame.height as i64 {
    return;
  }
  let i = ((y as u32 * frame.width + x as u32) * 4) as usize;
  frame.data[i..i + 3].copy_from_slice(&c);
}

/// Outline of a rect in output pixels, clipped to the frame.
pub fn stroke(frame: &mut Frame, r: &Rect, c: Rgb, thickness: i64) {
  let (x0, y0) = (r.x.round() as i64, r.y.round() as i64);
  let (x1, y1) = ((r.x + r.w).round() as i64, (r.y + r.h).round() as i64);
  for k in 0..thickness {
    for x in x0..=x1 {
      put(frame, x, y0 + k, c);
      put(frame, x, y1 - k, c);
    }
    for y in y0..=y1 {
      put(frame, x0 + k, y, c);
      put(frame, x1 - k, y, c);
    }
  }
}

pub fn dot(frame: &mut Frame, cx: f64, cy: f64, radius: i64, c: Rgb) {
  let (cx, cy) = (cx.round() as i64, cy.round() as i64);
  for dy in -radius..=radius {
    for dx in -radius..=radius {
      if dx * dx + dy * dy <= radius * radius {
        put(frame, cx + dx, cy + dy, c);
      }
    }
  }
}
