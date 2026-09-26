use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use fastflow_capture::{CaptureSession, CaptureSpec, ScreenCapture};
use fastflow_core::camera::Geometry;
use fastflow_core::config::{CameraConfig, Config};
use fastflow_core::geom::{Point, Rect};
use fastflow_core::recording::{
    FORMAT_VERSION, InputEvent, InputKind, Meta, SegmentInfo, WindowInfo, WindowSample,
};
use fastflow_daemon::paths;
use fastflow_desktop::macos::display::{self, Display};
use fastflow_desktop::macos::input::MacInputMonitor;
use fastflow_desktop::macos::window::MacWindowSource;
use fastflow_desktop::{InputMonitor, RawInput, WindowSource};
use serde::Serialize;

use crate::applog::log;

const FPS: u32 = 60;
const SAMPLE_PERIOD: Duration = Duration::from_millis(100);
/// A cursor crossing a display edge on its way somewhere else must not start a segment.
const DISPLAY_COMMIT: Duration = Duration::from_secs(1);
/// A new stream that has not delivered a frame by then is abandoned and the old one kept.
const SWITCH_TIMEOUT: Duration = Duration::from_secs(3);
const DISPLAY_CHECK_EVERY: Duration = Duration::from_secs(1);

enum Record {
    Anchor(Instant),
    Input(RawInput),
    Windows(Instant, u32, Point, Vec<WindowInfo>),
}

/// The surface the sampler normalizes against and the segment its samples belong to.
#[derive(Clone, Copy)]
struct Target {
    segment: u32,
    surface: Rect,
}

/// A second capture running alongside the current one until it delivers its first frame.
struct Switch {
    capture: Box<dyn CaptureSession>,
    display: Display,
    file: String,
    started: Instant,
}

pub enum Event {
    /// Recording moved to another display. The overlay should follow.
    Switched(OverlayParams),
}

pub struct Recorder {
    dir: PathBuf,
    meta: Meta,
    cfg: Config,
    backend: Box<dyn ScreenCapture>,
    display: Display,
    capture: Box<dyn CaptureSession>,
    switch: Option<Switch>,
    pending: Option<(u32, Instant)>,
    last_display_check: Instant,
    input: MacInputMonitor,
    records: Sender<Record>,
    anchor: Option<Instant>,
    target: Arc<Mutex<Target>>,
    sampling: Arc<AtomicBool>,
    sampler: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    latest: Arc<Mutex<Option<WindowSample>>>,
}

/// What the live overlay needs. Offered only when the backend keeps fastflow's own windows out
/// of the footage.
pub struct OverlayParams {
    pub display_pt: Rect,
    pub geo: Geometry,
    pub camera: CameraConfig,
}

#[derive(Serialize)]
struct State {
    pid: u32,
    capture_pid: Option<u32>,
}

impl Recorder {
    /// `before_capture` runs when the overlay is wanted, before the capture starts. ScreenCaptureKit
    /// can only exclude an app that already has a window, so the overlay must exist by then.
    pub fn start(before_capture: &mut dyn FnMut(&OverlayParams)) -> Result<Self, String> {
        let display = display::under_cursor().ok_or("no active display")?;
        let started = chrono::Local::now();
        let dir = paths::recordings().join(started.format("%Y-%m-%d-%H%M%S").to_string());
        fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        if paths::default_config().is_file()
            && let Err(e) = fs::copy(paths::default_config(), dir.join("config.toml"))
        {
            log(format!("copy default config: {e}"));
        }
        log(format!(
            "recording to {} on display {display:?}",
            dir.display()
        ));

        let cfg = read_config(&dir);
        let mut backend =
            fastflow_capture::detect(&cfg.capture.backend).map_err(|e| e.to_string())?;
        let file = fastflow_capture::segment_file(backend.name(), 0);
        if let Some(p) = overlay_params(backend.as_ref(), &cfg, &display, &file) {
            before_capture(&p);
        }
        let capture = backend
            .start(&CaptureSpec {
                display_index: display.index,
                display_id: display.id,
                size_px: display.pixels,
                fps: FPS,
                out: dir.join(&file),
            })
            .map_err(|e| e.to_string())?;
        log(format!("capturing with {} to {file}", backend.name()));

        let meta = Meta {
            format: FORMAT_VERSION,
            started_at: started.to_rfc3339(),
            backend: backend.name().into(),
            fps: FPS,
            first_frame: capture.confidence(),
            segments: vec![segment(0, &display, file, 0)],
            recovered: false,
            truncated_by: None,
            failed: None,
        };
        write_json(&dir.join("meta.json"), &meta)?;
        write_json(
            &dir.join("state.json"),
            &State {
                pid: std::process::id(),
                capture_pid: capture.pid(),
            },
        )?;

        let (tx, rx) = mpsc::channel();
        let writer = {
            let dir = dir.clone();
            thread::spawn(move || write_sidecars(&dir, rx))
        };

        let (input_tx, input_rx) = mpsc::channel::<RawInput>();
        let forward = tx.clone();
        thread::spawn(move || {
            for e in input_rx {
                if forward.send(Record::Input(e)).is_err() {
                    break;
                }
            }
        });
        let mut input = MacInputMonitor::default();
        if let Err(e) = input.start(input_tx) {
            log(format!("input monitor: {e}"));
        }

        let sampling = Arc::new(AtomicBool::new(true));
        let latest = Arc::new(Mutex::new(None));
        let target = Arc::new(Mutex::new(Target {
            segment: 0,
            surface: display.bounds_pt,
        }));
        let sampler = {
            let sampling = Arc::clone(&sampling);
            let tx = tx.clone();
            let latest = Arc::clone(&latest);
            let target = Arc::clone(&target);
            thread::spawn(move || sample_windows(&target, &sampling, &tx, &latest))
        };

        Ok(Recorder {
            dir,
            meta,
            cfg,
            backend,
            display,
            capture,
            switch: None,
            pending: None,
            last_display_check: Instant::now(),
            input,
            records: tx,
            anchor: None,
            target,
            sampling,
            sampler: Some(sampler),
            writer: Some(writer),
            latest,
        })
    }

    fn can_follow(&self) -> bool {
        self.backend.caps().can_follow_displays
    }

    /// Switches to a new segment on the current display.
    pub fn resegment(&mut self) -> Result<(), String> {
        if !self.can_follow() {
            return Err(format!(
                "the {} backend cannot switch segments",
                self.backend.name()
            ));
        }
        if self.switch.is_some() {
            return Err("a switch is already in progress".into());
        }
        self.begin_switch(self.display);
        Ok(())
    }

    /// Starts a capture of `display` alongside the current one.
    fn begin_switch(&mut self, display: Display) {
        let index = self.meta.segments.len() as u32;
        let file = fastflow_capture::segment_file(self.backend.name(), index);
        let spec = CaptureSpec {
            display_index: display.index,
            display_id: display.id,
            size_px: display.pixels,
            fps: FPS,
            out: self.dir.join(&file),
        };
        match self.backend.start(&spec) {
            Ok(capture) => {
                log(format!("switching to display {} as {file}", display.id));
                self.switch = Some(Switch {
                    capture,
                    display,
                    file,
                    started: Instant::now(),
                });
            }
            Err(e) => log(format!("switch to display {}: {e}", display.id)),
        }
    }

    /// Retires the old capture once the new one has a frame. That frame is the boundary.
    fn finish_switch(&mut self) -> Result<Option<Event>, String> {
        let Some(sw) = &mut self.switch else {
            return Ok(None);
        };
        if let Some(why) = sw.capture.exited() {
            log(format!("switch abandoned: {why}"));
            if let Some(sw) = self.switch.take() {
                let _ = sw.capture.stop();
                let _ = fs::remove_file(self.dir.join(&sw.file));
            }
            return Ok(None);
        }
        let Some(boundary) = sw.capture.first_frame_at() else {
            if sw.started.elapsed() > SWITCH_TIMEOUT {
                log("switch abandoned: the new display delivered no frame");
                if let Some(sw) = self.switch.take() {
                    let _ = sw.capture.stop();
                    let _ = fs::remove_file(self.dir.join(&sw.file));
                }
            }
            return Ok(None);
        };
        let sw = self.switch.take().expect("checked above");
        let Some(anchor) = self.anchor else {
            let _ = sw.capture.stop();
            return Ok(None);
        };
        let at = ms_since(anchor, boundary);
        let old = std::mem::replace(&mut self.capture, sw.capture);
        if let Err(e) = old.stop() {
            log(format!("stop previous segment: {e}"));
        }
        if let Some(last) = self.meta.segments.last_mut() {
            last.end_ms = Some(at);
        }
        let index = self.meta.segments.len() as u32;
        self.meta
            .segments
            .push(segment(index, &sw.display, sw.file.clone(), at));
        *self.target.lock().unwrap() = Target {
            segment: index,
            surface: sw.display.bounds_pt,
        };
        self.display = sw.display;
        write_json(&self.dir.join("meta.json"), &self.meta)?;
        log(format!("segment {index} starts at {at}ms"));
        Ok(
            overlay_params(self.backend.as_ref(), &self.cfg, &self.display, &sw.file)
                .map(Event::Switched),
        )
    }

    /// Follows the cursor to another display after `DISPLAY_COMMIT`. Also reacts to the captured
    /// display changing size or going away. A backend that cannot follow ends the recording.
    fn watch_displays(&mut self) -> Result<(), String> {
        if self.switch.is_some() {
            return Ok(());
        }
        if self.last_display_check.elapsed() >= DISPLAY_CHECK_EVERY {
            self.last_display_check = Instant::now();
            let now = display::active()
                .into_iter()
                .find(|d| d.id == self.display.id);
            let changed = match now {
                None => true,
                Some(d) => d.pixels != self.display.pixels || d.bounds_pt != self.display.bounds_pt,
            };
            if changed {
                if !self.can_follow() {
                    self.meta.truncated_by = Some("display_change".into());
                    return Err("the captured display changed".into());
                }
                if let Some(d) = now.or_else(display::under_cursor) {
                    self.begin_switch(d);
                }
                return Ok(());
            }
        }
        if !self.can_follow() {
            return Ok(());
        }
        let Some(under) = display::under_cursor() else {
            return Ok(());
        };
        if under.id == self.display.id {
            self.pending = None;
            return Ok(());
        }
        match self.pending {
            Some((id, since)) if id == under.id && since.elapsed() >= DISPLAY_COMMIT => {
                self.pending = None;
                self.begin_switch(under);
            }
            Some((id, _)) if id == under.id => {}
            _ => self.pending = Some((under.id, Instant::now())),
        }
        Ok(())
    }

    /// The newest window sample, timed from when sampling started.
    pub fn latest_sample(&self) -> Option<WindowSample> {
        self.latest.lock().unwrap().clone()
    }

    /// Call on the main thread every tick. `Err` means the recording is over.
    pub fn tick(&mut self) -> Result<Option<Event>, String> {
        self.input.maintain();
        if self.anchor.is_none()
            && let Some(at) = self.capture.first_frame_at()
        {
            self.anchor = Some(at);
            let _ = self.records.send(Record::Anchor(at));
            log("first frame observed");
        }
        if let Some(why) = self.capture.exited() {
            return Err(why);
        }
        let event = self.finish_switch()?;
        if self.anchor.is_some() {
            self.watch_displays()?;
        }
        Ok(event)
    }

    /// Writes a marker into `input.jsonl` at the current moment.
    pub fn mark(&self, kind: InputKind) {
        let _ = self.records.send(Record::Input(RawInput {
            at: Instant::now(),
            kind,
        }));
        log(format!("marker {kind:?}"));
    }

    /// Recorded in `meta.json` when the recording is stopped for a reason other than the user.
    pub fn truncate(&mut self, why: &str) {
        self.meta.truncated_by = Some(why.to_owned());
    }

    pub fn id(&self) -> String {
        recording_id(&self.dir)
    }

    /// `Ok` holds the recording's id. `Err` holds why it failed.
    pub fn stop(mut self) -> Result<String, String> {
        let end = Instant::now();
        self.input.stop();
        self.sampling.store(false, Ordering::Relaxed);
        if let Some(h) = self.sampler.take() {
            let _ = h.join();
        }
        if let Some(sw) = self.switch.take() {
            let _ = sw.capture.stop();
            let _ = fs::remove_file(self.dir.join(&sw.file));
        }
        let anchor = self.anchor.or_else(|| self.capture.first_frame_at());
        let result = self.capture.stop();
        if let Ok(fastflow_capture::CaptureArtifact {
            frames: Some((written, dropped)),
            ..
        }) = &result
        {
            log(format!("{written} frames written, {dropped} dropped"));
        }

        drop(self.records);
        if let Some(h) = self.writer.take() {
            let _ = h.join();
        }

        if let Some(seg) = self.meta.segments.last_mut() {
            seg.end_ms = anchor.map(|a| ms_since(a, end));
        }
        self.meta.failed = match (&result, anchor) {
            (Err(e), _) => Some(e.to_string()),
            (Ok(_), None) => Some("no frame was captured".into()),
            (Ok(_), Some(_)) => None,
        };
        if let Err(e) = write_json(&self.dir.join("meta.json"), &self.meta) {
            log(format!("meta.json: {e}"));
        }
        let _ = fs::remove_file(self.dir.join("state.json"));
        log(format!(
            "recording stopped: {} ({})",
            self.meta.failed.as_deref().unwrap_or("ok"),
            self.dir.display()
        ));
        match self.meta.failed.take() {
            Some(why) => Err(why),
            None => Ok(recording_id(&self.dir)),
        }
    }
}

/// The recording's own `config.toml`, copied from the defaults. Defaults when absent or invalid.
fn read_config(dir: &Path) -> Config {
    let path = dir.join("config.toml");
    match fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
            log(format!("{}: {e}", path.display()));
            Config::default()
        }),
        Err(_) => Config::default(),
    }
}

fn overlay_params(
    backend: &dyn ScreenCapture,
    cfg: &Config,
    display: &Display,
    file: &str,
) -> Option<OverlayParams> {
    (backend.caps().can_exclude_windows && cfg.camera.enabled && cfg.camera.live_overlay).then(
        || OverlayParams {
            display_pt: display.bounds_pt,
            geo: Geometry::new(&segment(0, display, file.to_owned(), 0), cfg.output.size),
            camera: cfg.camera.clone(),
        },
    )
}

fn segment(index: u32, d: &Display, file: String, start_ms: i64) -> SegmentInfo {
    SegmentInfo {
        index,
        file,
        display_id: d.id,
        surface_px: [d.pixels.0, d.pixels.1],
        surface_pt: [d.bounds_pt.w, d.bounds_pt.h],
        scale: d.scale,
        start_ms,
        end_ms: None,
    }
}

fn recording_id(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    fs::write(&tmp, json).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
}

fn ms_since(anchor: Instant, at: Instant) -> i64 {
    match at.checked_duration_since(anchor) {
        Some(d) => d.as_millis() as i64,
        None => -(anchor.duration_since(at).as_millis() as i64),
    }
}

fn sample_windows(
    target: &Mutex<Target>,
    running: &AtomicBool,
    tx: &Sender<Record>,
    latest: &Mutex<Option<WindowSample>>,
) {
    let started = Instant::now();
    let mut next = started;
    while running.load(Ordering::Relaxed) {
        let at = Instant::now();
        let Target { segment, surface } = *target.lock().unwrap();
        let mut source = MacWindowSource::new(surface);
        match (source.cursor(), source.sample()) {
            (Ok(cursor), Ok(windows)) => {
                *latest.lock().unwrap() = Some(WindowSample {
                    t: at.duration_since(started).as_millis() as i64,
                    segment,
                    cursor,
                    windows: windows.clone(),
                });
                if tx
                    .send(Record::Windows(at, segment, cursor, windows))
                    .is_err()
                {
                    return;
                }
            }
            (Err(e), _) | (_, Err(e)) => log(format!("window sample: {e}")),
        }
        next += SAMPLE_PERIOD;
        thread::sleep(next.saturating_duration_since(Instant::now()));
    }
}

/// Holds records until the first frame is known, since every `t` is relative to it. Records
/// from before the first frame have nothing on screen to line up with and are dropped.
fn write_sidecars(dir: &Path, rx: Receiver<Record>) {
    let open = |name: &str| File::create(dir.join(name)).map(BufWriter::new);
    let (Ok(mut input), Ok(mut windows)) = (open("input.jsonl"), open("windows.jsonl")) else {
        log("could not create sidecar files");
        return;
    };
    let mut anchor: Option<Instant> = None;
    let mut pending: Vec<Record> = Vec::new();

    let mut write = |r: Record, anchor: Instant| -> std::io::Result<()> {
        match r {
            Record::Anchor(_) => Ok(()),
            Record::Input(e) => {
                let t = ms_since(anchor, e.at);
                if t < 0 {
                    return Ok(());
                }
                let line = serde_json::to_string(&InputEvent { t, kind: e.kind })?;
                writeln!(input, "{line}")?;
                input.flush()
            }
            Record::Windows(at, segment, cursor, list) => {
                let t = ms_since(anchor, at);
                if t < 0 {
                    return Ok(());
                }
                let sample = WindowSample {
                    t,
                    segment,
                    cursor,
                    windows: list,
                };
                writeln!(windows, "{}", serde_json::to_string(&sample)?)?;
                windows.flush()
            }
        }
    };

    for r in rx {
        let result = match (anchor, r) {
            (None, Record::Anchor(at)) => {
                anchor = Some(at);
                pending.drain(..).try_for_each(|p| write(p, at))
            }
            (None, r) => {
                pending.push(r);
                Ok(())
            }
            (Some(at), r) => write(r, at),
        };
        if let Err(e) = result {
            log(format!("sidecar write: {e}"));
        }
    }
}
