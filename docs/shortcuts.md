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
| Compose… | picks slices of a recording and renders mp4, GIF and WebP outputs from them |
| Settings… | edits the defaults for new recordings. Apply to Last Recording also updates the newest one's preview |
| Screen Recording / Input Monitoring | shown while a grant is missing, opens the setup window |
| Quit | stops any recording cleanly first |

## Compose… slice track

| on the track | does |
|---|---|
| drag across empty track | make a new slice |
| click empty track | deselect and play the whole recording from there |
| click a slice | select it and loop it |
| drag a slice's edge | move its start or end |
| drag the middle of a slice | move it without changing its length |
| double-click a slice | add it to the end of the chosen output. If the output already plays it, take it out |
| Delete | remove the selected slice |

A number after a slice's name is its place in the chosen output. Slices that output does not play are dimmed.

| in the lists | does |
|---|---|
| double-click a name in Slices | rename the slice |
| Delete in Slices | remove the slice from the recording and from every output |
| drag a row from Slices to the play list | put it in the chosen output at that place. A slice already there moves |
| drag a row within the play list | change the playing order |
| Delete in the play list | take the slice out of the chosen output |

An output whose play list is empty plays the whole recording.

## Command line

| command | does |
|---|---|
| `fastflow start`, `fastflow stop` | start or stop recording. Stop prepares the preview and opens Compose… |
| `fastflow status` | what is recording, rendering and queued |
| `fastflow list [n]` | the newest recordings |
| `fastflow render <id> [--output <name>]` | queue a render in the app: every output or the one named |
| `fastflow render <dir> [--output <name>] [-o out] [--boxes]` | render locally without the app |
| `fastflow render <id> --slice <from>-<to> [--format gif] [--width px] [--dissolve ms]` | render a one-off output of these source seconds, in the order given |
| `fastflow scrub <id>` | rebuild the scrub preview Compose… plays |
| `fastflow preview <id>` | half-size render from the proxy, for tuning settings |
| `fastflow diagram <id> [cols]` | print what pacing and the camera decided |
| `fastflow proxy <id>` | rebuild the proxy |
| `fastflow gif <id> [--from s] [--to s] [--speed x] [--fps n] [--width px] [--webp]` | export an animated GIF or WebP. `--source raw` or `preview` cuts from those instead of the render |
| `fastflow pin <id>` | keep raw footage past the 7-day sweep |
| `fastflow unpin <id>` | let the sweep delete it again |
| `fastflow resegment` | force a new segment on the current display, for testing |
