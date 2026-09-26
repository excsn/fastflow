# Shortcuts

Every hotkey is ⌃⌥⌘ (Control, Option and Command) plus a letter. They work from any app and need no extra permission.

| keys | does |
|---|---|
| ⌃⌥⌘R | start or stop recording |
| ⌃⌥⌘K | Keep: toggle a range that plays at full speed however idle it looks |
| ⌃⌥⌘X | Cut: toggle a range that plays fast even while you type |
| ⌃⌥⌘M | Chapter: mark this moment as a chapter in the rendered mp4 |
| ⌃⌥⌘F | Frame: toggle pinning the camera to the window it is on |

The marker keys only work while recording. Keep, Cut and Frame start a range on the first press and end it on the second. A range left open runs to the end of the recording. A Cut still plays the range at idle speed rather than removing it.

Markers are stored in the recording's `input.jsonl`, so they apply again on every re-render. Editing or deleting those lines changes the next render.

## Menu bar

| item | does |
|---|---|
| Start Recording / Stop Recording | the same as ⌃⌥⌘R |
| Rendering… / Last render | the current render's progress. When idle, the last result |
| Show Last Render | reveals `render.mp4` in Finder |
| Make GIF… | opens the GIF and WebP exporter |
| Screen Recording / Input Monitoring | shown while a grant is missing, opens the setup window |
| Quit | stops any recording cleanly first |

## Command line

| command | does |
|---|---|
| `fastflow start`, `fastflow stop` | start or stop recording. Stop queues the render |
| `fastflow status` | what is recording, rendering and queued |
| `fastflow list [n]` | the newest recordings |
| `fastflow render <id>` | queue a re-render in the app after editing `config.toml` |
| `fastflow render <dir> [-o out.mp4] [--boxes]` | render locally without the app |
| `fastflow preview <id>` | half-size render from the proxy, for tuning settings |
| `fastflow diagram <id> [cols]` | print what pacing and the camera decided |
| `fastflow proxy <id>` | rebuild the proxy |
| `fastflow gif <id> [--from s] [--to s] [--speed x] [--fps n] [--width px] [--webp]` | export an animated GIF or WebP. `--source raw` or `preview` cuts from those instead of the render |
| `fastflow pin <id>` | keep raw footage past the 7-day sweep |
| `fastflow unpin <id>` | let the sweep delete it again |
| `fastflow resegment` | force a new segment on the current display, for testing |
