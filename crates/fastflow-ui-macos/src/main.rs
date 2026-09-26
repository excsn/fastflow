mod applog;
mod build_id;
mod notify;
mod recorder;
mod setup;

use std::process::Command;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use applog::log;
use fastflow_daemon::paths;
use fastflow_daemon::protocol::{Finished, Request, Response, Status};
use fastflow_daemon::queue::RenderQueue;
use fastflow_daemon::server::{self, Handler};
use fastflow_desktop::permissions::{self, Grant, Permission};
use fastflow_render::job::{self, RenderJob};
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
const RENDERING_TICK: Duration = Duration::from_millis(500);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

enum UserEvent {
    Menu(MenuEvent),
    Request(Request, Sender<Response>),
    RenderDone(Finished),
}

/// Socket requests run on the main thread, where the recorder and its event tap live.
struct SocketHandler {
    proxy: Mutex<EventLoopProxy<UserEvent>>,
}

impl Handler for SocketHandler {
    fn handle(&self, request: Request) -> Response {
        let (tx, rx) = mpsc::channel();
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
    rows: Vec<PermissionRow>,
    quit: MenuItem,
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    tray: Option<Tray>,
    setup: Option<SetupWindow>,
    recorder: Option<Recorder>,
    queue: Option<RenderQueue>,
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
        let record = MenuItem::new("Start Recording", true, None);
        let render_state = MenuItem::new("No renders yet", false, None);
        let reveal = MenuItem::new("Show Last Render", false, None);
        let quit = MenuItem::new("Quit", true, None);

        menu.append(&title).unwrap();
        menu.append(&PredefinedMenuItem::separator()).unwrap();
        menu.append(&record).unwrap();
        menu.append(&render_state).unwrap();
        menu.append(&reveal).unwrap();
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
            row.item
                .set_text(format!("{}: {state}", row.permission.label()));
            row.item.set_enabled(grant != Grant::Granted);
        }
    }

    fn show_setup(&mut self, updated: bool) {
        let mtm = MainThreadMarker::new().expect("main thread");
        self.setup
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
        self.start_daemon();
        notify::request_permission();
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
                    dir,
                    out,
                    boxes: false,
                },
                &mut |line| log(format!("render {id}: {line}")),
                &mut |done, total| progress(done as f64 / total.max(1) as f64),
            )?;
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

    fn start_recording(&mut self) -> Result<String, String> {
        if let Some(r) = &self.recorder {
            return Err(format!("already recording {}", r.id()));
        }
        let r = Recorder::start()?;
        let id = r.id();
        self.recorder = Some(r);
        self.show_recording_state();
        Ok(id)
    }

    /// A recording that stopped cleanly is queued for rendering straight away.
    fn stop_recording(&mut self) -> Result<String, String> {
        let r = self.recorder.take().ok_or("not recording")?;
        let result = r.stop();
        self.show_recording_state();
        let id = result?;
        if let Some(q) = &self.queue {
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
                    if let Some(q) = &self.queue {
                        q.push(&id);
                    }
                    self.show_render_state();
                    Ok(Response::with_id(id))
                }
            }
            Request::List { limit } => Ok(Response {
                recordings: Some(paths::list_recordings(limit)),
                ..Response::ok()
            }),
        };
        result.unwrap_or_else(Response::error)
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
        tray.reveal
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
            "Stop Recording"
        } else {
            "Start Recording"
        });
        let _ = tray.icon.set_icon(Some(tray_glyph(recording)));
        tray.icon.set_icon_as_template(true);
    }

    fn tick_recording(&mut self) {
        let Some(r) = &mut self.recorder else { return };
        if let Err(why) = r.tick() {
            log(format!("capture ended unexpectedly: {why}"));
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
        let period = if self.recorder.is_some() {
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
        }
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}

/// A ring when idle, a filled disc while recording.
fn tray_glyph(recording: bool) -> Icon {
    const SIZE: u32 = 36;
    let c = (SIZE as f32 - 1.0) / 2.0;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            let filled = if recording {
                d <= 16.0
            } else {
                (11.0..=16.0).contains(&d) || d <= 5.0
            };
            let alpha = if filled { 255 } else { 0 };
            rgba.extend_from_slice(&[0, 0, 0, alpha]);
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).expect("icon")
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
    };
    event_loop.run_app(&mut app).expect("event loop");
}
