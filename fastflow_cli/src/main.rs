use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use fastflow_core::slices::Format;
use fastflow_daemon::paths;
use fastflow_daemon::protocol::{Finished, Request, Response};
use fastflow_daemon::server;
use fastflow_render::job::{self, RenderJob, Target};

const USAGE: &str = "usage:
  fastflow start                   start recording
  fastflow stop                    stop recording and prepare its scrub preview
  fastflow status                  what is recording and rendering
  fastflow list [n]                the newest n recordings, default 5
  fastflow render <id> [--output <name>]
                                   queue a render in the app, every output or one
  fastflow pin <id>                keep its raw footage past the 7-day sweep
  fastflow unpin <id>
  fastflow render <dir> [options]  render here, without the app. Every output of
                                   slices.toml. Without one it writes render.mp4
      --output <name>              only this output
      --slice <from>-<to>          a one-off output of these source seconds, repeatable,
                                   played in the order given
      --format mp4|gif|webp        for --slice, default mp4
      --width <px> --fps <n>       for --slice, default the config's or 800 and 15
      --dissolve <ms>              for --slice, cross-dissolve at each join, default 0
      -o <file>                    where to write a single output
      --boxes                      draw the camera's inputs over the whole frame
  fastflow diagram <id|dir> [cols] print what pacing and the camera decided
  fastflow proxy <id|dir>          make the small proxy.mp4 that previews read
  fastflow scrub <id|dir>          make scrub.mp4, the paced and framed preview the
                                   composition window plays
  fastflow preview <id|dir> [--output <name>]
                                   render preview.mp4 from the proxy, for tuning config.toml
  fastflow gif <id|dir> [options]  make an animated GIF or WebP from a rendered video
      --source render|raw|preview  which video to cut from, default render
      --from <s> --to <s>          the range in seconds, default the whole video
      --speed <x>                  extra playback speed, default 1
      --fps <n> --width <px>       default 15 and 800
      --colors <n>                 GIF palette size, default 128
      --webp [--quality <n>]       WebP instead of GIF, lossy quality default 70
      -o <file>                    default clip.gif or clip.webp in the recording

  Slice times are source seconds, the axis fastflow diagram prints.";

fn main() -> ExitCode {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let result = match (args.first().map(String::as_str), &args[1.min(args.len())..]) {
    (Some("start"), []) => ask(Request::Start).map(|r| println!("recording {}", id(&r))),
    (Some("stop"), []) => ask(Request::Stop).map(|r| println!("stopped {}", id(&r))),
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
    (Some("scrub"), [target]) => resolve(target).and_then(|dir| scrub(&dir)),
    (Some("preview"), [target, rest @ ..]) => resolve(target).and_then(|dir| preview(dir, rest)),
    (Some("pin"), [id]) => set_pinned(id, true),
    (Some("unpin"), [id]) => set_pinned(id, false),
    (Some("render"), rest) => parse_render(rest).and_then(|cmd| match cmd {
      RenderCmd::Queue { id, output } => {
        ask(Request::Render { id, output }).map(|r| println!("queued {}", self::id(&r)))
      }
      RenderCmd::Local(jobs) => jobs.iter().try_for_each(render),
    }),
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
    Some(r) => println!("rendering  {} {:.0}%", r.job.label(), r.progress * 100.0),
    None => println!("rendering  -"),
  }
  if !s.queued.is_empty() {
    println!("queued     {}", s.queued.join(" "));
  }
  match &s.last {
    Some(Finished {
      job,
      result: Ok(path),
    }) => println!("last       {} -> {path}", job.label()),
    Some(Finished {
      job,
      result: Err(e),
    }) => println!("last       {} failed: {e}", job.label()),
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

enum RenderCmd {
  Queue { id: String, output: Option<String> },
  Local(Vec<RenderJob>),
}

fn parse_render(args: &[String]) -> Result<RenderCmd, String> {
  let (mut target, mut out, mut boxes, mut output) = (None::<String>, None, false, None);
  let mut ranges = Vec::new();
  let mut one_off = Target::whole();
  let mut one_off_flag = false;
  let mut it = args.iter();
  while let Some(a) = it.next() {
    let mut value = || it.next().ok_or_else(|| USAGE.to_string());
    let num = |v: &String| {
      v.parse::<u32>()
        .map_err(|_| format!("not a whole number: {v}"))
    };
    match a.as_str() {
      "-o" => out = Some(PathBuf::from(value()?)),
      "--boxes" => boxes = true,
      "--output" => output = Some(value()?.clone()),
      "--slice" => ranges.push(parse_range(value()?)?),
      "--format" => {
        one_off_flag = true;
        one_off.format = match value()?.as_str() {
          "mp4" => Format::Mp4,
          "gif" => Format::Gif,
          "webp" => Format::Webp,
          other => return Err(format!("unknown format {other}")),
        }
      }
      "--width" => {
        one_off_flag = true;
        one_off.width = Some(num(value()?)?);
      }
      "--fps" => {
        one_off_flag = true;
        one_off.fps = Some(num(value()?)?);
      }
      "--dissolve" => {
        one_off_flag = true;
        one_off.dissolve_ms = num(value()?)?;
      }
      _ if target.is_none() && !a.starts_with('-') => target = Some(a.clone()),
      _ => return Err(USAGE.into()),
    }
  }
  let target = target.ok_or(USAGE)?;
  if one_off_flag && ranges.is_empty() {
    return Err("--format, --width, --fps and --dissolve go with --slice".into());
  }
  if output.is_some() && !ranges.is_empty() {
    return Err("--output and --slice cannot be combined".into());
  }
  let local =
    std::path::Path::new(&target).is_dir() || out.is_some() || boxes || !ranges.is_empty();
  if !local {
    return Ok(RenderCmd::Queue { id: target, output });
  }
  let dir = resolve(&target)?;
  let jobs = if boxes {
    vec![RenderJob {
      out: out.unwrap_or_else(|| dir.join("boxes.mp4")),
      dir,
      boxes: true,
      preview: false,
      target: Target {
        ranges: (!ranges.is_empty()).then_some(ranges),
        ..one_off
      },
    }]
  } else if !ranges.is_empty() {
    one_off.ranges = Some(ranges);
    vec![RenderJob {
      out: out.unwrap_or_else(|| dir.join(format!("clip.{}", one_off.format.extension()))),
      dir,
      boxes: false,
      preview: false,
      target: one_off,
    }]
  } else if let Some(name) = output {
    let mut job = job::output_job(&dir, &name)?;
    if let Some(o) = out {
      job.out = o;
    }
    vec![job]
  } else {
    let slices = job::load_slices(&dir)?;
    if out.is_some() && slices.outputs.len() != 1 {
      return Err("-o needs a single output; pick one with --output".into());
    }
    slices
      .outputs
      .iter()
      .map(|o| RenderJob {
        out: out.clone().unwrap_or_else(|| job::output_path(&dir, o)),
        dir: dir.clone(),
        boxes: false,
        preview: false,
        target: Target::of(&slices, o),
      })
      .collect()
  };
  Ok(RenderCmd::Local(jobs))
}

fn parse_range(v: &str) -> Result<std::ops::Range<f64>, String> {
  let bad = || format!("a slice is <from>-<to> in seconds, not {v}");
  let (a, b) = v.split_once('-').ok_or_else(bad)?;
  let (a, b): (f64, f64) = (a.parse().map_err(|_| bad())?, b.parse().map_err(|_| bad())?);
  if a < b { Ok(a..b) } else { Err(bad()) }
}

fn preview(dir: PathBuf, args: &[String]) -> Result<(), String> {
  let target = match args {
    [] => Target::whole(),
    [flag, name] if flag == "--output" => job::output_job(&dir, name)?.target,
    _ => return Err(USAGE.into()),
  };
  render(&RenderJob {
    out: dir.join("preview.mp4"),
    dir,
    boxes: false,
    preview: true,
    target: Target {
      format: Format::Mp4,
      ..target
    },
  })
}

fn scrub(dir: &std::path::Path) -> Result<(), String> {
  if !dir.join(job::PROXY_FILE).is_file() {
    let p = job::make_proxy(dir)?;
    eprintln!("wrote {}", p.display());
  }
  let started = std::time::Instant::now();
  let out = job::make_scrub(dir, &mut |_| {}, &mut progress_line("scrubbing"))?;
  eprintln!(
    "\nwrote {} in {:.1}s",
    out.display(),
    started.elapsed().as_secs_f64()
  );
  Ok(())
}

fn progress_line(verb: &'static str) -> impl FnMut(u64, u64) {
  let mut last_shown = u64::MAX;
  move |done, total| {
    let pct = done * 100 / total.max(1);
    if pct != last_shown {
      last_shown = pct;
      eprint!("\r{verb} {pct:3}% ({done}/{total} frames)");
      let _ = std::io::stderr().flush();
    }
  }
}

fn render(job: &RenderJob) -> Result<(), String> {
  let report = job::run(
    job,
    &mut |line| eprintln!("{line}"),
    &mut progress_line("rendering"),
  )?;
  eprintln!(
    "\nwrote {} ({} frames) in {:.1}s",
    report.out.display(),
    report.frames,
    report.seconds
  );
  Ok(())
}
