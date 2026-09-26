pub mod compositor;
pub mod crop;
pub mod ffmpeg;
pub mod focus;
pub mod overlay;
pub mod synthetic;

use std::fmt;

use fastflow_core::recording::SegmentInfo;

/// RGBA8, rows packed with no padding.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Frame {
    pub fn black(width: u32, height: u32) -> Frame {
        let mut data = vec![0; (width * height * 4) as usize];
        data.as_chunks_mut::<4>()
            .0
            .iter_mut()
            .for_each(|px| px[3] = 255);
        Frame {
            width,
            height,
            data,
        }
    }
}

#[derive(Debug)]
pub enum RenderError {
    Io(std::io::Error),
    Decode(String),
    Encode(String),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderError::Io(e) => write!(f, "render i/o: {e}"),
            RenderError::Decode(why) => write!(f, "decode: {why}"),
            RenderError::Encode(why) => write!(f, "encode: {why}"),
        }
    }
}

impl std::error::Error for RenderError {}

impl From<std::io::Error> for RenderError {
    fn from(e: std::io::Error) -> Self {
        RenderError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, RenderError>;

pub trait FrameSource {
    /// Surface size and scale for the segment playing at `src_t`.
    fn segment_at(&self, src_t: f64) -> &SegmentInfo;
    fn fps(&self) -> f64;
    /// The latest frame at or before `src_t`. Decodes forward only; `src_t` must not decrease.
    fn advance_to(&mut self, src_t: f64) -> Result<&Frame>;
    /// Preview loops only. See docs/07-render.md.
    fn seek(&mut self, src_t: f64) -> Result<()>;
}

pub trait FrameSink {
    fn write(&mut self, frame: &Frame) -> Result<()>;
    fn finish(self: Box<Self>) -> Result<()>;
}
