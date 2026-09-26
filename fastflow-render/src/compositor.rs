use fastflow_core::camera::{CameraTrack, ease_in_out_cubic};
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
  (timeline.out_duration() * fps as f64).ceil() as u64
}

/// One stretch of footage from one display. Its file's time 0 is source time `start`.
pub struct Segment<'a> {
  pub source: &'a mut dyn FrameSource,
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
    framing,
    overlay,
    start: 0.0,
  }];
  render_segments(&mut segments, sink, timeline, 0.0, settings, progress)
}

/// One forward pass over consecutive segments. Within `dissolve` output seconds centred on each
/// boundary, the outgoing and incoming segments are cross-dissolved. The outgoing side plays on
/// into the footage the two captures overlapped by, then holds its last frame. The incoming side
/// holds its first frame until the boundary.
pub fn render_segments(
  segments: &mut [Segment],
  sink: &mut dyn FrameSink,
  timeline: &Timeline,
  dissolve: f64,
  settings: RenderSettings,
  progress: &mut dyn FnMut(u64, u64),
) -> Result<u64> {
  let total = out_frame_count(timeline, settings.fps);
  let mut out = Frame::black(settings.size.0, settings.size.1);
  let mut incoming = Frame::black(settings.size.0, settings.size.1);
  let mut cropper = Cropper::default();
  let boundaries: Vec<f64> = segments
    .iter()
    .skip(1)
    .map(|s| timeline.out_time_at(s.start))
    .collect();
  for n in 0..total {
    let out_t = n as f64 / settings.fps as f64;
    let src_t = timeline.src_time_at(out_t);
    let blend = boundaries.iter().enumerate().find_map(|(i, &b)| {
      let into = out_t - (b - dissolve / 2.0);
      (dissolve > 0.0 && (0.0..dissolve).contains(&into)).then_some((i, into / dissolve))
    });
    match blend {
      None => {
        let k = segments
          .partition_point(|s| s.start <= src_t)
          .saturating_sub(1);
        compose(&mut segments[k], out_t, src_t, &mut cropper, &mut out)?;
      }
      Some((i, u)) => {
        let boundary = segments[i + 1].start;
        compose(&mut segments[i], out_t, src_t, &mut cropper, &mut out)?;
        compose(
          &mut segments[i + 1],
          out_t,
          src_t.max(boundary),
          &mut cropper,
          &mut incoming,
        )?;
        mix(&mut out, &incoming, ease_in_out_cubic(u));
      }
    }
    sink.write(&out)?;
    progress(n + 1, total);
  }
  Ok(total)
}

fn compose(
  seg: &mut Segment,
  out_t: f64,
  src_t: f64,
  cropper: &mut Cropper,
  out: &mut Frame,
) -> Result<()> {
  let frame = seg.source.advance_to((src_t - seg.start).max(0.0))?;
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
        framing: Framing::Prefitted,
        overlay: None,
        start: 0.0,
      },
      Segment {
        source: &mut b,
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

  #[test]
  fn decoding_is_a_single_forward_pass() {
    let (_, src, indices) = run(&[3.0, 11.0], 20.0);
    assert!(indices.windows(2).all(|w| w[1] >= w[0]));
    assert_eq!(src.decoded(), indices.last().unwrap() + 1);
    assert_eq!(src.seeks(), 0);
  }
}
