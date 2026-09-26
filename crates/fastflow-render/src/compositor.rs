use fastflow_core::camera::CameraTrack;
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

/// One forward pass over the source.
pub fn render(
    src: &mut dyn FrameSource,
    sink: &mut dyn FrameSink,
    timeline: &Timeline,
    framing: Framing,
    mut overlay: Option<Overlay>,
    settings: RenderSettings,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<u64> {
    let total = out_frame_count(timeline, settings.fps);
    let mut out = Frame::black(settings.size.0, settings.size.1);
    let mut cropper = Cropper::default();
    for n in 0..total {
        let out_t = n as f64 / settings.fps as f64;
        let src_t = timeline.src_time_at(out_t);
        let frame = src.advance_to(src_t)?;
        match (&framing, overlay.as_mut()) {
            (Framing::Prefitted, None) => {
                check_size(frame, settings.size)?;
                sink.write(frame)?;
            }
            (Framing::Prefitted, Some(draw)) => {
                check_size(frame, settings.size)?;
                out.data.copy_from_slice(&frame.data);
                draw(&mut out, out_t, src_t)?;
                sink.write(&out)?;
            }
            (Framing::Camera(camera), draw) => {
                cropper.crop_into(frame, camera.rect_at(out_t), &mut out)?;
                if let Some(draw) = draw {
                    draw(&mut out, out_t, src_t)?;
                }
                sink.write(&out)?;
            }
        }
        progress(n + 1, total);
    }
    Ok(total)
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
    fn decoding_is_a_single_forward_pass() {
        let (_, src, indices) = run(&[3.0, 11.0], 20.0);
        assert!(indices.windows(2).all(|w| w[1] >= w[0]));
        assert_eq!(src.decoded(), indices.last().unwrap() + 1);
        assert_eq!(src.seeks(), 0);
    }
}
