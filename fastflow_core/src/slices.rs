//! `slices.toml`: named ranges of a recording and the outputs built from them.

use std::collections::BTreeMap;
use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::timeline::Timeline;

pub const IMPLICIT_OUTPUT: &str = "render";
/// The slice earlier files used for the whole recording. `migrate` removes it.
const LEGACY_WHOLE: &str = "all";

/// Window edges are sums of floats, so a frame time equal to an edge may land a hair short.
const EPS: f64 = 1e-9;

/// Source seconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slice {
  pub from: f64,
  pub to: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
  #[default]
  Mp4,
  Gif,
  Webp,
}

impl Format {
  pub fn extension(self) -> &'static str {
    match self {
      Format::Mp4 => "mp4",
      Format::Gif => "gif",
      Format::Webp => "webp",
    }
  }

  pub fn is_animation(self) -> bool {
    self != Format::Mp4
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Output {
  pub name: String,
  #[serde(default)]
  pub format: Format,
  /// Slice names in the order they play, each at most once. Empty plays the whole recording.
  #[serde(default)]
  pub slices: Vec<String>,
  /// Pixels. An mp4 defaults to `output.size`, an animation to 800.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub width: Option<u32>,
  /// An mp4 defaults to `output.fps`, an animation to 15.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub fps: Option<u32>,
  /// GIF palette size, 2 to 256.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub colors: Option<u32>,
  /// WebP lossy quality, 0 to 100.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub quality: Option<u32>,
  /// Cross-dissolve at each join. 0 is a hard cut.
  #[serde(default, skip_serializing_if = "is_zero")]
  pub dissolve_ms: u32,
}

fn is_zero(v: &u32) -> bool {
  *v == 0
}

impl Output {
  pub fn file_name(&self) -> String {
    format!("{}.{}", self.name, self.format.extension())
  }

  /// Puts `name` before the slice now at `at`, or last when `at` is past the end. A slice the
  /// output already plays is moved rather than added again.
  pub fn place(&mut self, name: &str, at: usize) {
    let mut at = at;
    if let Some(j) = self.slices.iter().position(|n| n == name) {
      self.slices.remove(j);
      if j < at {
        at -= 1;
      }
    }
    self
      .slices
      .insert(at.min(self.slices.len()), name.to_owned());
  }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slices {
  #[serde(default, rename = "slice")]
  pub slices: BTreeMap<String, Slice>,
  #[serde(default, rename = "output")]
  pub outputs: Vec<Output>,
}

impl Slices {
  /// No slices and one mp4 output of the whole recording named `render`.
  pub fn implicit() -> Slices {
    Slices {
      slices: BTreeMap::new(),
      outputs: vec![Output {
        name: IMPLICIT_OUTPUT.into(),
        format: Format::Mp4,
        slices: Vec::new(),
        width: None,
        fps: None,
        colors: None,
        quality: None,
        dissolve_ms: 0,
      }],
    }
  }

  /// Checks names and ranges against a recording `duration` source seconds long.
  pub fn validate(&self, duration: f64) -> Result<(), String> {
    for (name, s) in &self.slices {
      if !(s.from.is_finite() && s.to.is_finite() && 0.0 <= s.from && s.from < s.to) {
        return Err(format!(
          "slice {name:?} has an empty range {}..{}",
          s.from, s.to
        ));
      }
      if s.to > duration + 1e-6 {
        return Err(format!(
          "slice {name:?} ends at {}s, past the recording's {duration:.3}s",
          s.to
        ));
      }
    }
    for (i, o) in self.outputs.iter().enumerate() {
      if o.name.is_empty() || o.name.contains(['/', '\\']) || o.name.starts_with('.') {
        return Err(format!("output name {:?} is not a plain file name", o.name));
      }
      if self.outputs[..i].iter().any(|p| p.name == o.name) {
        return Err(format!("output {:?} is listed twice", o.name));
      }
      if let Some(twice) =
        (o.slices.iter().enumerate()).find_map(|(j, n)| o.slices[..j].contains(n).then_some(n))
      {
        return Err(format!("output {:?} lists slice {twice:?} twice", o.name));
      }
      if let Some(missing) = o.slices.iter().find(|n| !self.slices.contains_key(*n)) {
        return Err(format!(
          "output {:?} names slice {missing:?}, which does not exist",
          o.name
        ));
      }
    }
    Ok(())
  }

  pub fn output(&self, name: &str) -> Option<&Output> {
    self.outputs.iter().find(|o| o.name == name)
  }

  /// The source ranges an output plays, in order. `None` is the whole recording. Unknown
  /// names are skipped.
  pub fn ranges(&self, output: &Output) -> Option<Vec<Range<f64>>> {
    if output.slices.is_empty() {
      return None;
    }
    let ranges = (output.slices.iter())
      .filter_map(|n| self.slices.get(n))
      .map(|s| s.from..s.to)
      .collect();
    Some(ranges)
  }

  /// How `output` lays out on `timeline`, with its dissolve.
  pub fn projection(&self, timeline: &Timeline, output: &Output) -> Projection {
    match self.ranges(output) {
      None => Projection::whole(timeline),
      Some(ranges) => Projection::new(timeline, &ranges, output.dissolve_ms as f64 / 1000.0),
    }
  }

  /// Rewrites a file from before outputs could play the whole recording: drops the slice
  /// named `all` when it covers the recording and drops repeated names from each output.
  /// Returns whether anything changed.
  pub fn migrate(&mut self, duration: f64) -> bool {
    let whole =
      (self.slices.get(LEGACY_WHOLE)).is_some_and(|s| s.from <= 1e-3 && s.to >= duration - 1e-3);
    if whole {
      self.slices.remove(LEGACY_WHOLE);
    }
    let mut changed = whole;
    for o in &mut self.outputs {
      let mut kept: Vec<String> = Vec::with_capacity(o.slices.len());
      for n in &o.slices {
        if !(whole && n == LEGACY_WHOLE) && !kept.contains(n) {
          kept.push(n.clone());
        }
      }
      changed |= kept != o.slices;
      o.slices = kept;
    }
    changed
  }
}

/// One slice placed in an output. Times are in output seconds of the full timeline, except
/// `start`, which is where the slice begins in the output being rendered.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
  pub full: Range<f64>,
  pub start: f64,
  /// Dissolve shared with the previous window. 0 for the first.
  pub dissolve_in: f64,
}

impl Window {
  pub fn len(&self) -> f64 {
    self.full.end - self.full.start
  }

  pub fn is_empty(&self) -> bool {
    self.len() <= 0.0
  }

  pub fn end(&self) -> f64 {
    self.start + self.len()
  }
}

/// What an output frame shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Place {
  One {
    window: usize,
    full_t: f64,
  },
  /// Window `to` fading in over window `to - 1`. `u` runs from 0 to 1.
  Blend {
    to: usize,
    from_t: f64,
    to_t: f64,
    u: f64,
  },
}

/// Lays an output's slices end to end. Each slice is a window of the full timeline, so it is
/// paced exactly as the whole recording is.
#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
  windows: Vec<Window>,
}

impl Projection {
  /// The whole timeline as one window.
  pub fn whole(timeline: &Timeline) -> Projection {
    let windows = Some(Window {
      full: 0.0..timeline.out_duration(),
      start: 0.0,
      dissolve_in: 0.0,
    })
    .filter(|w| !w.is_empty())
    .into_iter()
    .collect();
    Projection { windows }
  }

  /// `ranges` in source seconds. A dissolve is capped at half of either neighbour.
  pub fn new(timeline: &Timeline, ranges: &[Range<f64>], dissolve: f64) -> Projection {
    let mut windows: Vec<Window> = Vec::with_capacity(ranges.len());
    for r in ranges {
      let full = timeline.out_time_at(r.start)..timeline.out_time_at(r.end);
      if full.end - full.start <= 1e-9 {
        continue;
      }
      let len = full.end - full.start;
      let (start, dissolve_in) = match windows.last() {
        None => (0.0, 0.0),
        Some(prev) => {
          let d = dissolve.max(0.0).min(prev.len() / 2.0).min(len / 2.0);
          (prev.end() - d, d)
        }
      };
      windows.push(Window {
        full,
        start,
        dissolve_in,
      });
    }
    Projection { windows }
  }

  pub fn windows(&self) -> &[Window] {
    &self.windows
  }

  pub fn duration(&self) -> f64 {
    self.windows.last().map_or(0.0, Window::end)
  }

  pub fn at(&self, t: f64) -> Place {
    if self.windows.is_empty() {
      return Place::One {
        window: 0,
        full_t: 0.0,
      };
    }
    let t = t.clamp(0.0, self.duration());
    let j = self
      .windows
      .partition_point(|w| w.start <= t + EPS)
      .saturating_sub(1);
    let w = &self.windows[j];
    let local = (t - w.start).max(0.0);
    if j > 0 && local < w.dissolve_in {
      let prev = &self.windows[j - 1];
      return Place::Blend {
        to: j,
        from_t: (prev.full.start + t - prev.start).min(prev.full.end),
        to_t: w.full.start + local,
        u: local / w.dissolve_in,
      };
    }
    Place::One {
      window: j,
      full_t: (w.full.start + local).min(w.full.end),
    }
  }

  /// Where a full-timeline output time first appears in this output.
  pub fn project(&self, full_t: f64) -> Option<f64> {
    self
      .windows
      .iter()
      .find(|w| w.full.contains(&full_t))
      .map(|w| w.start + full_t - w.full.start)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::config::PacingConfig;
  use proptest::prelude::*;

  fn timeline() -> Timeline {
    Timeline::build(
      &[1.0, 2.0, 3.0, 20.0, 21.0, 40.0],
      60.0,
      &PacingConfig::default(),
    )
  }

  fn demo() -> Slices {
    let mut slices = BTreeMap::new();
    slices.insert("a".into(), Slice { from: 1.0, to: 4.0 });
    slices.insert(
      "b".into(),
      Slice {
        from: 19.0,
        to: 23.0,
      },
    );
    Slices {
      slices,
      outputs: vec![Output {
        name: "demo".into(),
        format: Format::Gif,
        slices: vec!["b".into(), "a".into()],
        width: Some(800),
        fps: Some(15),
        colors: None,
        quality: None,
        dissolve_ms: 0,
      }],
    }
  }

  #[test]
  fn implicit_is_render_mp4_of_the_whole_recording_with_no_slices() {
    let s = Slices::implicit();
    assert!(s.slices.is_empty());
    assert_eq!(s.outputs.len(), 1);
    let out = &s.outputs[0];
    assert_eq!(out.file_name(), "render.mp4");
    assert_eq!(s.ranges(out), None);
    assert!(s.validate(42.0).is_ok());
  }

  fn placed(list: &[&str], name: &str, at: usize) -> Vec<String> {
    let mut o = demo().outputs.remove(0);
    o.slices = list.iter().map(|s| s.to_string()).collect();
    o.place(name, at);
    o.slices
  }

  #[test]
  fn placing_a_new_slice_inserts_it() {
    assert_eq!(placed(&["a", "b"], "c", 1), vec!["a", "c", "b"]);
    assert_eq!(placed(&["a", "b"], "c", 0), vec!["c", "a", "b"]);
    assert_eq!(placed(&["a", "b"], "c", 9), vec!["a", "b", "c"]);
    assert_eq!(placed(&[], "c", 0), vec!["c"]);
  }

  #[test]
  fn placing_a_slice_already_played_moves_it() {
    assert_eq!(placed(&["a", "b", "c"], "a", 3), vec!["b", "c", "a"]);
    assert_eq!(placed(&["a", "b", "c"], "a", 2), vec!["b", "a", "c"]);
    assert_eq!(placed(&["a", "b", "c"], "c", 0), vec!["c", "a", "b"]);
    assert_eq!(placed(&["a", "b", "c"], "b", 1), vec!["a", "b", "c"]);
    assert_eq!(placed(&["a", "b", "c"], "b", 2), vec!["a", "b", "c"]);
  }

  #[test]
  fn ranges_follow_the_output_order() {
    let s = demo();
    assert_eq!(s.ranges(&s.outputs[0]), Some(vec![19.0..23.0, 1.0..4.0]));
  }

  #[test]
  fn an_output_without_slices_projects_the_whole_timeline() {
    let t = timeline();
    let mut s = demo();
    s.outputs[0].slices.clear();
    s.outputs[0].dissolve_ms = 400;
    assert_eq!(s.projection(&t, &s.outputs[0]), Projection::whole(&t));
  }

  #[test]
  fn an_output_projects_its_slices_with_its_dissolve() {
    let t = timeline();
    let mut s = demo();
    s.outputs[0].dissolve_ms = 400;
    let want = Projection::new(&t, &[19.0..23.0, 1.0..4.0], 0.4);
    assert_eq!(s.projection(&t, &s.outputs[0]), want);
  }

  #[test]
  fn an_output_without_slices_written_by_hand_parses() {
    let s: Slices = serde_json::from_str(r#"{"output": [{"name": "render"}]}"#).unwrap();
    assert!(s.outputs[0].slices.is_empty());
  }

  #[test]
  fn validation_rejects_a_slice_listed_twice_in_one_output() {
    let mut s = demo();
    s.outputs[0].slices.push("b".into());
    assert!(s.validate(60.0).unwrap_err().contains("\"b\" twice"));
  }

  #[test]
  fn migration_drops_the_whole_recording_slice_and_repeats() {
    let mut s = demo();
    s.slices.insert(
      LEGACY_WHOLE.into(),
      Slice {
        from: 0.0,
        to: 60.0,
      },
    );
    s.outputs[0].slices = ["all", "b", "all", "b", "a"].map(String::from).to_vec();
    s.outputs.push(Output {
      name: "render".into(),
      format: Format::Mp4,
      slices: vec!["all".into(), "all".into()],
      width: None,
      fps: None,
      colors: None,
      quality: None,
      dissolve_ms: 0,
    });
    assert!(s.migrate(60.0));
    assert!(!s.slices.contains_key(LEGACY_WHOLE));
    assert_eq!(s.outputs[0].slices, vec!["b", "a"]);
    assert!(s.outputs[1].slices.is_empty());
    assert!(s.validate(60.0).is_ok());
    assert!(!s.migrate(60.0));
  }

  #[test]
  fn migration_keeps_a_slice_named_all_that_is_only_part_of_the_recording() {
    let mut s = demo();
    s.slices
      .insert(LEGACY_WHOLE.into(), Slice { from: 5.0, to: 9.0 });
    s.outputs[0].slices = vec!["all".into(), "a".into()];
    assert!(!s.migrate(60.0));
    assert_eq!(s.outputs[0].slices, vec!["all", "a"]);
  }

  #[test]
  fn validation_rejects_unknown_names_empty_ranges_and_overruns() {
    let ok = demo();
    assert!(ok.validate(60.0).is_ok());

    let mut unknown = ok.clone();
    unknown.outputs[0].slices.push("nope".into());
    assert!(unknown.validate(60.0).unwrap_err().contains("nope"));

    let mut empty = ok.clone();
    empty
      .slices
      .insert("e".into(), Slice { from: 5.0, to: 5.0 });
    assert!(empty.validate(60.0).unwrap_err().contains("\"e\""));

    assert!(ok.validate(20.0).unwrap_err().contains("\"b\""));

    let mut whole = ok.clone();
    whole.outputs[0].slices.clear();
    assert!(whole.validate(60.0).is_ok());

    let mut twice = ok.clone();
    twice.outputs.push(twice.outputs[0].clone());
    assert!(twice.validate(60.0).unwrap_err().contains("demo"));
  }

  #[test]
  fn the_whole_projection_is_the_identity() {
    let t = timeline();
    let p = Projection::whole(&t);
    assert_eq!(p.duration(), t.out_duration());
    for i in 0..100 {
      let out = i as f64 * t.out_duration() / 100.0;
      assert_eq!(
        p.at(out),
        Place::One {
          window: 0,
          full_t: out
        }
      );
    }
  }

  #[test]
  fn a_slice_is_the_full_timeline_between_its_edges() {
    let t = timeline();
    let p = Projection::new(&t, &[19.0..23.0], 0.0);
    let w = &p.windows()[0];
    assert_eq!(w.full, t.out_time_at(19.0)..t.out_time_at(23.0));
    assert_eq!(w.start, 0.0);
    assert!((p.duration() - w.len()).abs() < 1e-9);
  }

  #[test]
  fn reordered_slices_play_end_to_end() {
    let t = timeline();
    let p = Projection::new(&t, &[19.0..23.0, 1.0..4.0], 0.0);
    let [b, a] = p.windows() else { panic!() };
    assert_eq!(a.start, b.len());
    match p.at(b.len() + 0.1) {
      Place::One { window: 1, full_t } => assert!((full_t - (a.full.start + 0.1)).abs() < 1e-9),
      other => panic!("{other:?}"),
    }
  }

  #[test]
  fn a_dissolve_overlaps_neighbours_and_shortens_the_output() {
    let t = timeline();
    let hard = Projection::new(&t, &[19.0..23.0, 1.0..4.0], 0.0);
    let soft = Projection::new(&t, &[19.0..23.0, 1.0..4.0], 0.4);
    assert!((hard.duration() - soft.duration() - 0.4).abs() < 1e-9);
    let join = soft.windows()[1].start;
    match soft.at(join + 0.2) {
      Place::Blend {
        to: 1,
        from_t,
        to_t,
        u,
      } => {
        assert!((u - 0.5).abs() < 1e-9);
        assert!((to_t - (soft.windows()[1].full.start + 0.2)).abs() < 1e-9);
        assert!((from_t - (soft.windows()[0].full.start + join + 0.2)).abs() < 1e-9);
      }
      other => panic!("{other:?}"),
    }
    assert!(matches!(soft.at(join + 0.41), Place::One { window: 1, .. }));
    assert!(matches!(soft.at(join - 0.01), Place::One { window: 0, .. }));
  }

  #[test]
  fn a_dissolve_is_capped_at_half_a_short_neighbour() {
    let t = timeline();
    let p = Projection::new(&t, &[19.0..23.0, 1.0..1.2], 5.0);
    let short = p.windows()[1].len();
    assert!((p.windows()[1].dissolve_in - short / 2.0).abs() < 1e-9);
  }

  #[test]
  fn chapters_project_into_the_slice_that_holds_them() {
    let t = timeline();
    let p = Projection::new(&t, &[19.0..23.0, 1.0..4.0], 0.0);
    let inside_a = t.out_time_at(2.0);
    let want = p.windows()[1].start + inside_a - p.windows()[1].full.start;
    assert!((p.project(inside_a).unwrap() - want).abs() < 1e-9);
    assert_eq!(p.project(t.out_time_at(30.0)), None);
  }

  proptest! {
    #[test]
    fn every_output_time_lands_inside_its_slice(
      a in 0.0f64..55.0, la in 0.5f64..5.0, b in 0.0f64..55.0, lb in 0.5f64..5.0,
      d in 0.0f64..1.0, f in 0.0f64..1.0,
    ) {
      let t = timeline();
      let ranges = [a..a + la, b..b + lb];
      let p = Projection::new(&t, &ranges, d);
      let out = f * p.duration();
      let inside = |w: usize, full_t: f64| {
        let win = &p.windows()[w];
        full_t >= win.full.start - 1e-9 && full_t <= win.full.end + 1e-9
      };
      match p.at(out) {
        Place::One { window, full_t } => prop_assert!(inside(window, full_t)),
        Place::Blend { to, from_t, to_t, u } => {
          prop_assert!(inside(to - 1, from_t));
          prop_assert!(inside(to, to_t));
          prop_assert!((0.0..=1.0).contains(&u));
        }
      }
    }
  }
}
