use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use fastflow_core::camera::{self, CameraTrack, Geometry};
use fastflow_core::config::Config;
use fastflow_core::geom::Rect;
use fastflow_core::recording::{InputEvent, Meta, WindowSample};
use fastflow_core::timeline::{SpanKind, Timeline};
use fastflow_render::compositor::{self, Framing, RenderSettings};
use fastflow_render::ffmpeg::{Decode, FfmpegSink, FfmpegSource};
use fastflow_render::focus::{FocusBlur, FocusMask, FocusSettings};
use fastflow_render::overlay::{self, Letterbox};
use fastflow_render::{Frame, FrameSink};

const USAGE: &str = "usage: fastflow render <recording-dir> [-o <out.mp4>] [--boxes]

  --boxes   render the whole frame with the sampled windows, cursor, chosen subject
            and camera rect drawn on it, to check the camera's inputs";

struct RenderArgs {
    dir: PathBuf,
    out: PathBuf,
    boxes: bool,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("render") => parse_render(&args[1..]).and_then(|a| render(&a)),
        _ => Err(USAGE.into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fastflow: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse_render(args: &[String]) -> Result<RenderArgs, String> {
    let (mut dir, mut out, mut boxes) = (None, None, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => out = Some(PathBuf::from(it.next().ok_or(USAGE)?)),
            "--boxes" => boxes = true,
            _ if dir.is_none() && !a.starts_with('-') => dir = Some(PathBuf::from(a)),
            _ => return Err(USAGE.into()),
        }
    }
    let dir = dir.ok_or(USAGE)?;
    let out = out.unwrap_or_else(|| dir.join(if boxes { "boxes.mp4" } else { "render.mp4" }));
    Ok(RenderArgs { dir, out, boxes })
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
fn read_input(path: &Path) -> Result<Vec<f64>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(text
        .lines()
        .filter_map(|l| serde_json::from_str::<InputEvent>(l).ok())
        .map(|e| e.t as f64 / 1000.0)
        .collect())
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

fn render(args: &RenderArgs) -> Result<(), String> {
    let (dir, out) = (args.dir.as_path(), args.out.as_path());
    let meta: Meta = read_json(&dir.join("meta.json"))?;
    let cfg = read_config(dir)?;
    let events = read_input(&dir.join("input.jsonl"))?;
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

    let timeline = Timeline::build(&events, duration, &cfg.pacing);
    print_summary(&timeline, duration, events.len());

    let geo = Geometry::new(&segment, cfg.output.size);
    let samples = read_windows(&dir.join("windows.jsonl"))?;
    let track = if cfg.camera.enabled {
        CameraTrack::build(&samples, &timeline, &geo, &cfg.camera)
    } else {
        CameraTrack::fixed(&geo)
    };
    eprintln!(
        "camera: {}, {} moves from {} window samples",
        if cfg.camera.enabled { "on" } else { "off" },
        track.move_count(),
        samples.len()
    );

    let size = (cfg.output.size[0], cfg.output.size[1]);
    let fps = cfg.output.fps;
    let cropping = cfg.camera.enabled && !args.boxes;
    let decode = if cropping {
        Decode::Native
    } else {
        Decode::Fit(size.0, size.1)
    };
    let letterbox = Letterbox::new((segment.surface_px[0], segment.surface_px[1]), size);
    let mut src = FfmpegSource::open(&dir.join(&segment.file), segment, decode, fps as f64)
        .map_err(|e| e.to_string())?;
    let mut sink = Box::new(FfmpegSink::create(out, size, fps).map_err(|e| e.to_string())?);

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
        if args.boxes {
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
    let overlay: Option<compositor::Overlay> = if cfg.focus.enabled || args.boxes {
        Some(&mut draw)
    } else {
        None
    };

    let started = Instant::now();
    let mut last_shown = 0;
    let frames = compositor::render(
        &mut src,
        sink.as_mut(),
        &timeline,
        framing,
        overlay,
        RenderSettings { size, fps },
        &mut |done, total| {
            let pct = done * 100 / total.max(1);
            if pct != last_shown || done == total {
                last_shown = pct;
                eprint!("\rrendering {pct:3}% ({done}/{total} frames)");
                let _ = std::io::stderr().flush();
            }
        },
    )
    .map_err(|e| e.to_string())?;
    sink.finish().map_err(|e| e.to_string())?;
    eprintln!(
        "\nwrote {} ({frames} frames) in {:.1}s",
        out.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn print_summary(timeline: &Timeline, duration: f64, events: usize) {
    let (mut human, mut idle, mut ramps, mut holds) = (0.0, 0.0, 0.0, 0);
    for s in timeline.spans() {
        match s.kind {
            SpanKind::Human => human += s.src.end - s.src.start,
            SpanKind::Idle => idle += s.src.end - s.src.start,
            SpanKind::Ramp => ramps += s.src.end - s.src.start,
            SpanKind::Hold => holds += 1,
        }
    }
    eprintln!(
        "source {duration:.1}s, {events} input events: {human:.1}s human, {ramps:.1}s ramping, {idle:.1}s idle, {holds} holds"
    );
    eprintln!("output {:.1}s", timeline.out_duration());
}
