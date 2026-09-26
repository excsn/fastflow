# Working on fastflow

Read [ARCHITECTURE.md](ARCHITECTURE.md) before changing code. [docs/v0_design.md](docs/v0_design.md) holds the design decisions and their reasons.

## Commands

```sh
cargo build
cargo test                        # everything runs without ffmpeg, a display or permissions
cargo clippy --all-targets        # keep it at zero warnings
cargo fmt

cargo build --release -p fastflow_cli
./target/release/fastflow render ~/Movies/fastflow/<id>          # render a recording locally
./target/release/fastflow diagram <id>                           # what pacing and the camera decided
./target/release/fastflow preview <id>                           # half-size render from the proxy

FASTFLOW_SIGN_IDENTITY="<identity>" fastflow_ui_macos/bundle/bundle.sh --install
open /Applications/fastflow.app

scripts/icons.sh                                                 # regenerate the icons from assets/
FASTFLOW_RELEASE_IDENTITY="Developer ID Application: <name>" scripts/release.sh   # notarized dmg and cask in dist/
```

## Before calling a change done

- `cargo test` passes and clippy is clean.
- A change to pacing, camera, focus or rendering is checked on a real recording: render it, run `fastflow diagram`, extract frames with `ffmpeg -ss <t> -i render.mp4 -frames:v 1 frame.png` and look at them.
- A change to capture or the app is checked by installing the bundle and recording. The log at `~/Library/Logs/fastflow/fastflow.log` shows what the app did.
- A change to the camera's inputs is checked with `fastflow render <dir> --boxes` before trusting the camera itself.
- Reinstalling the app while the user is recording kills their recording. Check `fastflow status` first.

## Constraints that are easy to break

- **Timestamps.** Every sidecar `t` is milliseconds since the first captured frame, never since start was pressed.
- **Coordinates.** Rects in the recording and the tracks are normalized to the segment's surface. Only the compositor's crop converts to pixels.
- **Tracks are pure.** `fastflow_core` does no I/O and calls no platform API. The timeline and camera track are functions of the sidecars and the config.
- **The camera works in output time.** Moves are placed with `Timeline::out_time_at`. Building them in source time makes a sped-up span whip the camera around.
- **One director.** The offline track and the live overlay both use `Director` and `plan_move`. Change them there, never in one caller.
- **Forward decoding only.** `FrameSource::advance_to` must never be asked to go back. Seeking per frame turns a linear render quadratic.
- **Crash safety.** Capture writes fragmented files and `meta.json` is rewritten at every segment switch. A killed recording must still render.
- **Main thread.** The event tap and the recorder live on the main run loop. Socket requests reach them through the winit event loop.
- **Permissions.** Grants are tied to the code signature. An ad-hoc build loses them on every rebuild; sign with a real identity while developing.
- **Overlay exclusion.** ScreenCaptureKit excludes fastflow only if the app has a window when the filter is built, so the overlay is created before capture starts.

## Style

- No comment that restates the code or narrates the change. Write one only for a constraint, an invariant or a reason the code cannot show.
- No comma before `and` or `or`, in code, comments, docs and commit messages.
- Commit messages are one short line saying what changed. No co-author trailers.
- Plain declarative sentences in docs. A heading names its section.
