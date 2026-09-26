//! Manual overrides recorded with hotkeys. See docs/04-tracks.md.

use std::ops::Range;

use crate::recording::{InputEvent, InputKind};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Markers {
  /// Forced to human speed.
  pub keep: Vec<Range<f64>>,
  /// Forced to idle speed.
  pub cut: Vec<Range<f64>>,
  /// The camera stays on its current subject.
  pub frame: Vec<Range<f64>>,
  pub chapters: Vec<f64>,
}

/// Pairs presses into ranges. An unpaired last press runs to `end`.
fn toggles(times: &[f64], end: f64) -> Vec<Range<f64>> {
  times
    .chunks(2)
    .map(|p| p[0]..p.get(1).copied().unwrap_or(end))
    .filter(|r| r.start < r.end)
    .collect()
}

impl Markers {
  /// `events` in any order; times in the result are seconds.
  pub fn from_events(events: &[InputEvent], duration: f64) -> Markers {
    let times = |kind: InputKind| {
      let mut t: Vec<f64> = events
        .iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.t as f64 / 1000.0)
        .collect();
      t.sort_by(f64::total_cmp);
      t
    };
    Markers {
      keep: toggles(&times(InputKind::Keep), duration),
      cut: toggles(&times(InputKind::Cut), duration),
      frame: toggles(&times(InputKind::Frame), duration),
      chapters: times(InputKind::Chapter),
    }
  }
}

/// Removes every `cut` range from sorted, disjoint `ranges`.
pub fn subtract(ranges: &[(f64, f64)], cut: &[Range<f64>]) -> Vec<(f64, f64)> {
  let mut out = ranges.to_vec();
  for c in cut {
    out = out
      .into_iter()
      .flat_map(|(a, b)| {
        let mut pieces = Vec::with_capacity(2);
        if a < c.start {
          pieces.push((a, b.min(c.start)));
        }
        if b > c.end {
          pieces.push((a.max(c.end), b));
        }
        if b <= c.start || a >= c.end {
          pieces = vec![(a, b)];
        }
        pieces
      })
      .filter(|(a, b)| a < b)
      .collect();
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  fn ev(t: i64, kind: InputKind) -> InputEvent {
    InputEvent { t, kind }
  }

  #[test]
  fn toggles_pair_up_and_an_open_one_runs_to_the_end() {
    let m = Markers::from_events(
      &[
        ev(1000, InputKind::Keep),
        ev(3000, InputKind::Keep),
        ev(8000, InputKind::Keep),
        ev(5000, InputKind::Chapter),
        ev(500, InputKind::Key),
      ],
      10.0,
    );
    assert_eq!(m.keep, vec![1.0..3.0, 8.0..10.0]);
    assert_eq!(m.chapters, vec![5.0]);
    assert!(m.cut.is_empty() && m.frame.is_empty());
  }

  #[test]
  fn subtract_splits_overlapping_ranges() {
    let r = subtract(&[(0.0, 10.0), (12.0, 14.0)], &[2.0..4.0, 13.0..20.0]);
    assert_eq!(r, vec![(0.0, 2.0), (4.0, 10.0), (12.0, 13.0)]);
  }
}
