//! The settings the Settings window edits in the order its tabs show them. Reads and writes
//! them as `config.toml`.
//!
//! A field's type comes from its default value in `Config`, so the table here only holds labels.

use std::fs;
use std::path::Path;

use fastflow_core::config::Config;
use toml::{Table, Value};

pub enum Kind {
  Toggle,
  Number,
  /// Two numbers, such as a width and a height.
  Pair,
  Choice(&'static [&'static str]),
}

pub struct Field {
  pub section: &'static str,
  pub key: &'static str,
  pub label: &'static str,
  pub unit: &'static str,
  pub kind: Kind,
  /// Shown on hover and by the row's help button.
  pub help: &'static str,
}

pub struct Group {
  pub title: &'static str,
  pub fields: &'static [Field],
}

const fn field(
  section: &'static str,
  key: &'static str,
  label: &'static str,
  unit: &'static str,
  kind: Kind,
  help: &'static str,
) -> Field {
  Field {
    section,
    key,
    label,
    unit,
    kind,
    help,
  }
}

pub const GROUPS: &[Group] = &[
  Group {
    title: "Capture",
    fields: &[
      field(
        "capture",
        "backend",
        "Backend",
        "",
        Kind::Choice(&["auto", "sck", "ffmpeg"]),
        "How the screen is captured. auto uses ScreenCaptureKit, which keeps fastflow's own windows out of the footage and follows the cursor across displays. ffmpeg is a fallback that can do neither.",
      ),
      field(
        "capture",
        "max_width",
        "Max capture width",
        "px, 0 captures native",
        Kind::Number,
        "Displays wider than this are scaled down while recording. 4096 is the widest H.264 encodes and keeps renders fast. Higher or 0 records wide displays as HEVC at more pixels and renders more slowly.",
      ),
    ],
  },
  Group {
    title: "Pacing",
    fields: &[
      field(
        "pacing",
        "pad_before",
        "Full speed before input",
        "s",
        Kind::Number,
        "Seconds kept at normal speed before each key press or click, so the lead-up to an action is visible.",
      ),
      field(
        "pacing",
        "pad_after",
        "Full speed after input",
        "s",
        Kind::Number,
        "Seconds kept at normal speed after each key press or click. Too short and typing gets chopped into fast and slow slices.",
      ),
      field(
        "pacing",
        "human_speed",
        "Human speed",
        "×",
        Kind::Number,
        "Playback speed while you are typing or clicking. 1 is real time.",
      ),
      field(
        "pacing",
        "idle_speed",
        "Idle speed",
        "×",
        Kind::Number,
        "How fast the middle of an idle stretch plays once the ramp up has finished. Higher skips waits faster.",
      ),
      field(
        "pacing",
        "ramp",
        "Ramp length",
        "s of output",
        Kind::Number,
        "How long playback takes to ease between human speed and the ramp speed. Longer feels smoother but spends more time at in-between speeds.",
      ),
      field(
        "pacing",
        "ramp_speed",
        "Ramp speed",
        "×",
        Kind::Number,
        "The speed a ramp eases up to before the idle middle starts.",
      ),
      field(
        "pacing",
        "max_dead",
        "Longest idle middle",
        "s of output",
        Kind::Number,
        "The most time the middle of one idle stretch may take in the video, however long the wait was. Lower makes long waits vanish faster.",
      ),
      field(
        "pacing",
        "hold",
        "Hold after input",
        "s of output",
        Kind::Number,
        "A short pause after each burst of input so the eye lands before the next thing moves. 0 turns it off.",
      ),
    ],
  },
  Group {
    title: "Camera",
    fields: &[
      field(
        "camera",
        "enabled",
        "Follow windows",
        "",
        Kind::Toggle,
        "Frames the window under the cursor and eases between windows. Off shows the whole display.",
      ),
      field(
        "camera",
        "live_overlay",
        "Show the framing while recording",
        "",
        Kind::Toggle,
        "Draws a yellow border on screen where the camera is pointing. It never appears in the recording.",
      ),
      field(
        "camera",
        "commit_ms",
        "Commit after",
        "ms",
        Kind::Number,
        "How long the cursor must stay over a window before the camera moves to it. Lower reacts faster but follows the cursor across windows it only passes over.",
      ),
      field(
        "camera",
        "transition_ms",
        "Move time",
        "ms",
        Kind::Number,
        "How long a camera move takes in the video.",
      ),
      field(
        "camera",
        "window_pad",
        "Window padding",
        "pt",
        Kind::Number,
        "Space left around the window inside the frame.",
      ),
      field(
        "camera",
        "pull_back",
        "Pull back",
        "×",
        Kind::Number,
        "How far the camera zooms out partway through a move between windows. 1 moves straight across.",
      ),
      field(
        "camera",
        "max_zoom",
        "Max zoom",
        "×",
        Kind::Number,
        "The tightest the camera may zoom, as a multiple of the captured pixels. Higher frames small windows closer but looks softer.",
      ),
      field(
        "camera",
        "min_window_size",
        "Smallest window followed",
        "pt",
        Kind::Pair,
        "Windows smaller than this are ignored, which keeps menus, tooltips and panels from moving the camera.",
      ),
      field(
        "camera",
        "overlap_hold",
        "Hold when overlap is at least",
        "0 to 1",
        Kind::Number,
        "When the new framing overlaps the current one this much the camera stays put. Lower moves less often.",
      ),
    ],
  },
  Group {
    title: "Focus",
    fields: &[
      field(
        "focus",
        "enabled",
        "Blur outside the window",
        "",
        Kind::Toggle,
        "Blurs and dims everything outside the window the camera is following.",
      ),
      field(
        "focus",
        "blur",
        "Blur",
        "px",
        Kind::Number,
        "Blur radius for everything outside the window, in output pixels.",
      ),
      field(
        "focus",
        "dim",
        "Dim",
        "0 to 1",
        Kind::Number,
        "How much everything outside the window is darkened. 0 leaves it at full brightness.",
      ),
      field(
        "focus",
        "feather",
        "Feather",
        "px",
        Kind::Number,
        "Width of the soft edge between the sharp window and the blur, in output pixels.",
      ),
      field(
        "focus",
        "corner_radius",
        "Corner radius",
        "pt",
        Kind::Number,
        "Rounding of the sharp area's corners. Match it to the window corners on your macOS version.",
      ),
    ],
  },
  Group {
    title: "Output",
    fields: &[
      field(
        "output",
        "size",
        "Size",
        "px",
        Kind::Pair,
        "Width and height of the rendered video. The camera frames to this aspect.",
      ),
      field(
        "output",
        "fps",
        "Frame rate",
        "fps",
        Kind::Number,
        "Frames per second of the rendered video.",
      ),
      field(
        "render",
        "switch_ms",
        "Display switch dissolve",
        "ms",
        Kind::Number,
        "How long the cross-dissolve takes when the recording moves to another display.",
      ),
    ],
  },
];

pub fn to_table(cfg: &Config) -> Table {
  match Value::try_from(cfg).expect("config serializes") {
    Value::Table(t) => t,
    _ => unreachable!("a struct serializes to a table"),
  }
}

pub fn get<'a>(table: &'a Table, f: &Field) -> &'a Value {
  &table[f.section][f.key]
}

pub fn set(table: &mut Table, f: &Field, value: Value) {
  if let Some(Value::Table(section)) = table.get_mut(f.section) {
    section.insert(f.key.to_owned(), value);
  }
}

/// How a number appears in a text field.
pub fn text(v: &Value) -> String {
  match v {
    Value::Integer(i) => i.to_string(),
    Value::Float(f) => f.to_string(),
    Value::String(s) => s.clone(),
    other => other.to_string(),
  }
}

/// Parses a text field as the same type as `like`. Numbers may not be negative.
pub fn parse(like: &Value, text: &str) -> Result<Value, String> {
  let text = text.trim();
  match like {
    Value::Integer(_) => match text.parse::<i64>() {
      Ok(i) if i >= 0 => Ok(Value::Integer(i)),
      _ => Err(format!("\"{text}\" is not a whole number")),
    },
    Value::Float(_) => match text.parse::<f64>() {
      Ok(f) if f.is_finite() && f >= 0.0 => Ok(Value::Float(f)),
      _ => Err(format!("\"{text}\" is not a number")),
    },
    _ => Err(format!("\"{text}\" cannot be parsed")),
  }
}

pub fn from_table(table: Table) -> Result<Config, String> {
  Value::Table(table).try_into().map_err(|e| e.to_string())
}

/// `config.toml` holding only what differs from the defaults, so a later change to a default
/// still reaches every setting left alone.
pub fn minimal_toml(cfg: &Config) -> String {
  let defaults = to_table(&Config::default());
  let mut out = Table::new();
  for (name, section) in to_table(cfg) {
    let Value::Table(section) = section else {
      continue;
    };
    let changed: Table = section
      .into_iter()
      .filter(|(k, v)| defaults[&name].get(k) != Some(v))
      .collect();
    if !changed.is_empty() {
      out.insert(name, Value::Table(changed));
    }
  }
  toml::to_string(&out).expect("a table serializes")
}

/// The defaults when the file does not exist.
pub fn load(path: &Path) -> Result<Config, String> {
  match fs::read_to_string(path) {
    Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
    Err(e) => Err(format!("{}: {e}", path.display())),
  }
}

pub fn save(path: &Path, cfg: &Config) -> Result<(), String> {
  if let Some(dir) = path.parent() {
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
  }
  fs::write(path, minimal_toml(cfg)).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn every_field_exists_in_the_config() {
    let table = to_table(&Config::default());
    for f in GROUPS.iter().flat_map(|g| g.fields) {
      let v = table
        .get(f.section)
        .and_then(|s| s.get(f.key))
        .unwrap_or_else(|| panic!("{}.{} is not a config field", f.section, f.key));
      let fits = match f.kind {
        Kind::Toggle => v.is_bool(),
        Kind::Number => v.is_integer() || v.is_float(),
        Kind::Pair => v.as_array().is_some_and(|a| a.len() == 2),
        Kind::Choice(options) => v.as_str().is_some_and(|s| options.contains(&s)),
      };
      assert!(fits, "{}.{} does not fit its control", f.section, f.key);
    }
  }

  #[test]
  fn every_field_has_help() {
    for f in GROUPS.iter().flat_map(|g| g.fields) {
      assert!(!f.help.is_empty(), "{}.{} has no help", f.section, f.key);
    }
  }

  #[test]
  fn defaults_write_an_empty_file() {
    assert_eq!(minimal_toml(&Config::default()), "");
  }

  #[test]
  fn only_changed_settings_are_written() {
    let mut cfg = Config::default();
    cfg.pacing.idle_speed = 12.0;
    cfg.output.size = [3840, 2160];
    let text = minimal_toml(&cfg);
    assert_eq!(
      text,
      "[output]\nsize = [3840, 2160]\n\n[pacing]\nidle_speed = 12.0\n"
    );
    assert_eq!(toml::from_str::<Config>(&text).unwrap(), cfg);
  }

  #[test]
  fn numbers_parse_as_their_default_type() {
    let int = Value::Integer(4096);
    let float = Value::Float(0.5);
    assert_eq!(parse(&int, " 2048 "), Ok(Value::Integer(2048)));
    assert!(parse(&int, "1.5").is_err());
    assert!(parse(&int, "-1").is_err());
    assert_eq!(parse(&float, "2"), Ok(Value::Float(2.0)));
    assert!(parse(&float, "-0.1").is_err());
    assert!(parse(&float, "fast").is_err());
  }

  #[test]
  fn a_table_of_edits_becomes_a_config() {
    let mut table = to_table(&Config::default());
    let f = &GROUPS[0].fields[1];
    set(&mut table, f, Value::Integer(0));
    assert_eq!(from_table(table).unwrap().capture.max_width, 0);
  }
}
