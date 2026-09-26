# Architecture

fastflow captures once and edits afterwards. A recording is the untouched screen capture plus two sidecar streams: input event times and 10Hz window geometry. Every edit is a pure function of those sidecars and a settings file, so changing a setting costs a render and never a new recording.

## Crates

| crate | holds |
|---|---|
| `fastflow_core` | the domain model: recording format, config, timeline, camera director and track, markers, diagram. No I/O, no platform calls |
| `fastflow_capture` | the `ScreenCapture` trait and its backends: ScreenCaptureKit with `AVAssetWriter` plus an ffmpeg child process |
| `fastflow_desktop` | window sampling, the input event tap, display lookup and permission checks |
| `fastflow_render` | frame sources and sinks, the compositor, crop, focus blur, proxies and the render job |
| `fastflow_daemon` | the socket protocol, server, render queue, retention sweep and standard paths |
| `fastflow_cli` | the `fastflow` command: socket client plus local render, preview and diagram |
| `fastflow_ui_macos` | the menu bar app: tray, setup window, recorder, live overlay, hotkeys, notifications, crash recovery |

`fastflow_core` depends on nothing platform-specific. `fastflow_daemon` takes the render as an injected function, so it depends on no backend. `fastflow_ui_macos` is the only crate that knows about the app bundle.

## Recording

```
~/Movies/fastflow/2026-09-25-221304/
├─ raw.0.mov       one segment per display visit, native pixels
├─ input.jsonl     {"t":ms,"kind":"key"} per input event and marker, never which key
├─ windows.jsonl   {"t","segment","cursor","windows":[…]} every 100ms, front to back
├─ meta.json       per-segment surface size, scale and time span, backend, flags
├─ config.toml     copied from the defaults at record time
├─ state.json      present only while recording
├─ proxy.mp4       960 wide, 30fps, for previews
└─ render.mp4      the output
```

Two contracts hold across every backend:

- **One clock.** Every `t` is milliseconds since the first captured frame. ScreenCaptureKit reports host-clock timestamps, the clock `Instant` reads, so the anchor is exact. The ffmpeg backend estimates it and runs about 180ms late.
- **Normalized coordinates.** Window, cursor and camera rects are fractions of the segment's captured surface. Pixels appear in exactly one place, the compositor's crop.

The recorder in `fastflow_ui_macos/src/recorder.rs` runs the capture session, a listen-only event tap on the main run loop and a sampler thread. A writer thread holds records until the first frame is known, then writes each jsonl line and flushes it. When the cursor settles on another display for 1s it starts a second capture there and retires the first once the new one delivers a frame, which becomes the segment boundary.

## Rendering

`fastflow_render::job::plan` reads a recording into a `Plan`:

1. `Timeline::build_with` turns input times into spans: human spans at full speed, ramps easing up to `ramp_speed`, idle middles capped at `max_dead`. Keep and Cut markers override the automatic spans.
2. Per segment, `CameraTrack::build` feeds the window samples to a `Director`, which debounces the subject under the cursor and emits decisions. Moves are placed in output time with `plan_move`. Mission Control pauses the director, blurs everything and commits to the frontmost window when it closes.

`job::run` then opens an ffmpeg decoder per segment and drives `compositor::render_segments`: for each output frame it asks the timeline for a source time, decodes forward to it, crops the camera rect with Lanczos, applies the focus blur and writes RGBA to an ffmpeg encoder. Segment boundaries cross-dissolve. Decoding never seeks, so a render is one forward pass.

The live overlay feeds the same `Director` and `plan_move` in wall-clock time, so it cannot disagree with the render about when the camera commits.

## The app

The tray, the socket and hotkeys all end up on the main thread through the winit event loop, where the recorder lives. Renders run one at a time on the queue's worker thread. The socket is `~/Library/Application Support/com.excsn.mac.fastflow/sock`, newline-delimited json. At launch the app recovers recordings a dead instance left behind and sweeps raw footage kept 7 days past its render. It opens the setup window if a grant is missing.

## Testing

`cargo test` covers the timeline with property tests, the camera director, markers, the crop, the focus mask, the socket, the queue and the retention sweep. The compositor is tested against `SyntheticSource`, which numbers its frames, so no ffmpeg, display or permission is needed. Capture and the app are checked by recording: `fastflow render <dir> --boxes` draws the camera's inputs over the footage and `fastflow diagram <id>` prints its decisions.
