use core_graphics::display::CGDisplay;
use core_graphics::event::CGEvent;
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGRect;
use fastflow_core::geom::{Point, Rect};

/// Bounds are in global points with a top-left origin, the same space as `kCGWindowBounds`.
#[derive(Debug, Clone, Copy)]
pub struct Display {
    pub id: u32,
    /// Position in `CGGetActiveDisplayList`, which is the order avfoundation numbers screens in.
    pub index: usize,
    pub bounds_pt: Rect,
    pub pixels: (u32, u32),
    pub scale: f64,
}

pub(crate) fn rect(r: CGRect) -> Rect {
    Rect {
        x: r.origin.x,
        y: r.origin.y,
        w: r.size.width,
        h: r.size.height,
    }
}

pub fn active() -> Vec<Display> {
    let ids = CGDisplay::active_displays().unwrap_or_default();
    ids.into_iter()
        .enumerate()
        .map(|(index, id)| {
            let d = CGDisplay::new(id);
            let bounds_pt = rect(d.bounds());
            let pixels = d
                .display_mode()
                .map(|m| (m.pixel_width() as u32, m.pixel_height() as u32))
                .unwrap_or((d.pixels_wide() as u32, d.pixels_high() as u32));
            Display {
                id,
                index,
                bounds_pt,
                pixels,
                scale: pixels.0 as f64 / bounds_pt.w,
            }
        })
        .collect()
}

/// Global cursor position in points, top-left origin.
pub fn cursor() -> Option<Point> {
    let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok()?;
    let p = CGEvent::new(source).ok()?.location();
    Some(Point { x: p.x, y: p.y })
}

pub fn under_cursor() -> Option<Display> {
    let displays = active();
    let c = cursor()?;
    displays
        .iter()
        .find(|d| {
            let b = d.bounds_pt;
            c.x >= b.x && c.x < b.x + b.w && c.y >= b.y && c.y < b.y + b.h
        })
        .or(displays.first())
        .copied()
}

/// Height of the primary display in points. AppKit window frames count y up from its bottom.
pub fn primary_height() -> f64 {
    CGDisplay::main().bounds().size.height
}
