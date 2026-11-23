mod applog;
mod build_id;
mod compose_window;
mod list_table;
mod notify;
mod overlay;
mod recorder;
mod recovery;
mod render_panel;
mod settings;
mod settings_window;
mod setup;
mod track;
mod tray_art;

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use applog::log;
use fastflow_core::config::OnStop;
use fastflow_core::recording::InputKind;
use fastflow_daemon::paths;
use fastflow_daemon::protocol::{Finished, Job, Request, Response, Status, Task};
use fastflow_daemon::queue::RenderQueue;
use fastflow_daemon::retention;
use fastflow_daemon::server::{self, Handler};
use fastflow_desktop::permissions::{self, Grant, Permission};
use fastflow_render::job::{self, RenderJob, Target};
use fibre::mpsc;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use objc2::MainThreadMarker;
use recorder::Recorder;
use setup::SetupWindow;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
use winit::window::WindowId;

const PERMISSION_POLL: Duration = Duration::from_secs(2);
const RECORDING_TICK: Duration = Duration::from_millis(100);
const OVERLAY_TICK: Duration = Duration::from_millis(16);
const COMPOSE_WINDOW_TICK: Duration = Duration::from_millis(50);
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
  glyphs: tray_art::Glyphs,
  shown: std::cell::Cell<tray_art::State>,
  record: MenuItem,
  render_state: MenuItem,
  last: Submenu,
  last_folder: MenuItem,
  last_render: MenuItem,
  last_raw: MenuItem,
  last_compose: MenuItem,
  compose: MenuItem,
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
  compose: Option<compose_window::ComposeWindow>,
  /// A recording whose composition window opens once its preview is ready.
  open_when_prepared: Option<(String, OnStop)>,
  settings: Option<settings_window::SettingsWindow>,
  panel: Option<render_panel::RenderPanel>,
  /// The newest recording that is not still recording.
  last_id: Option<String>,
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
    let last = Submenu::new("Last Recording", false);
    let last_folder = MenuItem::new("Open Folder", true, None);
    let last_render = MenuItem::new("Play Render", false, None);
    let last_raw = MenuItem::new("Play Raw", true, None);
    let last_compose = MenuItem::new("Compose…", true, None);
    for item in [&last_folder, &last_render, &last_raw, &last_compose] {
      last.append(item).unwrap();
    }
    let compose = MenuItem::new("Compose…", true, None);
    let settings = MenuItem::new("Settings…", true, None);
    let quit = MenuItem::new("Quit", true, None);

    menu.append(&title).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&record).unwrap();
    menu.append(&render_state).unwrap();
    menu.append(&last).unwrap();
    menu.append(&compose).unwrap();
    menu.append(&settings).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    for row in &rows {
      menu.append(&row.item).unwrap();
    }
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&quit).unwrap();

    let glyphs = tray_art::Glyphs::load();
    let icon = TrayIconBuilder::new()
      .with_menu(Box::new(menu))
      .with_icon(glyphs.icon(tray_art::State::Idle))
      .with_icon_as_template(true)
      .with_tooltip("fastflow")
      .build()
      .expect("tray icon");

    self.tray = Some(Tray {
      icon,
      glyphs,
      shown: std::cell::Cell::new(tray_art::State::Idle),
      record,
      render_state,
      last,
      last_folder,
      last_render,
      last_raw,
      last_compose,
      compose,
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
    let runner = Box::new(|work: &Job, progress: &mut dyn FnMut(f64)| {
      let id = &work.id;
      let dir = paths::recordings().join(id);
      let done = |d: u64, t: u64| d as f64 / t.max(1) as f64;
      if work.task == Task::Prepare {
        log(format!("prepare {id} started"));
        let out = job::prepare(
          &dir,
          &mut |line| log(format!("prepare {id}: {line}")),
          &mut |d, t| progress(done(d, t)),
        )?;
        return Ok(out.to_string_lossy().into_owned());
      }
      let jobs: Vec<RenderJob> = match &work.output {
        Some(name) => vec![job::output_job(&dir, name)?],
        None => {
          let slices = job::load_slices(&dir)?;
          slices
            .outputs
            .iter()
            .map(|o| RenderJob {
              dir: dir.clone(),
              out: job::output_path(&dir, o),
              target: Target::of(&slices, o),
              boxes: false,
              preview: false,
            })
            .collect()
        }
      };
      let mut written = Vec::new();
      for (i, rj) in jobs.iter().enumerate() {
        log(format!("render {id} started: {}", rj.out.display()));
        let report = job::run(
          rj,
          &mut |line| log(format!("render {id}: {line}")),
          &mut |d, t| progress((i as f64 + done(d, t)) / jobs.len() as f64),
        )?;
        written.push(report.out.to_string_lossy().into_owned());
      }
      written
        .pop()
        .ok_or_else(|| format!("{id} has no outputs to render"))
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

  /// Nothing is rendered when a recording stops. `render.on_stop` decides whether its preview
  /// is prepared and the composition window opened.
  fn stop_recording(&mut self) -> Result<String, String> {
    let r = self.recorder.take().ok_or("not recording")?;
    if let Some(o) = self.overlay.take() {
      o.close();
    }
    let result = r.stop();
    self.show_recording_state();
    let id = result?;
    let config = paths::recordings().join(&id).join("config.toml");
    let on_stop = settings::load(&config).map_or(OnStop::Editor, |c| c.render.on_stop);
    if on_stop != OnStop::None
      && let Some(q) = &mut self.queue
    {
      q.push(Job::prepare(&id));
      self.open_when_prepared = Some((id.clone(), on_stop));
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
      queued: q.queued.iter().map(Job::label).collect(),
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
      Request::Render { id, output } => self
        .queue_job(Job::render(&id, output.as_deref()))
        .map(|()| Response::with_id(id)),
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

  fn queue_job(&mut self, work: Job) -> Result<(), String> {
    let id = &work.id;
    if !paths::recordings().join(id).join("meta.json").is_file() {
      return Err(format!("no recording {id}"));
    }
    if self.recorder.as_ref().is_some_and(|r| r.id() == *id) {
      return Err(format!("{id} is still recording"));
    }
    if let Some(q) = &mut self.queue {
      q.push(work);
    }
    self.show_render_state();
    Ok(())
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
    let label = f.job.label();
    match (f.job.task, &f.result) {
      (Task::Render, Ok(path)) => {
        log(format!("render {label} done: {path}"));
        notify::post(&f.job.id, "Render finished", &label);
      }
      (Task::Render, Err(e)) => {
        log(format!("render {label} failed: {e}"));
        notify::post(&f.job.id, "Render failed", e);
      }
      (Task::Prepare, Ok(path)) => log(format!("prepare {} done: {path}", f.job.id)),
      (Task::Prepare, Err(e)) => {
        log(format!("prepare {} failed: {e}", f.job.id));
        notify::post(&f.job.id, "Preview failed", e);
      }
    }
    if f.job.task == Task::Prepare {
      let mtm = MainThreadMarker::new().expect("main thread");
      if let Some(w) = &mut self.compose {
        w.preview_ready(&f.job.id, &f.result, mtm);
      }
      let waiting = self
        .open_when_prepared
        .take_if(|(id, _)| *id == f.job.id)
        .map(|(_, mode)| mode);
      match (waiting, &f.result) {
        (Some(OnStop::Editor), Ok(_)) => self.open_compose(Some(&f.job.id)),
        (Some(OnStop::Scrub), Ok(_)) => notify::post(
          &f.job.id,
          "Preview ready",
          &format!("{}: pick slices and render in Compose.", f.job.id),
        ),
        _ => {}
      }
    }
    self.show_render_state();
  }

  fn open_compose(&mut self, id: Option<&str>) {
    let mtm = MainThreadMarker::new().expect("main thread");
    let w = self
      .compose
      .get_or_insert_with(|| compose_window::ComposeWindow::new(mtm));
    w.show(mtm);
    if let Some(id) = id {
      w.select(id);
    }
  }

  fn show_render_state(&self) {
    let (Some(tray), Some(q)) = (&self.tray, &self.queue) else {
      return;
    };
    let s = q.status();
    let text = match (&s.rendering, &s.last) {
      (Some(r), _) if s.queued.is_empty() => {
        format!("Rendering {}: {:.0}%", r.job.label(), r.progress * 100.0)
      }
      (Some(r), _) => format!(
        "Rendering {}: {:.0}%, {} queued",
        r.job.label(),
        r.progress * 100.0,
        s.queued.len()
      ),
      (None, Some(Finished { job, result: Ok(_) })) => format!("Last: {}", job.label()),
      (
        None,
        Some(Finished {
          job,
          result: Err(_),
        }),
      ) => {
        format!("Failed: {}", job.label())
      }
      (None, None) => "No renders yet".into(),
    };
    tray.render_state.set_text(text);
    self.refresh_icon();

    tray.last.set_enabled(self.last_id.is_some());
    let Some(id) = &self.last_id else { return };
    let dir = paths::recordings().join(id);
    let renders = |j: &Job| j.id == *id && j.task == Task::Render;
    let (label, ready) = match &s.rendering {
      Some(r) if renders(&r.job) => (
        format!("Play Render (rendering {:.0}%)", r.progress * 100.0),
        false,
      ),
      _ if s.queued.iter().any(renders) => ("Play Render (queued)".into(), false),
      _ if dir.join("render.mp4").is_file() => ("Play Render".into(), true),
      _ => ("Play Render (not rendered)".into(), false),
    };
    tray.last_render.set_text(label);
    tray.last_render.set_enabled(ready);
    tray.last_raw.set_enabled(first_raw(&dir).is_some());
  }

  /// The menu bar icon: filled while recording, filling from the left while rendering.
  fn refresh_icon(&self) {
    let Some(tray) = &self.tray else { return };
    let rendering = self.queue.as_ref().and_then(|q| q.status().rendering);
    let state = match (self.recorder.is_some(), rendering) {
      (true, _) => tray_art::State::Recording,
      (false, Some(r)) => tray_art::State::rendering(r.progress),
      (false, None) => tray_art::State::Idle,
    };
    if tray.shown.replace(state) != state {
      let _ = tray.icon.set_icon(Some(tray.glyphs.icon(state)));
      tray.icon.set_icon_as_template(true);
    }
  }

  fn refresh_last_recording(&mut self) {
    self.last_id = paths::list_recordings(10)
      .into_iter()
      .find(|r| !r.recording)
      .map(|r| r.id);
  }

  fn open_last(&mut self, what: LastAction) {
    let Some(id) = self.last_id.clone() else {
      return;
    };
    let dir = paths::recordings().join(&id);
    let target = match what {
      LastAction::Folder => Some(dir),
      LastAction::Render => Some(dir.join("render.mp4")),
      LastAction::Raw => first_raw(&dir),
      LastAction::Compose => {
        self.open_compose(Some(&id));
        None
      }
    };
    if let Some(t) = target {
      let _ = Command::new("open").arg(&t).status();
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
    self.refresh_icon();
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
    let last = [
      (tray.last_folder.id(), LastAction::Folder),
      (tray.last_render.id(), LastAction::Render),
      (tray.last_raw.id(), LastAction::Raw),
      (tray.last_compose.id(), LastAction::Compose),
    ]
    .into_iter()
    .find(|(id, _)| event.id == **id)
    .map(|(_, a)| a);
    if let Some(action) = last {
      self.open_last(action);
      return;
    }
    if event.id == *tray.compose.id() {
      self.open_compose(None);
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
        self.refresh_last_recording();
        self.show_render_state();
        if let Some(q) = &self.queue {
          let mtm = MainThreadMarker::new().expect("main thread");
          self
            .panel
            .get_or_insert_with(|| render_panel::RenderPanel::new(mtm))
            .update(&q.status(), mtm);
        }
        let mut asked = Vec::new();
        if let Some(w) = &mut self.compose {
          let mtm = MainThreadMarker::new().expect("main thread");
          w.tick(mtm);
          asked = w.take_jobs();
        }
        if let Some(id) = self.settings.as_mut().and_then(|s| s.tick()) {
          asked.push(Job::prepare(&id));
        }
        for work in asked {
          if let Err(e) = self.queue_job(work) {
            log(format!("queue: {e}"));
          }
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
      .is_some_and(|q| q.status().rendering.is_some())
      || self.panel.as_ref().is_some_and(|p| p.is_active());
    let window_open = self.compose.as_ref().is_some_and(|g| g.is_visible())
      || self.settings.as_ref().is_some_and(|s| s.is_visible());
    let period = if self.overlay.is_some() {
      OVERLAY_TICK
    } else if window_open {
      COMPOSE_WINDOW_TICK
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

#[derive(Clone, Copy)]
enum LastAction {
  Folder,
  Render,
  Raw,
  Compose,
}

/// The first segment's raw capture, which is the one a player can open on its own.
fn first_raw(dir: &std::path::Path) -> Option<std::path::PathBuf> {
  let text = std::fs::read_to_string(dir.join("meta.json")).ok()?;
  let meta: fastflow_core::recording::Meta = serde_json::from_str(&text).ok()?;
  let path = dir.join(&meta.segments.first()?.file);
  path.is_file().then_some(path)
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
    compose: None,
    open_when_prepared: None,
    settings: None,
    panel: None,
    last_id: None,
  };
  event_loop.run_app(&mut app).expect("event loop");
}
