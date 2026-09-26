use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Logs/fastflow/fastflow.log"))
}

pub fn log(msg: impl AsRef<str>) {
    let Some(path) = path() else { return };
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let _ = writeln!(
        f,
        "{}.{:03} [{}] {}",
        t.as_secs(),
        t.subsec_millis(),
        std::process::id(),
        msg.as_ref()
    );
}
