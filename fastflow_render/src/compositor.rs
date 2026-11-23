use fastflow_core::camera::{CameraTrack, ease_in_out_cubic};
use fastflow_core::slices::{Place, Projection};
use fastflow_core::timeline::Timeline;

use crate::crop::Cropper;
use crate::{Frame, FrameSink, FrameSource, RenderError, Result};

pub enum Framing<'a> {
  /// The source already delivers whole frames at the output size.
  Prefitted,
  /// The source delivers native frames and the camera crops them.
  Camera(&'a CameraTrack),
}

/// Draws onto each output frame before it is written. Arguments are output and source time.
pub type Overlay<'a> = &'a mut dyn FnMut(&mut Frame, f64, f64) -> Result<()>;

#[derive(Debug, Clone, Copy)]
pub struct RenderSettings {
  pub size: (u32, u32),
  pub fps: u32,
}

pub fn out_frame_count(timeline: &Timeline, fps: u32) -> u64 {
  frames_in(timeline.out_duration(), fps)
}

/// A duration a rounding error past a whole frame does not get an extra frame.
fn frames_in(seconds: f64, fps: u32) -> u64 {
  (seconds * fps as f64 - 1e-6).ceil().max(0.0) as u64
}

/// One stretch of footage from one display. Its file's time 0 is source time `start`.
pub struct Segment<'a> {
  pub source: &'a mut dyn FrameSource,
  /// A second decoder of the same file. A dissolve between two slices of this segment needs
  /// two positions at once.
  pub spare: Option<&'a mut dyn FrameSource>,
  pub framing: Framing<'a>,
  pub overlay: Option<Overlay<'a>>,
  pub start: f64,
}

/// One forward pass over a single source.
pub fn render<'a>(
  src: &'a mut dyn FrameSource,
  sink: &mut dyn FrameSink,
  timeline: &Timeline,
  framing: Framing<'a>,
  overlay: Option<Overlay<'a>>,
  settings: RenderSettings,
  progress: &mut dyn FnMut(u64, u64),
) -> Result<u64> {
  let mut segments = [Segment {
    source: src,
    spare: None,
    framing,
    overlay,
    start: 0.0,
  }];
  render_segments(&mut segments, sink, timeline, 0.0, settings, progress)
}

pub fn projection_frame_count(projection: &Projection, fps: u32) -> u64 {
  frames_in(projection.duration(), fps)
}

/// A slice starting further ahead of its decoder than this restarts the decoder instead of
/// decoding through the gap.
const JUMP: f64 = 2.0;

#[derive(Debug, Clone, Copy, Default)]
struct Cursor {
  /// Seconds into the segment's file.
  local: f64,
  window: Option<usize>,
}

/// Where each segment's two decoders are and which one plays the current slice.
struct Decoders {
  cursors: Vec<[Cursor; 2]>,
  active: Vec<usize>,
  swap: Vec<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
  Main,
  /// The slice fading in at a join. It reads a segment's spare decoder when it has one.
  Incoming,
}

struct Switches<'t> {
  timeline: &'t Timeline,
  boundaries: Vec<f64>,
  dissolve: f64,
}

impl Switches<'_> {
  fn segment_at(&self, segments: &[Segment], full_t: f64) -> usize {
    let src_t = self.timeline.src_time_at(full_t);
    segments
      .partition_point(|s| s.start <= src_t)
      .saturating_sub(1)
  }

  fn blend_at(&self, full_t: f64) -> Option<(usize, f64)> {
    let d = self.dissolve;
    self.boundaries.iter().enumerate().find_map(|(i, &b)| {
      let into = full_t - (b - d / 2.0);
      (d > 0.0 && (0.0..d).contains(&into)).then_some((i, into / d))
    })
  }
}

/// Renders the output `projection` lays out. Times inside a slice are full-timeline output
/// times, so pacing, the camera, focus and display dissolves are those of the whole recording.
/// Each slice decodes forward from its start. Within `dissolve` output seconds centred on a
/// display boundary, the two segments are cross-dissolved.
pub fn render_projection(
  segments: &mut [Segment],
  sink: &mut dyn FrameSink,
  timeline: &Timeline,
  projection: &Projection,
  dissolve: f64,
  settings: RenderSettings,
  progress: &mut dyn FnMut(u64, u64),
) -> Result<u64> {
  let total = projection_frame_count(projection, settings.fps);
  let blank = || Frame::black(settings.size.0, settings.size.1);
  let (mut out, mut scratch, mut incoming, mut scratch_in) = (blank(), blank(), blank(), blank());
  let mut cropper = Cropper::default();
  let switches = Switches {
    timeline,
    boundaries: segments
      .iter()
      .skip(1)
      .map(|s| timeline.out_time_at(s.start))
      .collect(),
    dissolve,
  };
  let mut dec = Decoders {
    cursors: vec![[Cursor::default(); 2]; segments.len()],
    active: vec![0; segments.len()],
    swap: vec![false; segments.len()],
  };
  for n in 0..total {
    let t = n as f64 / settings.fps as f64;
    match projection.at(t) {
      Place::One { window, full_t } => {
        for k in 0..segments.len() {
          if std::mem::take(&mut dec.swap[k]) {
            dec.active[k] = 1 - dec.active[k];
          }
        }
        draw(
          segments,
          &mut dec,
          &switches,
          Side::Main,
          window,
          full_t,
          &mut cropper,
          &mut out,
          &mut scratch,
        )?;
      }
      Place::Blend {
        to,
        from_t,
        to_t,
        u,
      } => {
        draw(
          segments,
          &mut dec,
          &switches,
          Side::Main,
          to - 1,
          from_t,
          &mut cropper,
          &mut out,
          &mut scratch,
        )?;
        let k_from = switches.segment_at(segments, from_t);
        let k_to = switches.segment_at(segments, to_t);
        if k_from != k_to || segments[k_to].spare.is_some() {
          draw(
            segments,
            &mut dec,
            &switches,
            Side::Incoming,
            to,
            to_t,
            &mut cropper,
            &mut incoming,
            &mut scratch_in,
          )?;
          mix(&mut out, &incoming, ease_in_out_cubic(u));
        }
      }
    }
    sink.write(&out)?;
    progress(n + 1, total);
  }
  Ok(total)
}

/// One forward pass over consecutive segments.
pub fn render_segments(
  segments: &mut [Segment],
  sink: &mut dyn FrameSink,
  timeline: &Timeline,
  dissolve: f64,
  settings: RenderSettings,
  progress: &mut dyn FnMut(u64, u64),
) -> Result<u64> {
  let whole = Projection::whole(timeline);
  render_projection(
    segments, sink, timeline, &whole, dissolve, settings, progress,
  )
}

/// One side of a frame at a full-timeline time, including a display dissolve. The outgoing
/// display plays on into the footage the two captures overlapped by, then holds its last
/// frame. The incoming display holds its first frame until the boundary.
#[allow(clippy::too_many_arguments)]
fn draw(
  segments: &mut [Segment],
  dec: &mut Decoders,
  switches: &Switches,
  side: Side,
  window: usize,
  full_t: f64,
  cropper: &mut Cropper,
  out: &mut Frame,
  scratch: &mut Frame,
) -> Result<()> {
  let src_t = switches.timeline.src_time_at(full_t);
  match switches.blend_at(full_t) {
    None => {
      let k = switches.segment_at(segments, full_t);
      compose(segments, dec, k, side, window, full_t, src_t, cropper, out)
    }
    Some((i, u)) => {
      let boundary = segments[i + 1].start;
      compose(segments, dec, i, side, window, full_t, src_t, cropper, out)?;
      compose(
        segments,
        dec,
        i + 1,
        side,
        window,
        full_t,
        src_t.max(boundary),
        cropper,
        scratch,
      )?;
      mix(out, scratch, ease_in_out_cubic(u));
      Ok(())
    }
  }
}

#[allow(clippy::too_many_arguments)]
fn compose(
  segments: &mut [Segment],
  dec: &mut Decoders,
  k: usize,
  side: Side,
  window: usize,
  out_t: f64,
  src_t: f64,
  cropper: &mut Cropper,
  out: &mut Frame,
) -> Result<()> {
  let seg = &mut segments[k];
  let idx = match side {
    Side::Incoming if seg.spare.is_some() => {
      dec.swap[k] = true;
      1 - dec.active[k]
    }
    _ => dec.active[k],
  };
  let local = (src_t - seg.start).max(0.0);
  let cursor = &mut dec.cursors[k][idx];
  let source: &mut dyn FrameSource = match (idx, seg.spare.as_deref_mut()) {
    (1, Some(spare)) => spare,
    _ => &mut *seg.source,
  };
  let behind = local < cursor.local - 1e-9;
  let far_ahead = cursor.window != Some(window) && local > cursor.local + JUMP;
  if behind || far_ahead {
    source.seek(local)?;
  }
  *cursor = Cursor {
    local,
    window: Some(window),
  };
  let frame = source.advance_to(local)?;
  match &seg.framing {
    Framing::Prefitted => {
      check_size(frame, (out.width, out.height))?;
      out.data.copy_from_slice(&frame.data);
    }
    Framing::Camera(camera) => cropper.crop_into(frame, camera.rect_at(out_t), out)?,
  }
  if let Some(draw) = seg.overlay.as_mut() {
    draw(out, out_t, src_t)?;
  }
  Ok(())
}

/// `a` becomes `a * (1 - k) + b * k`.
fn mix(a: &mut Frame, b: &Frame, k: f64) {
  let k = (k.clamp(0.0, 1.0) * 256.0) as u32;
  for (x, &y) in a.data.iter_mut().zip(&b.data) {
    *x = ((*x as u32 * (256 - k) + y as u32 * k) >> 8) as u8;
  }
}

fn check_size(frame: &Frame, size: (u32, u32)) -> Result<()> {
  if (frame.width, frame.height) == size {
    Ok(())
  } else {
    Err(RenderError::Decode(format!(
      "source frame is {}x{}, output is {}x{}",
      frame.width, frame.height, size.0, size.1
    )))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::synthetic::{RecordingSink, SyntheticSource};
  use fastflow_core::config::PacingConfig;

  const FPS: u32 = 60;

  fn run(events: &[f64], duration: f64) -> (Timeline, SyntheticSource, Vec<u64>) {
    let timeline = Timeline::build(events, duration, &PacingConfig::default());
    let mut src = SyntheticSource::new((8, 4), FPS as f64, (duration * FPS as f64) as u64);
    let mut sink = RecordingSink::default();
    render(
      &mut src,
      &mut sink,
      &timeline,
      Framing::Prefitted,
      None,
      RenderSettings {
        size: (8, 4),
        fps: FPS,
      },
      &mut |_, _| {},
    )
    .unwrap();
    (timeline, src, sink.indices)
  }

  #[test]
  fn each_output_frame_carries_the_source_frame_the_timeline_names() {
    let (timeline, _, indices) = run(&[2.0, 2.4, 9.0], 20.0);
    assert_eq!(indices.len() as u64, out_frame_count(&timeline, FPS));
    for (n, &got) in indices.iter().enumerate() {
      let src_t = timeline.src_time_at(n as f64 / FPS as f64);
      let want = ((src_t * FPS as f64 + 1e-9).floor() as u64).min(20 * 60 - 1);
      assert_eq!(got, want, "output frame {n}");
    }
  }

  #[test]
  fn human_span_plays_frame_for_frame() {
    let (timeline, _, indices) = run(&[5.0], 20.0);
    let human = timeline
      .spans()
      .iter()
      .position(|s| s.kind == fastflow_core::timeline::SpanKind::Human)
      .unwrap();
    let start: f64 = timeline.spans()[..human].iter().map(|s| s.out).sum();
    let first = (start * FPS as f64).ceil() as usize;
    let run: Vec<u64> = indices[first..first + 30].to_vec();
    assert!(run.windows(2).all(|w| w[1] == w[0] + 1), "{run:?}");
  }

  #[test]
  fn a_boundary_dissolves_from_one_segment_to_the_next() {
    let timeline = Timeline::build(&[0.0, 1.0, 2.0, 3.0, 4.0], 4.0, &PacingConfig::default());
    let mut a = SyntheticSource::new((8, 4), FPS as f64, 150).filled(0);
    let mut b = SyntheticSource::new((8, 4), FPS as f64, 150).filled(200);
    let mut sink = RecordingSink::default();
    let mut segments = [
      Segment {
        source: &mut a,
        spare: None,
        framing: Framing::Prefitted,
        overlay: None,
        start: 0.0,
      },
      Segment {
        source: &mut b,
        spare: None,
        framing: Framing::Prefitted,
        overlay: None,
        start: 2.0,
      },
    ];
    render_segments(
      &mut segments,
      &mut sink,
      &timeline,
      0.4,
      RenderSettings {
        size: (8, 4),
        fps: FPS,
      },
      &mut |_, _| {},
    )
    .unwrap();
    let at = |t: f64| sink.fills[(timeline.out_time_at(t) * FPS as f64) as usize];
    assert_eq!(at(1.0), 0);
    assert_eq!(at(3.0), 200);
    let mid = at(2.0);
    assert!((60..=140).contains(&mid), "{mid}");
    let series: Vec<u8> = sink.fills.clone();
    assert!(
      series.windows(2).all(|w| w[1] >= w[0]),
      "dissolve should only rise"
    );
  }

  fn human(duration: f64) -> Timeline {
    let events: Vec<f64> = (0..(duration * 2.0) as usize)
      .map(|i| i as f64 * 0.5)
      .collect();
    Timeline::build(&events, duration, &PacingConfig::default())
  }

  fn render_slices(
    timeline: &Timeline,
    a: &mut SyntheticSource,
    spare: Option<&mut SyntheticSource>,
    ranges: &[std::ops::Range<f64>],
    dissolve: f64,
  ) -> RecordingSink {
    let projection = Projection::new(timeline, ranges, dissolve);
    let mut sink = RecordingSink::default();
    let mut segments = [Segment {
      source: a,
      spare: spare.map(|s| s as &mut dyn FrameSource),
      framing: Framing::Prefitted,
      overlay: None,
      start: 0.0,
    }];
    render_projection(
      &mut segments,
      &mut sink,
      timeline,
      &projection,
      0.0,
      RenderSettings {
        size: (8, 4),
        fps: FPS,
      },
      &mut |_, _| {},
    )
    .unwrap();
    assert_eq!(
      sink.indices.len() as u64,
      projection_frame_count(&projection, FPS)
    );
    sink
  }

  #[test]
  fn reordered_slices_each_play_forward_from_their_start() {
    let timeline = human(6.0);
    let mut src = SyntheticSource::new((8, 4), FPS as f64, 6 * 60);
    let sink = render_slices(&timeline, &mut src, None, &[3.0..4.0, 1.0..2.0], 0.0);
    let want: Vec<u64> = (180..240).chain(60..120).collect();
    assert_eq!(sink.indices, want);
    assert_eq!(src.seeks(), 2);
  }

  #[test]
  fn nearby_slices_in_order_decode_without_seeking() {
    let timeline = human(6.0);
    let mut src = SyntheticSource::new((8, 4), FPS as f64, 6 * 60);
    let sink = render_slices(&timeline, &mut src, None, &[1.0..2.0, 2.5..3.0], 0.0);
    let want: Vec<u64> = (60..120).chain(150..180).collect();
    assert_eq!(sink.indices, want);
    assert_eq!(src.seeks(), 0);
  }

  #[test]
  fn a_dissolve_within_one_segment_fades_into_the_spare_decoder() {
    let timeline = human(6.0);
    let mut a = SyntheticSource::new((8, 4), FPS as f64, 6 * 60).filled(0);
    let mut spare = SyntheticSource::new((8, 4), FPS as f64, 6 * 60).filled(200);
    let sink = render_slices(
      &timeline,
      &mut a,
      Some(&mut spare),
      &[1.0..2.0, 4.0..5.0],
      0.4,
    );
    assert_eq!(sink.indices.len(), 60 + 60 - 24);
    assert!(sink.fills[..36].iter().all(|&f| f == 0), "{:?}", sink.fills);
    let mid = sink.fills[36 + 12];
    assert!((60..=140).contains(&mid), "{mid}");
    assert!(
      sink.fills[60..].iter().all(|&f| f == 200),
      "{:?}",
      sink.fills
    );
    assert!(sink.fills[36..60].windows(2).all(|w| w[1] >= w[0]));
    assert_eq!(sink.indices[60], 4 * 60 + 24);
    assert_eq!(*sink.indices.last().unwrap(), 5 * 60 - 1);
    assert_eq!((a.seeks(), spare.seeks()), (0, 1));
  }

  #[test]
  fn decoding_is_a_single_forward_pass() {
    let (_, src, indices) = run(&[3.0, 11.0], 20.0);
    assert!(indices.windows(2).all(|w| w[1] >= w[0]));
    assert_eq!(src.decoded(), indices.last().unwrap() + 1);
    assert_eq!(src.seeks(), 0);
  }
}
