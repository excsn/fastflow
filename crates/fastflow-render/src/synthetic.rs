//! A frame source and sink with no ffmpeg, display or permissions, for testing the pipeline.

use fastflow_core::recording::SegmentInfo;

use crate::{Frame, FrameSink, FrameSource, RenderError, Result};

/// Frame `n` carries `n` as a little-endian u64 in its first eight bytes.
pub struct SyntheticSource {
    fps: f64,
    count: u64,
    next: u64,
    current: Frame,
    segment: SegmentInfo,
    seeks: u64,
}

impl SyntheticSource {
    pub fn new(size: (u32, u32), fps: f64, count: u64) -> Self {
        SyntheticSource {
            fps,
            count,
            next: 0,
            current: Frame::black(size.0, size.1),
            segment: SegmentInfo {
                index: 0,
                file: "synthetic".into(),
                display_id: 0,
                surface_px: [size.0, size.1],
                surface_pt: [size.0 as f64, size.1 as f64],
                scale: 1.0,
                start_ms: 0,
                end_ms: Some((count as f64 / fps * 1000.0) as i64),
            },
            seeks: 0,
        }
    }

    pub fn decoded(&self) -> u64 {
        self.next
    }

    pub fn seeks(&self) -> u64 {
        self.seeks
    }

    fn decode(&mut self) {
        self.current.data[..8].copy_from_slice(&self.next.to_le_bytes());
        self.next += 1;
    }
}

pub fn frame_index(frame: &Frame) -> u64 {
    u64::from_le_bytes(frame.data[..8].try_into().unwrap())
}

impl FrameSource for SyntheticSource {
    fn segment_at(&self, _src_t: f64) -> &SegmentInfo {
        &self.segment
    }

    fn fps(&self) -> f64 {
        self.fps
    }

    fn advance_to(&mut self, src_t: f64) -> Result<&Frame> {
        if self.count == 0 {
            return Err(RenderError::Decode("empty source".into()));
        }
        if self.next == 0 {
            self.decode();
        }
        while self.next < self.count && self.next as f64 / self.fps <= src_t + 1e-9 {
            self.decode();
        }
        Ok(&self.current)
    }

    fn seek(&mut self, src_t: f64) -> Result<()> {
        self.seeks += 1;
        self.next = ((src_t * self.fps).floor() as u64).min(self.count.saturating_sub(1));
        self.decode();
        Ok(())
    }
}

#[derive(Default)]
pub struct RecordingSink {
    pub indices: Vec<u64>,
}

impl FrameSink for RecordingSink {
    fn write(&mut self, frame: &Frame) -> Result<()> {
        self.indices.push(frame_index(frame));
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}
