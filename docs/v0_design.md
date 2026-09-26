# fastflow v0 design

The design of the first version as built: what fastflow does to a recording and why.

## Contents

* [Goal](#goal)
* [Capture](#capture)
* [Pacing](#pacing)
* [Camera](#camera)
* [Focus](#focus)
* [Mission Control](#mission-control)
* [Displays](#displays)
* [Rendering](#rendering)
* [Clips](#clips)
* [The app](#the-app)
* [Settings](#settings)
* [Open questions](#open-questions)

## Goal

The motivating case is recording work with an AI agent. Those sessions are mostly dead air: a short prompt, then a long wait while tools run and text streams. A raw recording is unwatchable and editing it by hand for every demo is worse. The information about which stretches were human is available at record time, so the edit is derived rather than performed.

fastflow films what actually happened on the real screen. It is not a scripted terminal recorder, not an editor with a timeline and not a cloud service. v1 has no audio. Speeding a stretch only moves video timestamps. Audio would need its own tempo handling per stretch.

## Capture

The whole display is captured once at native resolution, up to 4096 pixels wide. Nothing is cropped or sped up at capture time. Two sidecars are recorded alongside it. One holds the time and kind of every keyboard and mouse event. The other holds every on-screen window's rect plus the cursor, 10 times a second.

- **ScreenCaptureKit** is the default backend. Frames go from `SCStream` straight into a hardware H.264 `AVAssetWriter` as fragmented QuickTime, with fastflow's own windows excluded. Frame timestamps are on the host clock, so the anchor is exact to within a frame. Only frames where the screen changed are delivered.
- **ffmpeg** with `avfoundation` is the fallback, selected with `capture.backend = "ffmpeg"`. It writes fragmented mp4, cannot exclude windows or follow displays and anchors about 180ms late.
- **Input** is recorded as kind and time only. The file never holds which key was pressed. Mouse moves and drags are recorded at most every 50ms.
- **Resolution.** A 1600x1000pt window on a 2x display is 3200x2000 pixels, which leaves room to crop tightly and still fill a 1080p output.
- **Wide displays.** A display wider than `capture.max_width`, 4096 by default, is scaled down to it while it is captured. At the 2x zoom limit a crop of a 4096-wide capture still covers a 1080p output. Native 6400x3600 capture dropped 19% of its frames and rendered at about a quarter of real time. H.264 encodes at most 4096 pixels a side, so setting `max_width` above 4096 or to 0 for native size records wider displays as HEVC.

## Pacing

Each input event marks a human moment, padded 0.3s before and 1.2s after. Overlapping pads merge into human spans that play at full speed. The stretches between them are idle:

| idle stretch | output |
|---|---|
| under 1s of source | plays at full speed, so a pause between words never flickers |
| up to about 2s | eases up to a lower peak and back down |
| longer | eases up to 3x over 0.5s, plays the rest compressed into at most 0.5s, eases back down |

The ease is smoothstep on speed, so source time is its closed-form integral and stays exact. A 0.25s hold follows each human span so the eye lands before the next thing moves. Keep and Cut markers force a range to human or idle after the automatic spans are built. Chapter markers become mp4 chapters.

`pad_after` is the setting that matters most. Too small and one typed sentence is chopped into alternating fast and slow slices.

## Camera

The camera frames the window you are working in and eases between windows instead of cutting.

- **Subject.** The topmost normal window under the cursor, ignoring anything smaller than 300x200pt. That filter drops menus, tooltips and panels.
- **Debounce.** A window must stay the subject for 400ms of real time before the camera commits to it, so a cursor crossing a window on its way somewhere else never moves the camera.
- **Framing.** The window plus 24pt of padding, widened to the output aspect, never tighter than 2x zoom and moved onto the display without being resized.
- **Holding.** When the new framing overlaps the current one by at least 0.85 intersection over union, the camera does not move.
- **Moving.** A 0.7s move along a quadratic Bezier whose control point is the union of both framings pulled back by 1.15, timed with a cubic ease. The pull-back gives the zoom-out-then-in feel.
- **Output time.** Commits are debounced in source time but moves are placed and timed in output time, so a move takes 0.7s on screen however fast the source plays.

While recording, a live overlay draws the camera's framing on screen as a yellow border. It runs the same director as the render.

## Focus

Everything outside the focused window is blurred and dimmed by 25%, with rounded corners and a soft edge matching the window. The focused window is the camera's committed subject. On a switch the old window fades out while the new one fades in over the same 0.7s as the camera. Menus and popups over the focused window stay sharp because they sit inside its rect.

## Mission Control

While Mission Control or App Exposé is up, the Dock covers the display with windows at layers 18 and 20, which the window samples show. The window list keeps reporting windows at their real positions rather than their thumbnails, so the cursor means nothing meanwhile.

- The camera holds where it was and everything blurs.
- When it closes, the frontmost window is committed at once, without the debounce. If you picked another window the camera moves there. If you backed out, the same window is frontmost, the camera never moved and the blur fades back off it.

## Displays

A recording follows the cursor from display to display as separate segments, one per display visit. It never composites several displays into one frame.

- The cursor must stay on another display for 1s before the recording follows it.
- The new display's capture starts before the old one stops. The new stream's first frame is the boundary. The old file runs a little past it.
- Each segment has its own coordinate space, camera track and focus. The camera does not ease across a boundary. The render cross-dissolves over 0.4s instead, the one place a transition effect is used.
- A resolution change or an unplugged display starts a new segment at once. The ffmpeg backend ends the recording there instead.

## Rendering

The compositor makes one forward pass. For each output frame it asks the timeline for a source time, decodes forward to it, crops the camera rect in floating point with Lanczos and applies focus. It never seeks per frame, since each seek would decode from the previous keyframe and turn a linear render quadratic. Integer-snapped crops make camera motion stutter, so the crop box stays subpixel. The output is H.264 through ffmpeg at the output size, 1920x1080 at 60fps by default.

With the camera off, ffmpeg scales and pads each frame while decoding, since piping native frames would move about 34MB per frame.

After each render the app writes a 960-wide proxy. A preview plans the tracks exactly as the final render does and only decodes the proxy at half size, so it shows the same pacing and framing in seconds.

## Clips

Make GIF… turns a stretch of the rendered, raw or preview video into an animated GIF or WebP. A two-handled slider sets the range while the player loops it at the chosen speed. The file size is estimated by encoding two seconds from the middle of the range and scaling up. The estimate refreshes half a second after the settings stop changing.

- **GIF** goes through ffmpeg's palette pair. One pass builds a palette from the clip. The next maps frames onto it with ordered dithering and redraws only the rectangle that changed, which is most of the saving on screen content.
- **WebP** frames are written by ffmpeg and assembled by `img2webp`, since Homebrew's ffmpeg has no WebP encoder. Identical frames merge. On screen recordings it is usually several times smaller than the GIF.
- **Size** falls fastest with a shorter range, then a higher speed, a lower frame rate and a smaller width.

## The app

- **Packaging.** Screen Recording and Input Monitoring grants belong to an app's code signature, so fastflow is a background-only `.app` with a menu bar item, installed to `/Applications` and built from source on the Mac that runs it. An ad-hoc signature changes on every rebuild and loses the grants.
- **Setup.** A setup window resets stale grants with `tccutil`, requests Input Monitoring before Screen Recording and offers one restart, since neither grant takes effect in a running process.
- **Daemon.** The app owns recording and a render queue behind a unix socket. The tray and the CLI are both clients. Stopping a recording queues its render.
- **Recovery.** A recording left behind by a dead instance is found at launch by its `state.json`. An orphaned ffmpeg is stopped, the segment's length is read from the fragmented file and the recording is marked recovered. It is never rendered automatically.
- **Retention.** Raw footage is deleted 7 days after its render unless the recording is pinned. Recording is refused below 5GB free, warned below 20GB and stopped cleanly below 2GB.

## Settings

Each recording has a `config.toml`, copied at record time from `~/Library/Application Support/com.excsn.mac.fastflow/config.toml` when that exists. Every field has a default, so a file only needs what it changes.

Settings… in the menu bar edits that file in five tabs: Capture, Pacing, Camera, Focus and Output. It writes only the settings that differ from the defaults, so a later change to a default still reaches everything left alone. Apply to Last Recording also writes them into the newest recording and re-renders it.

```toml
[capture]
backend   = "auto"           # "auto", "sck" or "ffmpeg"
max_width = 4096             # pixels; wider displays are scaled down, 0 captures native

[pacing]
pad_before  = 0.3            # seconds of source before each input kept at full speed
pad_after   = 1.2            # and after it
human_speed = 1.0
idle_speed  = 8.0
max_dead    = 0.5            # output seconds for the middle of one idle stretch
hold        = 0.25           # output seconds frozen after each human span
ramp        = 0.5            # output seconds per ease between speeds
ramp_speed  = 3.0            # the speed a ramp eases to

[camera]
enabled         = true
commit_ms       = 400
transition_ms   = 700
window_pad      = 24         # points
pull_back       = 1.15
max_zoom        = 2.0
min_window_size = [300, 200] # points
overlap_hold    = 0.85
live_overlay    = true

[focus]
enabled       = true
blur          = 24           # output pixels
dim           = 0.25
feather       = 6            # output pixels
corner_radius = 10           # points

[render]
switch_ms = 400              # display-switch dissolve

[output]
size = [1920, 1080]
fps  = 60
```

## Open questions

- Whether a display switch should wait until typing stops rather than happening inside a human span.
- Whether the 1s display-switch debounce is right.
- Whether to draw a synthetic cursor. The captured cursor is magnified with the zoom.
- Whether averaging the frames skipped in sped-up stretches should be on by default. It costs little and removes strobing on streaming text.
- Whether 7 days is the right time to keep raw footage, since that decides how long "re-render any time" holds.
- Whether a `human_speed` below 1.0 reads as deliberate or artificial.
