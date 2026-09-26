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
    Ok(RenderJob { dir, out, boxes })
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
