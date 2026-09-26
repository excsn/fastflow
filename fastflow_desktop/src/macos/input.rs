use fibre::mpsc::UnboundedSyncSender as Sender;
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use core_foundation::runloop::{CFRunLoop, CFRunLoopSource, kCFRunLoopCommonModes};
use core_graphics::event::{
  CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
  CallbackResult,
};
use fastflow_core::recording::InputKind;

use crate::{DesktopError, InputMonitor, RawInput, Result};

/// Moves and drags arrive at the pointer's report rate. Pacing pads every event by hundreds of
/// milliseconds, so one per interval loses nothing.
const MOTION_INTERVAL: Duration = Duration::from_millis(50);

const EVENTS: [CGEventType; 15] = [
  CGEventType::KeyDown,
  CGEventType::FlagsChanged,
  CGEventType::LeftMouseDown,
  CGEventType::RightMouseDown,
  CGEventType::OtherMouseDown,
  CGEventType::LeftMouseUp,
  CGEventType::RightMouseUp,
  CGEventType::OtherMouseUp,
  CGEventType::MouseMoved,
  CGEventType::LeftMouseDragged,
  CGEventType::RightMouseDragged,
  CGEventType::OtherMouseDragged,
  CGEventType::ScrollWheel,
  CGEventType::TapDisabledByTimeout,
  CGEventType::TapDisabledByUserInput,
];

fn kind(t: CGEventType) -> Option<InputKind> {
  use CGEventType as T;
  Some(match t {
    T::KeyDown | T::FlagsChanged => InputKind::Key,
    T::LeftMouseDown | T::RightMouseDown | T::OtherMouseDown => InputKind::MouseDown,
    T::LeftMouseUp | T::RightMouseUp | T::OtherMouseUp => InputKind::MouseUp,
    T::MouseMoved => InputKind::MouseMove,
    T::LeftMouseDragged | T::RightMouseDragged | T::OtherMouseDragged => InputKind::MouseDrag,
    T::ScrollWheel => InputKind::Scroll,
    _ => return None,
  })
}

/// A listen-only `CGEventTap` on the main run loop. Must be started and stopped on the main thread.
#[derive(Default)]
pub struct MacInputMonitor {
  tap: Option<CGEventTap<'static>>,
  source: Option<CFRunLoopSource>,
  disabled: Arc<AtomicBool>,
}

impl InputMonitor for MacInputMonitor {
  fn start(&mut self, sink: Sender<RawInput>) -> Result<()> {
    let disabled = Arc::clone(&self.disabled);
    let sink = RefCell::new(sink);
    let last_motion: Cell<Option<(InputKind, Instant)>> = Cell::new(None);
    let tap = CGEventTap::new(
      CGEventTapLocation::Session,
      CGEventTapPlacement::TailAppendEventTap,
      CGEventTapOptions::ListenOnly,
      EVENTS.to_vec(),
      move |_, event_type, _| {
        let at = Instant::now();
        match kind(event_type) {
          Some(k @ (InputKind::MouseMove | InputKind::MouseDrag)) => {
            let recent = last_motion
              .get()
              .is_some_and(|(prev, t)| prev == k && at.duration_since(t) < MOTION_INTERVAL);
            if !recent {
              last_motion.set(Some((k, at)));
              let _ = sink.borrow_mut().send(RawInput { at, kind: k });
            }
          }
          Some(k) => {
            let _ = sink.borrow_mut().send(RawInput { at, kind: k });
          }
          None => disabled.store(true, Ordering::Relaxed),
        }
        CallbackResult::Keep
      },
    )
    .map_err(|()| DesktopError("CGEventTapCreate failed; Input Monitoring not granted".into()))?;

    let source = tap
      .mach_port()
      .create_runloop_source(0)
      .map_err(|()| DesktopError("event tap run loop source".into()))?;
    CFRunLoop::get_main().add_source(&source, unsafe { kCFRunLoopCommonModes });
    tap.enable();
    self.tap = Some(tap);
    self.source = Some(source);
    Ok(())
  }

  fn maintain(&mut self) {
    if self.disabled.swap(false, Ordering::Relaxed)
      && let Some(tap) = &self.tap
    {
      tap.enable();
    }
  }

  fn stop(&mut self) {
    if let Some(source) = self.source.take() {
      CFRunLoop::get_main().remove_source(&source, unsafe { kCFRunLoopCommonModes });
    }
    self.tap = None;
  }
}
