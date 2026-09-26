//! "Make GIF…": pick a recording and a video, trim it with a two-handled slider, set speed and
//! size, see an estimated file size and export an animated GIF or WebP.
//!
//! Controls are read on each app tick rather than through target-action, so the window needs no
//! callbacks beyond the export button.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use block2::RcBlock;
use fastflow_core::recording::Meta;
use fastflow_daemon::paths;
use fastflow_render::export::{self, ExportSpec, Format};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
  NSApplication, NSBackingStoreType, NSBezierPath, NSButton, NSColor, NSEvent, NSFont,
  NSLayoutAttribute, NSPopUpButton, NSProgressIndicator, NSProgressIndicatorStyle,
  NSSegmentSwitchTracking, NSSegmentedControl, NSSlider, NSStackView, NSTextField,
  NSUserInterfaceLayoutOrientation, NSView, NSWindow, NSWindowOrderingMode, NSWindowStyleMask,
};
use objc2_av_foundation::AVPlayer;
use objc2_av_kit::{AVPlayerView, AVPlayerViewControlsStyle};
use objc2_core_media::{CMTime, kCMTimeZero};
use objc2_foundation::{NSArray, NSEdgeInsets, NSObject, NSPoint, NSRect, NSSize, NSString, NSURL};

use crate::applog::log;
use crate::notify;

const PLAYER_SIZE: (f64, f64) = (640.0, 360.0);
const RANGE_HEIGHT: f64 = 28.0;
const KNOB: f64 = 8.0;
/// How long settings must stay unchanged before a new size estimate starts.
const ESTIMATE_AFTER: Duration = Duration::from_millis(500);

const SOURCES: [&str; 3] = ["Rendered", "Raw", "Preview"];
const FPS: [u32; 6] = [10, 12, 15, 20, 24, 30];
const WIDTHS: [u32; 5] = [480, 640, 800, 1024, 1280];
const COLORS: [u32; 3] = [64, 128, 256];
const QUALITY: [u32; 4] = [50, 70, 85, 95];

type OnDrag = Box<dyn Fn(&RangeView)>;

pub struct RangeIvars {
  from: Cell<f64>,
  to: Cell<f64>,
  grabbed: Cell<Grab>,
  /// Where in the range the middle was grabbed, as a fraction from `from`.
  grab_offset: Cell<f64>,
  /// Runs inside the mouse handlers, so feedback does not wait for the app's next tick.
  on_drag: RefCell<Option<OnDrag>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grab {
  None,
  Start,
  End,
  /// The range between the handles, which moves both.
  Middle,
}

define_class!(
    // SAFETY: NSView has no subclassing requirements for the overridden methods and RangeView
    // does not implement Drop.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "FastflowRangeView"]
    #[ivars = RangeIvars]
    pub struct RangeView;

    impl RangeView {
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let x = self.fraction_at(event);
            let iv = self.ivars();
            let (from, to) = (iv.from.get(), iv.to.get());
            let knob = KNOB / (self.bounds().size.width - 2.0 * KNOB);
            let near_start = (x - from).abs() <= knob;
            let near_end = (x - to).abs() <= knob;
            let grab = if near_start && near_end {
                if x < from { Grab::Start } else { Grab::End }
            } else if near_start {
                Grab::Start
            } else if near_end {
                Grab::End
            } else if x > from && x < to {
                Grab::Middle
            } else if (x - from).abs() < (x - to).abs() {
                Grab::Start
            } else {
                Grab::End
            };
            iv.grabbed.set(grab);
            iv.grab_offset.set(x - from);
            self.set_grabbed(x);
            self.notify();
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let x = self.fraction_at(event);
            self.set_grabbed(x);
            self.notify();
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            self.ivars().grabbed.set(Grab::None);
            self.notify();
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let b = self.bounds();
            let (x0, w) = (KNOB, b.size.width - 2.0 * KNOB);
            let mid = b.size.height / 2.0;
            let track = NSRect::new(NSPoint::new(x0, mid - 2.0), NSSize::new(w, 4.0));
            NSColor::tertiaryLabelColor().setFill();
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(track, 2.0, 2.0).fill();
            let (a, c) = (x0 + self.ivars().from.get() * w, x0 + self.ivars().to.get() * w);
            let chosen = NSRect::new(NSPoint::new(a, mid - 3.0), NSSize::new(c - a, 6.0));
            NSColor::controlAccentColor().setFill();
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(chosen, 3.0, 3.0).fill();
            for x in [a, c] {
                let knob = NSRect::new(
                    NSPoint::new(x - KNOB, mid - KNOB),
                    NSSize::new(2.0 * KNOB, 2.0 * KNOB),
                );
                let path = NSBezierPath::bezierPathWithOvalInRect(knob);
                NSColor::whiteColor().setFill();
                path.fill();
                NSColor::secondaryLabelColor().setStroke();
                path.stroke();
            }
        }
    }
);

impl RangeView {
  fn new(mtm: MainThreadMarker) -> Retained<Self> {
    let this = Self::alloc(mtm).set_ivars(RangeIvars {
      from: Cell::new(0.0),
      to: Cell::new(1.0),
      grabbed: Cell::new(Grab::None),
      grab_offset: Cell::new(0.0),
      on_drag: RefCell::new(None),
    });
    let frame = NSRect::new(
      NSPoint::new(0.0, 0.0),
      NSSize::new(PLAYER_SIZE.0, RANGE_HEIGHT),
    );
    unsafe { msg_send![super(this), initWithFrame: frame] }
  }

  fn fraction_at(&self, event: &NSEvent) -> f64 {
    let p = self.convertPoint_fromView(event.locationInWindow(), None);
    let w = self.bounds().size.width - 2.0 * KNOB;
    ((p.x - KNOB) / w).clamp(0.0, 1.0)
  }

  fn set_grabbed(&self, x: f64) {
    let iv = self.ivars();
    match iv.grabbed.get() {
      Grab::Start => iv.from.set(x.min(iv.to.get())),
      Grab::End => iv.to.set(x.max(iv.from.get())),
      Grab::Middle => {
        let len = iv.to.get() - iv.from.get();
        let from = (x - iv.grab_offset.get()).clamp(0.0, 1.0 - len);
        iv.from.set(from);
        iv.to.set(from + len);
      }
      Grab::None => {}
    }
    self.setNeedsDisplay(true);
  }

  fn notify(&self) {
    if let Some(f) = self.ivars().on_drag.borrow().as_ref() {
      f(self);
    }
  }

  /// The handle's position in screen coordinates: its x and the slider's top edge.
  fn handle_on_screen(&self, fraction: f64) -> Option<NSPoint> {
    let b = self.bounds();
    let x = KNOB + fraction * (b.size.width - 2.0 * KNOB);
    let local = NSRect::new(NSPoint::new(x, b.size.height), NSSize::new(0.0, 0.0));
    let window = self.window()?;
    let r = window.convertRectToScreen(self.convertRect_toView(local, None));
    Some(r.origin)
  }

  fn reset(&self) {
    self.ivars().from.set(0.0);
    self.ivars().to.set(1.0);
    self.setNeedsDisplay(true);
  }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and GifTarget does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FastflowGifTarget"]
    #[ivars = Cell<bool>]
    pub struct GifTarget;

    impl GifTarget {
        #[unsafe(method(export:))]
        fn export(&self, _sender: Option<&AnyObject>) {
            self.ivars().set(true);
        }
    }
);

impl GifTarget {
  fn new(mtm: MainThreadMarker) -> Retained<Self> {
    let this = Self::alloc(mtm).set_ivars(Cell::new(false));
    unsafe { msg_send![super(this), init] }
  }
}

/// Everything that changes the output. A change restarts the estimate.
#[derive(Debug, Clone, PartialEq)]
struct Settings {
  src: PathBuf,
  from: f64,
  to: f64,
  speed: f64,
  fps: u32,
  width: u32,
  detail: u32,
  format: Format,
}

impl Settings {
  fn spec(&self, out: PathBuf) -> ExportSpec {
    ExportSpec {
      src: self.src.clone(),
      from: self.from,
      to: self.to,
      speed: self.speed,
      fps: self.fps,
      width: self.width,
      colors: self.detail,
      quality: self.detail,
      format: self.format,
      out,
    }
  }
}

/// `Ok` holds the written file and its size in bytes. `Err` holds why the export failed.
type ExportResult = Result<(PathBuf, u64), String>;

struct Loaded {
  dir: PathBuf,
  src: PathBuf,
  duration: f64,
  player: Retained<AVPlayer>,
  seeker: Seeker,
}

pub struct GifWindow {
  window: Retained<NSWindow>,
  player_view: Retained<AVPlayerView>,
  recordings: Retained<NSPopUpButton>,
  ids: Vec<String>,
  source: Retained<NSSegmentedControl>,
  range: Retained<RangeView>,
  range_label: Retained<NSTextField>,
  played: Retained<NSProgressIndicator>,
  speed: Retained<NSSlider>,
  speed_label: Retained<NSTextField>,
  format: Retained<NSSegmentedControl>,
  fps: Retained<NSPopUpButton>,
  width: Retained<NSPopUpButton>,
  detail: Retained<NSPopUpButton>,
  detail_format: Format,
  estimate_label: Retained<NSTextField>,
  status: Retained<NSTextField>,
  export_button: Retained<NSButton>,
  target: Retained<GifTarget>,

  choice: Option<(isize, isize)>,
  loaded: Option<Loaded>,
  settings: Option<Settings>,
  changed_at: Instant,
  estimated: Option<Settings>,
  estimate: Option<(Settings, Receiver<Result<u64, String>>)>,
  export: Option<(Arc<Mutex<f64>>, Receiver<ExportResult>)>,
  last_grab: Grab,
  scrub: Rc<RefCell<Scrub>>,
  duration: Rc<Cell<f64>>,
}

const PREVIEW: (f64, f64) = (480.0, 270.0);
const PREVIEW_PAD: f64 = 8.0;
const PREVIEW_LABEL: f64 = 22.0;

/// A large frame preview floating over the slider while a handle is dragged. It has its own
/// player so the main one can show the other end of the range meanwhile.
struct Scrub {
  window: Retained<NSWindow>,
  view: Retained<AVPlayerView>,
  label: Retained<NSTextField>,
  seeker: Option<Seeker>,
  attached: bool,
  current: Option<(f64, &'static str)>,
}

impl Scrub {
  fn new(mtm: MainThreadMarker) -> Scrub {
    let size = NSSize::new(
      PREVIEW.0 + 2.0 * PREVIEW_PAD,
      PREVIEW.1 + 2.0 * PREVIEW_PAD + PREVIEW_LABEL,
    );
    let window = unsafe {
      NSWindow::initWithContentRect_styleMask_backing_defer(
        NSWindow::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), size),
        NSWindowStyleMask::Borderless,
        NSBackingStoreType::Buffered,
        false,
      )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(0.08, 0.82)));
    window.setHasShadow(true);
    window.setIgnoresMouseEvents(true);

    let content = NSView::initWithFrame(
      NSView::alloc(mtm),
      NSRect::new(NSPoint::new(0.0, 0.0), size),
    );
    let view = unsafe {
      AVPlayerView::initWithFrame(
        AVPlayerView::alloc(mtm),
        NSRect::new(
          NSPoint::new(PREVIEW_PAD, PREVIEW_PAD + PREVIEW_LABEL),
          NSSize::new(PREVIEW.0, PREVIEW.1),
        ),
      )
    };
    unsafe { view.setControlsStyle(AVPlayerViewControlsStyle::None) };
    let label = label(mtm, "");
    label.setTextColor(Some(&NSColor::whiteColor()));
    label.setFont(Some(&NSFont::monospacedDigitSystemFontOfSize_weight(
      13.0, 0.3,
    )));
    label.setFrame(NSRect::new(
      NSPoint::new(PREVIEW_PAD, PREVIEW_PAD / 2.0),
      NSSize::new(PREVIEW.0, PREVIEW_LABEL),
    ));
    content.addSubview(&view);
    content.addSubview(&label);
    window.setContentView(Some(&content));
    Scrub {
      window,
      view,
      label,
      seeker: None,
      attached: false,
      current: None,
    }
  }

  fn load(&mut self, url: &NSURL, mtm: MainThreadMarker) {
    let player = unsafe { AVPlayer::playerWithURL(url, mtm) };
    unsafe {
      player.setMuted(true);
      self.view.setPlayer(Some(&player));
    }
    self.seeker = Some(Seeker::new(player));
  }

  /// Attaches the panel to `parent` while invisible, so showing it later is only an alpha
  /// change rather than a window coming on screen.
  fn attach(&mut self, parent: &NSWindow) {
    if !self.attached {
      self.window.setAlphaValue(0.0);
      unsafe { parent.addChildWindow_ordered(&self.window, NSWindowOrderingMode::Above) };
      self.window.orderFront(None);
      self.attached = true;
    }
  }

  /// Shows the frame at `t` centred above `anchor`, kept inside `parent`'s screen. The panel
  /// appears at once and says so while the frame is still decoding.
  fn show(&mut self, parent: &NSWindow, anchor: NSPoint, t: f64, caption: &'static str) {
    let Some(seeker) = &mut self.seeker else {
      return;
    };
    seeker.aim(t);
    self.current = Some((t, caption));
    self.refresh();
    let size = self.window.frame().size;
    let pf = parent.frame();
    let x =
      (anchor.x - size.width / 2.0).clamp(pf.origin.x, pf.origin.x + pf.size.width - size.width);
    self.window.setFrameOrigin(NSPoint::new(x, anchor.y + 6.0));
    self.attach(parent);
    self.window.setAlphaValue(1.0);
  }

  /// Updates the label once the exact frame has landed. Also retries a seek the last drag
  /// event could not issue because one was in flight.
  fn refresh(&mut self) {
    let (Some((t, caption)), Some(seeker)) = (self.current, &mut self.seeker) else {
      return;
    };
    seeker.aim(t);
    let state = if seeker.showing(t) {
      ""
    } else {
      "  loading…"
    };
    self
      .label
      .setStringValue(&NSString::from_str(&format!("{caption}  {t:.2}s{state}")));
  }

  fn hide(&mut self) {
    if let Some(s) = &mut self.seeker {
      s.release();
    }
    self.current = None;
    self.window.setAlphaValue(0.0);
  }
}

fn label(mtm: MainThreadMarker, text: &str) -> Retained<NSTextField> {
  NSTextField::labelWithString(&NSString::from_str(text), mtm)
}

fn popup(mtm: MainThreadMarker, items: &[String], selected: usize) -> Retained<NSPopUpButton> {
  let p = NSPopUpButton::initWithFrame_pullsDown(
    NSPopUpButton::alloc(mtm),
    NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(120.0, 26.0)),
    false,
  );
  for i in items {
    p.addItemWithTitle(&NSString::from_str(i));
  }
  p.selectItemAtIndex(selected as isize);
  p
}

fn segmented(mtm: MainThreadMarker, labels: &[&str]) -> Retained<NSSegmentedControl> {
  let labels: Vec<Retained<NSString>> = labels.iter().map(|l| NSString::from_str(l)).collect();
  let control = unsafe {
    NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
      &NSArray::from_retained_slice(&labels),
      NSSegmentSwitchTracking::SelectOne,
      None,
      None,
      mtm,
    )
  };
  control.setSelectedSegment(0);
  control
}

fn view<T: objc2::Message + objc2::ClassType>(v: &Retained<T>) -> Retained<NSView> {
  // SAFETY: every control passed here is an NSView subclass.
  unsafe { Retained::cast_unchecked(v.clone()) }
}

fn row(mtm: MainThreadMarker, views: &[Retained<NSView>]) -> Retained<NSView> {
  let s = NSStackView::stackViewWithViews(&NSArray::from_retained_slice(views), mtm);
  s.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
  s.setSpacing(10.0);
  s.setAlignment(NSLayoutAttribute::CenterY);
  view(&s)
}

fn raw_file(dir: &Path) -> Option<String> {
  let text = std::fs::read_to_string(dir.join("meta.json")).ok()?;
  let meta: Meta = serde_json::from_str(&text).ok()?;
  meta.segments.first().map(|s| s.file.clone())
}

fn source_file(dir: &Path, source: isize) -> Option<PathBuf> {
  let file = match source {
    0 => "render.mp4".to_owned(),
    1 => raw_file(dir)?,
    _ => "preview.mp4".to_owned(),
  };
  let path = dir.join(file);
  path.is_file().then_some(path)
}

/// `clip.gif`, then `clip-2.gif` and so on, so an export never overwrites an earlier one.
fn unused_output(dir: &Path, format: Format) -> PathBuf {
  let ext = format.extension();
  (1..)
    .map(|n| match n {
      1 => dir.join(format!("clip.{ext}")),
      n => dir.join(format!("clip-{n}.{ext}")),
    })
    .find(|p| !p.exists())
    .expect("an unused name exists")
}

fn megabytes(bytes: u64) -> String {
  format!("{:.1} MB", bytes as f64 / 1e6)
}

fn seek(player: &AVPlayer, t: f64) {
  unsafe {
    player.seekToTime_toleranceBefore_toleranceAfter(
      CMTime::with_seconds(t, 600),
      kCMTimeZero,
      kCMTimeZero,
    );
  }
}

/// How long a dragged target must stay still before the seek becomes frame-exact.
const SETTLE: Duration = Duration::from_millis(150);
/// Keyframe slack allowed while the target is still moving, so each seek decodes little.
const ROUGH: f64 = 0.25;

/// Keeps one seek in flight per player. A new seek cancels the one before it, so seeking on
/// every tick while dragging means a frame rarely finishes decoding.
struct Seeker {
  player: Retained<AVPlayer>,
  busy: Arc<AtomicBool>,
  landed: Arc<Mutex<Option<(f64, bool)>>>,
  asked: Option<(f64, bool)>,
  target: Option<f64>,
  moved_at: Instant,
}

impl Seeker {
  fn new(player: Retained<AVPlayer>) -> Seeker {
    Seeker {
      player,
      busy: Arc::new(AtomicBool::new(false)),
      landed: Arc::new(Mutex::new(None)),
      asked: None,
      target: None,
      moved_at: Instant::now(),
    }
  }

  /// Call every tick with where the frame should be.
  fn aim(&mut self, t: f64) {
    if self.target != Some(t) {
      self.target = Some(t);
      self.moved_at = Instant::now();
    }
    if self.busy.load(Ordering::Acquire) {
      return;
    }
    let exact = self.moved_at.elapsed() >= SETTLE;
    if self.asked == Some((t, exact)) || self.asked == Some((t, true)) {
      return;
    }
    self.asked = Some((t, exact));
    self.busy.store(true, Ordering::Release);
    let (busy, landed) = (Arc::clone(&self.busy), Arc::clone(&self.landed));
    let done = RcBlock::new(move |finished: Bool| {
      if finished.as_bool() {
        *landed.lock().unwrap() = Some((t, exact));
      }
      busy.store(false, Ordering::Release);
    });
    let slack = if exact {
      unsafe { kCMTimeZero }
    } else {
      unsafe { CMTime::with_seconds(ROUGH, 600) }
    };
    unsafe {
      self
        .player
        .seekToTime_toleranceBefore_toleranceAfter_completionHandler(
          CMTime::with_seconds(t, 600),
          slack,
          slack,
          &done,
        );
    }
  }

  /// Whether the exact frame for `t` is on screen.
  fn showing(&self, t: f64) -> bool {
    *self.landed.lock().unwrap() == Some((t, true))
  }

  fn release(&mut self) {
    self.target = None;
    self.asked = None;
  }
}

impl GifWindow {
  pub fn new(mtm: MainThreadMarker) -> GifWindow {
    let target = GifTarget::new(mtm);

    let recordings = popup(mtm, &[], 0);
    let source = segmented(mtm, &SOURCES);

    let player_view = unsafe {
      AVPlayerView::initWithFrame(
        AVPlayerView::alloc(mtm),
        NSRect::new(
          NSPoint::new(0.0, 0.0),
          NSSize::new(PLAYER_SIZE.0, PLAYER_SIZE.1),
        ),
      )
    };
    unsafe { player_view.setControlsStyle(AVPlayerViewControlsStyle::None) };
    player_view
      .widthAnchor()
      .constraintEqualToConstant(PLAYER_SIZE.0)
      .setActive(true);
    player_view
      .heightAnchor()
      .constraintEqualToConstant(PLAYER_SIZE.1)
      .setActive(true);

    let played = NSProgressIndicator::new(mtm);
    played.setStyle(NSProgressIndicatorStyle::Bar);
    played.setIndeterminate(false);
    played.setMinValue(0.0);
    played.setMaxValue(1.0);
    played
      .widthAnchor()
      .constraintEqualToConstant(PLAYER_SIZE.0)
      .setActive(true);

    let range = RangeView::new(mtm);
    let scrub = Rc::new(RefCell::new(Scrub::new(mtm)));
    let duration = Rc::new(Cell::new(0.0));
    {
      let (scrub, duration) = (Rc::clone(&scrub), Rc::clone(&duration));
      *range.ivars().on_drag.borrow_mut() = Some(Box::new(move |r: &RangeView| {
        let (iv, d) = (r.ivars(), duration.get());
        let Some(parent) = r.window() else { return };
        let mut scrub = scrub.borrow_mut();
        let (fraction, caption) = match iv.grabbed.get() {
          Grab::None => return scrub.hide(),
          Grab::End => (iv.to.get(), "end"),
          Grab::Start | Grab::Middle => (iv.from.get(), "start"),
        };
        if d > 0.0
          && let Some(at) = r.handle_on_screen(fraction)
        {
          scrub.show(&parent, at, fraction * d, caption);
        }
      }));
    }
    range
      .widthAnchor()
      .constraintEqualToConstant(PLAYER_SIZE.0)
      .setActive(true);
    range
      .heightAnchor()
      .constraintEqualToConstant(RANGE_HEIGHT)
      .setActive(true);
    let range_label = label(mtm, "");

    let speed = unsafe {
      NSSlider::sliderWithValue_minValue_maxValue_target_action(1.0, 1.0, 4.0, None, None, mtm)
    };
    speed.setNumberOfTickMarks(13);
    speed.setAllowsTickMarkValuesOnly(true);
    speed
      .widthAnchor()
      .constraintEqualToConstant(200.0)
      .setActive(true);
    let speed_label = label(mtm, "1.00x");

    let format = segmented(mtm, &["GIF", "WebP"]);
    let fps = popup(mtm, &FPS.map(|f| format!("{f} fps")), 2);
    let width = popup(mtm, &WIDTHS.map(|w| format!("{w} px")), 2);
    let detail = popup(mtm, &COLORS.map(|c| format!("{c} colours")), 1);

    let estimate_label = label(mtm, "");
    estimate_label.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
    let status = label(mtm, "");
    let export_button = unsafe {
      NSButton::buttonWithTitle_target_action(
        &NSString::from_str("Export"),
        Some(&target),
        Some(sel!(export:)),
        mtm,
      )
    };

    let rows = [
      row(
        mtm,
        &[
          view(&label(mtm, "Recording")),
          view(&recordings),
          view(&source),
        ],
      ),
      view(&player_view),
      view(&played),
      view(&range),
      view(&range_label),
      row(
        mtm,
        &[view(&label(mtm, "Speed")), view(&speed), view(&speed_label)],
      ),
      row(
        mtm,
        &[view(&format), view(&fps), view(&width), view(&detail)],
      ),
      row(
        mtm,
        &[view(&estimate_label), view(&export_button), view(&status)],
      ),
    ];
    let stack = NSStackView::stackViewWithViews(&NSArray::from_retained_slice(&rows), mtm);
    stack.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
    stack.setAlignment(NSLayoutAttribute::Leading);
    stack.setSpacing(12.0);
    stack.setEdgeInsets(NSEdgeInsets {
      top: 20.0,
      left: 20.0,
      bottom: 20.0,
      right: 20.0,
    });

    let window = unsafe {
      NSWindow::initWithContentRect_styleMask_backing_defer(
        NSWindow::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(680.0, 640.0)),
        NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
        NSBackingStoreType::Buffered,
        false,
      )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("Make GIF"));
    window.setContentView(Some(&stack));

    GifWindow {
      window,
      player_view,
      recordings,
      ids: Vec::new(),
      source,
      range,
      range_label,
      played,
      speed,
      speed_label,
      format,
      fps,
      width,
      detail,
      detail_format: Format::Gif,
      estimate_label,
      status,
      export_button,
      target,
      choice: None,
      loaded: None,
      settings: None,
      changed_at: Instant::now(),
      estimated: None,
      estimate: None,
      export: None,
      last_grab: Grab::None,
      scrub,
      duration,
    }
  }

  /// Refreshes the recording list, newest first. Then brings the window forward.
  pub fn show(&mut self, mtm: MainThreadMarker) {
    let previous = self
      .ids
      .get(self.recordings.indexOfSelectedItem() as usize)
      .cloned();
    self.ids = paths::list_recordings(50)
      .into_iter()
      .filter(|r| !r.recording)
      .map(|r| r.id)
      .collect();
    self.recordings.removeAllItems();
    for id in &self.ids {
      self.recordings.addItemWithTitle(&NSString::from_str(id));
    }
    let keep = previous
      .and_then(|p| self.ids.iter().position(|i| *i == p))
      .unwrap_or(0);
    self.recordings.selectItemAtIndex(keep as isize);
    self.choice = None;
    self.window.center();
    // `activate()` does not bring a menu-bar-only app forward, so its window would open
    // behind the frontmost app.
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    self.window.orderFrontRegardless();
    self.window.makeKeyAndOrderFront(None);
  }

  pub fn is_visible(&self) -> bool {
    self.window.isVisible()
  }

  /// Call often while the window is visible: it reads the controls, loops the preview and
  /// collects estimate and export results.
  pub fn tick(&mut self, mtm: MainThreadMarker) {
    if !self.is_visible() {
      if let Some(l) = &self.loaded {
        unsafe { l.player.pause() };
      }
      return;
    }
    self.load_if_chosen(mtm);
    self.sync_detail_popup();
    self.play_range();
    self.track_settings();
    self.collect();
    if self.target.ivars().replace(false) {
      self.start_export();
    }
  }

  fn load_if_chosen(&mut self, mtm: MainThreadMarker) {
    let choice = (
      self.recordings.indexOfSelectedItem(),
      self.source.selectedSegment(),
    );
    if self.choice == Some(choice) {
      return;
    }
    self.choice = Some(choice);
    if let Some(l) = self.loaded.take() {
      unsafe { l.player.pause() };
    }
    self.range.reset();
    let Some(id) = self.ids.get(choice.0 as usize) else {
      self
        .status
        .setStringValue(&NSString::from_str("No recordings yet."));
      return;
    };
    let dir = paths::recordings().join(id);
    let Some(src) = source_file(&dir, choice.1) else {
      let which = SOURCES[choice.1.clamp(0, 2) as usize].to_lowercase();
      self.status.setStringValue(&NSString::from_str(&format!(
        "This recording has no {which} video."
      )));
      unsafe { self.player_view.setPlayer(None) };
      return;
    };
    let duration = match export::duration(&src) {
      Ok(d) => d,
      Err(e) => {
        self
          .status
          .setStringValue(&NSString::from_str(&e.to_string()));
        return;
      }
    };
    let url = NSURL::fileURLWithPath(&NSString::from_str(&src.to_string_lossy()));
    let player = unsafe { AVPlayer::playerWithURL(&url, mtm) };
    unsafe {
      player.setMuted(true);
      self.player_view.setPlayer(Some(&player));
      player.play();
    }
    {
      let mut scrub = self.scrub.borrow_mut();
      scrub.load(&url, mtm);
      scrub.attach(&self.window);
    }
    self.duration.set(duration);
    self.status.setStringValue(&NSString::from_str(""));
    self.loaded = Some(Loaded {
      dir,
      src,
      duration,
      seeker: Seeker::new(player.clone()),
      player,
    });
  }

  /// The last popup shows colours for GIF and quality for WebP.
  fn sync_detail_popup(&mut self) {
    let format = if self.format.selectedSegment() == 1 {
      Format::Webp
    } else {
      Format::Gif
    };
    if format == self.detail_format {
      return;
    }
    self.detail_format = format;
    self.detail.removeAllItems();
    let (items, pick): (Vec<String>, isize) = match format {
      Format::Gif => (COLORS.map(|c| format!("{c} colours")).to_vec(), 1),
      Format::Webp => (QUALITY.map(|q| format!("quality {q}")).to_vec(), 1),
    };
    for i in &items {
      self.detail.addItemWithTitle(&NSString::from_str(i));
    }
    self.detail.selectItemAtIndex(pick);
  }

  fn speed_value(&self) -> f64 {
    (self.speed.doubleValue() * 4.0).round() / 4.0
  }

  fn range_seconds(&self) -> Option<(f64, f64)> {
    let l = self.loaded.as_ref()?;
    let iv = self.range.ivars();
    Some((iv.from.get() * l.duration, iv.to.get() * l.duration))
  }

  /// While a handle is dragged the preview shows that handle's frame. Otherwise it loops the
  /// chosen range at the chosen speed.
  fn play_range(&mut self) {
    let Some((from, to)) = self.range_seconds() else {
      return;
    };
    let speed = self.speed_value();
    let grab = self.range.ivars().grabbed.get();
    let Some(l) = &mut self.loaded else { return };
    match grab {
      Grab::Start | Grab::Middle | Grab::End => {
        unsafe { l.player.pause() };
        l.seeker.aim(if grab == Grab::Start { from } else { to });
        self.scrub.borrow_mut().refresh();
      }
      Grab::None => {
        l.seeker.release();
        let now = unsafe { l.player.currentTime().seconds() };
        if self.last_grab != Grab::None || now >= to || now < from {
          seek(&l.player, from);
        }
        unsafe { l.player.setRate(speed as f32) };
        let played = if to > from {
          (now - from) / (to - from)
        } else {
          0.0
        };
        self.played.setDoubleValue(played.clamp(0.0, 1.0));
      }
    }
    self.last_grab = grab;
    self
      .speed_label
      .setStringValue(&NSString::from_str(&format!("{speed:.2}x")));
    self
      .range_label
      .setStringValue(&NSString::from_str(&format!(
        "{from:.1}s to {to:.1}s of {:.1}s, plays for {:.1}s",
        l.duration,
        (to - from) / speed
      )));
  }

  fn current_settings(&self) -> Option<Settings> {
    let l = self.loaded.as_ref()?;
    let (from, to) = self.range_seconds()?;
    let pick = |p: &NSPopUpButton, values: &[u32]| {
      values[(p.indexOfSelectedItem().max(0) as usize).min(values.len() - 1)]
    };
    Some(Settings {
      src: l.src.clone(),
      from,
      to,
      speed: self.speed_value(),
      fps: pick(&self.fps, &FPS),
      width: pick(&self.width, &WIDTHS),
      detail: match self.detail_format {
        Format::Gif => pick(&self.detail, &COLORS),
        Format::Webp => pick(&self.detail, &QUALITY),
      },
      format: self.detail_format,
    })
  }

  fn track_settings(&mut self) {
    let now = self.current_settings();
    if now != self.settings {
      self.settings = now;
      self.changed_at = Instant::now();
      if self.settings.is_some() {
        self
          .estimate_label
          .setStringValue(&NSString::from_str("estimating…"));
      }
      return;
    }
    let (Some(settings), Some(l)) = (&self.settings, &self.loaded) else {
      return;
    };
    let settled = self.changed_at.elapsed() >= ESTIMATE_AFTER;
    if settled
      && self.estimate.is_none()
      && self.estimated.as_ref() != Some(settings)
      && self.range.ivars().grabbed.get() == Grab::None
    {
      let (tx, rx) = mpsc::channel();
      let spec = settings.spec(PathBuf::new());
      let scratch = l.dir.join(".gif-estimate");
      thread::spawn(move || {
        let r = export::estimate(&spec, &scratch).map_err(|e| e.to_string());
        let _ = std::fs::remove_dir_all(&scratch);
        let _ = tx.send(r);
      });
      self.estimate = Some((settings.clone(), rx));
    }
  }

  fn collect(&mut self) {
    if let Some((settings, rx)) = &self.estimate
      && let Ok(result) = rx.try_recv()
    {
      let current = self.settings.as_ref() == Some(settings);
      if current {
        let text = match &result {
          Ok(bytes) => format!("about {}", megabytes(*bytes)),
          Err(e) => format!("estimate failed: {e}"),
        };
        self
          .estimate_label
          .setStringValue(&NSString::from_str(&text));
        self.estimated = Some(settings.clone());
      }
      self.estimate = None;
    }

    let Some((progress, rx)) = &self.export else {
      return;
    };
    match rx.try_recv() {
      Err(_) => {
        let p = *progress.lock().unwrap();
        self
          .status
          .setStringValue(&NSString::from_str(&format!("Exporting {:.0}%", p * 100.0)));
      }
      Ok(result) => {
        self.export = None;
        self.export_button.setEnabled(true);
        match result {
          Ok((path, bytes)) => {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
            let name = name.unwrap_or_default();
            self.status.setStringValue(&NSString::from_str(&format!(
              "Saved {name}, {}",
              megabytes(bytes)
            )));
            log(format!("exported {}", path.display()));
            notify::post("export", "Export finished", &name);
            let _ = Command::new("open").arg("-R").arg(&path).status();
          }
          Err(e) => {
            self
              .status
              .setStringValue(&NSString::from_str(&format!("Export failed: {e}")));
            log(format!("export failed: {e}"));
          }
        }
      }
    }
  }

  fn start_export(&mut self) {
    if self.export.is_some() {
      return;
    }
    let (Some(settings), Some(l)) = (&self.settings, &self.loaded) else {
      return;
    };
    let out = unused_output(&l.dir, settings.format);
    let spec = settings.spec(out.clone());
    let progress = Arc::new(Mutex::new(0.0));
    let (tx, rx) = mpsc::channel();
    let shared = Arc::clone(&progress);
    thread::spawn(move || {
      let r = export::export(&spec, &mut |p| *shared.lock().unwrap() = p)
        .map(|bytes| (out, bytes))
        .map_err(|e| e.to_string());
      let _ = tx.send(r);
    });
    self.export_button.setEnabled(false);
    self.export = Some((progress, rx));
  }
}
