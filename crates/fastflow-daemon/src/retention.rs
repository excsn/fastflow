//! Raw footage is tens of megabits a second, so the daemon deletes it once a render has kept it
//! long enough. Deleting it makes a recording unrenderable, which is what turns "re-render any
//! time" into "re-render this week".

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

pub const RAW_KEEP: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub const WARN_BELOW: u64 = 20 << 30;
pub const REFUSE_BELOW: u64 = 5 << 30;
pub const STOP_BELOW: u64 = 2 << 30;

/// A recording with this file in its directory is never swept.
pub const PIN_FILE: &str = "pinned";

#[derive(Debug, Clone, PartialEq)]
pub struct Swept {
    pub id: String,
    pub bytes: u64,
}

fn is_raw(name: &str) -> bool {
    name.starts_with("raw.") && name.ends_with(".mp4")
}

/// Deletes the raw segments of every recording whose render is older than `keep`. Recordings
/// still recording, pinned, failed or never rendered keep everything.
pub fn sweep(root: &Path, now: SystemTime, keep: Duration) -> Vec<Swept> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut swept = Vec::new();
    for dir in entries.filter_map(|e| e.ok()).map(|e| e.path()) {
        if dir.join("state.json").exists() || dir.join(PIN_FILE).exists() {
            continue;
        }
        let rendered_at = fs::metadata(dir.join("render.mp4")).and_then(|m| m.modified());
        let Ok(rendered_at) = rendered_at else {
            continue;
        };
        if now.duration_since(rendered_at).unwrap_or_default() < keep {
            continue;
        }
        let Ok(files) = fs::read_dir(&dir) else {
            continue;
        };
        let mut bytes = 0;
        for f in files.filter_map(|e| e.ok()) {
            if !is_raw(&f.file_name().to_string_lossy()) {
                continue;
            }
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if fs::remove_file(f.path()).is_ok() {
                bytes += len;
            }
        }
        if bytes > 0 {
            let id = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            swept.push(Swept { id, bytes });
        }
    }
    swept
}

/// Bytes available to this user on the volume holding `path`.
pub fn free_bytes(path: &Path) -> Option<u64> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording(root: &Path, id: &str, files: &[&str]) -> std::path::PathBuf {
        let dir = root.join(id);
        fs::create_dir_all(&dir).unwrap();
        for f in files {
            fs::write(dir.join(f), b"0123456789").unwrap();
        }
        dir
    }

    #[test]
    fn sweeps_only_old_unpinned_rendered_recordings() {
        let root = std::env::temp_dir().join(format!("fastflow-sweep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let old = recording(
            &root,
            "old",
            &["raw.0.mp4", "raw.1.mp4", "render.mp4", "meta.json"],
        );
        recording(&root, "pinned", &["raw.0.mp4", "render.mp4", PIN_FILE]);
        recording(&root, "unrendered", &["raw.0.mp4"]);
        recording(&root, "live", &["raw.0.mp4", "render.mp4", "state.json"]);

        let later = SystemTime::now() + RAW_KEEP + Duration::from_secs(60);
        let swept = sweep(&root, later, RAW_KEEP);
        assert_eq!(
            swept,
            vec![Swept {
                id: "old".into(),
                bytes: 20
            }]
        );
        assert!(!old.join("raw.0.mp4").exists() && old.join("render.mp4").exists());
        assert!(root.join("pinned/raw.0.mp4").exists());
        assert!(root.join("unrendered/raw.0.mp4").exists());
        assert!(root.join("live/raw.0.mp4").exists());

        assert!(sweep(&root, SystemTime::now(), RAW_KEEP).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reports_free_space_for_the_temp_volume() {
        assert!(free_bytes(&std::env::temp_dir()).is_some_and(|b| b > 0));
    }
}
