use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use fastflow_daemon::paths;
use fastflow_daemon::protocol::{Finished, Request, Response};
use fastflow_daemon::server;
use fastflow_render::job::{self, RenderJob};

const USAGE: &str = "usage:
  fastflow start                   start recording
  fastflow stop                    stop recording and queue its render
  fastflow status                  what is recording and rendering
  fastflow list [n]                the newest n recordings, default 5
  fastflow render <id>             queue a re-render in the app
  fastflow pin <id>                keep its raw footage past the 7-day sweep
  fastflow unpin <id>
  fastflow render <dir> [-o <out.mp4>] [--boxes]
                                   render a recording directory here, without the app
  fastflow diagram <id|dir> [cols] print what pacing and the camera decided
  fastflow proxy <id|dir>          make the small proxy.mp4 that previews read
  fastflow preview <id|dir>        render preview.mp4 from the proxy, for tuning config.toml
  fastflow gif <id|dir> [options]  make an animated GIF or WebP from a recording
      --source render|raw|preview  which video to cut from, default render
      --from <s> --to <s>          the range in seconds, default the whole video
      --speed <x>                  extra playback speed, default 1
      --fps <n> --width <px>       default 15 and 800
      --colors <n>                 GIF palette size, default 128
      --webp [--quality <n>]       WebP instead of GIF, lossy quality default 70
      -o <file>                    default clip.gif or clip.webp in the recording

  --boxes   render the whole frame with the sampled windows, cursor, chosen subject
            and camera rect drawn on it, to check the camera's inputs";

fn main() -> ExitCode {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let result = match (args.first().map(String::as_str), &args[1.min(args.len())..]) {
    (Some("start"), []) => ask(Request::Start).map(|r| println!("recording {}", id(&r))),
    (Some("stop"), []) => ask(Request::Stop).map(|r| println!("stopped {}, rendering", id(&r))),
    (Some("status"), []) => ask(Request::Status).map(print_status),
    (Some("list"), rest) => parse_limit(rest)
      .and_then(|limit| ask(Request::List { limit }))
      .map(print_list),
    (Some("resegment"), []) => {
      ask(Request::Resegment).map(|r| println!("new segment in {}", id(&r)))
    }
    (Some("gif"), [target, rest @ ..]) => gif(target, rest),
    (Some("diagram"), [target]) => diagram(target, 100),
    (Some("diagram"), [target, cols]) => cols
      .parse()
      .map_err(|_| USAGE.to_string())
      .and_then(|c| diagram(target, c)),
    (Some("proxy"), [target]) => resolve(target)
      .and_then(|dir| job::make_proxy(&dir))
      .map(|p| println!("wrote {}", p.display())),
    (Some("preview"), [target]) => resolve(target).and_then(|dir| {
      render(&RenderJob {
        out: dir.join("preview.mp4"),
        dir,
        boxes: false,
        preview: true,
      })
    }),
    (Some("pin"), [id]) => set_pinned(id, true),
    (Some("unpin"), [id]) => set_pinned(id, false),
    (Some("render"), [target]) if !std::path::Path::new(target).is_dir() => {
      ask(Request::Render { id: target.clone() }).map(|r| println!("queued {}", id(&r)))
    }
    (Some("render"), rest) => parse_render(rest).and_then(|j| render(&j)),
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

/// A directory path as given, otherwise a recording id under the recordings folder.
fn resolve(target: &str) -> Result<PathBuf, String> {
  let as_path = PathBuf::from(target);
  let dir = if as_path.is_dir() {
    as_path
  } else {
    paths::recordings().join(target)
  };
  if dir.join("meta.json").is_file() {
    Ok(dir)
  } else {
    Err(format!("no recording at {target}"))
  }
}

fn gif(target: &str, args: &[String]) -> Result<(), String> {
  use fastflow_render::export::{self, ExportSpec, Format};

  let dir = resolve(target)?;
  let mut source = "render".to_owned();
  let (mut from, mut to, mut out) = (None, None, None);
  let mut spec = ExportSpec {
    src: PathBuf::new(),
    from: 0.0,
    to: 0.0,
    speed: 1.0,
    fps: 15,
    width: 800,
    colors: 128,
    quality: 70,
    format: Format::Gif,
    out: PathBuf::new(),
  };
  let mut it = args.iter();
  while let Some(a) = it.next() {
    let mut value = || it.next().ok_or_else(|| USAGE.to_string());
    let num = |v: &String| v.parse::<f64>().map_err(|_| format!("not a number: {v}"));
    match a.as_str() {
      "--source" => source = value()?.clone(),
      "--from" => from = Some(num(value()?)?),
      "--to" => to = Some(num(value()?)?),
      "--speed" => spec.speed = num(value()?)?,
      "--fps" => spec.fps = num(value()?)? as u32,
      "--width" => spec.width = num(value()?)? as u32,
      "--colors" => spec.colors = num(value()?)? as u32,
      "--quality" => spec.quality = num(value()?)? as u32,
      "--webp" => spec.format = Format::Webp,
      "-o" => out = Some(PathBuf::from(value()?)),
      _ => return Err(USAGE.into()),
    }
  }
  spec.src = match source.as_str() {
    "render" => dir.join("render.mp4"),
    "preview" => dir.join("preview.mp4"),
    "raw" => dir.join(raw_file(&dir)?),
    other => return Err(format!("unknown source {other}")),
  };
  if !spec.src.is_file() {
    return Err(format!("{} does not exist yet", spec.src.display()));
  }
  let length = export::duration(&spec.src).map_err(|e| e.to_string())?;
  spec.from = from.unwrap_or(0.0).clamp(0.0, length);
  spec.to = to.unwrap_or(length).clamp(spec.from, length);
  spec.out = out.unwrap_or_else(|| dir.join(format!("clip.{}", spec.format.extension())));

  let mut last = u64::MAX;
  let bytes = export::export(&spec, &mut |p| {
    let pct = (p * 100.0) as u64;
    if pct != last {
      last = pct;
      eprint!("\rexporting {pct:3}%");
      let _ = std::io::stderr().flush();
    }
  })
  .map_err(|e| e.to_string())?;
  eprintln!(
    "\nwrote {} ({:.1} MB, {:.1}s)",
    spec.out.display(),
    bytes as f64 / 1e6,
    export::output_duration(&spec)
  );
  Ok(())
}

/// The first raw segment's file name, from `meta.json`.
fn raw_file(dir: &std::path::Path) -> Result<String, String> {
  let path = dir.join("meta.json");
  let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
  let meta: fastflow_core::recording::Meta =
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
  meta
    .segments
    .first()
    .map(|s| s.file.clone())
    .ok_or_else(|| "meta.json lists no segments".into())
}

fn diagram(target: &str, cols: usize) -> Result<(), String> {
  let dir = resolve(target)?;
  let plan = job::plan(&dir, &mut |_| {})?;
  let cameras: Vec<(f64, &fastflow_core::camera::CameraTrack)> =
    plan.segments.iter().map(|s| (s.start, &s.track)).collect();
  let text = fastflow_core::diagram::render(
    &fastflow_core::diagram::Inputs {
      timeline: &plan.timeline,
      cameras: &cameras,
      events: &plan.events,
      markers: &plan.markers,
      duration: plan.duration,
    },
    cols,
  );
  println!("{text}");
  Ok(())
}

fn set_pinned(id: &str, pinned: bool) -> Result<(), String> {
  let dir = paths::recordings().join(id);
  if !dir.join("meta.json").is_file() {
    return Err(format!("no recording {id}"));
  }
  let pin = dir.join(fastflow_daemon::retention::PIN_FILE);
  let result = if pinned {
    std::fs::write(&pin, b"")
  } else {
    std::fs::remove_file(&pin).or_else(|e| match e.kind() {
      std::io::ErrorKind::NotFound => Ok(()),
      _ => Err(e),
    })
  };
  result.map_err(|e| format!("{}: {e}", pin.display()))
}

fn ask(request: Request) -> Result<Response, String> {
  let response = server::request(&paths::socket(), &request)
    .map_err(|e| format!("cannot reach the fastflow app ({e}); is it running?"))?;
  if response.ok {
    Ok(response)
  } else {
    Err(response.error.unwrap_or_else(|| "request failed".into()))
  }
}

fn id(r: &Response) -> &str {
  r.id.as_deref().unwrap_or("?")
}

fn parse_limit(rest: &[String]) -> Result<usize, String> {
  match rest {
    [] => Ok(5),
    [n] => n.parse().map_err(|_| USAGE.to_string()),
    _ => Err(USAGE.into()),
  }
}

fn print_status(r: Response) {
  let s = r.status.unwrap_or_default();
  match &s.recording {
    Some(id) => println!("recording  {id}"),
    None => println!("recording  -"),
  }
  match &s.rendering {
    Some(r) => println!("rendering  {} {:.0}%", r.id, r.progress * 100.0),
    None => println!("rendering  -"),
  }
  if !s.queued.is_empty() {
    println!("queued     {}", s.queued.join(" "));
  }
  match &s.last {
    Some(Finished {
      id,
      result: Ok(path),
    }) => println!("last       {id} -> {path}"),
    Some(Finished { id, result: Err(e) }) => println!("last       {id} failed: {e}"),
    None => {}
  }
}

fn print_list(r: Response) {
  for e in r.recordings.unwrap_or_default() {
    let state = if e.recording {
      "recording"
    } else if e.rendered {
      "rendered"
    } else {
      "not rendered"
    };
    println!("{}  {state}", e.id);
  }
}

fn parse_render(args: &[String]) -> Result<RenderJob, String> {
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
  let dir: PathBuf = dir.ok_or(USAGE)?;
  let out = out.unwrap_or_else(|| dir.join(if boxes { "boxes.mp4" } else { "render.mp4" }));
  Ok(RenderJob {
    dir,
    out,
    boxes,
    preview: false,
  })
}

fn render(job: &RenderJob) -> Result<(), String> {
  let mut last_shown = u64::MAX;
  let report = job::run(job, &mut |line| eprintln!("{line}"), &mut |done, total| {
    let pct = done * 100 / total.max(1);
    if pct != last_shown {
      last_shown = pct;
      eprint!("\rrendering {pct:3}% ({done}/{total} frames)");
      let _ = std::io::stderr().flush();
    }
  })?;
  eprintln!(
    "\nwrote {} ({} frames) in {:.1}s",
    report.out.display(),
    report.frames,
    report.seconds
  );
  Ok(())
}
