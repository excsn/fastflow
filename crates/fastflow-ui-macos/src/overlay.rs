//! The live framing overlay: a border drawn over the recorded display where the camera is
//! framing, driven by the same director and move planner as the offline camera track.

use std::cell::Cell;
use std::time::Instant;

use fastflow_core::camera::{self, Decision, Director, Geometry, Move};
use fastflow_core::config::CameraConfig;
use fastflow_core::geom::Rect;
use fastflow_core::recording::WindowSample;
use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSBackingStoreType, NSBezierPath, NSColor, NSStatusWindowLevel, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowSharingType, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

const LINE_WIDTH: f64 = 3.0;
const CORNER: f64 = 12.0;

define_class!(
    // SAFETY: NSView has no subclassing requirements for drawRect: and FrameView does not
    // implement Drop.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "FastflowFrameView"]
    #[ivars = Cell<Option<Rect>>]
    struct FrameView;

    impl FrameView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let Some(r) = self.ivars().get() else { return };
            let b = self.bounds();
            let inset = LINE_WIDTH / 2.0;
            let rect = NSRect::new(
                NSPoint::new(
                    r.x * b.size.width + inset,
                    (1.0 - r.y - r.h) * b.size.height + inset,
                ),
                NSSize::new(
                    r.w * b.size.width - LINE_WIDTH,
                    r.h * b.size.height - LINE_WIDTH,
                ),
            );
            let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, CORNER, CORNER);
            path.setLineWidth(LINE_WIDTH);
            NSColor::systemYellowColor()
                .colorWithAlphaComponent(0.9)
                .setStroke();
            path.stroke();
        }
    }
);

impl FrameView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Cell::new(None));
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

/// `display_pt` is the display's bounds in global points with a top-left origin, as
/// CoreGraphics reports them. `primary_height` is the primary display's height in points.
pub struct Overlay {
    window: Retained<NSWindow>,
    view: Retained<FrameView>,
    geo: Geometry,
    cfg: CameraConfig,
    director: Director,
    started: Instant,
    rect: Rect,
    active: Option<Move>,
    last_sample: Option<i64>,
}

impl Overlay {
    pub fn new(
        mtm: MainThreadMarker,
        display_pt: Rect,
        primary_height: f64,
        geo: Geometry,
        cfg: CameraConfig,
    ) -> Overlay {
        let frame = NSRect::new(
            NSPoint::new(display_pt.x, primary_height - display_pt.y - display_pt.h),
            NSSize::new(display_pt.w, display_pt.h),
        );
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                NSWindowStyleMask::Borderless,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setOpaque(false);
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setHasShadow(false);
        window.setIgnoresMouseEvents(true);
        window.setSharingType(NSWindowSharingType::None);
        window.setLevel(NSStatusWindowLevel);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        let view = FrameView::new(mtm, NSRect::new(NSPoint::new(0.0, 0.0), frame.size));
        window.setContentView(Some(&view));
        window.orderFrontRegardless();
        Overlay {
            window,
            view,
            rect: geo.full(),
            geo,
            cfg,
            director: Director::default(),
            started: Instant::now(),
            active: None,
            last_sample: None,
        }
    }

    /// Feeds the latest window sample, if it is new, then redraws the frame for now.
    pub fn update(&mut self, sample: Option<&WindowSample>) {
        let now = self.started.elapsed().as_secs_f64();
        if let Some(s) = sample.filter(|s| Some(s.t) != self.last_sample) {
            self.last_sample = Some(s.t);
            match self
                .director
                .feed(s.t as f64 / 1000.0, s, &self.geo, &self.cfg, false)
            {
                Some(Decision::Adopt { rect, .. }) => {
                    self.rect = self.geo.frame(rect, &self.cfg);
                }
                Some(Decision::OverviewStart { .. }) => {}
                Some(Decision::Commit { rect, .. } | Decision::OverviewEnd { rect, .. }) => {
                    let from = self.current(now);
                    self.active = camera::plan_move(now, from, rect, &self.geo, &self.cfg);
                    if self.active.is_none() {
                        self.rect = from;
                    }
                }
                None => {}
            }
        }
        let r = self.current(now);
        if let Some(m) = &self.active
            && now >= m.end()
        {
            self.rect = r;
            self.active = None;
        }
        self.view.ivars().set(Some(r));
        self.view.setNeedsDisplay(true);
    }

    fn current(&self, now: f64) -> Rect {
        match &self.active {
            Some(m) => camera::clamp_to_surface(m.rect_at(now)),
            None => self.rect,
        }
    }

    pub fn close(&self) {
        self.window.orderOut(None);
    }
}
