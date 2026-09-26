//! The pacing model. See docs/04-tracks.md.

use std::ops::Range;

use crate::config::PacingConfig;

/// Maps a source range onto an output duration. A hold has `src.start == src.end`.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub src: Range<f64>,
    pub out: f64,
    pub kind: SpanKind,
    pub profile: Profile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    Human,
    /// The part of an idle stretch between its ramps, capped by `max_dead`.
    Idle,
    /// Speed easing between human speed and the ramp peak at the edge of an idle stretch.
    Ramp,
    Hold,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Profile {
    Constant,
    /// Speed follows smoothstep from `from` to `to` across the span's output time.
    Ease {
        from: f64,
        to: f64,
    },
}

/// Integral of smoothstep over `0..u`. Equals 1/2 at `u = 1`.
fn smoothstep_integral(u: f64) -> f64 {
    u * u * u - u * u * u * u / 2.0
}

impl Span {
    /// Average speed over the span.
    pub fn speed(&self) -> f64 {
        if self.out == 0.0 {
            0.0
        } else {
            (self.src.end - self.src.start) / self.out
        }
    }

    pub fn start_speed(&self) -> f64 {
        match self.profile {
            Profile::Constant => self.speed(),
            Profile::Ease { from, .. } => from,
        }
    }

    pub fn end_speed(&self) -> f64 {
        match self.profile {
            Profile::Constant => self.speed(),
            Profile::Ease { to, .. } => to,
        }
    }

    /// Fraction of the source range covered after fraction `u` of the output time.
    fn src_fraction(&self, u: f64) -> f64 {
        match self.profile {
            Profile::Constant => u,
            Profile::Ease { from, to } => {
                (from * u + (to - from) * smoothstep_integral(u)) / ((from + to) / 2.0)
            }
        }
    }

    fn constant(src: Range<f64>, out: f64, kind: SpanKind) -> Span {
        Span {
            src,
            out,
            kind,
            profile: Profile::Constant,
        }
    }

    /// Covers `out * (from + to) / 2` of source; `src` must be that long.
    fn ramp(src: Range<f64>, out: f64, from: f64, to: f64) -> Span {
        Span {
            src,
            out,
            kind: SpanKind::Ramp,
            profile: Profile::Ease { from, to },
        }
    }
}

/// An idle stretch from `a` to `b`, with a ramp on each side that touches a human span.
/// Full ramps take `cfg.ramp` of output each and peak at `ramp_speed`. What remains between
/// them plays at `idle_speed`, capped at `max_dead`. A stretch too short for full ramps peaks
/// lower. One too short to rise above human speed plays at human speed.
fn push_idle(
    spans: &mut Vec<Span>,
    a: f64,
    b: f64,
    ramp_in: bool,
    ramp_out: bool,
    cfg: &PacingConfig,
) {
    let len = b - a;
    let h = cfg.human_speed;
    let t = cfg.ramp;
    let ramps = if t > 0.0 {
        ramp_in as u32 + ramp_out as u32
    } else {
        0
    };
    let middle = |m0: f64, m1: f64| {
        Span::constant(
            m0..m1,
            ((m1 - m0) / cfg.idle_speed).min(cfg.max_dead),
            SpanKind::Idle,
        )
    };
    if ramps == 0 {
        spans.push(middle(a, b));
        return;
    }

    let peak = cfg.ramp_speed.max(h);
    let full = t * (h + peak) / 2.0;
    if len >= ramps as f64 * full {
        let m0 = if ramp_in { a + full } else { a };
        let m1 = if ramp_out { (b - full).max(m0) } else { b };
        if ramp_in {
            spans.push(Span::ramp(a..m0, t, h, peak));
        }
        if m1 > m0 {
            spans.push(middle(m0, m1));
        }
        if ramp_out {
            spans.push(Span::ramp(m1..b, t, peak, h));
        }
        return;
    }

    let per_ramp = len / ramps as f64;
    let top = 2.0 * per_ramp / t - h;
    if top <= h {
        spans.push(Span::ramp(a..b, len / h, h, h));
        return;
    }
    match (ramp_in, ramp_out) {
        (true, true) => {
            let mid = a + per_ramp;
            spans.push(Span::ramp(a..mid, t, h, top));
            spans.push(Span::ramp(mid..b, t, top, h));
        }
        (true, false) => spans.push(Span::ramp(a..b, t, h, top)),
        (false, _) => spans.push(Span::ramp(a..b, t, top, h)),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Timeline {
    spans: Vec<Span>,
    /// Output time at which each span starts.
    out_starts: Vec<f64>,
}

impl Timeline {
    /// `events` and `duration` are in source seconds. Events outside `0..duration` still widen
    /// the human spans they pad into.
    pub fn build(events: &[f64], duration: f64, cfg: &PacingConfig) -> Timeline {
        let mut spans = Vec::new();
        if duration <= 0.0 {
            return Timeline::from_spans(spans);
        }

        let mut human: Vec<(f64, f64)> = events
            .iter()
            .map(|&t| {
                (
                    (t - cfg.pad_before).max(0.0),
                    (t + cfg.pad_after).min(duration),
                )
            })
            .filter(|(a, b)| a < b)
            .collect();
        human.sort_by(|x, y| x.0.total_cmp(&y.0));
        let mut merged: Vec<(f64, f64)> = Vec::with_capacity(human.len());
        for (a, b) in human {
            match merged.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }

        let mut cursor = 0.0;
        for (i, &(a, b)) in merged.iter().enumerate() {
            if a > cursor {
                push_idle(&mut spans, cursor, a, i > 0, true, cfg);
            }
            spans.push(Span::constant(
                a..b,
                (b - a) / cfg.human_speed,
                SpanKind::Human,
            ));
            if cfg.hold > 0.0 {
                spans.push(Span::constant(b..b, cfg.hold, SpanKind::Hold));
            }
            cursor = b;
        }
        if cursor < duration {
            push_idle(&mut spans, cursor, duration, !merged.is_empty(), false, cfg);
        }
        Timeline::from_spans(spans)
    }

    fn from_spans(spans: Vec<Span>) -> Timeline {
        let mut t = 0.0;
        let out_starts = spans
            .iter()
            .map(|s| {
                let start = t;
                t += s.out;
                start
            })
            .collect();
        Timeline { spans, out_starts }
    }

    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    pub fn out_duration(&self) -> f64 {
        match (self.spans.last(), self.out_starts.last()) {
            (Some(s), Some(start)) => start + s.out,
            _ => 0.0,
        }
    }

    /// The inverse of `src_time_at`. A source time inside a hold maps to the hold's start, so the
    /// first output moment that shows it.
    pub fn out_time_at(&self, src_t: f64) -> f64 {
        let Some(first) = self.spans.first() else {
            return 0.0;
        };
        if src_t <= first.src.start {
            return 0.0;
        }
        let Some(i) = self.spans.iter().position(|s| src_t <= s.src.end) else {
            return self.out_duration();
        };
        let span = &self.spans[i];
        let len = span.src.end - span.src.start;
        if len == 0.0 || span.out == 0.0 {
            return self.out_starts[i];
        }
        let target = (src_t - span.src.start) / len;
        let (mut lo, mut hi) = (0.0, 1.0);
        for _ in 0..50 {
            let mid = (lo + hi) / 2.0;
            if span.src_fraction(mid) < target {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        self.out_starts[i] + hi * span.out
    }

    /// Clamped to the timeline at both ends. A span with zero output time is never landed in.
    pub fn src_time_at(&self, out_t: f64) -> f64 {
        let Some(last) = self.spans.last() else {
            return 0.0;
        };
        if out_t >= self.out_duration() {
            return last.src.end;
        }
        let out_t = out_t.max(0.0);
        let i = self.out_starts.partition_point(|&s| s <= out_t) - 1;
        let span = &self.spans[i];
        if span.out == 0.0 {
            return span.src.end;
        }
        let f = span.src_fraction((out_t - self.out_starts[i]) / span.out);
        span.src.start + f * (span.src.end - span.src.start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn cfg() -> PacingConfig {
        PacingConfig::default()
    }

    fn kinds(t: &Timeline) -> Vec<SpanKind> {
        t.spans().iter().map(|s| s.kind).collect()
    }

    #[test]
    fn no_events_is_one_clamped_idle_span() {
        let t = Timeline::build(&[], 60.0, &cfg());
        assert_eq!(kinds(&t), [SpanKind::Idle]);
        assert_eq!(t.out_duration(), 0.5);
        assert_eq!(t.src_time_at(0.25), 30.0);
    }

    #[test]
    fn idle_stretches_ramp_only_on_sides_that_touch_human_spans() {
        use SpanKind::*;
        let t = Timeline::build(&[10.0, 10.5, 11.0], 60.0, &cfg());
        assert_eq!(kinds(&t), [Idle, Ramp, Human, Hold, Ramp, Idle]);
        assert_eq!(t.spans()[2].src, 9.7..12.2);
    }

    #[test]
    fn full_ramps_ease_between_human_speed_and_the_cap() {
        let t = Timeline::build(&[1.0, 20.0], 30.0, &cfg());
        let gap: Vec<&Span> = t
            .spans()
            .iter()
            .skip_while(|s| s.kind != SpanKind::Hold)
            .skip(1)
            .take(3)
            .collect();
        assert_eq!(gap[0].profile, Profile::Ease { from: 1.0, to: 3.0 });
        assert_eq!(gap[0].out, 0.5);
        assert!((gap[0].src.end - gap[0].src.start - 1.0).abs() < 1e-12);
        assert_eq!(gap[1].kind, SpanKind::Idle);
        assert_eq!(gap[1].out, 0.5);
        assert_eq!(gap[2].profile, Profile::Ease { from: 3.0, to: 1.0 });
    }

    #[test]
    fn a_gap_too_short_for_full_ramps_peaks_lower() {
        let t = Timeline::build(&[1.0, 4.0], 10.0, &cfg());
        let ramps: Vec<&Span> = t
            .spans()
            .iter()
            .filter(|s| s.kind == SpanKind::Ramp && s.src.start >= 2.2)
            .collect();
        assert_eq!(ramps[0].src, 2.2..2.95);
        assert_eq!(ramps[0].profile, Profile::Ease { from: 1.0, to: 2.0 });
        assert_eq!(ramps[1].profile, Profile::Ease { from: 2.0, to: 1.0 });
    }

    #[test]
    fn a_gap_under_two_ramps_at_human_speed_is_not_sped_up() {
        let t = Timeline::build(&[1.0, 3.4], 10.0, &cfg());
        let gap = t.spans().iter().find(|s| s.src == (2.2..3.1)).unwrap();
        assert_eq!(gap.profile, Profile::Ease { from: 1.0, to: 1.0 });
        assert!((gap.out - 0.9).abs() < 1e-12);
    }

    #[test]
    fn a_ramp_follows_the_eased_speed_curve() {
        let t = Timeline::build(&[1.0, 20.0], 30.0, &cfg());
        let i = t
            .spans()
            .iter()
            .position(|s| s.kind == SpanKind::Ramp && s.src.start > 1.0)
            .unwrap();
        let (start, span) = (t.out_starts[i], &t.spans()[i]);
        let halfway = t.src_time_at(start + span.out / 2.0) - span.src.start;
        // out * (from * u + (to - from) * (u^3 - u^4 / 2)) at u = 1/2
        assert!((halfway - 0.5 * (0.5 + 2.0 * (0.125 - 0.03125))).abs() < 1e-9);
    }

    #[test]
    fn a_hold_freezes_source_time() {
        let t = Timeline::build(&[1.0], 10.0, &cfg());
        let i = t
            .spans()
            .iter()
            .position(|s| s.kind == SpanKind::Hold)
            .unwrap();
        let hold_start = t.out_starts[i];
        assert_eq!(t.src_time_at(hold_start + 0.1), 2.2);
        assert_eq!(t.src_time_at(hold_start + 0.2), 2.2);
    }

    fn pacing() -> impl Strategy<Value = PacingConfig> {
        (
            (0.0..2.0f64, 0.0..3.0f64, 0.25..2.0f64, 1.0..32.0f64),
            (0.0..3.0f64, 0.0..1.0f64, 0.0..1.5f64, 0.5..6.0f64),
        )
            .prop_map(
                |(
                    (pad_before, pad_after, human_speed, idle_speed),
                    (max_dead, hold, ramp, ramp_speed),
                )| PacingConfig {
                    pad_before,
                    pad_after,
                    human_speed,
                    idle_speed,
                    max_dead,
                    hold,
                    ramp,
                    ramp_speed,
                },
            )
    }

    fn case() -> impl Strategy<Value = (Vec<f64>, f64, PacingConfig)> {
        (1.0..600.0f64).prop_flat_map(|duration| {
            (
                prop::collection::vec(-5.0..duration + 5.0, 0..200),
                Just(duration),
                pacing(),
            )
        })
    }

    proptest! {
        #[test]
        fn src_time_is_monotonic((events, duration, cfg) in case()) {
            let t = Timeline::build(&events, duration, &cfg);
            let total = t.out_duration();
            let mut prev = f64::NEG_INFINITY;
            for i in 0..=2000 {
                let s = t.src_time_at(total * i as f64 / 2000.0);
                prop_assert!(s >= prev - 1e-9, "src went backwards at step {i}: {s} < {prev}");
                prev = s;
            }
        }

        #[test]
        fn endpoints_map_to_the_recording_ends((events, duration, cfg) in case()) {
            let t = Timeline::build(&events, duration, &cfg);
            prop_assert_eq!(t.src_time_at(0.0), 0.0);
            prop_assert_eq!(t.src_time_at(t.out_duration()), duration);
        }

        #[test]
        fn out_duration_is_the_sum_of_spans((events, duration, cfg) in case()) {
            let t = Timeline::build(&events, duration, &cfg);
            let sum: f64 = t.spans().iter().map(|s| s.out).sum();
            prop_assert!((t.out_duration() - sum).abs() < 1e-9);
        }

        #[test]
        fn spans_tile_the_source((events, duration, cfg) in case()) {
            let t = Timeline::build(&events, duration, &cfg);
            let spans = t.spans();
            prop_assert_eq!(spans[0].src.start, 0.0);
            prop_assert_eq!(spans.last().unwrap().src.end, duration);
            for w in spans.windows(2) {
                prop_assert_eq!(w[0].src.end, w[1].src.start);
            }
        }

        #[test]
        fn every_event_in_range_plays_at_human_speed((events, duration, cfg) in case()) {
            let t = Timeline::build(&events, duration, &cfg);
            for &e in events.iter().filter(|&&e| e > 0.0 && e < duration) {
                let span = t.spans().iter().find(|s| s.kind == SpanKind::Human && s.src.contains(&e));
                prop_assert!(span.is_some(), "event {e} is not inside a human span");
            }
        }

        #[test]
        fn idle_middles_never_exceed_max_dead((events, duration, cfg) in case()) {
            let t = Timeline::build(&events, duration, &cfg);
            for s in t.spans().iter().filter(|s| s.kind == SpanKind::Idle) {
                prop_assert!(s.out <= cfg.max_dead + 1e-12);
            }
        }

        #[test]
        fn ramps_never_exceed_the_peak((events, duration, cfg) in case()) {
            let t = Timeline::build(&events, duration, &cfg);
            let peak = cfg.ramp_speed.max(cfg.human_speed) + 1e-9;
            for s in t.spans().iter().filter(|s| s.kind == SpanKind::Ramp) {
                prop_assert!(s.start_speed() <= peak && s.end_speed() <= peak, "{s:?}");
            }
        }

        #[test]
        fn a_ramp_meets_its_human_span_at_human_speed((events, duration, cfg) in case()) {
            let t = Timeline::build(&events, duration, &cfg);
            for w in t.spans().windows(2) {
                let meets = match (w[0].kind, w[1].kind) {
                    (SpanKind::Ramp, SpanKind::Human) => Some(w[0].end_speed()),
                    (SpanKind::Human | SpanKind::Hold, SpanKind::Ramp) => Some(w[1].start_speed()),
                    _ => None,
                };
                if let Some(v) = meets {
                    prop_assert!((v - cfg.human_speed).abs() < 1e-9, "{:?} -> {:?}", w[0], w[1]);
                }
            }
        }

        #[test]
        fn out_time_at_inverts_src_time_at((events, duration, cfg) in case(), f in 0.0..1.0f64) {
            let t = Timeline::build(&events, duration, &cfg);
            let out = t.out_time_at(f * duration);
            prop_assert!((t.src_time_at(out) - f * duration).abs() < 1e-6);
        }

        #[test]
        fn ramps_cover_the_source_their_speeds_imply((events, duration, cfg) in case()) {
            let t = Timeline::build(&events, duration, &cfg);
            for s in t.spans().iter().filter(|s| s.kind == SpanKind::Ramp) {
                let implied = s.out * (s.start_speed() + s.end_speed()) / 2.0;
                prop_assert!((implied - (s.src.end - s.src.start)).abs() < 1e-6, "{s:?}");
            }
        }
    }
}
