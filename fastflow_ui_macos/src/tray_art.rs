//! Menu bar icons, built from the images `scripts/icons.sh` generates: the outline when idle, the
//! filled screen while recording and the outline filling from the left while a render runs.

use tray_icon::Icon;

/// Progress is drawn in steps of 5%, so the icon changes at most 20 times per render.
const STEPS: f64 = 20.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
  Idle,
  Recording,
  Rendering(u8),
}

impl State {
  pub fn rendering(progress: f64) -> State {
    State::Rendering((progress.clamp(0.0, 1.0) * STEPS).floor() as u8)
  }
}

pub struct Glyphs {
  idle: Vec<u8>,
  filled: Vec<u8>,
  width: u32,
  height: u32,
}

impl Glyphs {
  pub fn load() -> Glyphs {
    let (idle, width, height) = decode(include_bytes!("../bundle/tray.png"));
    let (filled, w, h) = decode(include_bytes!("../bundle/tray_recording.png"));
    assert_eq!((w, h), (width, height), "tray images differ in size");
    Glyphs {
      idle,
      filled,
      width,
      height,
    }
  }

  pub fn icon(&self, state: State) -> Icon {
    let rgba = match state {
      State::Idle => self.idle.clone(),
      State::Recording => self.filled.clone(),
      State::Rendering(step) => fill(&self.idle, &self.filled, self.width, step as f64 / STEPS),
    };
    Icon::from_rgba(rgba, self.width, self.height).expect("icon")
  }
}

/// `idle` with the columns left of `fraction` of the width taken from `filled`.
fn fill(idle: &[u8], filled: &[u8], width: u32, fraction: f64) -> Vec<u8> {
  let cut = (width as f64 * fraction).round() as usize;
  let row = width as usize * 4;
  idle
    .chunks(row)
    .zip(filled.chunks(row))
    .flat_map(|(i, f)| {
      let mut out = f[..cut * 4].to_vec();
      out.extend_from_slice(&i[cut * 4..]);
      out
    })
    .collect()
}

fn decode(bytes: &[u8]) -> (Vec<u8>, u32, u32) {
  let mut reader = png::Decoder::new(std::io::Cursor::new(bytes))
    .read_info()
    .expect("tray png");
  let mut rgba = vec![0; reader.output_buffer_size().expect("tray png size")];
  let info = reader.next_frame(&mut rgba).expect("tray png frame");
  rgba.truncate(info.buffer_size());
  (rgba, info.width, info.height)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn progress_fills_columns_from_the_left() {
    let idle = [0u8; 4 * 4 * 2];
    let filled = [9u8; 4 * 4 * 2];
    let half = fill(&idle, &filled, 4, 0.5);
    for row in half.chunks(16) {
      assert_eq!(row, &[9, 9, 9, 9, 9, 9, 9, 9, 0, 0, 0, 0, 0, 0, 0, 0]);
    }
    assert_eq!(fill(&idle, &filled, 4, 0.0), idle);
    assert_eq!(fill(&idle, &filled, 4, 1.0), filled);
  }

  #[test]
  fn progress_moves_in_steps() {
    assert_eq!(State::rendering(0.0), State::Rendering(0));
    assert_eq!(State::rendering(0.049), State::Rendering(0));
    assert_eq!(State::rendering(0.42), State::Rendering(8));
    assert_eq!(State::rendering(1.0), State::Rendering(20));
  }

  #[test]
  fn both_images_load_at_the_same_size() {
    let g = Glyphs::load();
    assert_eq!(g.idle.len(), g.filled.len());
  }
}
