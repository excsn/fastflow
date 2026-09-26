//! ScreenCaptureKit capture, encoded in process by `AVAssetWriter`.
//!
//! Frames carry host-clock presentation timestamps, which is the clock `Instant` reads, so the
//! first-frame anchor is exact rather than estimated.

use fibre::mpsc;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use fastflow_core::recording::Confidence;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
  AVAssetWriter, AVAssetWriterInput, AVAssetWriterStatus, AVFileTypeQuickTimeMovie,
  AVMediaTypeVideo, AVVideoAverageBitRateKey, AVVideoCodecKey, AVVideoCodecTypeH264,
  AVVideoCodecTypeHEVC, AVVideoCompressionPropertiesKey, AVVideoExpectedSourceFrameRateKey,
  AVVideoHeightKey, AVVideoMaxKeyFrameIntervalDurationKey, AVVideoWidthKey,
};
use objc2_core_graphics::kCGColorSpaceSRGB;
use objc2_core_media::{CMClock, CMSampleBuffer, CMTime};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSObject, NSString, NSURL};
use objc2_screen_capture_kit::{
  SCContentFilter, SCDisplay, SCFrameStatus, SCRunningApplication, SCShareableContent, SCStream,
  SCStreamConfiguration, SCStreamDelegate, SCStreamFrameInfoStatus, SCStreamOutput,
  SCStreamOutputType,
};

use crate::{
  CaptureArtifact, CaptureCaps, CaptureError, CaptureSession, CaptureSpec, Result, ScreenCapture,
};

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(10);
const BITRATE: i64 = 20_000_000;
const KEYFRAME_SECS: f64 = 2.0;
/// kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, which the H.264 encoder takes without a
/// conversion pass.
const PIXEL_FORMAT_420V: u32 = u32::from_be_bytes(*b"420v");

pub struct SckCapture;

/// Written from the sample queue, read from the thread that owns the session.
struct Shared {
  writer: Retained<AVAssetWriter>,
  input: Retained<AVAssetWriterInput>,
  first_frame: OnceLock<Instant>,
  frames: Mutex<u64>,
  dropped: Mutex<u64>,
  failed: Mutex<Option<String>>,
}

// SAFETY: the writer and its input are only appended to from the one serial sample queue, and
// finished only after the stream has stopped delivering to it.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and Output does not implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "FastflowCaptureOutput"]
    #[ivars = Arc<Shared>]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn did_output_sample_buffer(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Screen && is_complete(sample) {
                self.ivars().append(sample);
            }
        }
    }

    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn did_stop_with_error(&self, _stream: &SCStream, error: &NSError) {
            *self.ivars().failed.lock().unwrap() = Some(error.localizedDescription().to_string());
        }
    }
);

impl Output {
  fn new(shared: Arc<Shared>) -> Retained<Self> {
    let this = Self::alloc().set_ivars(shared);
    unsafe { msg_send![super(this), init] }
  }
}

/// ScreenCaptureKit also delivers idle and blank frames that carry no new image.
fn is_complete(sample: &CMSampleBuffer) -> bool {
  let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else {
    return false;
  };
  if attachments.count() == 0 {
    return false;
  }
  // SAFETY: each attachment is a CFDictionary, toll-free bridged to NSDictionary.
  let dict: &NSDictionary<NSString, AnyObject> =
    unsafe { &*(attachments.value_at_index(0) as *const NSDictionary<NSString, AnyObject>) };
  let status = dict
    .objectForKey(unsafe { SCStreamFrameInfoStatus })
    .and_then(|v| v.downcast::<NSNumber>().ok())
    .map(|n| n.integerValue());
  status == Some(SCFrameStatus::Complete.0)
}

impl Shared {
  fn append(&self, sample: &CMSampleBuffer) {
    let pts = unsafe { sample.presentation_time_stamp() };
    if self.first_frame.get().is_none() {
      unsafe { self.writer.startSessionAtSourceTime(pts) };
      let now = unsafe { CMClock::host_time_clock().time() };
      let behind = (unsafe { now.seconds() } - unsafe { pts.seconds() }).max(0.0);
      let _ = self
        .first_frame
        .set(Instant::now() - Duration::from_secs_f64(behind));
    }
    if !unsafe { self.input.isReadyForMoreMediaData() } {
      *self.dropped.lock().unwrap() += 1;
      return;
    }
    if unsafe { self.input.appendSampleBuffer(sample) } {
      *self.frames.lock().unwrap() += 1;
    } else {
      let why = unsafe { self.writer.error() }
        .map(|e| e.localizedDescription().to_string())
        .unwrap_or_else(|| "append failed".into());
      *self.failed.lock().unwrap() = Some(why);
    }
  }
}

/// Runs an API that reports through a completion block and waits for it.
fn wait<T: Send + 'static>(start: impl FnOnce(mpsc::BoundedSyncSender<T>)) -> Result<T> {
  let (tx, rx) = mpsc::bounded(1);
  start(tx);
  rx.recv_timeout(CALLBACK_TIMEOUT)
    .map_err(|_| CaptureError::Exited("ScreenCaptureKit did not answer".into()))
}

fn error_text(e: *mut NSError) -> Option<String> {
  // SAFETY: a non-null error from a completion block is a valid NSError for its duration.
  unsafe { e.as_ref() }.map(|e| e.localizedDescription().to_string())
}

struct Content(Retained<SCShareableContent>);
// SAFETY: SCShareableContent is an immutable snapshot.
unsafe impl Send for Content {}

fn shareable_content() -> Result<Retained<SCShareableContent>> {
  let result = wait(|tx| {
    let tx = Mutex::new(tx);
    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
      let r = match unsafe { Retained::retain(content) } {
        Some(c) => Ok(Content(c)),
        None => Err(error_text(err).unwrap_or_else(|| "no shareable content".into())),
      };
      let _ = tx.lock().unwrap().send(r);
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
  })?;
  result
    .map(|c| c.0)
    .map_err(|e| CaptureError::BackendMissing(format!("ScreenCaptureKit: {e}")))
}

fn number(n: Retained<NSNumber>) -> Retained<AnyObject> {
  Retained::into_super(Retained::into_super(n)).into()
}

fn writer(
  out: &Path,
  w: usize,
  h: usize,
  fps: u32,
) -> Result<(Retained<AVAssetWriter>, Retained<AVAssetWriterInput>)> {
  let err = |what: &str| CaptureError::Exited(format!("AVAssetWriter: {what}"));
  let url = NSURL::fileURLWithPath(&NSString::from_str(&out.to_string_lossy()));
  let file_type = unsafe { AVFileTypeQuickTimeMovie }.ok_or_else(|| err("no file type"))?;
  let writer = unsafe { AVAssetWriter::assetWriterWithURL_fileType_error(&url, file_type) }
    .map_err(|e| err(&e.localizedDescription().to_string()))?;
  // Fragments keep a killed recording readable up to the last one written.
  unsafe { writer.setMovieFragmentInterval(CMTime::new(2, 1)) };

  let key = |k: Option<&'static NSString>| k.ok_or_else(|| err("missing settings key"));
  let compression = NSDictionary::<NSString, AnyObject>::from_retained_objects(
    &[
      key(unsafe { AVVideoAverageBitRateKey })?,
      key(unsafe { AVVideoMaxKeyFrameIntervalDurationKey })?,
      key(unsafe { AVVideoExpectedSourceFrameRateKey })?,
    ],
    &[
      number(NSNumber::numberWithLongLong(BITRATE)),
      number(NSNumber::numberWithDouble(KEYFRAME_SECS)),
      number(NSNumber::numberWithUnsignedInt(fps)),
    ],
  );
  let codec = if crate::needs_hevc((w as u32, h as u32)) {
    unsafe { AVVideoCodecTypeHEVC }.ok_or_else(|| err("no HEVC codec"))?
  } else {
    unsafe { AVVideoCodecTypeH264 }.ok_or_else(|| err("no H.264 codec"))?
  };
  let settings = NSDictionary::<NSString, AnyObject>::from_retained_objects(
    &[
      key(unsafe { AVVideoCodecKey })?,
      key(unsafe { AVVideoWidthKey })?,
      key(unsafe { AVVideoHeightKey })?,
      key(unsafe { AVVideoCompressionPropertiesKey })?,
    ],
    &[
      Retained::into_super(NSString::from_str(&codec.to_string())).into(),
      number(NSNumber::numberWithUnsignedInteger(w)),
      number(NSNumber::numberWithUnsignedInteger(h)),
      Retained::into_super(compression).into(),
    ],
  );
  let media = unsafe { AVMediaTypeVideo }.ok_or_else(|| err("no video media type"))?;
  let input = unsafe {
    AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(media, Some(&settings))
  };
  unsafe { input.setExpectsMediaDataInRealTime(true) };
  if !unsafe { writer.canAddInput(&input) } {
    return Err(err("cannot add the video input"));
  }
  unsafe { writer.addInput(&input) };
  if !unsafe { writer.startWriting() } {
    let why = unsafe { writer.error() }.map(|e| e.localizedDescription().to_string());
    return Err(err(&why.unwrap_or_else(|| "could not start".into())));
  }
  Ok((writer, input))
}

impl ScreenCapture for SckCapture {
  fn name(&self) -> &'static str {
    "sck"
  }

  fn caps(&self) -> CaptureCaps {
    CaptureCaps {
      can_exclude_windows: true,
      can_deliver_frames: false,
      reports_frame_timestamps: true,
      can_follow_displays: true,
      max_fps: 60,
    }
  }

  fn start(&mut self, spec: &CaptureSpec) -> Result<Box<dyn CaptureSession>> {
    let content = shareable_content()?;
    let displays = unsafe { content.displays() };
    let display: Retained<SCDisplay> = displays
      .iter()
      .find(|d| unsafe { d.displayID() } == spec.display_id)
      .ok_or(CaptureError::NoSuchDisplay(spec.display_index))?;
    let me = std::process::id() as i32;
    let ours: Vec<Retained<SCRunningApplication>> = unsafe { content.applications() }
      .iter()
      .filter(|a| unsafe { a.processID() } == me)
      .collect();
    let excluded = NSArray::from_retained_slice(&ours);
    let filter = unsafe {
      SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
        SCContentFilter::alloc(),
        &display,
        &excluded,
        &NSArray::new(),
      )
    };

    let (w, h) = spec.size_px;
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
      config.setWidth(w as usize);
      config.setHeight(h as usize);
      config.setMinimumFrameInterval(CMTime::new(1, spec.fps as i32));
      config.setPixelFormat(PIXEL_FORMAT_420V);
      config.setColorSpaceName(kCGColorSpaceSRGB);
      config.setShowsCursor(true);
      config.setQueueDepth(8);
      config.setCapturesAudio(false);
    }

    let (writer, input) = writer(&spec.out, w as usize, h as usize, spec.fps)?;
    let shared = Arc::new(Shared {
      writer,
      input,
      first_frame: OnceLock::new(),
      frames: Mutex::new(0),
      dropped: Mutex::new(0),
      failed: Mutex::new(None),
    });
    let output = Output::new(Arc::clone(&shared));
    let delegate = ProtocolObject::from_ref(&*output);
    let stream = unsafe {
      SCStream::initWithFilter_configuration_delegate(
        SCStream::alloc(),
        &filter,
        &config,
        Some(delegate),
      )
    };
    let queue = DispatchQueue::new("com.excsn.mac.fastflow.capture", None);
    unsafe {
      stream.addStreamOutput_type_sampleHandlerQueue_error(
        ProtocolObject::from_ref(&*output),
        SCStreamOutputType::Screen,
        Some(&queue),
      )
    }
    .map_err(|e| CaptureError::Exited(e.localizedDescription().to_string()))?;

    let started = wait(|tx| {
      let tx = Mutex::new(tx);
      let block = RcBlock::new(move |err: *mut NSError| {
        let _ = tx.lock().unwrap().send(error_text(err));
      });
      unsafe { stream.startCaptureWithCompletionHandler(Some(&block)) };
    })?;
    if let Some(e) = started {
      return Err(CaptureError::Exited(format!("start: {e}")));
    }

    Ok(Box::new(SckSession {
      stream,
      _output: output,
      _queue: queue,
      shared,
      out: spec.out.clone(),
    }))
  }
}

struct SckSession {
  stream: Retained<SCStream>,
  _output: Retained<Output>,
  _queue: DispatchRetained<DispatchQueue>,
  shared: Arc<Shared>,
  out: PathBuf,
}

// SAFETY: the session is created, stopped and dropped on one thread at a time. The stream and
// output are only messaged from there. The sample queue reaches them through `Shared`.
unsafe impl Send for SckSession {}

impl CaptureSession for SckSession {
  fn first_frame_at(&self) -> Option<Instant> {
    self.shared.first_frame.get().copied()
  }

  fn confidence(&self) -> Confidence {
    Confidence::Exact
  }

  fn exited(&mut self) -> Option<String> {
    self.shared.failed.lock().unwrap().clone()
  }

  fn pid(&self) -> Option<u32> {
    None
  }

  fn stop(self: Box<Self>) -> Result<CaptureArtifact> {
    let stopped = wait(|tx| {
      let tx = Mutex::new(tx);
      let block = RcBlock::new(move |err: *mut NSError| {
        let _ = tx.lock().unwrap().send(error_text(err));
      });
      unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&block)) };
    })?;

    let shared = &self.shared;
    if shared.first_frame.get().is_none() {
      unsafe { shared.writer.cancelWriting() };
      return Err(CaptureError::Exited("no frame was captured".into()));
    }
    unsafe { shared.input.markAsFinished() };
    wait(|tx| {
      let tx = Mutex::new(tx);
      let block = RcBlock::new(move || {
        let _ = tx.lock().unwrap().send(());
      });
      unsafe { shared.writer.finishWritingWithCompletionHandler(&block) };
    })?;

    let status = unsafe { shared.writer.status() };
    if status != AVAssetWriterStatus::Completed {
      let why = unsafe { shared.writer.error() }
        .map(|e| e.localizedDescription().to_string())
        .unwrap_or_else(|| format!("writer status {}", status.0));
      return Err(CaptureError::Exited(why));
    }
    if let Some(e) = stopped.or_else(|| shared.failed.lock().unwrap().clone()) {
      return Err(CaptureError::Exited(e));
    }
    Ok(CaptureArtifact {
      path: self.out.clone(),
      status: None,
      frames: Some((
        *shared.frames.lock().unwrap(),
        *shared.dropped.lock().unwrap(),
      )),
    })
  }
}
