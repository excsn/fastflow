//! A small panel in the top right corner that follows the render queue: the recording being
//! rendered, its progress, the time left and how many renders wait behind it.
//!
//! It never takes focus and is kept out of screen recordings. Closing it hides it until the next
//! render starts.

use std::cell::Cell;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use fastflow_daemon::paths;
use fastflow_daemon::protocol::Finished;
use fastflow_daemon::queue::QueueStatus;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
  NSBackingStoreType, NSButton, NSColor, NSFont, NSPanel, NSProgressIndicator,
  NSProgressIndicatorStyle, NSScreen, NSStatusWindowLevel, NSTextField, NSWindowCollectionBehavior,
  NSWindowSharingType, NSWindowStyleMask,
};
use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize, NSString};

const SIZE: (f64, f64) = (340.0, 96.0);
const MARGIN: f64 = 12.0;
/// How long a finished render stays on screen.
const LINGER: Duration = Duration::from_secs(8);
/// Progress below this gives too noisy an estimate of the time left.
const ESTIMATE_FROM: f64 = 0.05;

define_class!(
    // SAFETY: NSObject has no subclassing requirements and PanelTarget does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FastflowRenderPanelTarget"]
    #[ivars = Cell<bool>]
    pub struct PanelTarget;

    impl PanelTarget {
        #[unsafe(method(open:))]
        fn open(&self, _sender: Option<&AnyObject>) {
            self.ivars().set(true);
        }
    }
);

impl PanelTarget {
  fn new(mtm: MainThreadMarker) -> Retained<Self> {
    let this = Self::alloc(mtm).set_ivars(Cell::new(false));
    unsafe { msg_send![super(this), init] }
  }
}

enum Showing {
  Nothing,
  Rendering { id: String, started: Instant },
  Done { finished: Finished, at: Instant },
}

pub struct RenderPanel {
  panel: Retained<NSPanel>,
  title: Retained<NSTextField>,
  bar: Retained<NSProgressIndicator>,
  detail: Retained<NSTextField>,
  button: Retained<NSButton>,
  target: Retained<PanelTarget>,
  showing: Showing,
}

impl RenderPanel {
  pub fn new(mtm: MainThreadMarker) -> RenderPanel {
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
      NSPanel::alloc(mtm),
      NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(SIZE.0, SIZE.1)),
      NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::UtilityWindow
        | NSWindowStyleMask::HUDWindow
        | NSWindowStyleMask::NonactivatingPanel,
      NSBackingStoreType::Buffered,
      false,
    );
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setTitle(&NSString::from_str("fastflow"));
    panel.setFloatingPanel(true);
    panel.setBecomesKeyOnlyIfNeeded(true);
    // A menu bar app is never active, so a panel that hides on deactivate would never show.
    panel.setHidesOnDeactivate(false);
    panel.setSharingType(NSWindowSharingType::None);
    panel.setLevel(NSStatusWindowLevel);
    panel.setCollectionBehavior(
      NSWindowCollectionBehavior::CanJoinAllSpaces
        | NSWindowCollectionBehavior::IgnoresCycle
        | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );

    let target = PanelTarget::new(mtm);
    let title = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    title.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
    title.setFrame(NSRect::new(
      NSPoint::new(16.0, 60.0),
      NSSize::new(SIZE.0 - 32.0, 20.0),
    ));
    let bar = NSProgressIndicator::new(mtm);
    bar.setStyle(NSProgressIndicatorStyle::Bar);
    bar.setIndeterminate(false);
    bar.setMinValue(0.0);
    bar.setMaxValue(1.0);
    bar.setFrame(NSRect::new(
      NSPoint::new(16.0, 40.0),
      NSSize::new(SIZE.0 - 32.0, 16.0),
    ));
    let detail = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    detail.setTextColor(Some(&NSColor::secondaryLabelColor()));
    detail.setFrame(NSRect::new(
      NSPoint::new(16.0, 12.0),
      NSSize::new(SIZE.0 - 150.0, 18.0),
    ));
    let button = unsafe {
      NSButton::buttonWithTitle_target_action(
        &NSString::from_str("Open Folder"),
        Some(&target),
        Some(sel!(open:)),
        mtm,
      )
    };
    button.setFrame(NSRect::new(
      NSPoint::new(SIZE.0 - 126.0, 6.0),
      NSSize::new(112.0, 28.0),
    ));
    if let Some(content) = panel.contentView() {
      content.addSubview(&title);
      content.addSubview(&bar);
      content.addSubview(&detail);
      content.addSubview(&button);
    }

    RenderPanel {
      panel,
      title,
      bar,
      detail,
      button,
      target,
      showing: Showing::Nothing,
    }
  }

  /// Call on every tick while the app runs.
  pub fn update(&mut self, s: &QueueStatus, mtm: MainThreadMarker) {
    if self.target.ivars().take() {
      self.open();
    }
    let now = Instant::now();
    if let Some(r) = &s.rendering {
      let started = match &self.showing {
        Showing::Rendering { id, started } if *id == r.id => *started,
        _ => {
          self.showing = Showing::Rendering {
            id: r.id.clone(),
            started: now,
          };
          self.present(mtm);
          now
        }
      };
      self.show_progress(&r.id, r.progress, now - started, s.queued.len());
    } else if let Showing::Rendering { id, .. } = &self.showing {
      match &s.last {
        Some(f) if f.id == *id => {
          self.show_done(f);
          self.showing = Showing::Done {
            finished: f.clone(),
            at: now,
          };
        }
        _ => self.hide(),
      }
    } else if let Showing::Done { at, .. } = &self.showing
      && now - *at >= LINGER
    {
      self.hide();
    }
  }

  fn current_id(&self) -> Option<String> {
    match &self.showing {
      Showing::Nothing => None,
      Showing::Rendering { id, .. } => Some(id.clone()),
      Showing::Done { finished, .. } => Some(finished.id.clone()),
    }
  }

  fn present(&self, mtm: MainThreadMarker) {
    if let Some(screen) = NSScreen::mainScreen(mtm) {
      let area = screen.visibleFrame();
      let frame = self.panel.frame();
      self.panel.setFrameOrigin(NSPoint::new(
        area.origin.x + area.size.width - frame.size.width - MARGIN,
        area.origin.y + area.size.height - frame.size.height - MARGIN,
      ));
    }
    self.panel.orderFrontRegardless();
  }

  fn show_progress(&self, id: &str, progress: f64, elapsed: Duration, queued: usize) {
    self.set(&self.title, &format!("Rendering {id}"));
    self.bar.setDoubleValue(progress);
    let mut parts = vec![format!("{:.0}%", progress * 100.0)];
    if progress >= ESTIMATE_FROM {
      let left = elapsed.as_secs_f64() * (1.0 - progress) / progress;
      parts.push(format!("about {} left", clock(left)));
    }
    if queued > 0 {
      parts.push(format!("{queued} more queued"));
    }
    self.set(&self.detail, &parts.join(" · "));
    self.button.setTitle(&NSString::from_str("Open Folder"));
  }

  fn show_done(&self, f: &Finished) {
    match &f.result {
      Ok(_) => {
        self.set(&self.title, &format!("Rendered {}", f.id));
        self.bar.setDoubleValue(1.0);
        self.set(&self.detail, "Done");
        self.button.setTitle(&NSString::from_str("Show Render"));
      }
      Err(e) => {
        self.set(&self.title, &format!("Render failed: {}", f.id));
        self.set(&self.detail, e);
        self.button.setTitle(&NSString::from_str("Open Folder"));
      }
    }
  }

  fn hide(&mut self) {
    self.showing = Showing::Nothing;
    self.panel.orderOut(None);
  }

  fn open(&self) {
    let path = match &self.showing {
      Showing::Done {
        finished: Finished {
          result: Ok(path), ..
        },
        ..
      } => PathBuf::from(path),
      _ => match self.current_id() {
        Some(id) => paths::recordings().join(id),
        None => return,
      },
    };
    let _ = Command::new("open").arg("-R").arg(&path).status();
  }

  fn set(&self, field: &NSTextField, text: &str) {
    field.setStringValue(&NSString::from_str(text));
  }

  /// Whether the panel needs frequent ticks.
  pub fn is_active(&self) -> bool {
    !matches!(self.showing, Showing::Nothing)
  }
}

/// "0:42" or "12:05".
fn clock(seconds: f64) -> String {
  let s = seconds.round().max(0.0) as u64;
  format!("{}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn clock_reads_minutes_and_seconds() {
    assert_eq!(clock(0.4), "0:00");
    assert_eq!(clock(42.0), "0:42");
    assert_eq!(clock(725.0), "12:05");
  }
}
