use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use fastflow_core::recording::Confidence;

use crate::{
    CaptureArtifact, CaptureCaps, CaptureError, CaptureSession, CaptureSpec, Result, ScreenCapture,
};

/// A Finder-launched app gets a minimal PATH, so Homebrew's prefixes are searched explicitly.
const SEARCH_DIRS: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const STDERR_TAIL: usize = 20;

/// A plain mp4 writes its index last, so a killed capture leaves nothing readable. A fragmented
/// one is readable up to its last complete fragment. A fragment closes at each keyframe.
/// `h264_videotoolbox` ignores `-g`, so keyframes are forced every 2s.
const RECOVERABLE: [&str; 4] = [
    "-force_key_frames",
    "expr:gte(t,n_forced*2)",
    "-movflags",
    "+frag_keyframe+empty_moov+default_base_moof",
];

pub struct FfmpegCapture {
    bin: PathBuf,
}

impl FfmpegCapture {
    pub fn locate() -> Result<Self> {
        let path_dirs = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .unwrap_or_default();
        path_dirs
            .into_iter()
            .chain(SEARCH_DIRS.iter().map(PathBuf::from))
            .map(|d| d.join("ffmpeg"))
            .find(|p| p.is_file())
            .map(|bin| FfmpegCapture { bin })
            .ok_or_else(|| CaptureError::BackendMissing("ffmpeg not found".into()))
    }

    pub fn bin(&self) -> &Path {
        &self.bin
    }

    /// Names of the avfoundation screen devices, in display order: "Capture screen 0", ...
    pub fn screen_devices(&self) -> Result<Vec<String>> {
        let out = Command::new(&self.bin)
            .args([
                "-hide_banner",
                "-f",
                "avfoundation",
                "-list_devices",
                "true",
                "-i",
                "",
            ])
            .stdin(Stdio::null())
            .output()?;
        Ok(parse_screen_devices(&String::from_utf8_lossy(&out.stderr)))
    }
}

fn parse_screen_devices(listing: &str) -> Vec<String> {
    let mut screens: Vec<(usize, String)> = listing
        .lines()
        .filter_map(|l| {
            let name = &l[l.find("] Capture screen ")? + 2..];
            let n = name.strip_prefix("Capture screen ")?.trim().parse().ok()?;
            Some((n, name.trim().to_owned()))
        })
        .collect();
    screens.sort();
    screens.into_iter().map(|(_, name)| name).collect()
}

impl ScreenCapture for FfmpegCapture {
    fn name(&self) -> &'static str {
        "ffmpeg"
    }

    fn caps(&self) -> CaptureCaps {
        CaptureCaps {
            can_exclude_windows: false,
            can_deliver_frames: false,
            reports_frame_timestamps: false,
            can_follow_displays: false,
            max_fps: 60,
        }
    }

    fn start(&mut self, spec: &CaptureSpec) -> Result<Box<dyn CaptureSession>> {
        let device = self
            .screen_devices()?
            .into_iter()
            .nth(spec.display_index)
            .ok_or(CaptureError::NoSuchDisplay(spec.display_index))?;
        let fps = spec.fps.to_string();
        let input = format!("{device}:none");
        let mut child = Command::new(&self.bin)
            .args(["-hide_banner", "-nostats", "-loglevel", "warning"])
            .args([
                "-f",
                "avfoundation",
                "-capture_cursor",
                "1",
                "-framerate",
                &fps,
            ])
            .args(["-i", &input])
            .args([
                "-c:v",
                "h264_videotoolbox",
                "-q:v",
                "60",
                "-pix_fmt",
                "yuv420p",
            ])
            .args(RECOVERABLE)
            .args(["-progress", "pipe:1", "-stats_period", "0.1", "-y"])
            .arg(&spec.out)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let first_frame = Arc::new(OnceLock::new());
        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL)));

        let stdout = child.stdout.take().expect("piped stdout");
        let ff = Arc::clone(&first_frame);
        let frame_period = 1.0 / spec.fps as f64;
        thread::spawn(move || watch_progress(BufReader::new(stdout), &ff, frame_period));

        let stderr = child.stderr.take().expect("piped stderr");
        let tail = Arc::clone(&stderr_tail);
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(|l| l.ok()) {
                let mut t = tail.lock().unwrap();
                if t.len() == STDERR_TAIL {
                    t.pop_front();
                }
                t.push_back(line);
            }
        });

        Ok(Box::new(FfmpegSession {
            stdin: child.stdin.take(),
            child,
            out: spec.out.clone(),
            first_frame,
            stderr_tail,
        }))
    }
}

/// ffmpeg reports progress every `-stats_period`, not per frame. The first report with frames
/// written is backdated by the frames it already counts, so the estimate is off by at most
/// the encoder's pipeline delay.
fn watch_progress(reader: impl BufRead, first_frame: &OnceLock<Instant>, frame_period: f64) {
    for line in reader.lines().map_while(|l| l.ok()) {
        let Some(n) = line.strip_prefix("frame=") else {
            continue;
        };
        let Ok(n) = n.trim().parse::<u64>() else {
            continue;
        };
        if n > 0 && first_frame.get().is_none() {
            let back = Duration::from_secs_f64((n - 1) as f64 * frame_period);
            let _ = first_frame.set(Instant::now() - back);
        }
    }
}

struct FfmpegSession {
    child: Child,
    stdin: Option<ChildStdin>,
    out: PathBuf,
    first_frame: Arc<OnceLock<Instant>>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

impl FfmpegSession {
    fn stderr_summary(&self) -> String {
        let tail = self.stderr_tail.lock().unwrap();
        tail.iter().cloned().collect::<Vec<_>>().join(" | ")
    }
}

impl CaptureSession for FfmpegSession {
    fn first_frame_at(&self) -> Option<Instant> {
        self.first_frame.get().copied()
    }

    fn confidence(&self) -> Confidence {
        Confidence::Estimated
    }

    fn exited(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(format!("{status}: {}", self.stderr_summary())),
            Ok(None) => None,
            Err(e) => Some(format!("wait failed: {e}")),
        }
    }

    fn pid(&self) -> Option<u32> {
        Some(self.child.id())
    }

    /// `q` on stdin makes ffmpeg finish the mp4 cleanly. Killing it would leave no moov atom.
    fn stop(mut self: Box<Self>) -> Result<CaptureArtifact> {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.write_all(b"q\n");
        }
        let deadline = Instant::now() + STOP_TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait()? {
                break Some(status);
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                break self.child.wait().ok();
            }
            thread::sleep(Duration::from_millis(50));
        };
        if !status.is_some_and(|s| s.success()) {
            return Err(CaptureError::Exited(format!(
                "{status:?}: {}",
                self.stderr_summary()
            )));
        }
        Ok(CaptureArtifact {
            path: self.out.clone(),
            status,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_screen_devices_in_order() {
        let listing = "\
[AVFoundation indev @ 0x1] AVFoundation video devices:
[AVFoundation indev @ 0x1] [0] MacBook Pro Camera
[AVFoundation indev @ 0x1] [5] Capture screen 1
[AVFoundation indev @ 0x1] [4] Capture screen 0
[AVFoundation indev @ 0x1] AVFoundation audio devices:
[AVFoundation indev @ 0x1] [0] MacBook Pro Microphone";
        assert_eq!(
            parse_screen_devices(listing),
            vec!["Capture screen 0", "Capture screen 1"]
        );
    }

    #[test]
    fn first_progress_report_is_backdated() {
        let first = OnceLock::new();
        let before = Instant::now();
        let progress = "frame=0\nprogress=continue\nframe=7\nframe=13\n";
        watch_progress(progress.as_bytes(), &first, 0.1);
        let at = *first.get().unwrap();
        let backdate = before.saturating_duration_since(at);
        assert!(backdate >= Duration::from_millis(550) && backdate <= Duration::from_millis(650));
    }
}
