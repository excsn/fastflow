//! Newline-delimited json over the unix socket. One request line, one response line.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
  Start,
  Stop,
  Status,
  /// Queue a render of an existing recording. `output` names one output of its `slices.toml`.
  /// Without it every output is rendered.
  Render {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output: Option<String>,
  },
  List {
    #[serde(default = "default_limit")]
    limit: usize,
  },
  /// Start a new segment on the current display through the same overlapping switch a display
  /// change uses. For testing segmented recordings on one display.
  Resegment,
}

fn default_limit() -> usize {
  5
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Response {
  pub ok: bool,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub error: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub id: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub status: Option<Status>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub recordings: Option<Vec<RecordingEntry>>,
}

impl Response {
  pub fn ok() -> Response {
    Response {
      ok: true,
      ..Response::default()
    }
  }

  pub fn with_id(id: impl Into<String>) -> Response {
    Response {
      id: Some(id.into()),
      ..Response::ok()
    }
  }

  pub fn error(e: impl Into<String>) -> Response {
    Response {
      ok: false,
      error: Some(e.into()),
      ..Response::default()
    }
  }
}

/// Recording and rendering are independent: a new recording can run while the last one renders.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Status {
  pub recording: Option<String>,
  pub rendering: Option<Rendering>,
  pub queued: Vec<String>,
  pub last: Option<Finished>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Task {
  /// Build the proxy and the scrub file.
  Prepare,
  #[default]
  Render,
}

/// One piece of work for the render queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
  pub id: String,
  #[serde(default)]
  pub task: Task,
  /// For a render, the output to render. `None` renders every output.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub output: Option<String>,
}

impl Job {
  pub fn prepare(id: &str) -> Job {
    Job {
      id: id.to_owned(),
      task: Task::Prepare,
      output: None,
    }
  }

  pub fn render(id: &str, output: Option<&str>) -> Job {
    Job {
      id: id.to_owned(),
      task: Task::Render,
      output: output.map(str::to_owned),
    }
  }

  /// How the job reads in a status line.
  pub fn label(&self) -> String {
    match (self.task, &self.output) {
      (Task::Prepare, _) => format!("preparing {}", self.id),
      (Task::Render, None) => self.id.clone(),
      (Task::Render, Some(o)) => format!("{} {o}", self.id),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rendering {
  #[serde(flatten)]
  pub job: Job,
  /// 0 to 1.
  pub progress: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finished {
  #[serde(flatten)]
  pub job: Job,
  /// `Ok` holds the rendered file. `Err` holds why the render failed.
  pub result: Result<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordingEntry {
  pub id: String,
  pub rendered: bool,
  /// Still being recorded. Also true for one left behind by a daemon that died mid-recording.
  pub recording: bool,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn requests_use_the_documented_wire_format() {
    let r: Request = serde_json::from_str(r#"{"cmd":"render","id":"2026-09-25-210217"}"#).unwrap();
    assert_eq!(
      r,
      Request::Render {
        id: "2026-09-25-210217".into(),
        output: None,
      }
    );
    let r: Request = serde_json::from_str(r#"{"cmd":"render","id":"x","output":"demo"}"#).unwrap();
    assert_eq!(
      r,
      Request::Render {
        id: "x".into(),
        output: Some("demo".into()),
      }
    );
    let r: Request = serde_json::from_str(r#"{"cmd":"list"}"#).unwrap();
    assert_eq!(r, Request::List { limit: 5 });
    assert_eq!(
      serde_json::to_string(&Request::Stop).unwrap(),
      r#"{"cmd":"stop"}"#
    );
  }

  #[test]
  fn responses_omit_absent_fields() {
    let line = serde_json::to_string(&Response::with_id("x")).unwrap();
    assert_eq!(line, r#"{"ok":true,"id":"x"}"#);
  }
}
