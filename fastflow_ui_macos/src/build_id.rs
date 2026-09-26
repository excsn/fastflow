use std::fs;
use std::path::PathBuf;

fn marker() -> Option<PathBuf> {
  let home = std::env::var_os("HOME")?;
  Some(PathBuf::from(home).join("Library/Application Support/com.excsn.mac.fastflow/granted-build"))
}

/// FNV-1a over the executable. An ad-hoc signature changes exactly when these bytes do.
pub fn current() -> Option<String> {
  let bytes = fs::read(std::env::current_exe().ok()?).ok()?;
  let mut h: u64 = 0xcbf2_9ce4_8422_2325;
  for b in bytes {
    h ^= b as u64;
    h = h.wrapping_mul(0x0100_0000_01b3);
  }
  Some(format!("{h:016x}"))
}

/// The build that last held every grant, if any.
pub fn last_granted() -> Option<String> {
  fs::read_to_string(marker()?)
    .ok()
    .map(|s| s.trim().to_owned())
}

pub fn record_granted(id: &str) {
  let Some(path) = marker() else { return };
  if let Some(dir) = path.parent() {
    let _ = fs::create_dir_all(dir);
  }
  let _ = fs::write(path, id);
}
