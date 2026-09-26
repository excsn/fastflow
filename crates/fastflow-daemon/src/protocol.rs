//! Newline-delimited json over the unix socket. One request line, one response line.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Start,
    Stop,
    Status,
    /// Queue a render of an existing recording, for after its `config.toml` changed.
    Render {
        id: String,
    },
    List {
        #[serde(default = "default_limit")]
        limit: usize,
    },
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rendering {
    pub id: String,
    /// 0 to 1.
    pub progress: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finished {
    pub id: String,
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
        let r: Request =
            serde_json::from_str(r#"{"cmd":"render","id":"2026-09-25-210217"}"#).unwrap();
        assert_eq!(
            r,
            Request::Render {
                id: "2026-09-25-210217".into()
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
