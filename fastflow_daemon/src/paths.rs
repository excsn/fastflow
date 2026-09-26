use std::fs;
use std::path::PathBuf;

use crate::protocol::RecordingEntry;

pub const BUNDLE_ID: &str = "com.excsn.mac.fastflow";

fn home() -> PathBuf {
  std::env::var_os("HOME")
    .map(PathBuf::from)
    .unwrap_or_default()
}

pub fn support_dir() -> PathBuf {
  home().join("Library/Application Support").join(BUNDLE_ID)
}

pub fn socket() -> PathBuf {
  support_dir().join("sock")
}

/// The user's defaults, copied into each recording at record time.
pub fn default_config() -> PathBuf {
  support_dir().join("config.toml")
}

pub fn recordings() -> PathBuf {
  home().join("Movies/fastflow")
}

/// Newest first. Recording ids are timestamps, so name order is time order.
pub fn list_recordings(limit: usize) -> Vec<RecordingEntry> {
  let Ok(entries) = fs::read_dir(recordings()) else {
    return Vec::new();
  };
  let mut dirs: Vec<PathBuf> = entries
    .filter_map(|e| e.ok())
    .map(|e| e.path())
    .filter(|p| p.join("meta.json").is_file())
    .collect();
  dirs.sort();
  dirs.reverse();
  dirs
    .into_iter()
    .take(limit)
    .filter_map(|p| {
      let id = p.file_name()?.to_string_lossy().into_owned();
      Some(RecordingEntry {
        rendered: p.join("render.mp4").is_file(),
        recording: p.join("state.json").is_file(),
        id,
      })
    })
    .collect()
}
