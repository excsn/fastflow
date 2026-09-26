//! The framing model. See docs/04-tracks.md.
//!
//! Rects are normalized to the captured surface. The camera rect always has the output's pixel
//! aspect, so it can be cropped and resized without distortion.

use crate::config::CameraConfig;
use crate::geom::{Point, Rect};
use crate::recording::{SegmentInfo, WindowInfo, WindowSample};
use crate::timeline::Timeline;

/// Captured surface and output sizes in pixels, plus the surface's size in points.
#[derive(Debug, Clone, Copy)]
pub struct Geometry {
    pub surface_px: (f64, f64),
    pub surface_pt: (f64, f64),
    pub output_px: (f64, f64),
}

impl Geometry {
    pub fn new(segment: &SegmentInfo, output: [u32; 2]) -> Geometry {
        Geometry {
            surface_px: (segment.surface_px[0] as f64, segment.surface_px[1] as f64),
            surface_pt: (segment.surface_pt[0], segment.surface_pt[1]),
            output_px: (output[0] as f64, output[1] as f64),
        }
    }

    fn aspect(&self) -> f64 {
        self.output_px.0 / self.output_px.1
    }

    /// Grows the shorter side so the rect has the output's pixel aspect, keeping its centre.
    pub fn fit_aspect(&self, r: Rect) -> Rect {
        let (sw, sh) = self.surface_px;
        let (w_px, h_px) = (r.w * sw, r.h * sh);
        let (w_px, h_px) = if w_px / h_px < self.aspect() {
            (h_px * self.aspect(), h_px)
        } else {
            (w_px, w_px / self.aspect())
        };
        centred(r, w_px / sw, h_px / sh)
    }

    /// The whole surface, fitted to the output aspect.
    pub fn full(&self) -> Rect {
        self.fit_aspect(Rect {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
        })
    }

    /// Target camera rect for a window: padded, fitted to the output aspect, no tighter than
    /// `max_zoom` allows, then moved onto the surface.
    pub fn frame(&self, window: Rect, cfg: &CameraConfig) -> Rect {
        let pad_x = cfg.window_pad / self.surface_pt.0;
        let pad_y = cfg.window_pad / self.surface_pt.1;
        let padded = Rect {
            x: window.x - pad_x,
            y: window.y - pad_y,
            w: window.w + 2.0 * pad_x,
            h: window.h + 2.0 * pad_y,
        };
        let r = self.fit_aspect(padded);
        let min_w = self.output_px.0 / cfg.max_zoom.max(1.0) / self.surface_px.0;
        let r = if r.w < min_w {
            let k = min_w / r.w;
            centred(r, r.w * k, r.h * k)
        } else {
            r
        };
        clamp_to_surface(r)
    }
}

fn centred(r: Rect, w: f64, h: f64) -> Rect {
    Rect {
        x: r.x + (r.w - w) / 2.0,
        y: r.y + (r.h - h) / 2.0,
        w,
        h,
    }
}

/// Moves the rect onto the unit surface without resizing it. A side longer than the surface is
/// centred instead, which the compositor fills with black.
pub fn clamp_to_surface(r: Rect) -> Rect {
    let axis = |pos: f64, len: f64| {
        if len >= 1.0 {
            (1.0 - len) / 2.0
        } else {
            pos.clamp(0.0, 1.0 - len)
        }
    };
    Rect {
        x: axis(r.x, r.w),
        y: axis(r.y, r.h),
        w: r.w,
        h: r.h,
    }
}

fn contains(r: &Rect, p: &Point) -> bool {
    p.x >= r.x && p.x < r.x + r.w && p.y >= r.y && p.y < r.y + r.h
}

/// Intersection over union. Measuring only how much of the new rect lies inside the old one
/// would let a wide shot hold forever, since every smaller window is inside it.
fn overlap(a: &Rect, b: &Rect) -> f64 {
    let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
    let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
    if w <= 0.0 || h <= 0.0 {
        return 0.0;
    }
    let i = w * h;
    i / (a.w * a.h + b.w * b.h - i)
}

/// The topmost normal window under the cursor, ignoring windows below `min_window_size`.
/// `windows` is front to back.
pub fn subject<'a>(
    sample: &'a WindowSample,
    geo: &Geometry,
    cfg: &CameraConfig,
) -> Option<&'a WindowInfo> {
    let min_w = cfg.min_window_size[0] / geo.surface_pt.0;
    let min_h = cfg.min_window_size[1] / geo.surface_pt.1;
    sample.windows.iter().find(|w| {
        w.layer == 0 && w.rect.w >= min_w && w.rect.h >= min_h && contains(&w.rect, &sample.cursor)
    })
}

#[derive(Debug, Clone, PartialEq)]
struct Move {
    start: f64,
    duration: f64,
    from: Rect,
    control: Rect,
    to: Rect,
}

/// A switch of the focused window, which the render crossfades over `duration`.
#[derive(Debug, Clone, PartialEq)]
struct FocusChange {
    start: f64,
    duration: f64,
    from: Option<u32>,
    to: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CameraTrack {
    initial: Rect,
    moves: Vec<Move>,
    initial_focus: Option<u32>,
    /// Every commit to a different window, including those the overlap hold kept the camera
    /// still for.
    focus: Vec<FocusChange>,
}

/// The rect of window `id` in the latest sample at or before `src_ms` that contains it.
/// `samples` must be in time order.
pub fn window_rect_at(samples: &[WindowSample], id: u32, src_ms: i64) -> Option<Rect> {
    let end = samples.partition_point(|s| s.t <= src_ms);
    samples[..end]
        .iter()
        .rev()
        .find_map(|s| s.windows.iter().find(|w| w.id == id).map(|w| w.rect))
}

/// Two window rects closer than this, per edge, count as the same framing target.
const SAME_RECT: f64 = 0.01;

fn same_rect(a: &Rect, b: &Rect) -> bool {
    (a.x - b.x).abs() < SAME_RECT
        && (a.y - b.y).abs() < SAME_RECT
        && (a.w - b.w).abs() < SAME_RECT
        && (a.h - b.h).abs() < SAME_RECT
}

fn lerp(a: f64, b: f64, c: f64, s: f64) -> f64 {
    (1.0 - s) * (1.0 - s) * a + 2.0 * (1.0 - s) * s * b + s * s * c
}

pub fn ease_in_out_cubic(u: f64) -> f64 {
    if u < 0.5 {
        4.0 * u * u * u
    } else {
        1.0 - (-2.0 * u + 2.0).powi(3) / 2.0
    }
}

impl Move {
    fn rect_at(&self, out_t: f64) -> Rect {
        let u = ((out_t - self.start) / self.duration).clamp(0.0, 1.0);
        let s = ease_in_out_cubic(u);
        let (a, c, b) = (&self.from, &self.control, &self.to);
        Rect {
            x: lerp(a.x, c.x, b.x, s),
            y: lerp(a.y, c.y, b.y, s),
            w: lerp(a.w, c.w, b.w, s),
            h: lerp(a.h, c.h, b.h, s),
        }
    }
}

impl CameraTrack {
    /// A camera that never moves off the whole surface.
    pub fn fixed(geo: &Geometry) -> CameraTrack {
        CameraTrack {
            initial: geo.full(),
            moves: Vec::new(),
            initial_focus: None,
            focus: Vec::new(),
        }
    }

    /// `samples` must be in time order. Candidates are debounced in source time, since dwell is
    /// what shows intent. Moves are placed and timed in output time, so a transition takes
    /// `transition_ms` on screen however fast the source is playing.
    pub fn build(
        samples: &[WindowSample],
        timeline: &Timeline,
        geo: &Geometry,
        cfg: &CameraConfig,
    ) -> CameraTrack {
        let mut track = CameraTrack::fixed(geo);
        let commit = cfg.commit_ms as f64 / 1000.0;
        let transition = cfg.transition_ms as f64 / 1000.0;

        let mut committed: Option<(u32, Rect)> = None;
        let mut candidate: Option<(u32, Rect, f64)> = None;

        for sample in samples {
            let t = sample.t as f64 / 1000.0;
            let Some(w) = subject(sample, geo, cfg) else {
                candidate = None;
                continue;
            };
            let same_as = |c: &(u32, Rect)| c.0 == w.id && same_rect(&c.1, &w.rect);
            if committed.as_ref().is_some_and(same_as) {
                candidate = None;
                continue;
            }
            let since = match candidate {
                Some((id, r, since)) if id == w.id && same_rect(&r, &w.rect) => since,
                _ => {
                    candidate = Some((w.id, w.rect, t));
                    t
                }
            };

            let Some((previous, _)) = committed else {
                track.initial = geo.frame(w.rect, cfg);
                track.initial_focus = Some(w.id);
                committed = Some((w.id, w.rect));
                candidate = None;
                continue;
            };
            if t - since < commit {
                continue;
            }

            committed = Some((w.id, w.rect));
            candidate = None;
            let at = timeline.out_time_at(since + commit);
            if previous != w.id {
                track.focus.push(FocusChange {
                    start: at,
                    duration: transition,
                    from: Some(previous),
                    to: w.id,
                });
            }
            let from = track.rect_at(at);
            let to = geo.frame(w.rect, cfg);
            if overlap(&to, &from) >= cfg.overlap_hold {
                continue;
            }
            let union = Rect {
                x: from.x.min(to.x),
                y: from.y.min(to.y),
                w: (from.x + from.w).max(to.x + to.w) - from.x.min(to.x),
                h: (from.y + from.h).max(to.y + to.h) - from.y.min(to.y),
            };
            let control = geo.fit_aspect(centred(
                union,
                union.w * cfg.pull_back,
                union.h * cfg.pull_back,
            ));
            track.moves.retain(|m| m.start < at);
            track.moves.push(Move {
                start: at,
                duration: transition,
                from,
                control,
                to,
            });
        }
        track
    }

    /// Clamped to the surface, so a pull-back never asks for pixels outside it.
    pub fn rect_at(&self, out_t: f64) -> Rect {
        let i = self.moves.partition_point(|m| m.start <= out_t);
        let r = match i.checked_sub(1) {
            None => self.initial,
            Some(i) => self.moves[i].rect_at(out_t),
        };
        clamp_to_surface(r)
    }

    pub fn move_count(&self) -> usize {
        self.moves.len()
    }

    /// Focused windows and how focused each is, 0 to 1. Two entries mid-crossfade, none before
    /// any window was chosen.
    pub fn focus_at(&self, out_t: f64) -> Vec<(u32, f64)> {
        let i = self.focus.partition_point(|c| c.start <= out_t);
        let Some(c) = i.checked_sub(1).map(|i| &self.focus[i]) else {
            return self.initial_focus.map(|id| (id, 1.0)).into_iter().collect();
        };
        let u = ((out_t - c.start) / c.duration).clamp(0.0, 1.0);
        if u >= 1.0 {
            return vec![(c.to, 1.0)];
        }
        let s = ease_in_out_cubic(u);
        let mut out = vec![(c.to, s)];
        if let Some(from) = c.from {
            out.push((from, 1.0 - s));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PacingConfig;

    fn geo() -> Geometry {
        Geometry {
            surface_px: (3600.0, 2338.0),
            surface_pt: (1800.0, 1169.0),
            output_px: (1920.0, 1080.0),
        }
    }

    fn cfg() -> CameraConfig {
        CameraConfig::default()
    }

    fn window(id: u32, x: f64, y: f64, w: f64, h: f64) -> WindowInfo {
        WindowInfo {
            id,
            pid: 1,
            layer: 0,
            rect: Rect { x, y, w, h },
        }
    }

    fn sample(t: i64, cx: f64, cy: f64, windows: Vec<WindowInfo>) -> WindowSample {
        WindowSample {
            t,
            segment: 0,
            cursor: Point { x: cx, y: cy },
            windows,
        }
    }

    fn pixel_aspect(g: &Geometry, r: &Rect) -> f64 {
        (r.w * g.surface_px.0) / (r.h * g.surface_px.1)
    }

    #[test]
    fn framing_keeps_the_output_aspect() {
        let g = geo();
        for r in [
            Rect {
                x: 0.1,
                y: 0.1,
                w: 0.2,
                h: 0.6,
            },
            Rect {
                x: 0.3,
                y: 0.4,
                w: 0.6,
                h: 0.1,
            },
        ] {
            let f = g.frame(r, &cfg());
            assert!((pixel_aspect(&g, &f) - 16.0 / 9.0).abs() < 1e-9);
        }
    }

    #[test]
    fn framing_never_zooms_past_max_zoom() {
        let g = geo();
        let f = g.frame(
            Rect {
                x: 0.5,
                y: 0.5,
                w: 0.01,
                h: 0.01,
            },
            &cfg(),
        );
        assert!((f.w * g.surface_px.0 - 1920.0 / 2.0).abs() < 1e-6);
    }

    #[test]
    fn framing_moves_a_rect_onto_the_surface() {
        let f = geo().frame(
            Rect {
                x: 0.8,
                y: 0.8,
                w: 0.3,
                h: 0.3,
            },
            &cfg(),
        );
        assert!(f.x >= 0.0 && f.x + f.w <= 1.0 + 1e-12);
        assert!(f.y >= 0.0 && f.y + f.h <= 1.0 + 1e-12);
    }

    #[test]
    fn subject_skips_small_and_non_normal_windows() {
        let g = geo();
        let mut menu = window(1, 0.4, 0.4, 0.3, 0.3);
        menu.layer = 25;
        let tooltip = window(2, 0.45, 0.45, 0.05, 0.05);
        let editor = window(3, 0.2, 0.2, 0.6, 0.6);
        let s = sample(0, 0.5, 0.5, vec![menu, tooltip, editor]);
        assert_eq!(subject(&s, &g, &cfg()).map(|w| w.id), Some(3));
    }

    fn two_windows(until_switch: i64, after: i64) -> Vec<WindowSample> {
        let a = window(1, 0.0, 0.0, 0.5, 0.5);
        let b = window(2, 0.5, 0.5, 0.5, 0.5);
        let mut samples = Vec::new();
        for t in (0..until_switch).step_by(100) {
            samples.push(sample(t, 0.25, 0.25, vec![a.clone(), b.clone()]));
        }
        for t in (until_switch..until_switch + after).step_by(100) {
            samples.push(sample(t, 0.75, 0.75, vec![a.clone(), b.clone()]));
        }
        samples
    }

    fn human_timeline(duration: f64) -> Timeline {
        let events: Vec<f64> = (0..(duration as usize * 2))
            .map(|i| i as f64 / 2.0)
            .collect();
        Timeline::build(&events, duration, &PacingConfig::default())
    }

    #[test]
    fn the_first_subject_is_adopted_without_a_move() {
        let samples = two_windows(2000, 0);
        let track = CameraTrack::build(&samples, &human_timeline(5.0), &geo(), &cfg());
        assert_eq!(track.move_count(), 0);
        assert_eq!(
            track.rect_at(1.0),
            geo().frame(samples[0].windows[0].rect, &cfg())
        );
    }

    #[test]
    fn a_brief_crossing_does_not_commit() {
        let a = window(1, 0.0, 0.0, 0.5, 0.5);
        let b = window(2, 0.5, 0.5, 0.5, 0.5);
        let mut samples: Vec<_> = (0..20)
            .map(|i| sample(i * 100, 0.25, 0.25, vec![a.clone(), b.clone()]))
            .collect();
        samples[10].cursor = Point { x: 0.75, y: 0.75 };
        samples[11].cursor = Point { x: 0.75, y: 0.75 };
        let track = CameraTrack::build(&samples, &human_timeline(5.0), &geo(), &cfg());
        assert_eq!(track.move_count(), 0);
    }

    #[test]
    fn a_wide_shot_still_zooms_into_a_smaller_window() {
        let huge = window(1, -0.2, -0.1, 1.4, 1.2);
        let small = window(2, 0.3, 0.3, 0.4, 0.4);
        let mut samples: Vec<_> = (0..10)
            .map(|i| sample(i * 100, 0.1, 0.1, vec![huge.clone()]))
            .collect();
        samples
            .extend((10..30).map(|i| sample(i * 100, 0.5, 0.5, vec![small.clone(), huge.clone()])));
        let track = CameraTrack::build(&samples, &human_timeline(5.0), &geo(), &cfg());
        assert_eq!(track.move_count(), 1);
    }

    #[test]
    fn a_nearly_identical_framing_holds() {
        let a = window(1, 0.2, 0.2, 0.5, 0.5);
        let b = window(2, 0.21, 0.2, 0.5, 0.5);
        let mut samples: Vec<_> = (0..10)
            .map(|i| sample(i * 100, 0.3, 0.3, vec![a.clone()]))
            .collect();
        samples.extend((10..30).map(|i| sample(i * 100, 0.3, 0.3, vec![b.clone()])));
        let track = CameraTrack::build(&samples, &human_timeline(5.0), &geo(), &cfg());
        assert_eq!(track.move_count(), 0);
    }

    #[test]
    fn focus_crossfades_on_a_switch_even_when_the_camera_holds() {
        let a = window(1, 0.2, 0.2, 0.5, 0.5);
        let b = window(2, 0.21, 0.2, 0.5, 0.5);
        let mut samples: Vec<_> = (0..10)
            .map(|i| sample(i * 100, 0.3, 0.3, vec![a.clone()]))
            .collect();
        samples.extend((10..30).map(|i| sample(i * 100, 0.3, 0.3, vec![b.clone()])));
        let timeline = human_timeline(5.0);
        let track = CameraTrack::build(&samples, &timeline, &geo(), &cfg());
        assert_eq!(track.move_count(), 0);
        let start = timeline.out_time_at(1.4);
        assert_eq!(track.focus_at(start - 0.01), vec![(1, 1.0)]);
        let mid = track.focus_at(start + 0.35);
        assert_eq!(mid.len(), 2);
        assert!((mid[0].1 + mid[1].1 - 1.0).abs() < 1e-12);
        assert_eq!(track.focus_at(start + 0.7), vec![(2, 1.0)]);
    }

    #[test]
    fn window_rect_at_uses_the_latest_sample_that_has_the_window() {
        let a = window(1, 0.1, 0.1, 0.5, 0.5);
        let moved = window(1, 0.2, 0.1, 0.5, 0.5);
        let samples = vec![
            sample(0, 0.3, 0.3, vec![a.clone()]),
            sample(100, 0.3, 0.3, vec![moved.clone()]),
            sample(200, 0.3, 0.3, vec![]),
        ];
        assert_eq!(window_rect_at(&samples, 1, 250), Some(moved.rect));
        assert_eq!(window_rect_at(&samples, 1, 50), Some(a.rect));
        assert_eq!(window_rect_at(&samples, 2, 250), None);
    }

    #[test]
    fn a_committed_switch_eases_over_the_transition() {
        let g = geo();
        let samples = two_windows(2000, 2000);
        let timeline = human_timeline(5.0);
        let track = CameraTrack::build(&samples, &timeline, &g, &cfg());
        assert_eq!(track.move_count(), 1);
        let start = timeline.out_time_at(2.4);
        let target = g.frame(samples.last().unwrap().windows[1].rect, &cfg());
        assert_eq!(
            track.rect_at(start - 0.01),
            g.frame(samples[0].windows[0].rect, &cfg())
        );
        assert_eq!(track.rect_at(start + 0.7), target);
        let mid = track.rect_at(start + 0.35);
        assert!(
            mid != target && mid.w >= target.w,
            "pulls back mid-move: {mid:?}"
        );
    }
}
