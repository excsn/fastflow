//! Finishes recordings that a previous run left behind when it died mid-recording.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use fastflow_core::recording::Meta;
use fastflow_daemon::paths;
use serde::Deserialize;

use crate::applog::log;

const CAPTURE_EXIT_WAIT: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
struct State {
  pid: u32,
  capture_pid: Option<u32>,
}

/// The command name of a live process. `None` when there is no such process.
fn process_name(pid: u32) -> Option<String> {
  let out = Command::new("/bin/ps")
    .args(["-p", &pid.to_string(), "-o", "comm="])
    .output()
    .ok()?;
  let name = String::from_utf8_lossy(&out.stdout).trim().to_owned();
  (!name.is_empty()).then_some(name)
}

fn signal(pid: u32, sig: &str) {
  let _ = Command::new("/bin/kill")
    .args([sig, &pid.to_string()])
    .status();
}

/// SIGINT lets ffmpeg close its last fragment. SIGKILL follows if it does not exit.
/// The name check guards against a pid that has since been reused by another program.
fn reap_capture(pid: u32) {
  if !process_name(pid).is_some_and(|n| n.ends_with("ffmpeg")) {
    return;
  }
  log(format!("stopping orphaned capture {pid}"));
  signal(pid, "-INT");
  let deadline = Instant::now() + CAPTURE_EXIT_WAIT;
  while process_name(pid).is_some() && Instant::now() < deadline {
    thread::sleep(Duration::from_millis(100));
  }
  if process_name(pid).is_some() {
    signal(pid, "-KILL");
  }
}

fn ffprobe() -> Option<PathBuf> {
  let ffmpeg = fastflow_render::ffmpeg::locate().ok()?;
  let probe = ffmpeg.with_file_name("ffprobe");
  probe.is_file().then_some(probe)
}

fn duration_ms(file: &Path) -> Option<i64> {
  let out = Command::new(ffprobe()?)
    .args([
      "-v",
      "error",
      "-show_entries",
      "format=duration",
      "-of",
      "csv=p=0",
    ])
    .arg(file)
    .output()
    .ok()?;
  let secs: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
  Some((secs * 1000.0) as i64)
}

/// Returns the ids of the recordings it recovered. The daemon never renders these on its own,
/// since it did not see them finish.
pub fn recover_all() -> Vec<String> {
  let Ok(entries) = fs::read_dir(paths::recordings()) else {
    return Vec::new();
  };
  let mut recovered = Vec::new();
  for dir in entries.filter_map(|e| e.ok()).map(|e| e.path()) {
    let state_path = dir.join("state.json");
    let Ok(text) = fs::read_to_string(&state_path) else {
      continue;
    };
    let id = dir
      .file_name()
      .map(|n| n.to_string_lossy().into_owned())
      .unwrap_or_default();
    match recover(&dir, &text) {
      Ok(()) => recovered.push(id),
      Err(e) => log(format!("recover {id}: {e}")),
    }
  }
  recovered
}

fn recover(dir: &Path, state_text: &str) -> Result<(), String> {
  let state: State = serde_json::from_str(state_text).map_err(|e| format!("state.json: {e}"))?;
  if state.pid != std::process::id()
    && process_name(state.pid).is_some_and(|n| n.ends_with("fastflow-app"))
  {
    return Err(format!("still being recorded by process {}", state.pid));
  }
  if let Some(pid) = state.capture_pid {
    reap_capture(pid);
  }

  let meta_path = dir.join("meta.json");
  let text = fs::read_to_string(&meta_path).map_err(|e| format!("meta.json: {e}"))?;
  let mut meta: Meta = serde_json::from_str(&text).map_err(|e| format!("meta.json: {e}"))?;
  meta.recovered = true;
  for seg in meta.segments.iter_mut().filter(|s| s.end_ms.is_none()) {
    seg.end_ms = duration_ms(&dir.join(&seg.file)).map(|d| seg.start_ms + d);
    if seg.end_ms.is_none() {
      meta.failed = Some(format!("{} has no readable frames", seg.file));
    }
  }
  let json = serde_json::to_vec_pretty(&meta).map_err(|e| e.to_string())?;
  fs::write(&meta_path, json).map_err(|e| format!("meta.json: {e}"))?;
  fs::remove_file(dir.join("state.json")).map_err(|e| format!("state.json: {e}"))?;
  log(format!(
    "recovered {}: {}",
    dir.display(),
    meta.failed.as_deref().unwrap_or("renderable")
  ));
  Ok(())
}
