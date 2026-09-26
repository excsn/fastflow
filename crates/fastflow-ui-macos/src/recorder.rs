use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use fastflow_capture::{CaptureSession, CaptureSpec};
use fastflow_core::geom::Point;
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
const SEGMENT_FILE: &str = "raw.0.mp4";

enum Record {
    Anchor(Instant),
    Input(RawInput),
    Windows(Instant, Point, Vec<WindowInfo>),
}

pub struct Recorder {
    dir: PathBuf,
    meta: Meta,
    capture: Box<dyn CaptureSession>,
    input: MacInputMonitor,
    records: Sender<Record>,
    anchored: bool,
    sampling: Arc<AtomicBool>,
    sampler: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
}

#[derive(Serialize)]
struct State {
    pid: u32,
    capture_pid: Option<u32>,
}

impl Recorder {
    pub fn start() -> Result<Self, String> {
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

        let mut backend = fastflow_capture::detect().map_err(|e| e.to_string())?;
        let capture = backend
            .start(&CaptureSpec {
                display_index: display.index,
                fps: FPS,
                out: dir.join(SEGMENT_FILE),
            })
            .map_err(|e| e.to_string())?;

        let meta = Meta {
            format: FORMAT_VERSION,
            started_at: started.to_rfc3339(),
            backend: backend.name().into(),
            fps: FPS,
            first_frame: capture.confidence(),
            segments: vec![segment(&display)],
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
        let sampler = {
            let sampling = Arc::clone(&sampling);
            let tx = tx.clone();
            let mut source = MacWindowSource::new(display.bounds_pt);
            thread::spawn(move || sample_windows(&mut source, &sampling, &tx))
        };

        Ok(Recorder {
            dir,
            meta,
            capture,
            input,
            records: tx,
            anchored: false,
            sampling,
            sampler: Some(sampler),
            writer: Some(writer),
        })
    }

    /// Call on the main thread every tick. `Err` means the capture died and the recording is over.
    pub fn tick(&mut self) -> Result<(), String> {
        self.input.maintain();
        if !self.anchored
            && let Some(at) = self.capture.first_frame_at()
        {
            self.anchored = true;
            let _ = self.records.send(Record::Anchor(at));
            log("first frame observed");
        }
        match self.capture.exited() {
            Some(why) => Err(why),
            None => Ok(()),
        }
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
        let anchor = self.capture.first_frame_at();
        let result = self.capture.stop();

        drop(self.records);
        if let Some(h) = self.writer.take() {
            let _ = h.join();
        }

        let seg = &mut self.meta.segments[0];
        seg.end_ms = anchor.map(|a| ms_since(a, end));
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

fn segment(d: &Display) -> SegmentInfo {
    SegmentInfo {
        index: 0,
        file: SEGMENT_FILE.into(),
        display_id: d.id,
        surface_px: [d.pixels.0, d.pixels.1],
        surface_pt: [d.bounds_pt.w, d.bounds_pt.h],
        scale: d.scale,
        start_ms: 0,
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

fn sample_windows(source: &mut MacWindowSource, running: &AtomicBool, tx: &Sender<Record>) {
    let mut next = Instant::now();
    while running.load(Ordering::Relaxed) {
        let at = Instant::now();
        match (source.cursor(), source.sample()) {
            (Ok(cursor), Ok(windows)) => {
                if tx.send(Record::Windows(at, cursor, windows)).is_err() {
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
            Record::Windows(at, cursor, list) => {
                let t = ms_since(anchor, at);
                if t < 0 {
                    return Ok(());
                }
                let sample = WindowSample {
                    t,
                    segment: 0,
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
