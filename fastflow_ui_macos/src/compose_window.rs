//! "Compose…": pick parts of a recording and turn them into videos and animations.
//!
//! The player shows `scrub.mp4`, the whole recording paced and framed as a render would be, so
//! the slice track runs in output time. Slices are saved in source seconds. An output plays a
//! list of slices in order as an mp4, GIF or WebP. An empty list plays the whole recording.
//! Render queues it. Everything is kept in the recording's `slices.toml`.
//!
//! Controls are read on each app tick rather than through target-action, so the window needs no
//! callbacks beyond its buttons.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use block2::RcBlock;
use fastflow_core::slices::{Format, Output, Slice, Slices};
use fastflow_core::timeline::Timeline;
use fastflow_daemon::paths;
use fastflow_daemon::protocol::Job;
use fastflow_render::export::{self, ExportSpec};
use fastflow_render::job;
use fibre::oneshot;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
  NSApplication, NSBackingStoreType, NSBezierPath, NSButton, NSColor, NSEvent, NSFont,
  NSFontAttributeName, NSForegroundColorAttributeName, NSGraphicsContext, NSLayoutAttribute,
  NSPopUpButton, NSProgressIndicator, NSProgressIndicatorStyle, NSSegmentSwitchTracking,
  NSSegmentedControl, NSStackView, NSStringDrawing, NSTextField, NSUserInterfaceLayoutOrientation,
  NSView, NSWindow, NSWindowOrderingMode, NSWindowStyleMask,
};
use objc2_av_foundation::AVPlayer;
use objc2_av_kit::{AVPlayerView, AVPlayerViewControlsStyle};
use objc2_core_media::{CMTime, kCMTimeZero};
use objc2_foundation::{
  NSArray, NSDictionary, NSEdgeInsets, NSObject, NSPoint, NSRect, NSSize, NSString, NSURL,
};

use crate::applog::log;
use crate::list_table::{Kind, ListEvent, ListTable};
use crate::track::{Drag, Hit, Layout, apply, lanes};

const PLAYER_SIZE: (f64, f64) = (640.0, 360.0);
const RANGE_HEIGHT: f64 = 44.0;
const NAME_WIDTH: f64 = 140.0;
/// The slice list and the play list. Together with the gap they span the player's width.
const LIST_SIZES: ((f64, f64), (f64, f64)) = ((390.0, 130.0), (240.0, 130.0));
/// How long settings must stay unchanged before a new size estimate starts.
const ESTIMATE_AFTER: Duration = Duration::from_millis(500);

const FORMATS: [Format; 3] = [Format::Mp4, Format::Gif, Format::Webp];
/// 0 is the default: the config's for an mp4, 800 and 15 for an animation.
const WIDTHS: [u32; 7] = [0, 480, 640, 800, 1024, 1280, 1920];
const FPS: [u32; 8] = [0, 10, 12, 15, 20, 24, 30, 60];
const DISSOLVES: [u32; 4] = [0, 200, 400, 700];
const COLORS: [u32; 3] = [64, 128, 256];
const QUALITY: [u32; 4] = [50, 70, 85, 95];

type OnDrag = Box<dyn Fn(&SliceTrack)>;

/// A slice as the track draws it.
#[derive(Debug, Clone, PartialEq)]
pub struct Band {
  name: String,
  /// Fractions of the scrub file's length.
  range: Range<f64>,
  /// 1-based positions in the chosen output's playing order.
  order: Vec<usize>,
  /// False when an output is chosen and does not play this slice.
  used: bool,
}

/// What the track asks the window to do. Read on the next tick.
#[derive(Debug, Clone, PartialEq)]
pub enum TrackEvent {
  Select(Option<String>),
  /// Play from this fraction of the whole recording.
  Seek(f64),
  Created(Range<f64>),
  Changed(String, Range<f64>),
  Delete(String),
  /// Append the slice to the chosen output.
  Use(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Held {
  drag: Drag,
  band: Option<usize>,
}

pub struct TrackIvars {
  bands: RefCell<Vec<Band>>,
  lanes: RefCell<(Vec<usize>, usize)>,
  selected: Cell<Option<usize>>,
  held: Cell<Option<Held>>,
  /// The held band's range while it is dragged.
  live: RefCell<Range<f64>>,
  pointer: Cell<f64>,
  moved: Cell<bool>,
  events: RefCell<Vec<TrackEvent>>,
  /// Runs inside the mouse handlers, so feedback does not wait for the app's next tick.
  on_drag: RefCell<Option<OnDrag>>,
}

const BAND_COLOURS: usize = 6;
/// Pixels a press must move before it makes a new slice rather than a seek.
const CREATE_AFTER: f64 = 3.0;
const DELETE_KEYS: [u16; 2] = [51, 117];

define_class!(
    // SAFETY: NSView has no subclassing requirements for the overridden methods and SliceTrack
    // does not implement Drop.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "FastflowSliceTrack"]
    #[ivars = TrackIvars]
    pub struct SliceTrack;

    impl SliceTrack {
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if let Some(w) = self.window() {
                w.makeFirstResponder(Some(self));
            }
            let p = self.local(event);
            let layout = self.layout();
            let iv = self.ivars();
            let f = layout.fraction(p.x);
            let hit = {
                let bands = iv.bands.borrow();
                let ranges: Vec<Range<f64>> = bands.iter().map(|b| b.range.clone()).collect();
                layout.hit(&ranges, &iv.lanes.borrow().0, iv.selected.get(), p.x, p.y)
            };
            let band = match hit {
                Hit::Start(i) | Hit::End(i) | Hit::Body(i) => Some(i),
                Hit::Empty => None,
            };
            if event.clickCount() >= 2 && let Some(i) = band {
                let name = iv.bands.borrow()[i].name.clone();
                iv.events.borrow_mut().push(TrackEvent::Use(name));
                return;
            }
            if band != iv.selected.get() && band.is_some() {
                iv.selected.set(band);
                let name = band.map(|i| iv.bands.borrow()[i].name.clone());
                iv.events.borrow_mut().push(TrackEvent::Select(name));
            }
            let range = band.map_or(f..f, |i| iv.bands.borrow()[i].range.clone());
            let drag = match hit {
                Hit::Empty => Drag::Create { anchor: f },
                Hit::Start(_) => Drag::Start,
                Hit::End(_) => Drag::End,
                Hit::Body(_) => Drag::Move { offset: f - range.start },
            };
            *iv.live.borrow_mut() = range;
            iv.held.set(Some(Held { drag, band }));
            iv.pointer.set(f);
            iv.moved.set(false);
            self.setNeedsDisplay(true);
            self.notify();
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let iv = self.ivars();
            let Some(held) = iv.held.get() else { return };
            let layout = self.layout();
            let f = layout.fraction(self.local(event).x);
            let next = apply(held.drag, iv.live.borrow().clone(), f);
            let moved = match held.drag {
                Drag::Create { .. } => (next.end - next.start) * layout.width >= CREATE_AFTER,
                _ => held.band.is_some_and(|i| iv.bands.borrow()[i].range != next),
            };
            iv.moved.set(iv.moved.get() || moved);
            *iv.live.borrow_mut() = next;
            iv.pointer.set(f);
            self.setNeedsDisplay(true);
            self.notify();
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            let iv = self.ivars();
            let Some(held) = iv.held.take() else { return };
            let live = iv.live.borrow().clone();
            let event = match (held.drag, held.band) {
                (Drag::Create { .. }, _) if iv.moved.get() => Some(TrackEvent::Created(live)),
                (Drag::Create { anchor }, _) => {
                    if iv.selected.take().is_some() {
                        iv.events.borrow_mut().push(TrackEvent::Select(None));
                    }
                    Some(TrackEvent::Seek(anchor))
                }
                (_, Some(i)) if iv.moved.get() => {
                    let mut bands = iv.bands.borrow_mut();
                    bands[i].range = live.clone();
                    Some(TrackEvent::Changed(bands[i].name.clone(), live))
                }
                _ => None,
            };
            if let Some(e) = event {
                iv.events.borrow_mut().push(e);
            }
            self.setNeedsDisplay(true);
            self.notify();
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let iv = self.ivars();
            if DELETE_KEYS.contains(&event.keyCode()) && let Some(i) = iv.selected.get() {
                let name = iv.bands.borrow()[i].name.clone();
                iv.events.borrow_mut().push(TrackEvent::Delete(name));
                return;
            }
            unsafe { msg_send![super(self), keyDown: event] }
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }
    }
);

impl SliceTrack {
  fn new(mtm: MainThreadMarker) -> Retained<Self> {
    let this = Self::alloc(mtm).set_ivars(TrackIvars {
      bands: RefCell::new(Vec::new()),
      lanes: RefCell::new((Vec::new(), 1)),
      selected: Cell::new(None),
      held: Cell::new(None),
      live: RefCell::new(0.0..0.0),
      pointer: Cell::new(0.0),
      moved: Cell::new(false),
      events: RefCell::new(Vec::new()),
      on_drag: RefCell::new(None),
    });
    let frame = NSRect::new(
      NSPoint::new(0.0, 0.0),
      NSSize::new(PLAYER_SIZE.0, RANGE_HEIGHT),
    );
    unsafe { msg_send![super(this), initWithFrame: frame] }
  }

  fn local(&self, event: &NSEvent) -> NSPoint {
    self.convertPoint_fromView(event.locationInWindow(), None)
  }

  fn layout(&self) -> Layout {
    let b = self.bounds();
    Layout {
      x0: 0.0,
      width: b.size.width,
      height: b.size.height,
      lanes: self.ivars().lanes.borrow().1,
    }
  }

  fn notify(&self) {
    if let Some(f) = self.ivars().on_drag.borrow().as_ref() {
      f(self);
    }
  }

  /// Replaces the bands unless one is being dragged. `selected` indexes `bands`.
  fn set_bands(&self, bands: Vec<Band>, selected: Option<usize>) {
    let iv = self.ivars();
    if iv.held.get().is_some() || (*iv.bands.borrow() == bands && iv.selected.get() == selected) {
      return;
    }
    let ranges: Vec<Range<f64>> = bands.iter().map(|b| b.range.clone()).collect();
    *iv.lanes.borrow_mut() = lanes(&ranges);
    *iv.bands.borrow_mut() = bands;
    iv.selected.set(selected);
    self.setNeedsDisplay(true);
  }

  fn take_events(&self) -> Vec<TrackEvent> {
    std::mem::take(&mut *self.ivars().events.borrow_mut())
  }

  /// The frame a drag is showing and what to call it, once the drag has moved something.
  fn dragged_at(&self) -> Option<(f64, &'static str)> {
    let iv = self.ivars();
    let held = iv.held.get()?;
    let live = iv.live.borrow();
    match held.drag {
      Drag::Create { .. } if !iv.moved.get() => None,
      Drag::Create { anchor } if iv.pointer.get() < anchor => Some((live.start, "start")),
      Drag::Create { .. } | Drag::End => Some((live.end, "end")),
      Drag::Start | Drag::Move { .. } => Some((live.start, "start")),
    }
  }

  /// The selected slice's range, following a drag while it lasts.
  fn active_range(&self) -> Option<Range<f64>> {
    let iv = self.ivars();
    let selected = iv.selected.get()?;
    match iv.held.get() {
      Some(Held { band: Some(i), .. }) if i == selected => Some(iv.live.borrow().clone()),
      _ => iv.bands.borrow().get(selected).map(|b| b.range.clone()),
    }
  }

  /// The point above `fraction` on the track's top edge, in screen coordinates.
  fn point_on_screen(&self, fraction: f64) -> Option<NSPoint> {
    let b = self.bounds();
    let local = NSRect::new(
      NSPoint::new(self.layout().x(fraction), b.size.height),
      NSSize::new(0.0, 0.0),
    );
    let window = self.window()?;
    let r = window.convertRectToScreen(self.convertRect_toView(local, None));
    Some(r.origin)
  }

  fn draw(&self) {
    let iv = self.ivars();
    let layout = self.layout();
    let b = self.bounds();
    let back = NSRect::new(NSPoint::new(0.0, 0.0), b.size);
    NSColor::quaternaryLabelColor().setFill();
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(back, 4.0, 4.0).fill();

    let bands = iv.bands.borrow();
    let lanes = iv.lanes.borrow();
    let held = iv.held.get();
    let selected = iv.selected.get();
    let font = NSFont::systemFontOfSize_weight(10.0, 0.3);
    let rect_of = |range: &Range<f64>, lane: usize| {
      let (y, h) = layout.lane_rect(lane);
      let (a, c) = (layout.x(range.start), layout.x(range.end));
      NSRect::new(
        NSPoint::new(a, y + 1.0),
        NSSize::new((c - a).max(2.0), h - 2.0),
      )
    };
    let mut order: Vec<usize> = (0..bands.len()).filter(|&i| Some(i) != selected).collect();
    order.extend(selected);
    for i in order {
      let band = &bands[i];
      let range = match held {
        Some(Held { band: Some(h), .. }) if h == i => iv.live.borrow().clone(),
        _ => band.range.clone(),
      };
      let r = rect_of(&range, lanes.0.get(i).copied().unwrap_or(0));
      let alpha = if band.used { 0.9 } else { 0.35 };
      band_colour(i).colorWithAlphaComponent(alpha).setFill();
      let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, 3.0, 3.0);
      path.fill();
      if Some(i) == selected {
        NSColor::labelColor().setStroke();
        path.setLineWidth(2.0);
        path.stroke();
        NSColor::whiteColor().setFill();
        for x in [r.origin.x, r.origin.x + r.size.width - 3.0] {
          let knob = NSRect::new(
            NSPoint::new(x, r.origin.y + 2.0),
            NSSize::new(3.0, (r.size.height - 4.0).max(1.0)),
          );
          NSBezierPath::fillRect(knob);
        }
      }
      let text = if band.order.is_empty() {
        band.name.clone()
      } else {
        let order: Vec<String> = band.order.iter().map(|o| o.to_string()).collect();
        format!("{} · {}", band.name, order.join(","))
      };
      draw_label(&text, &font, r);
    }
    if let Some(Held {
      drag: Drag::Create { .. },
      ..
    }) = held
      && iv.moved.get()
    {
      let lane = layout.lane_at(b.size.height / 2.0);
      let r = rect_of(&iv.live.borrow(), lane);
      NSColor::controlAccentColor()
        .colorWithAlphaComponent(0.6)
        .setFill();
      NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, 3.0, 3.0).fill();
    }
  }
}

fn band_colour(i: usize) -> Retained<NSColor> {
  match i % BAND_COLOURS {
    0 => NSColor::systemBlueColor(),
    1 => NSColor::systemOrangeColor(),
    2 => NSColor::systemGreenColor(),
    3 => NSColor::systemPinkColor(),
    4 => NSColor::systemPurpleColor(),
    _ => NSColor::systemTealColor(),
  }
}

/// Draws `text` inside `r` when it fits, clipped to its width otherwise.
fn draw_label(text: &str, font: &NSFont, r: NSRect) {
  let colour = NSColor::whiteColor();
  let keys = unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] };
  let values: [&AnyObject; 2] = [font, &colour];
  let attrs = NSDictionary::from_slices(&keys, &values);
  let s = NSString::from_str(text);
  let size = unsafe { s.sizeWithAttributes(Some(&attrs)) };
  if r.size.width < 12.0 || r.size.height < size.height - 2.0 {
    return;
  }
  NSGraphicsContext::saveGraphicsState_class();
  NSBezierPath::clipRect(r);
  let at = NSPoint::new(
    r.origin.x + 4.0,
    r.origin.y + (r.size.height - size.height) / 2.0,
  );
  unsafe { s.drawAtPoint_withAttributes(at, Some(&attrs)) };
  NSGraphicsContext::restoreGraphicsState_class();
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

fn column(mtm: MainThreadMarker, views: &[Retained<NSView>]) -> Retained<NSView> {
  let s = NSStackView::stackViewWithViews(&NSArray::from_retained_slice(views), mtm);
  s.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
  s.setSpacing(4.0);
  s.setAlignment(NSLayoutAttribute::Leading);
  view(&s)
}

fn row(mtm: MainThreadMarker, views: &[Retained<NSView>]) -> Retained<NSView> {
  let s = NSStackView::stackViewWithViews(&NSArray::from_retained_slice(views), mtm);
  s.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
  s.setSpacing(10.0);
  s.setAlignment(NSLayoutAttribute::CenterY);
  view(&s)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pressed {
  NewOutput,
  DeleteOutput,
  Play,
  Render,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and ComposeTarget does not implement
    // Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FastflowComposeTarget"]
    #[ivars = RefCell<Vec<Pressed>>]
    pub struct ComposeTarget;

    impl ComposeTarget {
        #[unsafe(method(newOutput:))]
        fn new_output(&self, _sender: Option<&AnyObject>) {
            self.ivars().borrow_mut().push(Pressed::NewOutput);
        }

        #[unsafe(method(deleteOutput:))]
        fn delete_output(&self, _sender: Option<&AnyObject>) {
            self.ivars().borrow_mut().push(Pressed::DeleteOutput);
        }

        #[unsafe(method(play:))]
        fn play(&self, _sender: Option<&AnyObject>) {
            self.ivars().borrow_mut().push(Pressed::Play);
        }

        #[unsafe(method(render:))]
        fn render(&self, _sender: Option<&AnyObject>) {
            self.ivars().borrow_mut().push(Pressed::Render);
        }
    }
);

impl ComposeTarget {
  fn new(mtm: MainThreadMarker) -> Retained<Self> {
    let this = Self::alloc(mtm).set_ivars(RefCell::new(Vec::new()));
    unsafe { msg_send![super(this), init] }
  }
}

/// A recording open in the window.
struct Loaded {
  id: String,
  dir: PathBuf,
  timeline: Timeline,
  /// Output seconds of the whole recording, which is the scrub file's length.
  length: f64,
  slices: Slices,
  /// Present once the scrub file is current.
  player: Option<(Retained<AVPlayer>, Seeker)>,
}

impl Loaded {
  fn to_source(&self, fraction: f64) -> f64 {
    self.timeline.src_time_at(fraction * self.length)
  }

  fn to_fraction(&self, src_t: f64) -> f64 {
    if self.length > 0.0 {
      self.timeline.out_time_at(src_t) / self.length
    } else {
      0.0
    }
  }

  /// Full-timeline output ranges an output plays, in order.
  fn windows(&self, output: &Output) -> Vec<Range<f64>> {
    (self
      .slices
      .projection(&self.timeline, output)
      .windows()
      .iter())
    .map(|w| w.full.clone())
    .collect()
  }

  fn duration(&self, output: &Output) -> f64 {
    self.slices.projection(&self.timeline, output).duration()
  }

  /// Source seconds of the named slice.
  fn source_length(&self, name: &str) -> f64 {
    self.slices.slices.get(name).map_or(0.0, |s| s.to - s.from)
  }
}

enum Mode {
  /// Loops the selected slice. With none selected it plays the whole recording.
  Range,
  /// Plays an output's slices one after another.
  Output {
    windows: Vec<Range<f64>>,
    i: usize,
    jumped: Instant,
  },
}

/// What an estimate was made for. A change starts a new one.
#[derive(Debug, Clone, PartialEq)]
struct EstimateKey {
  output: Output,
  windows: Vec<Range<f64>>,
}

pub struct ComposeWindow {
  window: Retained<NSWindow>,
  player_view: Retained<AVPlayerView>,
  recordings: Retained<NSPopUpButton>,
  ids: Vec<String>,
  range: Retained<SliceTrack>,
  range_label: Retained<NSTextField>,
  played: Retained<NSProgressIndicator>,
  slice_table: Retained<ListTable>,
  /// The slice the table showed selected last tick.
  table_selected: Option<String>,
  play_table: Retained<ListTable>,
  plays_label: Retained<NSTextField>,
  output_list: Retained<NSPopUpButton>,
  output_name: Retained<NSTextField>,
  format: Retained<NSSegmentedControl>,
  width: Retained<NSPopUpButton>,
  fps: Retained<NSPopUpButton>,
  dissolve: Retained<NSPopUpButton>,
  detail: Retained<NSPopUpButton>,
  detail_format: Format,
  play_button: Retained<NSButton>,
  summary: Retained<NSTextField>,
  status: Retained<NSTextField>,
  target: Retained<ComposeTarget>,

  choice: Option<isize>,
  loaded: Option<Loaded>,
  selected: Option<String>,
  shown_output: Option<isize>,
  mode: Mode,
  /// Seek to the start of the playing range on the next tick.
  reseek: bool,
  jobs: Vec<Job>,
  estimate_key: Option<EstimateKey>,
  changed_at: Instant,
  estimated: Option<EstimateKey>,
  estimate: Option<(EstimateKey, oneshot::Receiver<Result<u64, String>>)>,
  scrub: Rc<RefCell<Scrub>>,
  duration: Rc<Cell<f64>>,
}

fn text_field(mtm: MainThreadMarker, width: f64, placeholder: &str) -> Retained<NSTextField> {
  let t = NSTextField::textFieldWithString(&NSString::from_str(""), mtm);
  t.setPlaceholderString(Some(&NSString::from_str(placeholder)));
  t.widthAnchor()
    .constraintEqualToConstant(width)
    .setActive(true);
  t
}

fn button(
  mtm: MainThreadMarker,
  title: &str,
  target: &ComposeTarget,
  action: objc2::runtime::Sel,
) -> Retained<NSButton> {
  unsafe {
    NSButton::buttonWithTitle_target_action(
      &NSString::from_str(title),
      Some(target),
      Some(action),
      mtm,
    )
  }
}

fn pick(p: &NSPopUpButton, values: &[u32]) -> u32 {
  values[(p.indexOfSelectedItem().max(0) as usize).min(values.len() - 1)]
}

fn select_value(p: &NSPopUpButton, values: &[u32], v: u32) {
  let i = values.iter().position(|&x| x == v).unwrap_or(0);
  p.selectItemAtIndex(i as isize);
}

fn string(t: &NSTextField) -> String {
  t.stringValue().to_string().trim().to_owned()
}

fn set(t: &NSTextField, text: &str) {
  t.setStringValue(&NSString::from_str(text));
}

/// `base-1`, `base-2` and so on, the first not already taken.
fn unused_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
  (1..)
    .map(|n| format!("{base}-{n}"))
    .find(|n| !taken(n))
    .expect("an unused name exists")
}

impl ComposeWindow {
  pub fn new(mtm: MainThreadMarker) -> ComposeWindow {
    let target = ComposeTarget::new(mtm);

    let recordings = popup(mtm, &[], 0);

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

    let range = SliceTrack::new(mtm);
    let scrub = Rc::new(RefCell::new(Scrub::new(mtm)));
    let duration = Rc::new(Cell::new(0.0));
    {
      let (scrub, duration) = (Rc::clone(&scrub), Rc::clone(&duration));
      *range.ivars().on_drag.borrow_mut() = Some(Box::new(move |r: &SliceTrack| {
        let d = duration.get();
        let Some(parent) = r.window() else { return };
        let mut scrub = scrub.borrow_mut();
        let Some((fraction, caption)) = r.dragged_at() else {
          return scrub.hide();
        };
        if d > 0.0
          && let Some(at) = r.point_on_screen(fraction)
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

    let slice_table = ListTable::new(
      mtm,
      Kind::Slices,
      &[
        ("Slice", 150.0),
        ("Start", 60.0),
        ("End", 60.0),
        ("Length", 60.0),
      ],
    );
    let play_table = ListTable::new(mtm, Kind::Plays, &[("Slice", 150.0), ("Length", 60.0)]);
    let plays_label = label(mtm, "");
    let slices_column = column(
      mtm,
      &[
        view(&label(mtm, "Slices")),
        view(&ListTable::scrolled(&slice_table, mtm, LIST_SIZES.0)),
      ],
    );
    let plays_column = column(
      mtm,
      &[
        view(&plays_label),
        view(&ListTable::scrolled(&play_table, mtm, LIST_SIZES.1)),
      ],
    );

    let output_list = popup(mtm, &[], 0);
    let new_output = button(mtm, "New Output", &target, sel!(newOutput:));
    let delete_output = button(mtm, "Delete", &target, sel!(deleteOutput:));
    let output_name = text_field(mtm, NAME_WIDTH, "output name");

    let format = segmented(mtm, &["MP4", "GIF", "WebP"]);
    let width = popup(
      mtm,
      &WIDTHS.map(|w| match w {
        0 => "default width".to_owned(),
        w => format!("{w} px"),
      }),
      0,
    );
    let fps = popup(
      mtm,
      &FPS.map(|f| match f {
        0 => "default fps".to_owned(),
        f => format!("{f} fps"),
      }),
      0,
    );
    let dissolve = popup(
      mtm,
      &DISSOLVES.map(|d| match d {
        0 => "hard cuts".to_owned(),
        d => format!("{d} ms dissolve"),
      }),
      0,
    );
    let detail = popup(mtm, &COLORS.map(|c| format!("{c} colours")), 1);

    let play_button = button(mtm, "Play Output", &target, sel!(play:));
    let render_button = button(mtm, "Render", &target, sel!(render:));
    let summary = label(mtm, "");
    summary.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
    let status = label(mtm, "");

    let rows = [
      row(mtm, &[view(&label(mtm, "Recording")), view(&recordings)]),
      view(&player_view),
      view(&played),
      view(&range),
      view(&range_label),
      row(
        mtm,
        &[
          view(&label(mtm, "Output")),
          view(&output_list),
          view(&new_output),
          view(&delete_output),
          view(&label(mtm, "Name")),
          view(&output_name),
        ],
      ),
      row(mtm, &[slices_column, plays_column]),
      row(
        mtm,
        &[
          view(&format),
          view(&width),
          view(&fps),
          view(&dissolve),
          view(&detail),
        ],
      ),
      row(
        mtm,
        &[view(&play_button), view(&render_button), view(&summary)],
      ),
      view(&status),
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
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(720.0, 760.0)),
        NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
        NSBackingStoreType::Buffered,
        false,
      )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("Compose"));
    window.setContentView(Some(&stack));
    window.setContentSize(stack.fittingSize());

    ComposeWindow {
      window,
      player_view,
      recordings,
      ids: Vec::new(),
      range,
      range_label,
      played,
      slice_table,
      table_selected: None,
      play_table,
      plays_label,
      output_list,
      output_name,
      format,
      width,
      fps,
      dissolve,
      detail,
      detail_format: Format::Gif,
      play_button,
      summary,
      status,
      target,
      choice: None,
      loaded: None,
      selected: None,
      shown_output: None,
      mode: Mode::Range,
      reseek: true,
      jobs: Vec::new(),
      estimate_key: None,
      changed_at: Instant::now(),
      estimated: None,
      estimate: None,
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

  /// Picks `id` in the recording list, when it is there.
  pub fn select(&mut self, id: &str) {
    if let Some(i) = self.ids.iter().position(|r| r == id) {
      self.recordings.selectItemAtIndex(i as isize);
      self.choice = None;
    }
  }

  pub fn is_visible(&self) -> bool {
    self.window.isVisible()
  }

  /// Work the window asked for since the last call: scrub previews to build and renders.
  pub fn take_jobs(&mut self) -> Vec<Job> {
    std::mem::take(&mut self.jobs)
  }

  /// Call when the scrub preview for `id` was built or failed to build.
  pub fn preview_ready(
    &mut self,
    id: &str,
    result: &Result<String, String>,
    mtm: MainThreadMarker,
  ) {
    let Some(l) = &self.loaded else { return };
    if l.id != id || l.player.is_some() {
      return;
    }
    match result {
      Ok(_) => self.attach_player(mtm),
      Err(e) => self.say(&format!("The preview could not be built: {e}")),
    }
  }

  /// Call often while the window is visible: it reads the controls, plays the preview and
  /// collects estimates.
  pub fn tick(&mut self, mtm: MainThreadMarker) {
    if !self.is_visible() {
      if let Some((player, _)) = self.loaded.as_ref().and_then(|l| l.player.as_ref()) {
        unsafe { player.pause() };
      }
      return;
    }
    self.load_if_chosen(mtm);
    let pressed: Vec<Pressed> = self.target.ivars().borrow_mut().drain(..).collect();
    for p in pressed {
      self.on_pressed(p);
    }
    for e in self.range.take_events() {
      self.on_track(e);
    }
    for e in self.slice_table.take_events() {
      self.on_slice_list(e);
    }
    for e in self.play_table.take_events() {
      self.on_play_list(e);
    }
    self.sync_table_selection();
    self.sync_output();
    self.sync_track();
    self.sync_lists();
    self.sync_detail_popup();
    self.play();
    self.track_estimate();
    self.collect_estimate();
  }

  fn say(&self, text: &str) {
    set(&self.status, text);
  }

  fn load_if_chosen(&mut self, mtm: MainThreadMarker) {
    let choice = self.recordings.indexOfSelectedItem();
    if self.choice == Some(choice) {
      return;
    }
    self.choice = Some(choice);
    if let Some((player, _)) = self.loaded.take().and_then(|l| l.player) {
      unsafe { player.pause() };
    }
    unsafe { self.player_view.setPlayer(None) };
    self.mode = Mode::Range;
    self.select_slice(None);
    self.shown_output = None;
    self.say("");
    let Some(id) = self.ids.get(choice as usize).cloned() else {
      self.say("No recordings yet.");
      return;
    };
    let dir = paths::recordings().join(&id);
    let loaded = job::plan(&dir, &mut |_| {}).and_then(|plan| {
      let slices = job::load_slices(&dir)?;
      Ok(Loaded {
        id: id.clone(),
        length: plan.timeline.out_duration(),
        timeline: plan.timeline,
        dir: dir.clone(),
        slices,
        player: None,
      })
    });
    match loaded {
      Ok(l) => self.loaded = Some(l),
      Err(e) => {
        self.say(&e);
        self.refill_outputs(None);
        return;
      }
    }
    self.refill_outputs(None);
    if job::scrub_is_current(&dir) {
      self.attach_player(mtm);
    } else {
      self.jobs.push(Job::prepare(&id));
      self.say("Preparing the preview…");
    }
  }

  fn attach_player(&mut self, mtm: MainThreadMarker) {
    let Some(l) = &mut self.loaded else { return };
    let url = NSURL::fileURLWithPath(&NSString::from_str(
      &l.dir.join(job::SCRUB_FILE).to_string_lossy(),
    ));
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
    self.duration.set(l.length);
    l.player = Some((player.clone(), Seeker::new(player)));
    self.say("");
  }

  /// Rebuilds the output popup from the model and selects the named output.
  fn refill_outputs(&mut self, output: Option<&str>) {
    self.output_list.removeAllItems();
    self.shown_output = None;
    let Some(l) = &self.loaded else { return };
    for o in &l.slices.outputs {
      self
        .output_list
        .addItemWithTitle(&NSString::from_str(&o.file_name()));
    }
    let oi = output
      .and_then(|n| l.slices.outputs.iter().position(|o| o.name == n))
      .unwrap_or(0);
    self.output_list.selectItemAtIndex(oi as isize);
  }

  fn selected_output_name(&self) -> Option<String> {
    let i = self.selected_output()?;
    Some(self.loaded.as_ref()?.slices.outputs[i].name.clone())
  }

  /// Selects the named slice or, with `None`, the whole recording. Playing restarts at its start.
  fn select_slice(&mut self, name: Option<String>) {
    self.selected = name;
    if let Mode::Output { .. } = self.mode {
      self.stop_output();
    }
    self.reseek = true;
  }

  /// Draws the model's slices on the track, numbered by the chosen output's order.
  fn sync_track(&mut self) {
    let Some(l) = &self.loaded else {
      return self.range.set_bands(Vec::new(), None);
    };
    let output = self.selected_output().map(|i| &l.slices.outputs[i]);
    let bands: Vec<Band> = l
      .slices
      .slices
      .iter()
      .map(|(name, s)| {
        let order: Vec<usize> = output
          .map(|o| {
            (o.slices.iter().enumerate())
              .filter(|(_, n)| *n == name)
              .map(|(j, _)| j + 1)
              .collect()
          })
          .unwrap_or_default();
        Band {
          name: name.clone(),
          range: l.to_fraction(s.from)..l.to_fraction(s.to),
          used: output.is_none_or(|o| o.slices.is_empty()) || !order.is_empty(),
          order,
        }
      })
      .collect();
    let selected = (self.selected.as_ref()).and_then(|n| bands.iter().position(|b| b.name == *n));
    self.range.set_bands(bands, selected);
  }

  fn selected_output(&self) -> Option<usize> {
    let l = self.loaded.as_ref()?;
    let i = self.output_list.indexOfSelectedItem();
    (i >= 0 && (i as usize) < l.slices.outputs.len()).then_some(i as usize)
  }

  /// Writes `slices` to disk and keeps it when it is valid.
  fn commit(&mut self, slices: Slices) -> bool {
    let Some(l) = &mut self.loaded else {
      return false;
    };
    match job::save_slices(&l.dir, &slices) {
      Ok(()) => {
        l.slices = slices;
        true
      }
      Err(e) => {
        self.say(&e);
        false
      }
    }
  }

  fn on_pressed(&mut self, p: Pressed) {
    let Some(l) = &self.loaded else { return };
    let mut slices = l.slices.clone();
    let selected_output = self.selected_output();
    match p {
      Pressed::NewOutput => {
        let first = self
          .selected
          .clone()
          .or_else(|| slices.slices.keys().next().cloned());
        let Some(first) = first else {
          return self.say("Drag across the track to make a slice first.");
        };
        let name = unused_name("clip", |n| slices.output(n).is_some());
        slices.outputs.push(Output {
          name: name.clone(),
          format: Format::Gif,
          slices: vec![first],
          width: None,
          fps: None,
          colors: None,
          quality: None,
          dissolve_ms: 0,
        });
        if self.commit(slices) {
          self.refill_outputs(Some(&name));
        }
      }
      Pressed::DeleteOutput => {
        let Some(i) = selected_output else { return };
        let name = slices.outputs.remove(i).name;
        if self.commit(slices) {
          self.refill_outputs(None);
          self.say(&format!("Deleted {name}. Files it rendered are kept."));
        }
      }
      Pressed::Play => match self.mode {
        Mode::Output { .. } => self.stop_output(),
        Mode::Range => {
          let Some(i) = selected_output else { return };
          let windows = l.windows(&l.slices.outputs[i]);
          if windows.is_empty() || l.player.is_none() {
            return;
          }
          self.mode = Mode::Output {
            windows,
            i: usize::MAX,
            jumped: Instant::now(),
          };
          self.play_button.setTitle(&NSString::from_str("Stop"));
        }
      },
      Pressed::Render => {
        let Some(i) = selected_output else {
          return self.say("Make an output first.");
        };
        let name = l.slices.outputs[i].name.clone();
        let id = l.id.clone();
        self.jobs.push(Job::render(&id, Some(&name)));
        self.say(&format!(
          "Queued {}. The render panel shows its progress.",
          l.slices.outputs[i].file_name()
        ));
      }
    }
  }

  fn on_track(&mut self, e: TrackEvent) {
    let Some(l) = &self.loaded else { return };
    let mut slices = l.slices.clone();
    match e {
      TrackEvent::Select(name) => self.select_slice(name),
      TrackEvent::Seek(f) => {
        if let Some((player, _)) = &l.player {
          seek(player, f * l.length);
        }
        self.reseek = false;
      }
      TrackEvent::Created(r) => {
        let (from, to) = (l.to_source(r.start), l.to_source(r.end));
        if to - from < 0.05 {
          return self.say("That slice is too short. Drag further across the track.");
        }
        let name = unused_name("slice", |n| slices.slices.contains_key(n));
        slices.slices.insert(name.clone(), Slice { from, to });
        let output = self.selected_output_name();
        if self.commit(slices) {
          self.select_slice(Some(name.clone()));
          self.say(&match output {
            Some(o) => format!("Added {name}. Double-click it to add it to {o}."),
            None => format!("Added {name}. New Output turns it into a GIF or video."),
          });
        }
      }
      TrackEvent::Changed(name, r) => {
        let (from, to) = (l.to_source(r.start), l.to_source(r.end));
        if to - from < 0.05 {
          return self.say("A slice needs some length. Drag its edges apart.");
        }
        slices.slices.insert(name, Slice { from, to });
        if self.commit(slices) {
          self.say("");
          self.reseek = true;
          if let Mode::Output { .. } = self.mode {
            self.stop_output();
          }
          self.show_summary();
        }
      }
      TrackEvent::Delete(name) => self.remove_slice(&name),
      TrackEvent::Use(name) => {
        let Some(i) = self.selected_output() else {
          return self.say("Make an output first with New Output.");
        };
        if slices.outputs[i].slices.contains(&name) {
          self.take_out(&name);
        } else {
          let end = slices.outputs[i].slices.len();
          self.place(&name, end);
        }
      }
    }
  }

  fn on_slice_list(&mut self, e: ListEvent) {
    match e {
      ListEvent::Rename { from, to } => self.rename_slice(&from, &to),
      ListEvent::Delete(name) => self.remove_slice(&name),
      ListEvent::Place { .. } => {}
    }
  }

  fn on_play_list(&mut self, e: ListEvent) {
    match e {
      ListEvent::Place { name, at } => self.place(&name, at),
      ListEvent::Delete(name) => self.take_out(&name),
      ListEvent::Rename { .. } => {}
    }
  }

  /// Puts a slice into the chosen output before row `at`, moving it when it already plays.
  fn place(&mut self, name: &str, at: usize) {
    let (Some(i), Some(l)) = (self.selected_output(), &self.loaded) else {
      return self.say("Make an output first with New Output.");
    };
    let mut slices = l.slices.clone();
    let was_whole = slices.outputs[i].slices.is_empty();
    slices.outputs[i].place(name, at);
    let output = slices.outputs[i].name.clone();
    if self.commit(slices) {
      self.refill_outputs(Some(&output));
      self.say(&if was_whole {
        format!("{output} now plays only {name}.")
      } else {
        String::new()
      });
    }
  }

  /// Stops the chosen output playing a slice. The slice itself stays.
  fn take_out(&mut self, name: &str) {
    let (Some(i), Some(l)) = (self.selected_output(), &self.loaded) else {
      return;
    };
    let mut slices = l.slices.clone();
    slices.outputs[i].slices.retain(|n| n != name);
    let output = slices.outputs[i].name.clone();
    let whole = slices.outputs[i].slices.is_empty();
    if self.commit(slices) {
      self.refill_outputs(Some(&output));
      self.say(&if whole {
        format!("{output} plays the whole recording again.")
      } else {
        format!("{output} no longer plays {name}.")
      });
    }
  }

  fn rename_slice(&mut self, old: &str, new: &str) {
    let Some(l) = &self.loaded else { return };
    let mut slices = l.slices.clone();
    if slices.slices.contains_key(new) {
      return self.say(&format!("A slice named {new} already exists."));
    }
    let Some(s) = slices.slices.remove(old) else {
      return;
    };
    slices.slices.insert(new.to_owned(), s);
    for o in &mut slices.outputs {
      for n in &mut o.slices {
        if n == old {
          *n = new.to_owned();
        }
      }
    }
    let output = self.selected_output_name();
    if self.commit(slices) {
      if self.selected.as_deref() == Some(old) {
        self.selected = Some(new.to_owned());
      }
      self.refill_outputs(output.as_deref());
      self.say(&format!("Renamed {old} to {new}."));
    }
  }

  /// A row picked in the slice list selects that slice. A slice picked elsewhere is shown
  /// selected in the list.
  fn sync_table_selection(&mut self) {
    let picked = self.slice_table.selected_name();
    if picked != self.table_selected {
      self.table_selected = picked.clone();
      self.select_slice(picked);
    }
    if self.table_selected != self.selected {
      self.slice_table.select_name(self.selected.as_deref());
      self.table_selected = self.slice_table.selected_name();
    }
  }

  /// Fills the slice list and the chosen output's play list from the model.
  fn sync_lists(&mut self) {
    let Some(l) = &self.loaded else {
      self.slice_table.set_rows(Vec::new(), Vec::new());
      self.play_table.set_rows(Vec::new(), Vec::new());
      set(&self.plays_label, "Plays");
      return;
    };
    let secs = |v: f64| format!("{v:.1}s");
    let (rows, names): (Vec<Vec<String>>, Vec<String>) = (l.slices.slices.iter())
      .map(|(name, s)| {
        let row = vec![name.clone(), secs(s.from), secs(s.to), secs(s.to - s.from)];
        (row, name.clone())
      })
      .unzip();
    self.slice_table.set_rows(rows, names);
    let output = self.selected_output().map(|i| &l.slices.outputs[i]);
    let names = output.map_or(Vec::new(), |o| o.slices.clone());
    let rows = (names.iter())
      .map(|n| vec![n.clone(), secs(l.source_length(n))])
      .collect();
    self.play_table.set_rows(rows, names);
    let text = match output {
      None => "Plays".to_owned(),
      Some(o) if o.slices.is_empty() => {
        format!("{} plays the whole recording. Drag slices here.", o.name)
      }
      Some(o) => format!("{} plays, in order", o.name),
    };
    set(&self.plays_label, &text);
  }

  fn remove_slice(&mut self, name: &str) {
    let Some(l) = &self.loaded else { return };
    let mut slices = l.slices.clone();
    slices.slices.remove(name);
    let mut now_whole = Vec::new();
    for o in &mut slices.outputs {
      let before = o.slices.len();
      o.slices.retain(|s| s != name);
      if before > 0 && o.slices.is_empty() {
        now_whole.push(o.name.clone());
      }
    }
    let output = self.selected_output_name();
    if self.commit(slices) {
      self.select_slice(None);
      self.refill_outputs(output.as_deref());
      self.say(&if now_whole.is_empty() {
        format!("Removed {name}.")
      } else {
        format!(
          "Removed {name}. {} now plays the whole recording.",
          now_whole.join(" and ")
        )
      });
    }
  }

  fn stop_output(&mut self) {
    self.mode = Mode::Range;
    self.reseek = true;
    self
      .play_button
      .setTitle(&NSString::from_str("Play Output"));
  }

  /// A newly chosen output fills the controls. Otherwise the controls edit the chosen output.
  fn sync_output(&mut self) {
    let Some(i) = self.selected_output() else {
      set(&self.summary, "");
      return;
    };
    let index = Some(i as isize);
    let l = self.loaded.as_ref().unwrap();
    let current = l.slices.outputs[i].clone();
    if self.shown_output != index {
      self.shown_output = index;
      set(&self.output_name, &current.name);
      self.format.setSelectedSegment(
        FORMATS
          .iter()
          .position(|f| *f == current.format)
          .unwrap_or(0) as isize,
      );
      select_value(&self.width, &WIDTHS, current.width.unwrap_or(0));
      select_value(&self.fps, &FPS, current.fps.unwrap_or(0));
      select_value(&self.dissolve, &DISSOLVES, current.dissolve_ms);
      self.sync_detail_popup();
      match current.format {
        Format::Gif => select_value(&self.detail, &COLORS, current.colors.unwrap_or(128)),
        Format::Webp => select_value(&self.detail, &QUALITY, current.quality.unwrap_or(70)),
        Format::Mp4 => {}
      }
      self.show_summary();
      return;
    }

    let format = FORMATS[(self.format.selectedSegment().max(0) as usize).min(2)];
    let edited = Output {
      name: string(&self.output_name),
      format,
      slices: current.slices.clone(),
      width: Some(pick(&self.width, &WIDTHS)).filter(|&w| w > 0),
      fps: Some(pick(&self.fps, &FPS)).filter(|&f| f > 0),
      colors: (format == Format::Gif).then(|| pick(&self.detail, &COLORS)),
      quality: (format == Format::Webp).then(|| pick(&self.detail, &QUALITY)),
      dissolve_ms: pick(&self.dissolve, &DISSOLVES),
    };
    let shown = Output {
      colors: (format == Format::Gif).then(|| current.colors.unwrap_or(128)),
      quality: (format == Format::Webp).then(|| current.quality.unwrap_or(70)),
      ..current.clone()
    };
    if edited == shown {
      return;
    }
    let mut slices = l.slices.clone();
    slices.outputs[i] = edited.clone();
    if self.commit(slices) {
      self.say("");
      if let Some(item) = self.output_list.itemAtIndex(i as isize) {
        item.setTitle(&NSString::from_str(&edited.file_name()));
      }
      if let Mode::Output { .. } = self.mode {
        self.stop_output();
      }
      self.show_summary();
    }
  }

  fn show_summary(&mut self) {
    let Some(i) = self.selected_output() else {
      return;
    };
    let l = self.loaded.as_ref().unwrap();
    let output = &l.slices.outputs[i];
    let length = l.duration(output);
    let text = format!("{} plays for {length:.1}s", output.file_name());
    set(&self.summary, &text);
    self.estimate_key = Some(EstimateKey {
      output: output.clone(),
      windows: l.windows(output),
    });
    self.changed_at = Instant::now();
  }

  /// The last popup shows colours for GIF and quality for WebP. An mp4 has neither.
  fn sync_detail_popup(&mut self) {
    let format = FORMATS[(self.format.selectedSegment().max(0) as usize).min(2)];
    self.detail.setEnabled(format != Format::Mp4);
    if format == self.detail_format || format == Format::Mp4 {
      return;
    }
    self.detail_format = format;
    self.detail.removeAllItems();
    let items: Vec<String> = match format {
      Format::Webp => QUALITY.map(|q| format!("quality {q}")).to_vec(),
      _ => COLORS.map(|c| format!("{c} colours")).to_vec(),
    };
    for i in &items {
      self.detail.addItemWithTitle(&NSString::from_str(i));
    }
    self.detail.selectItemAtIndex(1);
  }

  /// While a slice is dragged the preview shows the frame under the edge being moved.
  /// Otherwise it loops the selected slice, plays the whole recording or plays the chosen
  /// output's slices in order.
  fn play(&mut self) {
    let dragged = self.range.dragged_at();
    if dragged.is_some()
      && let Mode::Output { .. } = self.mode
    {
      self.stop_output();
    }
    let Some(l) = &mut self.loaded else { return };
    let active = self.range.active_range();
    let range = active.clone().unwrap_or(0.0..1.0);
    let (from, to) = (range.start * l.length, range.end * l.length);
    let text = match (&self.selected, active) {
      (Some(name), Some(_)) => format!(
        "{name}: {from:.1}s to {to:.1}s of {:.1}s output · source {:.1}s to {:.1}s",
        l.length,
        l.timeline.src_time_at(from),
        l.timeline.src_time_at(to)
      ),
      _ => format!(
        "Whole recording, {:.1}s output. Drag across the track to make a slice.",
        l.length
      ),
    };
    set(&self.range_label, &text);
    let Some((player, seeker)) = &mut l.player else {
      return;
    };
    let now = unsafe { player.currentTime().seconds() };
    match &mut self.mode {
      Mode::Output { windows, i, jumped } => {
        seeker.release();
        let settled = jumped.elapsed() >= Duration::from_millis(150);
        let off = *i >= windows.len() || now >= windows[*i].end || now < windows[*i].start - 0.05;
        if off && (settled || *i == usize::MAX) {
          *i = if *i >= windows.len() {
            0
          } else {
            (*i + 1) % windows.len()
          };
          seek(player, windows[*i].start);
          *jumped = Instant::now();
        }
        unsafe { player.setRate(1.0) };
        let total: f64 = windows.iter().map(|w| w.end - w.start).sum();
        let i = (*i).min(windows.len() - 1);
        let done: f64 = windows[..i].iter().map(|w| w.end - w.start).sum::<f64>()
          + (now - windows[i].start).clamp(0.0, windows[i].end - windows[i].start);
        self
          .played
          .setDoubleValue((done / total.max(1e-9)).clamp(0.0, 1.0));
      }
      Mode::Range => match dragged {
        Some((fraction, _)) => {
          unsafe { player.pause() };
          seeker.aim(fraction * l.length);
          self.scrub.borrow_mut().refresh();
          self.reseek = true;
        }
        None => {
          seeker.release();
          if self.reseek || now >= to || now < from - 0.05 {
            seek(player, from);
          }
          self.reseek = false;
          unsafe { player.setRate(1.0) };
          let played = if to > from {
            (now - from) / (to - from)
          } else {
            0.0
          };
          self.played.setDoubleValue(played.clamp(0.0, 1.0));
        }
      },
    }
  }

  /// Starts a size estimate once an output's settings have stopped changing. An animation is
  /// estimated by encoding two seconds of the scrub file and scaling up. An mp4 is estimated
  /// from its bitrate.
  fn track_estimate(&mut self) {
    let (Some(key), Some(l)) = (&self.estimate_key, &self.loaded) else {
      return;
    };
    if self.estimated.as_ref() == Some(key) || self.estimate.is_some() {
      return;
    }
    let length = l.duration(&key.output);
    if !key.output.format.is_animation() {
      let bytes = (12e6 / 8.0 * length) as u64;
      let text = format!(
        "{} plays for {length:.1}s, about {}",
        key.output.file_name(),
        megabytes(bytes)
      );
      set(&self.summary, &text);
      self.estimated = Some(key.clone());
      return;
    }
    if self.changed_at.elapsed() < ESTIMATE_AFTER || l.player.is_none() {
      return;
    }
    let Some(first) = key.windows.first().cloned() else {
      return;
    };
    let width = key.output.width.unwrap_or(800);
    let sample_width = width.min(640);
    let spec = ExportSpec {
      src: l.dir.join(job::SCRUB_FILE),
      from: first.start,
      to: first.end,
      speed: 1.0,
      fps: key.output.fps.unwrap_or(15),
      width: sample_width,
      colors: key.output.colors.unwrap_or(128),
      quality: key.output.quality.unwrap_or(70),
      format: match key.output.format {
        Format::Webp => export::Format::Webp,
        _ => export::Format::Gif,
      },
      out: PathBuf::new(),
    };
    let scale =
      length / (first.end - first.start).max(1e-3) * (width as f64 / sample_width as f64).powi(2);
    let scratch = l.dir.join(".compose-estimate");
    let (tx, rx) = oneshot::oneshot();
    thread::spawn(move || {
      let r = export::estimate(&spec, &scratch)
        .map(|b| (b as f64 * scale) as u64)
        .map_err(|e| e.to_string());
      let _ = std::fs::remove_dir_all(&scratch);
      let _ = tx.send(r);
    });
    self.estimate = Some((key.clone(), rx));
  }

  fn collect_estimate(&mut self) {
    let Some((key, rx)) = &self.estimate else {
      return;
    };
    let Ok(result) = rx.try_recv() else { return };
    if self.estimate_key.as_ref() == Some(key)
      && let Some(l) = &self.loaded
    {
      let length = l.duration(&key.output);
      let size = match &result {
        Ok(bytes) => format!("about {}", megabytes(*bytes)),
        Err(e) => {
          log(format!("estimate failed: {e}"));
          "size unknown".into()
        }
      };
      set(
        &self.summary,
        &format!("{} plays for {length:.1}s, {size}", key.output.file_name()),
      );
      self.estimated = Some(key.clone());
    }
    self.estimate = None;
  }
}
