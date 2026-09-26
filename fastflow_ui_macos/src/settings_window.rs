//! "Settings…": one tab per group of `config.toml` settings. Saving writes the defaults every new
//! recording copies. Applying also writes them into the last recording and re-renders it.

use std::cell::Cell;

use fastflow_core::config::Config;
use fastflow_daemon::paths;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
  NSApplication, NSBackingStoreType, NSBezelStyle, NSButton, NSColor, NSControlStateValueOff,
  NSControlStateValueOn, NSLayoutAttribute, NSPopUpButton, NSPopover, NSPopoverBehavior,
  NSStackView, NSTabView, NSTabViewItem, NSTextAlignment, NSTextField,
  NSUserInterfaceLayoutOrientation, NSView, NSViewController, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
  NSArray, NSEdgeInsets, NSObject, NSPoint, NSRect, NSRectEdge, NSSize, NSString,
};
use toml::Value;

use crate::applog::log;
use crate::settings::{self, Field, GROUPS, Kind};

const SIZE: (f64, f64) = (580.0, 500.0);
const LABEL_WIDTH: f64 = 230.0;
const NUMBER_WIDTH: f64 = 80.0;
const HINT: &str = "New recordings use these settings.";
const HELP_WIDTH: f64 = 280.0;

#[derive(Clone, Copy)]
pub enum Action {
  Save,
  Apply,
  Defaults,
  /// The index of the field whose help button was pressed.
  Help(usize),
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and SettingsTarget does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FastflowSettingsTarget"]
    #[ivars = Cell<Option<Action>>]
    pub struct SettingsTarget;

    impl SettingsTarget {
        #[unsafe(method(save:))]
        fn save(&self, _sender: Option<&AnyObject>) {
            self.ivars().set(Some(Action::Save));
        }

        #[unsafe(method(apply:))]
        fn apply(&self, _sender: Option<&AnyObject>) {
            self.ivars().set(Some(Action::Apply));
        }

        #[unsafe(method(defaults:))]
        fn defaults(&self, _sender: Option<&AnyObject>) {
            self.ivars().set(Some(Action::Defaults));
        }

        #[unsafe(method(help:))]
        fn help(&self, sender: Option<&AnyObject>) {
            let Some(sender) = sender else { return };
            let tag: isize = unsafe { msg_send![sender, tag] };
            self.ivars().set(Some(Action::Help(tag as usize)));
        }
    }
);

impl SettingsTarget {
  fn new(mtm: MainThreadMarker) -> Retained<Self> {
    let this = Self::alloc(mtm).set_ivars(Cell::new(None));
    unsafe { msg_send![super(this), init] }
  }
}

enum Control {
  Toggle(Retained<NSButton>),
  Number(Retained<NSTextField>),
  Pair(Retained<NSTextField>, Retained<NSTextField>),
  Choice(Retained<NSPopUpButton>),
}

pub struct SettingsWindow {
  window: Retained<NSWindow>,
  controls: Vec<(&'static Field, Control)>,
  help_buttons: Vec<Retained<NSButton>>,
  popover: Option<Retained<NSPopover>>,
  status: Retained<NSTextField>,
  target: Retained<SettingsTarget>,
}

impl SettingsWindow {
  pub fn new(mtm: MainThreadMarker) -> SettingsWindow {
    let target = SettingsTarget::new(mtm);
    let tabs = NSTabView::new(mtm);
    tabs.setFrame(NSRect::new(
      NSPoint::new(12.0, 80.0),
      NSSize::new(SIZE.0 - 24.0, SIZE.1 - 92.0),
    ));
    let mut controls = Vec::new();
    let mut help_buttons = Vec::new();
    for group in GROUPS {
      let rows: Vec<Retained<NSView>> = group
        .fields
        .iter()
        .map(|f| {
          let (row, control, help) = field_row(mtm, f, &target, controls.len());
          controls.push((f, control));
          help_buttons.push(help);
          row
        })
        .collect();
      let stack = NSStackView::stackViewWithViews(&NSArray::from_retained_slice(&rows), mtm);
      stack.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
      stack.setAlignment(NSLayoutAttribute::Leading);
      stack.setSpacing(10.0);
      stack.setEdgeInsets(NSEdgeInsets {
        top: 16.0,
        left: 12.0,
        bottom: 16.0,
        right: 12.0,
      });
      let item = NSTabViewItem::new();
      item.setLabel(&NSString::from_str(group.title));
      item.setView(Some(&view(&stack)));
      tabs.addTabViewItem(&item);
    }

    let status = label(mtm, HINT);
    status.setTextColor(Some(&NSColor::secondaryLabelColor()));
    let button = |title: &str, action| unsafe {
      NSButton::buttonWithTitle_target_action(
        &NSString::from_str(title),
        Some(&target),
        Some(action),
        mtm,
      )
    };
    let defaults = button("Restore Defaults", sel!(defaults:));
    let apply = button("Apply to Last Recording", sel!(apply:));
    let save = button("Save", sel!(save:));
    save.setKeyEquivalent(&NSString::from_str("\r"));
    let buttons = NSStackView::stackViewWithViews(
      &NSArray::from_retained_slice(&[view(&defaults), view(&apply), view(&save)]),
      mtm,
    );
    buttons.setSpacing(8.0);
    buttons.setFrame(NSRect::new(
      NSPoint::new(SIZE.0 - 420.0, 12.0),
      NSSize::new(404.0, 28.0),
    ));
    status.setFrame(NSRect::new(
      NSPoint::new(20.0, 50.0),
      NSSize::new(SIZE.0 - 40.0, 20.0),
    ));

    let content = NSView::new(mtm);
    content.addSubview(&tabs);
    content.addSubview(&status);
    content.addSubview(&buttons);

    let window = unsafe {
      NSWindow::initWithContentRect_styleMask_backing_defer(
        NSWindow::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(SIZE.0, SIZE.1)),
        NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
        NSBackingStoreType::Buffered,
        false,
      )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("fastflow Settings"));
    window.setContentView(Some(&content));

    SettingsWindow {
      window,
      controls,
      help_buttons,
      popover: None,
      status,
      target,
    }
  }

  /// Loads the saved defaults into the controls and brings the window forward.
  pub fn show(&mut self, mtm: MainThreadMarker) {
    match settings::load(&paths::default_config()) {
      Ok(cfg) => {
        self.fill(&cfg);
        self.say(HINT, false);
      }
      Err(e) => {
        self.fill(&Config::default());
        self.say(&format!("Showing defaults, cannot read {e}"), true);
      }
    }
    self.window.center();
    // `activate()` does not bring a menu-bar-only app forward.
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    self.window.orderFrontRegardless();
    self.window.makeKeyAndOrderFront(None);
  }

  pub fn is_visible(&self) -> bool {
    self.window.isVisible()
  }

  /// Handles a pressed button. Returns a recording to re-render.
  pub fn tick(&mut self) -> Option<String> {
    let action = self.target.ivars().take()?;
    if let Action::Help(i) = action {
      self.show_help(i);
      return None;
    }
    if let Action::Defaults = action {
      self.fill(&Config::default());
      self.say("Defaults restored. Save to keep them.", false);
      return None;
    }
    // Commits a text field that is still being edited.
    self.window.makeFirstResponder(None);
    let cfg = match self.read() {
      Ok(cfg) => cfg,
      Err(e) => {
        self.say(&e, true);
        return None;
      }
    };
    if let Err(e) = settings::save(&paths::default_config(), &cfg) {
      self.say(&e, true);
      return None;
    }
    log("settings saved");
    if let Action::Save = action {
      self.say("Saved. New recordings use these settings.", false);
      return None;
    }
    let Some(last) = paths::list_recordings(10)
      .into_iter()
      .find(|r| !r.recording)
    else {
      self.say("Saved. There is no recording to apply them to.", false);
      return None;
    };
    let path = paths::recordings().join(&last.id).join("config.toml");
    if let Err(e) = settings::save(&path, &cfg) {
      self.say(&e, true);
      return None;
    }
    log(format!("settings applied to {}", last.id));
    self.say(&format!("Saved. Re-rendering {}.", last.id), false);
    Some(last.id)
  }

  fn show_help(&mut self, i: usize) {
    let (Some((f, _)), Some(anchor)) = (self.controls.get(i), self.help_buttons.get(i)) else {
      return;
    };
    let mtm = MainThreadMarker::new().expect("main thread");
    let text = NSTextField::wrappingLabelWithString(&NSString::from_str(f.help), mtm);
    text.setPreferredMaxLayoutWidth(HELP_WIDTH);
    let fit = text.fittingSize();
    text.setFrame(NSRect::new(NSPoint::new(14.0, 12.0), fit));
    let content = NSView::new(mtm);
    let size = NSSize::new(fit.width + 28.0, fit.height + 24.0);
    content.setFrameSize(size);
    content.addSubview(&text);
    let controller = NSViewController::new(mtm);
    controller.setView(&content);
    let popover = NSPopover::new(mtm);
    popover.setBehavior(NSPopoverBehavior::Transient);
    popover.setContentViewController(Some(&controller));
    popover.setContentSize(size);
    popover.showRelativeToRect_ofView_preferredEdge(anchor.bounds(), anchor, NSRectEdge::MaxX);
    self.popover = Some(popover);
  }

  fn fill(&self, cfg: &Config) {
    let table = settings::to_table(cfg);
    for (f, control) in &self.controls {
      let v = settings::get(&table, f);
      match control {
        Control::Toggle(b) => b.setState(if v.as_bool() == Some(true) {
          NSControlStateValueOn
        } else {
          NSControlStateValueOff
        }),
        Control::Number(t) => t.setStringValue(&NSString::from_str(&settings::text(v))),
        Control::Pair(a, b) => {
          if let Some([x, y]) = v.as_array().map(Vec::as_slice) {
            a.setStringValue(&NSString::from_str(&settings::text(x)));
            b.setStringValue(&NSString::from_str(&settings::text(y)));
          }
        }
        Control::Choice(p) => {
          if let (Some(s), Kind::Choice(options)) = (v.as_str(), &f.kind)
            && let Some(i) = options.iter().position(|o| *o == s)
          {
            p.selectItemAtIndex(i as isize);
          }
        }
      }
    }
  }

  fn read(&self) -> Result<Config, String> {
    let defaults = settings::to_table(&Config::default());
    let mut table = defaults.clone();
    for (f, control) in &self.controls {
      let like = settings::get(&defaults, f);
      let named = |e: String| format!("{}: {e}", f.label);
      let value = match control {
        Control::Toggle(b) => Value::Boolean(b.state() == NSControlStateValueOn),
        Control::Number(t) => settings::parse(like, &t.stringValue().to_string()).map_err(named)?,
        Control::Pair(a, b) => {
          let [x, y] = like.as_array().map(Vec::as_slice).unwrap_or_default() else {
            return Err(named("not a pair".into()));
          };
          Value::Array(vec![
            settings::parse(x, &a.stringValue().to_string()).map_err(named)?,
            settings::parse(y, &b.stringValue().to_string()).map_err(named)?,
          ])
        }
        Control::Choice(p) => Value::String(
          p.titleOfSelectedItem()
            .map(|s| s.to_string())
            .unwrap_or_default(),
        ),
      };
      settings::set(&mut table, f, value);
    }
    settings::from_table(table)
  }

  fn say(&self, text: &str, error: bool) {
    self.status.setStringValue(&NSString::from_str(text));
    let color = if error {
      NSColor::systemRedColor()
    } else {
      NSColor::secondaryLabelColor()
    };
    self.status.setTextColor(Some(&color));
  }
}

fn field_row(
  mtm: MainThreadMarker,
  f: &Field,
  target: &SettingsTarget,
  index: usize,
) -> (Retained<NSView>, Control, Retained<NSButton>) {
  let name = label(
    mtm,
    if matches!(f.kind, Kind::Toggle) {
      ""
    } else {
      f.label
    },
  );
  name.setAlignment(NSTextAlignment::Right);
  name
    .widthAnchor()
    .constraintEqualToConstant(LABEL_WIDTH)
    .setActive(true);
  let unit = label(mtm, f.unit);
  unit.setTextColor(Some(&NSColor::secondaryLabelColor()));
  let (views, control) = match f.kind {
    Kind::Toggle => {
      let b = unsafe {
        NSButton::checkboxWithTitle_target_action(&NSString::from_str(f.label), None, None, mtm)
      };
      (vec![view(&b)], Control::Toggle(b))
    }
    Kind::Number => {
      let t = number(mtm);
      (vec![view(&t), view(&unit)], Control::Number(t))
    }
    Kind::Pair => {
      let (a, b) = (number(mtm), number(mtm));
      (
        vec![view(&a), view(&label(mtm, "×")), view(&b), view(&unit)],
        Control::Pair(a, b),
      )
    }
    Kind::Choice(options) => {
      let p = NSPopUpButton::initWithFrame_pullsDown(
        NSPopUpButton::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(120.0, 26.0)),
        false,
      );
      for o in options {
        p.addItemWithTitle(&NSString::from_str(o));
      }
      (vec![view(&p)], Control::Choice(p))
    }
  };
  let help = unsafe {
    NSButton::buttonWithTitle_target_action(
      &NSString::from_str(""),
      Some(target),
      Some(sel!(help:)),
      mtm,
    )
  };
  help.setBezelStyle(NSBezelStyle::HelpButton);
  help.setTag(index as isize);
  let tip = NSString::from_str(f.help);
  let mut all = vec![view(&name)];
  all.extend(views);
  for v in &all {
    v.setToolTip(Some(&tip));
  }
  all.push(view(&help));
  let row = NSStackView::stackViewWithViews(&NSArray::from_retained_slice(&all), mtm);
  row.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
  row.setSpacing(8.0);
  row.setAlignment(NSLayoutAttribute::FirstBaseline);
  (view(&row), control, help)
}

fn number(mtm: MainThreadMarker) -> Retained<NSTextField> {
  let t = NSTextField::textFieldWithString(&NSString::from_str(""), mtm);
  t.setAlignment(NSTextAlignment::Right);
  t.widthAnchor()
    .constraintEqualToConstant(NUMBER_WIDTH)
    .setActive(true);
  t
}

fn label(mtm: MainThreadMarker, text: &str) -> Retained<NSTextField> {
  NSTextField::labelWithString(&NSString::from_str(text), mtm)
}

fn view<T: objc2::Message + objc2::ClassType>(v: &Retained<T>) -> Retained<NSView> {
  // SAFETY: every control passed here is an NSView subclass.
  unsafe { Retained::cast_unchecked(v.clone()) }
}
