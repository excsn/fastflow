//! Renders a recording directory end to end. Shared by the CLI and the daemon.

use std::fs;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Instant;

use fastflow_core::camera::{self, CameraTrack, Geometry};
use fastflow_core::config::Config;
use fastflow_core::geom::Rect;
use fastflow_core::markers::Markers;
use fastflow_core::recording::{InputEvent, Meta, SegmentInfo, WindowSample};
use fastflow_core::slices::{Format, Output, Projection, Slices};
use fastflow_core::timeline::{SpanKind, Timeline};

use crate::compositor::{self, Framing, RenderSettings, Segment};
use crate::export::{self, ExportSpec};
use crate::ffmpeg::{Decode, Encoding, FfmpegSink, FfmpegSource};
use crate::focus::{FocusBlur, FocusMask, FocusSettings};
use crate::overlay::{self, Letterbox};
use crate::{Frame, FrameSink};

type Draw<'a> = Box<dyn FnMut(&mut Frame, f64, f64) -> crate::Result<()> + 'a>;

pub struct RenderJob {
  pub dir: PathBuf,
  pub out: PathBuf,
  pub target: Target,
  /// Draw the camera's inputs over the whole frame instead of cropping.
  pub boxes: bool,
  /// Read `proxy.mp4` and write a half-size, 30fps preview. The tracks are planned exactly as
  /// for the final render, so the preview shows the same pacing and framing.
  pub preview: bool,
}

pub const PROXY_FILE: &str = "proxy.mp4";
pub const PROXY_WIDTH: u32 = 960;
const PREVIEW_FPS: u32 = 30;

/// Everything the tracks decided for a recording, before any pixels are touched.
pub struct Plan {
  pub cfg: Config,
  pub duration: f64,
  pub events: Vec<f64>,
  pub markers: Markers,
  pub timeline: Timeline,
  pub segments: Vec<SegmentPlan>,
}

/// One display visit. Each has its own coordinate space, so its own camera track: the camera
/// resets at a boundary instead of easing across it.
pub struct SegmentPlan {
  pub info: SegmentInfo,
  /// Source seconds at which the segment's file starts.
  pub start: f64,
  pub geo: Geometry,
  pub samples: Vec<WindowSample>,
  pub track: CameraTrack,
}

pub fn proxy_file(segment: usize) -> String {
  match segment {
    0 => PROXY_FILE.to_owned(),
    n => format!("proxy.{n}.mp4"),
  }
}

/// The proxy's pixel size for a surface: `PROXY_WIDTH` wide, height rounded to even.
pub fn proxy_size(surface_px: [u32; 2]) -> (u32, u32) {
  let h =
    (PROXY_WIDTH as f64 * surface_px[1] as f64 / surface_px[0] as f64 / 2.0).round() as u32 * 2;
  (PROXY_WIDTH, h.max(2))
}

/// Writes a proxy next to each raw segment. Returns the first.
pub fn make_proxy(dir: &Path) -> Result<PathBuf, String> {
  let meta: Meta = read_json(&dir.join("meta.json"))?;
  if meta.segments.is_empty() {
    return Err("meta.json lists no segments".into());
  }
  for (i, segment) in meta.segments.iter().enumerate() {
    crate::ffmpeg::transcode_proxy(
      &dir.join(&segment.file),
      &dir.join(proxy_file(i)),
      proxy_size(segment.surface_px),
      PREVIEW_FPS,
    )
    .map_err(|e| e.to_string())?;
  }
  Ok(dir.join(PROXY_FILE))
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
  Ok(
    text
      .lines()
      .filter_map(|l| serde_json::from_str::<InputEvent>(l).ok())
      .collect(),
  )
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

/// Reads the recording and builds its timeline and camera track.
pub fn plan(dir: &Path, log: &mut dyn FnMut(String)) -> Result<Plan, String> {
  let meta: Meta = read_json(&dir.join("meta.json"))?;
  let cfg = read_config(dir)?;
  let input = read_input(&dir.join("input.jsonl"))?;
  let events: Vec<f64> = input
    .iter()
    .filter(|e| !e.kind.is_marker())
    .map(|e| e.t as f64 / 1000.0)
    .collect();
  let last = meta.segments.last().ok_or("meta.json lists no segments")?;
  let duration = last
    .end_ms
    .ok_or("meta.json has no end time for the last segment; the recording did not finish")?
    as f64
    / 1000.0;

  let markers = Markers::from_events(&input, duration);
  let timeline = Timeline::build_with(&events, duration, &cfg.pacing, &markers.keep, &markers.cut);
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

  let all_samples = read_windows(&dir.join("windows.jsonl"))?;
  let mut segments = Vec::with_capacity(meta.segments.len());
  for (i, info) in meta.segments.into_iter().enumerate() {
    let geo = Geometry::new(&info, cfg.output.size);
    let samples: Vec<WindowSample> = all_samples
      .iter()
      .filter(|s| s.segment as usize == i)
      .cloned()
      .collect();
    let track = if cfg.camera.enabled {
      CameraTrack::build(&samples, &timeline, &geo, &cfg.camera, &markers.frame)
    } else {
      CameraTrack::fixed(&geo)
    };
    log(format!(
      "segment {i} ({}): camera {}, {} moves from {} window samples",
      info.file,
      if cfg.camera.enabled { "on" } else { "off" },
      track.move_count(),
      samples.len()
    ));
    segments.push(SegmentPlan {
      start: info.start_ms as f64 / 1000.0,
      info,
      geo,
      samples,
      track,
    });
  }
  Ok(Plan {
    cfg,
    duration,
    events,
    markers,
    timeline,
    segments,
  })
}

/// What a render produces: slices of the recording in one format.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
  pub format: Format,
  /// Source ranges in playing order. `None` is the whole recording.
  pub ranges: Option<Vec<Range<f64>>>,
  /// Pixels. Height follows the aspect of `output.size`.
  pub width: Option<u32>,
  pub fps: Option<u32>,
  pub colors: Option<u32>,
  pub quality: Option<u32>,
  pub dissolve_ms: u32,
}

impl Target {
  pub fn whole() -> Target {
    Target {
      format: Format::Mp4,
      ranges: None,
      width: None,
      fps: None,
      colors: None,
      quality: None,
      dissolve_ms: 0,
    }
  }

  pub fn of(slices: &Slices, output: &Output) -> Target {
    Target {
      format: output.format,
      ranges: slices.ranges(output),
      width: output.width,
      fps: output.fps,
      colors: output.colors,
      quality: output.quality,
      dissolve_ms: output.dissolve_ms,
    }
  }
}

pub const SLICES_FILE: &str = "slices.toml";
pub const SCRUB_FILE: &str = "scrub.mp4";
const SCRUB_STAMP: &str = "scrub.stamp";
const SCRUB_WIDTH: u32 = 640;
const SCRUB_FPS: u32 = 10;
const ANIMATION_WIDTH: u32 = 800;
const ANIMATION_FPS: u32 = 15;

/// Source seconds the recording runs for, from `meta.json`.
pub fn duration(dir: &Path) -> Result<f64, String> {
  let meta: Meta = read_json(&dir.join("meta.json"))?;
  let last = meta.segments.last().ok_or("meta.json lists no segments")?;
  last.end_ms.map(|ms| ms as f64 / 1000.0).ok_or_else(|| {
    "meta.json has no end time for the last segment; the recording did not finish".into()
  })
}

/// `slices.toml` or the implicit single output when there is none. Files from before outputs
/// could play the whole recording are migrated in memory. Always validated.
pub fn load_slices(dir: &Path) -> Result<Slices, String> {
  let duration = duration(dir)?;
  let path = dir.join(SLICES_FILE);
  let mut slices: Slices = match fs::read_to_string(&path) {
    Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?,
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Slices::implicit(),
    Err(e) => return Err(format!("{}: {e}", path.display())),
  };
  slices.migrate(duration);
  slices
    .validate(duration)
    .map_err(|e| format!("{}: {e}", path.display()))?;
  Ok(slices)
}

pub fn save_slices(dir: &Path, slices: &Slices) -> Result<(), String> {
  slices.validate(duration(dir)?)?;
  let text = toml::to_string(slices).map_err(|e| e.to_string())?;
  let path = dir.join(SLICES_FILE);
  fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn output_path(dir: &Path, output: &Output) -> PathBuf {
  dir.join(output.file_name())
}

/// The job for one named output of `slices.toml`.
pub fn output_job(dir: &Path, name: &str) -> Result<RenderJob, String> {
  let slices = load_slices(dir)?;
  let output = slices.output(name).ok_or_else(|| {
    format!(
      "no output named {name:?} in {}",
      dir.join(SLICES_FILE).display()
    )
  })?;
  Ok(RenderJob {
    dir: dir.to_owned(),
    out: output_path(dir, output),
    boxes: false,
    preview: false,
    target: Target::of(&slices, output),
  })
}

fn even(v: f64) -> u32 {
  ((v / 2.0).round() as u32 * 2).max(2)
}

/// Pixel size with `width` and the aspect of `full`.
fn sized(full: [u32; 2], width: u32) -> (u32, u32) {
  (
    even(width as f64),
    even(width as f64 * full[1] as f64 / full[0] as f64),
  )
}

/// How one render reads and writes, after the job's defaults are resolved.
struct Pass<'a> {
  out: &'a Path,
  size: (u32, u32),
  fps: u32,
  proxy: bool,
  boxes: bool,
  ranges: Option<&'a [Range<f64>]>,
  dissolve: f64,
  encoding: Encoding,
  chapters: bool,
}

/// Renders `job.dir` to `job.out`. `log` receives the summary lines, `progress` frames done and
/// the total.
pub fn run(
  job: &RenderJob,
  log: &mut dyn FnMut(String),
  progress: &mut dyn FnMut(u64, u64),
) -> Result<Report, String> {
  let started = Instant::now();
  let plan = plan(&job.dir, log)?;
  let t = &job.target;
  let animation = t.format.is_animation() && !job.preview;
  let cfg_size = plan.cfg.output.size;
  let (mut size, mut fps) = if animation {
    (
      sized(cfg_size, t.width.unwrap_or(ANIMATION_WIDTH)),
      t.fps.unwrap_or(ANIMATION_FPS),
    )
  } else {
    (
      t.width
        .map_or((cfg_size[0], cfg_size[1]), |w| sized(cfg_size, w)),
      t.fps.unwrap_or(plan.cfg.output.fps),
    )
  };
  if job.preview {
    size = (even(size.0 as f64 / 2.0), even(size.1 as f64 / 2.0));
    fps = PREVIEW_FPS;
  }
  let intermediate = job.out.with_extension("tmp.mp4");
  let pass = Pass {
    out: if animation { &intermediate } else { &job.out },
    size,
    fps,
    proxy: job.preview,
    boxes: job.boxes,
    ranges: t.ranges.as_deref(),
    dissolve: t.dissolve_ms as f64 / 1000.0,
    encoding: if animation {
      Encoding::Intermediate
    } else {
      Encoding::Final
    },
    chapters: !animation,
  };
  if !animation {
    let frames = render_plan(&job.dir, &plan, &pass, log, progress)?;
    return Ok(Report {
      out: job.out.clone(),
      frames,
      seconds: started.elapsed().as_secs_f64(),
    });
  }

  let mut total = 0;
  let rendered = render_plan(&job.dir, &plan, &pass, log, &mut |done, all| {
    total = all + all / 9;
    progress(done, total);
  });
  let frames = match rendered {
    Ok(f) => f,
    Err(e) => {
      let _ = fs::remove_file(&intermediate);
      return Err(e);
    }
  };
  let spec = ExportSpec {
    src: intermediate.clone(),
    from: 0.0,
    to: frames as f64 / fps as f64,
    speed: 1.0,
    fps,
    width: size.0,
    colors: t.colors.unwrap_or(128),
    quality: t.quality.unwrap_or(70),
    format: match t.format {
      Format::Webp => export::Format::Webp,
      _ => export::Format::Gif,
    },
    out: job.out.clone(),
  };
  let exported = export::export(&spec, &mut |p| {
    progress(frames + ((total - frames) as f64 * p) as u64, total)
  });
  let _ = fs::remove_file(&intermediate);
  exported.map_err(|e| e.to_string())?;
  Ok(Report {
    out: job.out.clone(),
    frames,
    seconds: started.elapsed().as_secs_f64(),
  })
}

fn render_plan(
  dir: &Path,
  plan: &Plan,
  pass: &Pass,
  log: &mut dyn FnMut(String),
  progress: &mut dyn FnMut(u64, u64),
) -> Result<u64, String> {
  let Plan {
    cfg,
    markers,
    timeline,
    segments: plans,
    ..
  } = plan;
  let size = pass.size;
  let px_scale = size.0 as f64 / cfg.output.size[0] as f64;
  let cropping = cfg.camera.enabled && !pass.boxes;
  let focus_settings = FocusSettings {
    blur: cfg.focus.blur * px_scale,
    dim: cfg.focus.dim,
    feather: cfg.focus.feather * px_scale,
  };
  let projection = match pass.ranges {
    Some(ranges) => Projection::new(timeline, ranges, pass.dissolve),
    None => Projection::whole(timeline),
  };
  if projection.windows().is_empty() {
    return Err("the slices cover no output time".into());
  }
  if pass.ranges.is_some() {
    for (i, w) in projection.windows().iter().enumerate() {
      log(format!(
        "slice {i}: output {:.2}s to {:.2}s, full timeline {:.2}s to {:.2}s",
        w.start,
        w.end(),
        w.full.start,
        w.full.end
      ));
    }
  }
  let spares_needed = projection.windows().iter().any(|w| w.dissolve_in > 0.0);

  let decode = if cropping {
    Decode::Native
  } else {
    Decode::Fit(size.0, size.1)
  };
  let mut sources = Vec::with_capacity(plans.len());
  let mut spares = Vec::with_capacity(plans.len());
  for (i, p) in plans.iter().enumerate() {
    let (file, decoded) = if pass.proxy {
      let proxy = dir.join(proxy_file(i));
      if !proxy.is_file() {
        return Err(format!("{} has no {} yet", dir.display(), proxy_file(i)));
      }
      let (w, h) = proxy_size(p.info.surface_px);
      let decoded = SegmentInfo {
        surface_px: [w, h],
        ..p.info.clone()
      };
      (proxy, decoded)
    } else {
      (dir.join(&p.info.file), p.info.clone())
    };
    let open = || {
      FfmpegSource::open(&file, decoded.clone(), decode, pass.fps as f64).map_err(|e| e.to_string())
    };
    sources.push(open()?);
    spares.push(if spares_needed { Some(open()?) } else { None });
  }

  let mut draws: Vec<Draw<'_>> = plans
    .iter()
    .map(|p| {
      let letterbox = Letterbox::new((p.info.surface_px[0], p.info.surface_px[1]), size);
      let to_output = move |r: &Rect, out_t: f64| -> Rect {
        if !cropping {
          return letterbox.rect(r);
        }
        let cam = p.track.rect_at(out_t);
        Rect {
          x: (r.x - cam.x) / cam.w * size.0 as f64,
          y: (r.y - cam.y) / cam.h * size.1 as f64,
          w: r.w / cam.w * size.0 as f64,
          h: r.h / cam.h * size.1 as f64,
        }
      };
      let corner = cfg.focus.corner_radius / p.geo.surface_pt.0;
      let mut focus = FocusBlur::default();
      let boxes = pass.boxes;
      Box::new(move |frame: &mut Frame, out_t: f64, src_t: f64| {
        if cfg.focus.enabled && p.track.focused_at(out_t) {
          let ms = (src_t * 1000.0) as i64;
          let corner_px = to_output(
            &Rect {
              x: 0.0,
              y: 0.0,
              w: corner,
              h: 0.0,
            },
            out_t,
          )
          .w
          .abs();
          let masks: Vec<FocusMask> = p
            .track
            .focus_at(out_t)
            .into_iter()
            .filter_map(|(id, weight)| {
              let r = camera::window_rect_at(&p.samples, id, ms)?;
              Some(FocusMask {
                rect: to_output(&r, out_t),
                corner: corner_px,
                weight,
              })
            })
            .collect();
          focus.apply(frame, &masks, &focus_settings)?;
        }
        if boxes {
          draw_boxes(
            frame,
            &p.samples,
            src_t,
            p.track.rect_at(out_t),
            &p.geo,
            cfg,
            &letterbox,
          );
        }
        Ok(())
      }) as Draw<'_>
    })
    .collect();
  let drawing = cfg.focus.enabled || pass.boxes;

  let mut segments: Vec<Segment> = sources
    .iter_mut()
    .zip(spares.iter_mut())
    .zip(draws.iter_mut())
    .zip(plans)
    .map(|(((source, spare), draw), p)| Segment {
      source,
      spare: spare.as_mut().map(|s| s as &mut dyn crate::FrameSource),
      framing: if cropping {
        Framing::Camera(&p.track)
      } else {
        Framing::Prefitted
      },
      overlay: drawing.then_some(draw.as_mut() as compositor::Overlay),
      start: p.start,
    })
    .collect();

  let out = pass.out;
  let chapters_file = out.with_extension("chapters.txt");
  let starts: Vec<i64> = markers
    .chapters
    .iter()
    .filter_map(|&c| projection.project(timeline.out_time_at(c)))
    .map(|t| (t * 1000.0) as i64)
    .collect();
  let chapters = if !pass.chapters || starts.is_empty() {
    None
  } else {
    write_chapters(
      &chapters_file,
      &starts,
      (projection.duration() * 1000.0) as i64,
    )?;
    Some(chapters_file.as_path())
  };
  let mut sink = Box::new(
    FfmpegSink::create(out, size, pass.fps, chapters, pass.encoding).map_err(|e| e.to_string())?,
  );

  let rendered = compositor::render_projection(
    &mut segments,
    sink.as_mut(),
    timeline,
    &projection,
    cfg.render.switch_ms as f64 / 1000.0,
    RenderSettings {
      size,
      fps: pass.fps,
    },
    progress,
  )
  .map_err(|e| e.to_string());
  let finished = sink.finish().map_err(|e| e.to_string());
  if chapters.is_some() {
    let _ = fs::remove_file(&chapters_file);
  }
  let frames = rendered?;
  finished?;
  Ok(frames)
}

/// What the scrub file was built from. A change to any of these makes it stale.
fn scrub_stamp(dir: &Path) -> String {
  let mut h = std::hash::DefaultHasher::new();
  for name in ["meta.json", "config.toml", "input.jsonl", "windows.jsonl"] {
    name.hash(&mut h);
    fs::read(dir.join(name)).unwrap_or_default().hash(&mut h);
  }
  (SCRUB_WIDTH, SCRUB_FPS).hash(&mut h);
  format!("{:016x}", h.finish())
}

pub fn scrub_is_current(dir: &Path) -> bool {
  dir.join(SCRUB_FILE).is_file()
    && fs::read_to_string(dir.join(SCRUB_STAMP)).is_ok_and(|s| s.trim() == scrub_stamp(dir))
}

/// Renders `scrub.mp4` from the proxy: the whole recording, paced, with the camera and focus,
/// small and with every frame a keyframe. Its time is the full timeline's output time.
pub fn make_scrub(
  dir: &Path,
  log: &mut dyn FnMut(String),
  progress: &mut dyn FnMut(u64, u64),
) -> Result<PathBuf, String> {
  let plan = plan(dir, log)?;
  let out = dir.join(SCRUB_FILE);
  let partial = dir.join("scrub.tmp.mp4");
  let pass = Pass {
    out: &partial,
    size: sized(plan.cfg.output.size, SCRUB_WIDTH),
    fps: SCRUB_FPS,
    proxy: true,
    boxes: false,
    ranges: None,
    dissolve: 0.0,
    encoding: Encoding::Scrub,
    chapters: false,
  };
  if let Err(e) = render_plan(dir, &plan, &pass, log, progress) {
    let _ = fs::remove_file(&partial);
    return Err(e);
  }
  fs::rename(&partial, &out).map_err(|e| format!("{}: {e}", out.display()))?;
  fs::write(dir.join(SCRUB_STAMP), scrub_stamp(dir)).map_err(|e| e.to_string())?;
  Ok(out)
}

/// The proxy for every segment, then the scrub file, each only when missing or stale.
pub fn prepare(
  dir: &Path,
  log: &mut dyn FnMut(String),
  progress: &mut dyn FnMut(u64, u64),
) -> Result<PathBuf, String> {
  let meta: Meta = read_json(&dir.join("meta.json"))?;
  if !(0..meta.segments.len()).all(|i| dir.join(proxy_file(i)).is_file()) {
    let p = make_proxy(dir)?;
    log(format!("proxy: {}", p.display()));
  }
  if scrub_is_current(dir) {
    return Ok(dir.join(SCRUB_FILE));
  }
  make_scrub(dir, log, progress)
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
