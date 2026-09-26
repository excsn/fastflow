mod applog;
mod build_id;
mod gif_window;
mod notify;
mod overlay;
mod recorder;
mod recovery;
mod settings;
mod settings_window;
mod setup;

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use applog::log;
use fastflow_core::recording::InputKind;
use fastflow_daemon::paths;
use fastflow_daemon::protocol::{Finished, Request, Response, Status};
use fastflow_daemon::queue::RenderQueue;
use fastflow_daemon::retention;
use fastflow_daemon::server::{self, Handler};
use fastflow_desktop::permissions::{self, Grant, Permission};
use fastflow_render::job::{self, RenderJob};
use fibre::mpsc;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use objc2::MainThreadMarker;
use recorder::Recorder;
use setup::SetupWindow;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
use winit::window::WindowId;

const PERMISSION_POLL: Duration = Duration::from_secs(2);
const RECORDING_TICK: Duration = Duration::from_millis(100);
const OVERLAY_TICK: Duration = Duration::from_millis(16);
const GIF_WINDOW_TICK: Duration = Duration::from_millis(50);
const RENDERING_TICK: Duration = Duration::from_millis(500);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const SWEEP_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
const DISK_CHECK_EVERY: Duration = Duration::from_secs(5);

enum UserEvent {
  Menu(MenuEvent),
  Request(Request, mpsc::BoundedSyncSender<Response>),
  RenderDone(Finished),
  Hotkey(u32),
}

#[derive(Debug, Clone, Copy)]
enum Action {
  ToggleRecording,
  Mark(InputKind),
}

/// ⌃⌥⌘ plus a letter, chosen to stay clear of common app shortcuts.
const BINDINGS: [(Code, Action); 5] = [
  (Code::KeyR, Action::ToggleRecording),
  (Code::KeyK, Action::Mark(InputKind::Keep)),
  (Code::KeyX, Action::Mark(InputKind::Cut)),
  (Code::KeyM, Action::Mark(InputKind::Chapter)),
  (Code::KeyF, Action::Mark(InputKind::Frame)),
];

struct Hotkeys {
  _manager: GlobalHotKeyManager,
  actions: Vec<(u32, Action)>,
}

fn register_hotkeys(proxy: EventLoopProxy<UserEvent>) -> Result<Hotkeys, String> {
  let manager = GlobalHotKeyManager::new().map_err(|e| e.to_string())?;
  let mods = Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER;
  let mut actions = Vec::new();
  for (code, action) in BINDINGS {
    let hotkey = HotKey::new(Some(mods), code);
    match manager.register(hotkey) {
      Ok(()) => actions.push((hotkey.id(), action)),
      Err(e) => log(format!("hotkey {code:?}: {e}")),
    }
  }
  let proxy = Mutex::new(proxy);
  GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
    if e.state == HotKeyState::Pressed {
      let _ = proxy.lock().unwrap().send_event(UserEvent::Hotkey(e.id));
    }
  }));
  Ok(Hotkeys {
    _manager: manager,
    actions,
  })
}

/// Socket requests run on the main thread, where the recorder and its event tap live.
struct SocketHandler {
  proxy: Mutex<EventLoopProxy<UserEvent>>,
}

impl Handler for SocketHandler {
  fn handle(&self, request: Request) -> Response {
    let (tx, rx) = mpsc::bounded(1);
    let sent = self
      .proxy
      .lock()
      .unwrap()
      .send_event(UserEvent::Request(request, tx));
    if sent.is_err() {
      return Response::error("fastflow is shutting down");
    }
    rx.recv_timeout(REQUEST_TIMEOUT)
      .unwrap_or_else(|_| Response::error("timed out waiting for the app"))
  }
}

struct PermissionRow {
  permission: Permission,
  item: MenuItem,
  grant: Option<Grant>,
}

struct Tray {
  icon: TrayIcon,
  record: MenuItem,
  render_state: MenuItem,
  reveal: MenuItem,
  make_gif: MenuItem,
  settings: MenuItem,
  rows: Vec<PermissionRow>,
  quit: MenuItem,
}

struct App {
  proxy: EventLoopProxy<UserEvent>,
  tray: Option<Tray>,
  setup: Option<SetupWindow>,
  recorder: Option<Recorder>,
  queue: Option<RenderQueue>,
  last_sweep: Option<Instant>,
  last_disk_check: Option<Instant>,
  hotkeys: Option<Hotkeys>,
  overlay: Option<overlay::Overlay>,
  gif: Option<gif_window::GifWindow>,
  settings: Option<settings_window::SettingsWindow>,
}

impl App {
  fn build_tray(&mut self) {
    let menu = Menu::new();
    let title = MenuItem::new("fastflow", false, None);
    let rows: Vec<PermissionRow> = Permission::ALL
      .into_iter()
      .map(|permission| PermissionRow {
        permission,
        item: MenuItem::new(permission.label(), true, None),
        grant: None,
      })
      .collect();
    let record = MenuItem::new("Start Recording  ⌃⌥⌘R", true, None);
    let render_state = MenuItem::new("No renders yet", false, None);
    let reveal = MenuItem::new("Show Last Render", false, None);
    let make_gif = MenuItem::new("Make GIF…", true, None);
    let settings = MenuItem::new("Settings…", true, None);
    let quit = MenuItem::new("Quit", true, None);

    menu.append(&title).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&record).unwrap();
    menu.append(&render_state).unwrap();
    menu.append(&reveal).unwrap();
    menu.append(&make_gif).unwrap();
    menu.append(&settings).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    for row in &rows {
      menu.append(&row.item).unwrap();
    }
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&quit).unwrap();

    let icon = TrayIconBuilder::new()
      .with_menu(Box::new(menu))
      .with_icon(tray_glyph(false))
      .with_icon_as_template(true)
      .with_tooltip("fastflow")
      .build()
      .expect("tray icon");

    self.tray = Some(Tray {
      icon,
      record,
      render_state,
      reveal,
      make_gif,
      settings,
      rows,
      quit,
    });
    self.refresh_permissions();
  }

  fn refresh_permissions(&mut self) {
    let Some(tray) = &mut self.tray else { return };
    for row in &mut tray.rows {
      let grant = permissions::check(row.permission);
      if row.grant == Some(grant) {
        continue;
      }
      row.grant = Some(grant);
      log(format!("check {}: {grant:?}", row.permission.label()));
      let state = match grant {
        Grant::Granted => "granted",
        Grant::Denied => "denied, click to set up",
        Grant::Unknown => "not granted, click to set up",
      };
      row
        .item
        .set_text(format!("{}: {state}", row.permission.label()));
      row.item.set_enabled(grant != Grant::Granted);
    }
  }

  fn show_setup(&mut self, updated: bool) {
    let mtm = MainThreadMarker::new().expect("main thread");
    self
      .setup
      .get_or_insert_with(|| SetupWindow::new(mtm, updated))
      .show(mtm);
  }

  fn on_launch(&mut self) {
    let build = build_id::current();
    let last = build_id::last_granted();
    log(format!(
      "launch build {build:?}, last granted build {last:?}"
    ));
    self.build_tray();
    let recovered = recovery::recover_all();
    self.sweep();
    self.start_daemon();
    match register_hotkeys(self.proxy.clone()) {
      Ok(h) => self.hotkeys = Some(h),
      Err(e) => log(format!("hotkeys: {e}")),
    }
    notify::request_permission();
    if !recovered.is_empty() {
      notify::post(
        "recovered",
        "Recovered an unfinished recording",
        &format!(
          "{}. Render it from the CLI with fastflow render <id>.",
          recovered.join(", ")
        ),
      );
    }
    let all_granted = Permission::ALL
      .iter()
      .all(|&p| permissions::check(p) == Grant::Granted);
    if all_granted {
      if let Some(build) = build.filter(|b| last.as_ref() != Some(b)) {
        build_id::record_granted(&build);
      }
    } else {
      let updated = last.is_some() && last != build;
      self.show_setup(updated);
    }
  }

  fn start_daemon(&mut self) {
    let runner = Box::new(|id: &str, progress: &mut dyn FnMut(f64)| {
      let dir = paths::recordings().join(id);
      let out = dir.join("render.mp4");
      log(format!("render {id} started"));
      let report = job::run(
        &RenderJob {
          dir: dir.clone(),
          out,
          boxes: false,
          preview: false,
        },
        &mut |line| log(format!("render {id}: {line}")),
        &mut |done, total| progress(done as f64 / total.max(1) as f64),
      )?;
      match job::make_proxy(&dir) {
        Ok(p) => log(format!("proxy {id}: {}", p.display())),
        Err(e) => log(format!("proxy {id}: {e}")),
      }
      Ok(report.out.to_string_lossy().into_owned())
    });
    let proxy = self.proxy.clone();
    let on_done = Box::new(move |f: &Finished| {
      let _ = proxy.send_event(UserEvent::RenderDone(f.clone()));
    });
    self.queue = Some(RenderQueue::start(runner, on_done));

    let handler = Arc::new(SocketHandler {
      proxy: Mutex::new(self.proxy.clone()),
    });
    match server::serve(&paths::socket(), handler) {
      Ok(_) => log(format!("listening on {}", paths::socket().display())),
      Err(e) => log(format!("socket: {e}")),
    }
  }

  fn sweep(&mut self) {
    self.last_sweep = Some(Instant::now());
    let swept = retention::sweep(
      &paths::recordings(),
      std::time::SystemTime::now(),
      retention::RAW_KEEP,
    );
    for s in &swept {
      log(format!(
        "swept raw footage of {}: {} MB",
        s.id,
        s.bytes >> 20
      ));
    }
  }

  fn free_space(&self) -> Option<u64> {
    let root = paths::recordings();
    let _ = std::fs::create_dir_all(&root);
    retention::free_bytes(&root)
  }

  fn start_recording(&mut self) -> Result<String, String> {
    if let Some(r) = &self.recorder {
      return Err(format!("already recording {}", r.id()));
    }
    match self.free_space() {
      Some(free) if free < retention::REFUSE_BELOW => {
        return Err(format!("only {} GB free, not starting", free >> 30));
      }
      Some(free) if free < retention::WARN_BELOW => {
        notify::post(
          "disk",
          "Low disk space",
          &format!("{} GB free. Recording stops below 2 GB.", free >> 30),
        );
      }
      _ => {}
    }
    let mut shown = None;
    let started = Recorder::start(&mut |p| {
      let mtm = MainThreadMarker::new().expect("main thread");
      shown = Some(overlay::Overlay::new(
        mtm,
        p.display_pt,
        fastflow_desktop::macos::display::primary_height(),
        p.geo,
        p.camera.clone(),
      ));
    });
    let r = match started {
      Ok(r) => r,
      Err(e) => {
        if let Some(o) = shown {
          o.close();
        }
        return Err(e);
      }
    };
    let id = r.id();
    self.overlay = shown;
    self.recorder = Some(r);
    self.show_recording_state();
    Ok(id)
  }

  /// A recording that stopped cleanly is queued for rendering straight away.
  fn stop_recording(&mut self) -> Result<String, String> {
    let r = self.recorder.take().ok_or("not recording")?;
    if let Some(o) = self.overlay.take() {
      o.close();
    }
    let result = r.stop();
    self.show_recording_state();
    let id = result?;
    if let Some(q) = &mut self.queue {
      q.push(&id);
    }
    self.show_render_state();
    Ok(id)
  }

  fn toggle_recording(&mut self) {
    let result = if self.recorder.is_some() {
      self.stop_recording()
    } else {
      self.start_recording()
    };
    if let Err(e) = result {
      log(format!("recording: {e}"));
      notify::post("recording", "Recording failed", &e);
    }
  }

  fn status(&self) -> Status {
    let q = self.queue.as_ref().map(|q| q.status()).unwrap_or_default();
    Status {
      recording: self.recorder.as_ref().map(|r| r.id()),
      rendering: q.rendering,
      queued: q.queued.into_iter().collect(),
      last: q.last,
    }
  }

  fn handle_request(&mut self, request: Request) -> Response {
    let result = match request {
      Request::Start => self.start_recording().map(Response::with_id),
      Request::Stop => self.stop_recording().map(Response::with_id),
      Request::Status => Ok(Response {
        status: Some(self.status()),
        ..Response::ok()
      }),
      Request::Render { id } => {
        if !paths::recordings().join(&id).join("meta.json").is_file() {
          Err(format!("no recording {id}"))
        } else if self.recorder.as_ref().is_some_and(|r| r.id() == id) {
          Err(format!("{id} is still recording"))
        } else {
          if let Some(q) = &mut self.queue {
            q.push(&id);
          }
          self.show_render_state();
          Ok(Response::with_id(id))
        }
      }
      Request::Resegment => match self.recorder.as_mut() {
        Some(r) => r.resegment().map(|()| Response::with_id(r.id())),
        None => Err("not recording".into()),
      },
      Request::List { limit } => Ok(Response {
        recordings: Some(paths::list_recordings(limit)),
        ..Response::ok()
      }),
    };
    result.unwrap_or_else(Response::error)
  }

  fn on_hotkey(&mut self, id: u32) {
    let action = self
      .hotkeys
      .as_ref()
      .and_then(|h| h.actions.iter().find(|(i, _)| *i == id))
      .map(|(_, a)| *a);
    match action {
      Some(Action::ToggleRecording) => self.toggle_recording(),
      Some(Action::Mark(kind)) => match &mut self.recorder {
        Some(r) => r.mark(kind),
        None => log(format!("marker {kind:?} ignored: not recording")),
      },
      None => {}
    }
  }

  fn on_render_done(&mut self, f: Finished) {
    match &f.result {
      Ok(path) => {
        log(format!("render {} done: {path}", f.id));
        notify::post(&f.id, "Render finished", &f.id);
      }
      Err(e) => {
        log(format!("render {} failed: {e}", f.id));
        notify::post(&f.id, "Render failed", e);
      }
    }
    self.show_render_state();
  }

  fn show_render_state(&self) {
    let (Some(tray), Some(q)) = (&self.tray, &self.queue) else {
      return;
    };
    let s = q.status();
    let text = match (&s.rendering, &s.last) {
      (Some(r), _) if s.queued.is_empty() => {
        format!("Rendering {}: {:.0}%", r.id, r.progress * 100.0)
      }
      (Some(r), _) => format!(
        "Rendering {}: {:.0}%, {} queued",
        r.id,
        r.progress * 100.0,
        s.queued.len()
      ),
      (None, Some(Finished { id, result: Ok(_) })) => format!("Last render: {id}"),
      (None, Some(Finished { id, result: Err(_) })) => format!("Last render failed: {id}"),
      (None, None) => "No renders yet".into(),
    };
    tray.render_state.set_text(text);
    tray
      .reveal
      .set_enabled(matches!(&s.last, Some(Finished { result: Ok(_), .. })));
  }

  fn reveal_last_render(&self) {
    let Some(q) = &self.queue else { return };
    if let Some(Finished {
      result: Ok(path), ..
    }) = q.status().last
    {
      let _ = Command::new("open").args(["-R", &path]).status();
    }
  }

  fn show_recording_state(&self) {
    let Some(tray) = &self.tray else { return };
    let recording = self.recorder.is_some();
    tray.record.set_text(if recording {
      "Stop Recording  ⌃⌥⌘R"
    } else {
      "Start Recording  ⌃⌥⌘R"
    });
    let _ = tray.icon.set_icon(Some(tray_glyph(recording)));
    tray.icon.set_icon_as_template(true);
  }

  fn tick_recording(&mut self) {
    let due = self
      .last_disk_check
      .is_none_or(|t| t.elapsed() >= DISK_CHECK_EVERY);
    let low = due && self.free_space().is_some_and(|f| f < retention::STOP_BELOW);
    if due {
      self.last_disk_check = Some(Instant::now());
    }
    let Some(r) = &mut self.recorder else { return };
    if let Some(o) = &mut self.overlay {
      o.update(r.latest_sample().as_ref());
    }
    if low {
      log("disk nearly full, stopping the recording");
      r.truncate("disk_full");
      if let Err(e) = self.stop_recording() {
        log(format!("stop: {e}"));
      }
      notify::post(
        "disk",
        "Recording stopped",
        "Less than 2 GB of disk space left.",
      );
      return;
    }
    let ticked = r.tick();
    if let Ok(Some(recorder::Event::Switched(p))) = &ticked {
      if let Some(o) = self.overlay.take() {
        o.close();
      }
      let mtm = MainThreadMarker::new().expect("main thread");
      self.overlay = Some(overlay::Overlay::new(
        mtm,
        p.display_pt,
        fastflow_desktop::macos::display::primary_height(),
        p.geo,
        p.camera.clone(),
      ));
    }
    if let Err(why) = ticked {
      log(format!("capture ended unexpectedly: {why}"));
      if let Some(o) = self.overlay.take() {
        o.close();
      }
      if let Some(r) = self.recorder.take() {
        let _ = r.stop();
      }
      self.show_recording_state();
      notify::post("recording", "Recording stopped", &why);
    }
  }

  fn on_menu(&mut self, event: MenuEvent, event_loop: &ActiveEventLoop) {
    let Some(tray) = &self.tray else { return };
    if event.id == *tray.quit.id() {
      if let Some(r) = self.recorder.take() {
        let _ = r.stop();
      }
      event_loop.exit();
      return;
    }
    if event.id == *tray.record.id() {
      self.toggle_recording();
      return;
    }
    if event.id == *tray.reveal.id() {
      self.reveal_last_render();
      return;
    }
    if event.id == *tray.make_gif.id() {
      let mtm = MainThreadMarker::new().expect("main thread");
      self
        .gif
        .get_or_insert_with(|| gif_window::GifWindow::new(mtm))
        .show(mtm);
      return;
    }
    if event.id == *tray.settings.id() {
      let mtm = MainThreadMarker::new().expect("main thread");
      self
        .settings
        .get_or_insert_with(|| settings_window::SettingsWindow::new(mtm))
        .show(mtm);
      return;
    }
    if tray.rows.iter().any(|r| event.id == *r.item.id()) {
      self.show_setup(false);
    }
  }
}

impl ApplicationHandler<UserEvent> for App {
  fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
    match cause {
      // tray-icon requires the NSApplication run loop to be live before the item is created.
      StartCause::Init => self.on_launch(),
      StartCause::ResumeTimeReached { .. } => {
        self.tick_recording();
        self.show_render_state();
        if let Some(g) = &mut self.gif {
          let mtm = MainThreadMarker::new().expect("main thread");
          g.tick(mtm);
        }
        if let Some(id) = self.settings.as_mut().and_then(|s| s.tick()) {
          let _ = self.handle_request(Request::Render { id });
        }
        if self.last_sweep.is_none_or(|t| t.elapsed() >= SWEEP_EVERY) {
          self.sweep();
        }
        self.refresh_permissions();
        if let Some(setup) = self.setup.as_ref().filter(|s| s.is_visible()) {
          setup.refresh();
        }
      }
      _ => {}
    }
    let rendering = self
      .queue
      .as_ref()
      .is_some_and(|q| q.status().rendering.is_some());
    let gif_open = self.gif.as_ref().is_some_and(|g| g.is_visible())
      || self.settings.as_ref().is_some_and(|s| s.is_visible());
    let period = if self.overlay.is_some() {
      OVERLAY_TICK
    } else if gif_open {
      GIF_WINDOW_TICK
    } else if self.recorder.is_some() {
      RECORDING_TICK
    } else if rendering {
      RENDERING_TICK
    } else {
      PERMISSION_POLL
    };
    event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + period));
  }

  fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

  fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
    match event {
      UserEvent::Menu(e) => self.on_menu(e, event_loop),
      UserEvent::Request(request, reply) => {
        let _ = reply.send(self.handle_request(request));
      }
      UserEvent::RenderDone(f) => self.on_render_done(f),
      UserEvent::Hotkey(id) => self.on_hotkey(id),
    }
  }

  fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}

/// Generated by `scripts/icons.sh`.
fn tray_glyph(recording: bool) -> Icon {
  let bytes: &[u8] = if recording {
    include_bytes!("../bundle/tray_recording.png")
  } else {
    include_bytes!("../bundle/tray.png")
  };
  let mut reader = png::Decoder::new(std::io::Cursor::new(bytes))
    .read_info()
    .expect("tray png");
  let mut rgba = vec![0; reader.output_buffer_size().expect("tray png size")];
  let info = reader.next_frame(&mut rgba).expect("tray png frame");
  rgba.truncate(info.buffer_size());
  Icon::from_rgba(rgba, info.width, info.height).expect("icon")
}

fn main() {
  let event_loop = EventLoop::<UserEvent>::with_user_event()
    .with_activation_policy(ActivationPolicy::Accessory)
    .build()
    .expect("event loop");

  let proxy: EventLoopProxy<UserEvent> = event_loop.create_proxy();
  let menu_proxy = proxy.clone();
  MenuEvent::set_event_handler(Some(move |e| {
    let _ = menu_proxy.send_event(UserEvent::Menu(e));
  }));

  let mut app = App {
    proxy,
    tray: None,
    setup: None,
    recorder: None,
    queue: None,
    last_sweep: None,
    last_disk_check: None,
    hotkeys: None,
    overlay: None,
    gif: None,
    settings: None,
  };
  event_loop.run_app(&mut app).expect("event loop");
}
