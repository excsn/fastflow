mod applog;
mod build_id;
mod recorder;
mod setup;

use std::time::{Duration, Instant};

use applog::log;
use fastflow_desktop::permissions::{self, Grant, Permission};
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

enum UserEvent {
    Menu(MenuEvent),
}

struct PermissionRow {
    permission: Permission,
    item: MenuItem,
    grant: Option<Grant>,
}

struct Tray {
    icon: TrayIcon,
    record: MenuItem,
    rows: Vec<PermissionRow>,
    quit: MenuItem,
}

#[derive(Default)]
struct App {
    tray: Option<Tray>,
    setup: Option<SetupWindow>,
    recorder: Option<Recorder>,
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
        let quit = MenuItem::new("Quit", true, None);

        menu.append(&title).unwrap();
        menu.append(&PredefinedMenuItem::separator()).unwrap();
        menu.append(&record).unwrap();
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

    fn toggle_recording(&mut self) {
        match self.recorder.take() {
            Some(r) => {
                r.stop();
            }
            None => match Recorder::start() {
                Ok(r) => self.recorder = Some(r),
                Err(e) => log(format!("start recording: {e}")),
            },
        }
        self.show_recording_state();
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
                r.stop();
            }
            self.show_recording_state();
        }
    }

    fn on_menu(&mut self, event: MenuEvent, event_loop: &ActiveEventLoop) {
        let Some(tray) = &self.tray else { return };
        if event.id == *tray.quit.id() {
            if let Some(r) = self.recorder.take() {
                r.stop();
            }
            event_loop.exit();
            return;
        }
        if event.id == *tray.record.id() {
            self.toggle_recording();
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
                self.refresh_permissions();
                if let Some(setup) = self.setup.as_ref().filter(|s| s.is_visible()) {
                    setup.refresh();
                }
            }
            _ => {}
        }
        let period = if self.recorder.is_some() {
            RECORDING_TICK
        } else {
            PERMISSION_POLL
        };
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + period));
    }

    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Menu(e) => self.on_menu(e, event_loop),
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
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(UserEvent::Menu(e));
    }));

    event_loop.run_app(&mut App::default()).expect("event loop");
}
