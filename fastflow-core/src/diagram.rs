//! An ascii strip of what the tracks decided, over source time. A pad that merged two spans or a
//! debounce that fired at the wrong moment is obvious here and nearly invisible in the video.

use crate::camera::CameraTrack;
use crate::markers::Markers;
use crate::timeline::{SpanKind, Timeline};

const LABEL: usize = 8;
const TICK_EVERY: usize = 10;

pub struct Inputs<'a> {
  pub timeline: &'a Timeline,
  /// Each segment's source start and camera track, in order.
  pub cameras: &'a [(f64, &'a CameraTrack)],
  /// Human input times in source seconds.
  pub events: &'a [f64],
  pub markers: &'a Markers,
  pub duration: f64,
}

fn column(t: f64, duration: f64, width: usize) -> usize {
  ((t / duration * width as f64) as usize).min(width - 1)
}

fn row(label: &str, cells: &[char]) -> String {
  let mut s = format!("{label:<LABEL$}");
  s.extend(cells);
  s.trim_end().to_owned()
}

/// `width` is the number of columns for the strip itself, after the row labels.
pub fn render(i: &Inputs, width: usize) -> String {
  let width = width.max(TICK_EVERY);
  let d = i.duration.max(f64::MIN_POSITIVE);
  let at = |c: usize| (c as f64 + 0.5) / width as f64 * d;
  let mut lines = Vec::new();

  let mut ticks = vec![' '; width];
  for c in (0..width).step_by(TICK_EVERY) {
    for (k, ch) in format!("{:.0}s", c as f64 / width as f64 * d)
      .chars()
      .enumerate()
    {
      if c + k < width {
        ticks[c + k] = ch;
      }
    }
  }
  lines.push(row("source", &ticks));

  let mut input = vec![' '; width];
  for &e in i.events.iter().filter(|&&e| e >= 0.0 && e < d) {
    input[column(e, d, width)] = '█';
  }
  lines.push(row("input", &input));

  let m = i.markers;
  if !(m.keep.is_empty() && m.cut.is_empty() && m.frame.is_empty() && m.chapters.is_empty()) {
    let mut marks = vec![' '; width];
    for (ranges, ch) in [(&m.keep, 'K'), (&m.cut, 'X'), (&m.frame, 'F')] {
      for (c, cell) in marks.iter_mut().enumerate() {
        if ranges.iter().any(|r| r.contains(&at(c))) {
          *cell = ch;
        }
      }
    }
    for &c in &m.chapters {
      marks[column(c, d, width)] = '|';
    }
    lines.push(row("marks", &marks));
  }

  let pacing: Vec<char> = (0..width)
    .map(|c| {
      let t = at(c);
      let span = i
        .timeline
        .spans()
        .iter()
        .find(|s| s.src.start <= t && t < s.src.end);
      match span.map(|s| s.kind) {
        Some(SpanKind::Human) => '█',
        Some(SpanKind::Ramp) => '▒',
        Some(SpanKind::Idle) => '░',
        _ => ' ',
      }
    })
    .collect();
  lines.push(row("pacing", &pacing));

  let mut camera = vec!['─'; width];
  let mut names: Vec<u32> = Vec::new();
  for (k, &(seg_start, track)) in i.cameras.iter().enumerate() {
    let seg_end = i.cameras.get(k + 1).map_or(d, |c| c.0);
    let in_segment = |t: f64| t >= seg_start && t < seg_end;
    for (start, dur) in track.moves() {
      let (a, b) = (
        i.timeline.src_time_at(start),
        i.timeline.src_time_at(start + dur),
      );
      if !in_segment(a) {
        continue;
      }
      let (a, b) = (column(a, d, width), column(b.min(seg_end), d, width));
      for cell in &mut camera[a..=b.max(a)] {
        *cell = '~';
      }
    }
    for (start, id) in track.focus_changes() {
      let t = i.timeline.src_time_at(start).max(seg_start);
      if !in_segment(t) {
        continue;
      }
      if !names.contains(&id) {
        names.push(id);
      }
      let letter = (b'A' + (names.iter().position(|&n| n == id).unwrap() % 26) as u8) as char;
      camera[column(t, d, width)] = letter;
    }
  }
  for &(_, track) in i.cameras {
    for &(a, b) in track.overviews() {
      let a = column(i.timeline.src_time_at(a), d, width);
      let b = column(
        i.timeline.src_time_at(b.min(i.timeline.out_duration())),
        d,
        width,
      );
      for cell in &mut camera[a..=b.max(a)] {
        *cell = 'M';
      }
    }
  }
  for &(seg_start, _) in i.cameras.iter().skip(1) {
    camera[column(seg_start, d, width)] = '┃';
  }
  lines.push(row("camera", &camera));

  lines.push(String::new());
  lines.push(format!(
        "{:<LABEL$}█ human  ▒ ramp  ░ idle   camera: A.. window, ~ moving, M mission control, ┃ display switch",
        ""
    ));
  lines.push(format!(
    "{:<LABEL$}source {:.1}s -> output {:.1}s, {} camera moves, {} segments",
    "",
    d,
    i.timeline.out_duration(),
    i.cameras.iter().map(|c| c.1.move_count()).sum::<usize>(),
    i.cameras.len()
  ));
  lines.join("\n")
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::camera::{CameraTrack, Geometry};
  use crate::config::PacingConfig;

  #[test]
  fn marks_human_idle_and_input_columns() {
    let t = Timeline::build(&[10.0, 10.5], 40.0, &PacingConfig::default());
    let geo = Geometry {
      surface_px: (3600.0, 2338.0),
      surface_pt: (1800.0, 1169.0),
      output_px: (1920.0, 1080.0),
    };
    let cam = CameraTrack::fixed(&geo);
    let out = render(
      &Inputs {
        timeline: &t,
        cameras: &[(0.0, &cam)],
        events: &[10.0, 10.5],
        markers: &Markers::default(),
        duration: 40.0,
      },
      40,
    );
    let lines: Vec<&str> = out.lines().collect();
    let strip = |name: &str| -> Vec<char> {
      let l = lines.iter().find(|l| l.starts_with(name)).unwrap();
      l.chars().skip(LABEL).collect()
    };
    assert_eq!(strip("input")[10], '█');
    assert_eq!(strip("pacing")[10], '█');
    assert_eq!(strip("pacing")[30], '░');
    assert!(!out.contains("marks"));
    assert!(out.contains("source 40.0s ->"));
  }
}
