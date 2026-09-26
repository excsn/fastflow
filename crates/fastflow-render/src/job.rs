//! Renders a recording directory end to end. Shared by the CLI and the daemon.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use fastflow_core::camera::{self, CameraTrack, Geometry};
use fastflow_core::config::Config;
use fastflow_core::geom::Rect;
use fastflow_core::markers::Markers;
use fastflow_core::recording::{InputEvent, Meta, WindowSample};
use fastflow_core::timeline::{SpanKind, Timeline};

use crate::compositor::{self, Framing, RenderSettings};
use crate::ffmpeg::{Decode, FfmpegSink, FfmpegSource};
use crate::focus::{FocusBlur, FocusMask, FocusSettings};
use crate::overlay::{self, Letterbox};
use crate::{Frame, FrameSink};

pub struct RenderJob {
    pub dir: PathBuf,
    pub out: PathBuf,
    /// Draw the camera's inputs over the whole frame instead of cropping.
    pub boxes: bool,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub out: PathBuf,
    pub frames: u64,
    pub seconds: f64,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn read_config(dir: &Path) -> Result<Config, String> {
    let path = dir.join("config.toml");
    match fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// A truncated final line is what a killed recorder leaves behind, so it is skipped.
fn read_input(path: &Path) -> Result<Vec<InputEvent>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(text
        .lines()
        .filter_map(|l| serde_json::from_str::<InputEvent>(l).ok())
        .collect())
}

/// An ffmetadata file with a chapter at the start and one at each Chapter marker.
fn write_chapters(path: &Path, starts_ms: &[i64], end_ms: i64) -> Result<(), String> {
    let mut text = String::from(";FFMETADATA1\n");
    let mut starts: Vec<i64> = std::iter::once(0)
        .chain(starts_ms.iter().copied())
        .filter(|&s| s >= 0 && s < end_ms)
        .collect();
    starts.dedup();
    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(end_ms);
        let title = if i == 0 {
            "Start".to_owned()
        } else {
            format!("Chapter {i}")
        };
        text.push_str(&format!(
            "[CHAPTER]\nTIMEBASE=1/1000\nSTART={start}\nEND={end}\ntitle={title}\n"
        ));
    }
    fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

fn read_windows(path: &Path) -> Result<Vec<WindowSample>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut samples: Vec<WindowSample> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    samples.sort_by_key(|s| s.t);
    Ok(samples)
}

const GREY: overlay::Rgb = [150, 150, 150];
const GREEN: overlay::Rgb = [40, 220, 90];
const RED: overlay::Rgb = [240, 50, 50];
const YELLOW: overlay::Rgb = [250, 220, 40];

fn draw_boxes(
    frame: &mut Frame,
    samples: &[WindowSample],
    src_t: f64,
    camera_rect: Rect,
    geo: &Geometry,
    cfg: &Config,
    lb: &Letterbox,
) {
    let ms = (src_t * 1000.0) as i64;
    let i = samples.partition_point(|s| s.t <= ms);
    let Some(sample) = i.checked_sub(1).map(|i| &samples[i]) else {
        return;
    };
    for w in sample.windows.iter().filter(|w| w.layer == 0) {
        overlay::stroke(frame, &lb.rect(&w.rect), GREY, 1);
    }
    if let Some(w) = camera::subject(sample, geo, &cfg.camera) {
        overlay::stroke(frame, &lb.rect(&w.rect), GREEN, 3);
    }
    overlay::stroke(frame, &lb.rect(&camera_rect), RED, 3);
    let c = lb.rect(&Rect {
        x: sample.cursor.x,
        y: sample.cursor.y,
        w: 0.0,
        h: 0.0,
    });
    overlay::dot(frame, c.x, c.y, 6, YELLOW);
}

/// Renders `job.dir` to `job.out`. `log` receives the summary lines, `progress` frames done and
/// the total.
pub fn run(
    job: &RenderJob,
    log: &mut dyn FnMut(String),
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Report, String> {
    let (dir, out) = (job.dir.as_path(), job.out.as_path());
    let meta: Meta = read_json(&dir.join("meta.json"))?;
    let cfg = read_config(dir)?;
    let input = read_input(&dir.join("input.jsonl"))?;
    let events: Vec<f64> = input
        .iter()
        .filter(|e| !e.kind.is_marker())
        .map(|e| e.t as f64 / 1000.0)
        .collect();
    let segment = meta
        .segments
        .first()
        .cloned()
        .ok_or("meta.json lists no segments")?;
    if meta.segments.len() > 1 {
        return Err("multi-segment recordings are not supported yet".into());
    }
    let duration = segment
        .end_ms
        .ok_or("meta.json has no end time for the segment; the recording did not finish")?
        as f64
        / 1000.0;

    let markers = Markers::from_events(&input, duration);
    let timeline =
        Timeline::build_with(&events, duration, &cfg.pacing, &markers.keep, &markers.cut);
    if markers != Markers::default() {
        log(format!(
            "markers: {} keep, {} cut, {} frame, {} chapters",
            markers.keep.len(),
            markers.cut.len(),
            markers.frame.len(),
            markers.chapters.len()
        ));
    }
    summarize(&timeline, duration, events.len(), log);

    let geo = Geometry::new(&segment, cfg.output.size);
    let samples = read_windows(&dir.join("windows.jsonl"))?;
    let track = if cfg.camera.enabled {
        CameraTrack::build(&samples, &timeline, &geo, &cfg.camera, &markers.frame)
    } else {
        CameraTrack::fixed(&geo)
    };
    log(format!(
        "camera: {}, {} moves from {} window samples",
        if cfg.camera.enabled { "on" } else { "off" },
        track.move_count(),
        samples.len()
    ));

    let size = (cfg.output.size[0], cfg.output.size[1]);
    let fps = cfg.output.fps;
    let cropping = cfg.camera.enabled && !job.boxes;
    let decode = if cropping {
        Decode::Native
    } else {
        Decode::Fit(size.0, size.1)
    };
    let letterbox = Letterbox::new((segment.surface_px[0], segment.surface_px[1]), size);
    let mut src = FfmpegSource::open(&dir.join(&segment.file), segment, decode, fps as f64)
        .map_err(|e| e.to_string())?;
    let chapters_file = out.with_extension("chapters.txt");
    let chapters = if markers.chapters.is_empty() {
        None
    } else {
        let starts: Vec<i64> = markers
            .chapters
            .iter()
            .map(|&c| (timeline.out_time_at(c) * 1000.0) as i64)
            .collect();
        write_chapters(
            &chapters_file,
            &starts,
            (timeline.out_duration() * 1000.0) as i64,
        )?;
        Some(chapters_file.as_path())
    };
    let mut sink =
        Box::new(FfmpegSink::create(out, size, fps, chapters).map_err(|e| e.to_string())?);

    let framing = if cropping {
        Framing::Camera(&track)
    } else {
        Framing::Prefitted
    };
    let to_output = |r: &Rect, out_t: f64| -> Rect {
        if !cropping {
            return letterbox.rect(r);
        }
        let cam = track.rect_at(out_t);
        Rect {
            x: (r.x - cam.x) / cam.w * size.0 as f64,
            y: (r.y - cam.y) / cam.h * size.1 as f64,
            w: r.w / cam.w * size.0 as f64,
            h: r.h / cam.h * size.1 as f64,
        }
    };
    let focus_settings = FocusSettings {
        blur: cfg.focus.blur,
        dim: cfg.focus.dim,
        feather: cfg.focus.feather,
    };
    let corner = cfg.focus.corner_radius / geo.surface_pt.0;
    let mut focus = FocusBlur::default();
    let mut draw = |frame: &mut Frame, out_t: f64, src_t: f64| {
        if cfg.focus.enabled {
            let ms = (src_t * 1000.0) as i64;
            let masks: Vec<FocusMask> = track
                .focus_at(out_t)
                .into_iter()
                .filter_map(|(id, weight)| {
                    let r = camera::window_rect_at(&samples, id, ms)?;
                    let corner = to_output(
                        &Rect {
                            x: 0.0,
                            y: 0.0,
                            w: corner,
                            h: 0.0,
                        },
                        out_t,
                    )
                    .w;
                    Some(FocusMask {
                        rect: to_output(&r, out_t),
                        corner: corner.abs(),
                        weight,
                    })
                })
                .collect();
            focus.apply(frame, &masks, &focus_settings)?;
        }
        if job.boxes {
            draw_boxes(
                frame,
                &samples,
                src_t,
                track.rect_at(out_t),
                &geo,
                &cfg,
                &letterbox,
            );
        }
        Ok(())
    };
    let overlay: Option<compositor::Overlay> = if cfg.focus.enabled || job.boxes {
        Some(&mut draw)
    } else {
        None
    };

    let started = Instant::now();
    let frames = compositor::render(
        &mut src,
        sink.as_mut(),
        &timeline,
        framing,
        overlay,
        RenderSettings { size, fps },
        progress,
    )
    .map_err(|e| e.to_string())?;
    sink.finish().map_err(|e| e.to_string())?;
    if chapters.is_some() {
        let _ = fs::remove_file(&chapters_file);
    }
    Ok(Report {
        out: out.to_owned(),
        frames,
        seconds: started.elapsed().as_secs_f64(),
    })
}

fn summarize(timeline: &Timeline, duration: f64, events: usize, log: &mut dyn FnMut(String)) {
    let (mut human, mut idle, mut ramps, mut holds) = (0.0, 0.0, 0.0, 0);
    for s in timeline.spans() {
        match s.kind {
            SpanKind::Human => human += s.src.end - s.src.start,
            SpanKind::Idle => idle += s.src.end - s.src.start,
            SpanKind::Ramp => ramps += s.src.end - s.src.start,
            SpanKind::Hold => holds += 1,
        }
    }
    log(format!(
        "source {duration:.1}s, {events} input events: {human:.1}s human, {ramps:.1}s ramping, {idle:.1}s idle, {holds} holds"
    ));
    log(format!("output {:.1}s", timeline.out_duration()));
}
