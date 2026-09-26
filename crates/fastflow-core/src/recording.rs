//! The on-disk recording format. Every `t` is milliseconds since the first captured frame.

use serde::{Deserialize, Serialize};

use crate::geom::{Point, Rect};

pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Key,
    MouseDown,
    MouseUp,
    MouseMove,
    MouseDrag,
    Scroll,
}

/// One line of `input.jsonl`. Records that an event happened, never which key.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct InputEvent {
    pub t: i64,
    pub kind: InputKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: u32,
    pub pid: i32,
    pub layer: i32,
    /// Normalized to the segment's surface. May extend past 0..1 for a window partly off the display.
    pub rect: Rect,
}

/// One line of `windows.jsonl`. `windows` is front to back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowSample {
    pub t: i64,
    pub segment: u32,
    pub cursor: Point,
    pub windows: Vec<WindowInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Exact,
    Estimated,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentInfo {
    pub index: u32,
    pub file: String,
    pub display_id: u32,
    pub surface_px: [u32; 2],
    pub surface_pt: [f64; 2],
    pub scale: f64,
    pub start_ms: i64,
    pub end_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub format: u32,
    pub started_at: String,
    pub backend: String,
    pub fps: u32,
    pub first_frame: Confidence,
    pub segments: Vec<SegmentInfo>,
    #[serde(default)]
    pub recovered: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_event_line_format() {
        let e = InputEvent {
            t: 12,
            kind: InputKind::MouseDown,
        };
        let line = serde_json::to_string(&e).unwrap();
        assert_eq!(line, r#"{"t":12,"kind":"mouse_down"}"#);
        assert_eq!(serde_json::from_str::<InputEvent>(&line).unwrap(), e);
    }

    #[test]
    fn meta_omits_absent_flags() {
        let meta = Meta {
            format: FORMAT_VERSION,
            started_at: "2026-09-25T20:00:00+01:00".into(),
            backend: "ffmpeg".into(),
            fps: 60,
            first_frame: Confidence::Estimated,
            segments: vec![],
            recovered: false,
            truncated_by: None,
            failed: None,
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert!(!json.contains("truncated_by") && !json.contains("failed"));
        assert_eq!(serde_json::from_str::<Meta>(&json).unwrap(), meta);
    }
}
