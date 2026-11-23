//! Geometry of the slice track: which lane each slice is drawn in, what a click lands on and
//! where a drag moves a slice. Positions are fractions of the scrub file's length.

use std::ops::Range;

/// Pixels either side of a slice's edge that grab the edge.
pub const EDGE: f64 = 6.0;
/// Pixels above and below the lanes.
pub const PAD: f64 = 3.0;

/// The lane of each band and how many lanes there are. A band goes in the first lane whose
/// bands all end at or before it starts, taking bands in order of their start.
pub fn lanes(bands: &[Range<f64>]) -> (Vec<usize>, usize) {
  let mut order: Vec<usize> = (0..bands.len()).collect();
  order.sort_by(|&a, &b| bands[a].start.total_cmp(&bands[b].start).then(a.cmp(&b)));
  let mut ends: Vec<f64> = Vec::new();
  let mut lane = vec![0; bands.len()];
  for i in order {
    let free = ends.iter().position(|&e| e <= bands[i].start);
    let l = free.unwrap_or_else(|| {
      ends.push(f64::NEG_INFINITY);
      ends.len() - 1
    });
    ends[l] = bands[i].end;
    lane[i] = l;
  }
  (lane, ends.len().max(1))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
  Empty,
  Start(usize),
  End(usize),
  Body(usize),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
  /// Pixels from the view's left edge to fraction 0.
  pub x0: f64,
  /// Pixels from fraction 0 to fraction 1.
  pub width: f64,
  pub height: f64,
  pub lanes: usize,
}

impl Layout {
  pub fn x(&self, fraction: f64) -> f64 {
    self.x0 + fraction * self.width
  }

  pub fn fraction(&self, x: f64) -> f64 {
    ((x - self.x0) / self.width).clamp(0.0, 1.0)
  }

  /// Bottom and height of `lane`. Lane 0 is at the top and y grows upwards as in AppKit.
  pub fn lane_rect(&self, lane: usize) -> (f64, f64) {
    let h = self.lane_height();
    (self.height - PAD - (lane + 1) as f64 * h, h)
  }

  fn lane_height(&self) -> f64 {
    (self.height - 2.0 * PAD) / self.lanes.max(1) as f64
  }

  /// The lane under `y`, clamped to the lanes there are.
  pub fn lane_at(&self, y: f64) -> usize {
    let i = ((self.height - PAD - y) / self.lane_height())
      .floor()
      .max(0.0) as usize;
    i.min(self.lanes.max(1) - 1)
  }

  /// What a press at `(x, y)` takes hold of. The selected band's edges win over others, and
  /// the nearer edge wins on a band too narrow to tell them apart.
  pub fn hit(
    &self,
    bands: &[Range<f64>],
    lanes: &[usize],
    selected: Option<usize>,
    x: f64,
    y: f64,
  ) -> Hit {
    let lane = self.lane_at(y);
    let here = |i: &usize| lanes.get(*i) == Some(&lane);
    let edge = |i: usize| {
      let (a, b) = (
        (self.x(bands[i].start) - x).abs(),
        (self.x(bands[i].end) - x).abs(),
      );
      let d = a.min(b);
      (d <= EDGE).then_some((d, if a <= b { Hit::Start(i) } else { Hit::End(i) }))
    };
    if let Some((_, hit)) = selected.filter(here).and_then(edge) {
      return hit;
    }
    let nearest = (0..bands.len())
      .filter(here)
      .filter_map(edge)
      .min_by(|a, b| a.0.total_cmp(&b.0));
    if let Some((_, hit)) = nearest {
      return hit;
    }
    let inside = |i: &usize| {
      here(i) && bands[*i].start <= self.fraction(x) && self.fraction(x) <= bands[*i].end
    };
    selected
      .filter(inside)
      .or_else(|| (0..bands.len()).find(inside))
      .map_or(Hit::Empty, Hit::Body)
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Drag {
  /// A new band from where the press started.
  Create {
    anchor: f64,
  },
  Start,
  End,
  /// The whole band, held `offset` after its start.
  Move {
    offset: f64,
  },
}

/// The band `drag` makes of `band` with the pointer at fraction `x`.
pub fn apply(drag: Drag, band: Range<f64>, x: f64) -> Range<f64> {
  match drag {
    Drag::Create { anchor } => anchor.min(x)..anchor.max(x),
    Drag::Start => x.min(band.end)..band.end,
    Drag::End => band.start..x.max(band.start),
    Drag::Move { offset } => {
      let len = band.end - band.start;
      let start = (x - offset).clamp(0.0, 1.0 - len);
      start..start + len
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn layout(lanes: usize) -> Layout {
    Layout {
      x0: 0.0,
      width: 1000.0,
      height: 40.0,
      lanes,
    }
  }

  #[test]
  fn bands_apart_or_touching_share_a_lane() {
    assert_eq!(lanes(&[0.0..0.2, 0.2..0.4, 0.6..0.9]), (vec![0, 0, 0], 1));
  }

  #[test]
  fn overlapping_bands_go_to_the_next_free_lane() {
    let (l, n) = lanes(&[0.5..0.9, 0.0..0.6, 0.1..0.3, 0.65..0.7, 0.15..0.25]);
    assert_eq!(n, 3);
    assert_eq!(l, vec![1, 0, 1, 0, 2]);
  }

  #[test]
  fn no_bands_still_has_one_lane() {
    assert_eq!(lanes(&[]), (vec![], 1));
  }

  #[test]
  fn lanes_fill_the_height_from_the_top() {
    let l = layout(2);
    assert_eq!(l.lane_rect(0), (20.0, 17.0));
    assert_eq!(l.lane_rect(1), (3.0, 17.0));
    assert_eq!(l.lane_at(30.0), 0);
    assert_eq!(l.lane_at(10.0), 1);
    assert_eq!(l.lane_at(-5.0), 1);
    assert_eq!(l.lane_at(50.0), 0);
  }

  #[test]
  fn a_press_finds_the_edge_the_body_or_nothing() {
    let l = layout(1);
    let bands = [0.1..0.3, 0.5..0.6];
    let lanes = [0, 0];
    assert_eq!(l.hit(&bands, &lanes, None, 102.0, 20.0), Hit::Start(0));
    assert_eq!(l.hit(&bands, &lanes, None, 297.0, 20.0), Hit::End(0));
    assert_eq!(l.hit(&bands, &lanes, None, 200.0, 20.0), Hit::Body(0));
    assert_eq!(l.hit(&bands, &lanes, None, 400.0, 20.0), Hit::Empty);
    assert_eq!(l.hit(&bands, &lanes, None, 604.0, 20.0), Hit::End(1));
  }

  #[test]
  fn the_selected_band_wins_a_shared_edge() {
    let l = layout(1);
    let bands = [0.1..0.3, 0.3..0.5];
    let lanes = [0, 0];
    assert_eq!(l.hit(&bands, &lanes, Some(1), 301.0, 20.0), Hit::Start(1));
    assert_eq!(l.hit(&bands, &lanes, Some(0), 299.0, 20.0), Hit::End(0));
  }

  #[test]
  fn a_narrow_band_gives_the_nearer_edge() {
    let l = layout(1);
    let bands = [0.500..0.504];
    assert_eq!(l.hit(&bands, &[0], None, 499.0, 20.0), Hit::Start(0));
    assert_eq!(l.hit(&bands, &[0], None, 505.0, 20.0), Hit::End(0));
  }

  #[test]
  fn overlapping_bands_are_told_apart_by_lane() {
    let l = layout(2);
    let bands = [0.0..0.6, 0.5..0.9];
    let lanes = [0, 1];
    assert_eq!(l.hit(&bands, &lanes, None, 550.0, 30.0), Hit::Body(0));
    assert_eq!(l.hit(&bands, &lanes, None, 550.0, 10.0), Hit::Body(1));
    assert_eq!(l.hit(&bands, &lanes, None, 700.0, 30.0), Hit::Empty);
  }

  #[test]
  fn an_edge_cannot_cross_the_other() {
    assert_eq!(apply(Drag::Start, 0.2..0.4, 0.5), 0.4..0.4);
    assert_eq!(apply(Drag::End, 0.2..0.4, 0.1), 0.2..0.2);
    assert_eq!(apply(Drag::Start, 0.2..0.4, 0.1), 0.1..0.4);
  }

  #[test]
  fn a_moved_band_keeps_its_length_inside_the_track() {
    let r = apply(Drag::Move { offset: 0.05 }, 0.2..0.4, 0.95);
    assert!((r.start - 0.8).abs() < 1e-12 && (r.end - 1.0).abs() < 1e-12);
    let r = apply(Drag::Move { offset: 0.05 }, 0.2..0.4, 0.0);
    assert!(r.start.abs() < 1e-12 && (r.end - 0.2).abs() < 1e-12);
    let r = apply(Drag::Move { offset: 0.05 }, 0.2..0.4, 0.5);
    assert!((r.start - 0.45).abs() < 1e-12 && (r.end - 0.65).abs() < 1e-12);
  }

  #[test]
  fn a_new_band_spans_the_press_and_the_pointer_either_way() {
    let c = Drag::Create { anchor: 0.5 };
    assert_eq!(apply(c, 0.5..0.5, 0.7), 0.5..0.7);
    assert_eq!(apply(c, 0.5..0.5, 0.2), 0.2..0.5);
  }
}
