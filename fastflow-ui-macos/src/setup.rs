use std::cell::Cell;
use std::process::Command;

use fastflow_desktop::permissions::{self, Grant, Permission};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
  NSApplication, NSBackingStoreType, NSButton, NSFont, NSLayoutAttribute, NSStackView, NSTextField,
  NSUserInterfaceLayoutOrientation, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
  NSArray, NSBundle, NSEdgeInsets, NSObject, NSPoint, NSRect, NSSize, NSString,
};

use crate::applog::log;

pub const BUNDLE_ID: &str = "com.novenseri.fastflow";

const WIDTH: f64 = 460.0;
const INSET: f64 = 20.0;

pub struct TargetIvars {
  input_requested: Cell<bool>,
  screen_requested: Cell<bool>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and SetupTarget does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FastflowSetupTarget"]
    #[ivars = TargetIvars]
    pub struct SetupTarget;

    impl SetupTarget {
        #[unsafe(method(grantInputMonitoring:))]
        fn grant_input_monitoring(&self, _sender: Option<&AnyObject>) {
            let first = !self.ivars().input_requested.replace(true);
            grant(Permission::InputMonitoring, first);
        }

        #[unsafe(method(grantScreenRecording:))]
        fn grant_screen_recording(&self, _sender: Option<&AnyObject>) {
            let first = !self.ivars().screen_requested.replace(true);
            grant(Permission::ScreenRecording, first);
        }

        #[unsafe(method(restart:))]
        fn restart(&self, _sender: Option<&AnyObject>) {
            restart();
        }
    }
);

impl SetupTarget {
  fn new(mtm: MainThreadMarker) -> Retained<Self> {
    let this = Self::alloc(mtm).set_ivars(TargetIvars {
      input_requested: Cell::new(false),
      screen_requested: Cell::new(false),
    });
    unsafe { msg_send![super(this), init] }
  }
}

/// The first click clears any existing entry, which covers a stale entry from an earlier build
/// and an earlier denial. Later clicks only reopen the pane, so a grant made this session survives.
fn grant(p: Permission, first: bool) {
  if first {
    match permissions::reset(p, BUNDLE_ID) {
      Ok(out) => log(format!(
        "tccutil reset {} {BUNDLE_ID}: status {} stdout {:?} stderr {:?}",
        p.tcc_service(),
        out.status,
        String::from_utf8_lossy(&out.stdout).trim(),
        String::from_utf8_lossy(&out.stderr).trim(),
      )),
      Err(e) => log(format!(
        "tccutil reset {} failed to run: {e}",
        p.tcc_service()
      )),
    }
    let prompted = permissions::request(p);
    log(format!("request {}: {prompted}", p.label()));
  }
  let _ = Command::new("open").arg(p.settings_url()).status();
}

/// Waits for this process to exit before reopening the bundle, so two tray items never coexist.
fn restart() {
  log("restart requested");
  let bundle = NSBundle::mainBundle().bundlePath().to_string();
  let script = "while kill -0 \"$1\" 2>/dev/null; do sleep 0.1; done; open \"$2\"";
  let spawned = Command::new("/bin/sh")
    .args(["-c", script, "sh", &std::process::id().to_string(), &bundle])
    .spawn();
  if spawned.is_ok() {
    std::process::exit(0);
  }
}

struct Row {
  permission: Permission,
  title: Retained<NSTextField>,
  detail: Retained<NSTextField>,
  button: Retained<NSButton>,
}

pub struct SetupWindow {
  window: Retained<NSWindow>,
  target: Retained<SetupTarget>,
  rows: [Row; 2],
  footer: Retained<NSTextField>,
  restart: Retained<NSButton>,
}

impl SetupWindow {
  pub fn new(mtm: MainThreadMarker, updated: bool) -> Self {
    let target = SetupTarget::new(mtm);

    let heading_text = if updated {
      "fastflow was updated. macOS needs its permissions again."
    } else {
      "fastflow needs two permissions to record."
    };
    let heading = wrapping_label(mtm, heading_text, WIDTH - 2.0 * INSET);
    heading.setFont(Some(&NSFont::boldSystemFontOfSize(14.0)));

    let rows = [
      row(
        mtm,
        &target,
        Permission::InputMonitoring,
        sel!(grantInputMonitoring:),
      ),
      row(
        mtm,
        &target,
        Permission::ScreenRecording,
        sel!(grantScreenRecording:),
      ),
    ];

    let footer = wrapping_label(mtm, "", WIDTH - 2.0 * INSET);
    footer.setFont(Some(&NSFont::systemFontOfSize(11.0)));

    let restart = unsafe {
      NSButton::buttonWithTitle_target_action(
        &NSString::from_str("Restart fastflow"),
        Some(&target),
        Some(sel!(restart:)),
        mtm,
      )
    };

    let mut views: Vec<Retained<NSView>> =
      vec![Retained::into_super(Retained::into_super(heading))];
    for r in &rows {
      views.push(row_view(mtm, r));
    }
    views.push(Retained::into_super(Retained::into_super(footer.clone())));
    views.push(Retained::into_super(Retained::into_super(restart.clone())));

    let stack = NSStackView::stackViewWithViews(&NSArray::from_retained_slice(&views), mtm);
    stack.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
    stack.setAlignment(NSLayoutAttribute::Leading);
    stack.setSpacing(16.0);
    stack.setEdgeInsets(NSEdgeInsets {
      top: INSET,
      left: INSET,
      bottom: INSET,
      right: INSET,
    });

    let window = unsafe {
      NSWindow::initWithContentRect_styleMask_backing_defer(
        NSWindow::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, 300.0)),
        NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
        NSBackingStoreType::Buffered,
        false,
      )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("fastflow setup"));
    window.setContentView(Some(&stack));

    let this = Self {
      window,
      target,
      rows,
      footer,
      restart,
    };
    this.refresh();
    this
  }

  pub fn show(&self, mtm: MainThreadMarker) {
    self.refresh();
    self.window.center();
    self.window.makeKeyAndOrderFront(None);
    NSApplication::sharedApplication(mtm).activate();
  }

  pub fn refresh(&self) {
    let ivars = self.target.ivars();
    let input_granted = permissions::check(Permission::InputMonitoring) == Grant::Granted;
    let input_ready = input_granted || ivars.input_requested.get();
    let mut all_handled = true;

    for r in &self.rows {
      let granted = permissions::check(r.permission) == Grant::Granted;
      let requested = match r.permission {
        Permission::InputMonitoring => ivars.input_requested.get(),
        Permission::ScreenRecording => ivars.screen_requested.get(),
      };
      all_handled &= granted || requested;

      let mark = if granted { "✓" } else { "○" };
      r.title.setStringValue(&NSString::from_str(&format!(
        "{mark}  {}",
        r.permission.label()
      )));
      r.button.setHidden(granted);
      r.button.setTitle(&NSString::from_str(if requested {
        "Open Settings"
      } else {
        "Grant"
      }));

      let detail = match (r.permission, granted, requested) {
        (_, true, _) => "Granted.",
        (_, false, true) => "Turn fastflow on in the list that opened.",
        (Permission::InputMonitoring, false, false) => {
          "Records when you type or click. Never what you type."
        }
        (Permission::ScreenRecording, false, false) if !input_ready => {
          "Grant Input Monitoring first."
        }
        (Permission::ScreenRecording, false, false) => "Records the display you work on.",
      };
      r.detail.setStringValue(&NSString::from_str(detail));
      if r.permission == Permission::ScreenRecording {
        r.button.setEnabled(input_ready);
      }
    }

    let footer = if all_handled {
      "Both are set. Restart fastflow to apply them."
    } else {
      "macOS applies these permissions when fastflow restarts, so restart once both are on."
    };
    self.footer.setStringValue(&NSString::from_str(footer));
    self
      .restart
      .setKeyEquivalent(&NSString::from_str(if all_handled { "\r" } else { "" }));
  }

  pub fn is_visible(&self) -> bool {
    self.window.isVisible()
  }
}

fn row(mtm: MainThreadMarker, target: &SetupTarget, permission: Permission, action: Sel) -> Row {
  let title = NSTextField::labelWithString(&NSString::from_str(permission.label()), mtm);
  title.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
  let detail = wrapping_label(mtm, "", WIDTH - 2.0 * INSET - 120.0);
  let button = unsafe {
    NSButton::buttonWithTitle_target_action(
      &NSString::from_str("Grant"),
      Some(target),
      Some(action),
      mtm,
    )
  };
  Row {
    permission,
    title,
    detail,
    button,
  }
}

fn row_view(mtm: MainThreadMarker, r: &Row) -> Retained<NSView> {
  let text_views: [Retained<NSView>; 2] = [
    Retained::into_super(Retained::into_super(r.title.clone())),
    Retained::into_super(Retained::into_super(r.detail.clone())),
  ];
  let text = NSStackView::stackViewWithViews(&NSArray::from_retained_slice(&text_views), mtm);
  text.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
  text.setAlignment(NSLayoutAttribute::Leading);
  text.setSpacing(2.0);

  let parts: [Retained<NSView>; 2] = [
    Retained::into_super(text),
    Retained::into_super(Retained::into_super(r.button.clone())),
  ];
  let line = NSStackView::stackViewWithViews(&NSArray::from_retained_slice(&parts), mtm);
  line.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
  line.setAlignment(NSLayoutAttribute::CenterY);
  line.setSpacing(12.0);
  Retained::into_super(line)
}

fn wrapping_label(mtm: MainThreadMarker, text: &str, width: f64) -> Retained<NSTextField> {
  let label = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
  label.setPreferredMaxLayoutWidth(width);
  label
}
