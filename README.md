# loom-wallpaper

> Part of [Loom](https://github.com/fram446742/loom).

Per-workspace wallpaper scenes for niri. It is its own Cargo workspace with its
own lockfile and build.

## Features

- Named **scenes** mapping workspace names (plus an optional `default` image or
  video) to file paths, with free-form `metadata` and JSON import/export.
- **Video wallpapers** on the native backend: `.mp4`, `.mkv`, `.webm`, `.mov`,
  `.m4v` and `.avi` are decoded by `ffmpeg` at the output's buffer size and
  committed through the same shm path as images. HDR sources play as PQ/BT.2020
  on an HDR-active output when the compositor offers `wp_color_manager_v1`;
  otherwise they are tone-mapped to SDR (zscale + Hable).
- Optional **playlists** (ordered or deterministically shuffled, with an
  interval), **time rules** (`HH:MM` day/night ranges, including ranges that
  wrap midnight) and **random** scene selection.
- A configurable CPU **cross-fade** on the native backend (`transition`:
  duration and easing), driven by a frame timer only while an animation runs.
- **Per-session scenes**: `scene session <name> <scene>` binds a session, read
  from `LOOM_WALLPAPER_SESSION`, `LOOM_SESSION_CONTEXT` (loom-session's
  hook variable), `LOOM_WALLPAPER_SESSION_FILE` or `--session`.
- Two backends:
  - **native** — one built-in `zwlr_layer_shell_v1` background surface per
    output, drawn with `wl_shm`. No `swaybg`/`swww` needed. Supports PNG and
    JPEG, per-output images, HiDPI scale and output rotation, and reloads an
    image when its file changes.
  - **command** — the original behaviour: an external command template per
    output (`swaybg`/`ssww` style) with `{image}`, `{workspace}` and `{output}`
    placeholders.
- `auto` (the default) prefers the native backend and degrades to the command
  template when the compositor cannot host layer-shell backgrounds.
- `apply` sets the wallpaper once; `watch` follows niri's event stream and
  re-applies on workspace focus changes.
- JSON state under `$XDG_STATE_HOME/loom-wallpaper/scenes.json` (override with
  `LOOM_WALLPAPER_STATE`).

## Usage

```sh
export NIRI_SOCKET=...        # normally set inside niri

loom-wallpaper scene set work main ~/Pictures/main.png
loom-wallpaper scene set work default ~/Pictures/default.png
loom-wallpaper scene use work
loom-wallpaper scene session work office    # per-session scene
loom-wallpaper scene export work ~/work-scene.json
loom-wallpaper scene import ~/work-scene.json copy
loom-wallpaper status
loom-wallpaper apply --dry-run
loom-wallpaper watch --dry-run
loom-wallpaper watch        # keep running, e.g. from niri's spawn-at-startup
```

Backends are selected with `--backend=auto|native|command` on `apply`/`watch`,
or with `$LOOM_WALLPAPER_BACKEND`:

```sh
# Native renderer on every output, failing loudly if unavailable.
loom-wallpaper watch --backend=native

# External commands (the pre-native behaviour).
loom-wallpaper watch --backend=command

# Equivalent to the default: native when supported, command otherwise.
export LOOM_WALLPAPER_BACKEND=auto
loom-wallpaper watch
```

Example with `swww` (command backend):

```sh
export LOOM_WALLPAPER_COMMAND='swww img {image} --outputs {output}'
loom-wallpaper watch --backend=command
```

Transitions and playlists belong to the native backend; the command backend is
an external program, so its own flags implement them. For example:

```sh
# swww: built-in transition types/durations
export LOOM_WALLPAPER_COMMAND='swww img {image} --outputs {output} --transition-type wipe --transition-duration 1'
# swaybg: no transitions; it swaps immediately
export LOOM_WALLPAPER_COMMAND='swaybg -i {image} -o {output}'
```

`loom-wallpaper` does not add a transition flag to the template; use the
backend's native option so the renderer stays in charge of its own animation
loop.

### Scene extras

```json
{
  "default": "/wallpapers/default.png",
  "wallpapers": { "main": "/wallpapers/main.png" },
  "transition": { "duration_ms": 600, "easing": "in-out" },
  "playlist": {
    "images": ["/wallpapers/a.png", "/wallpapers/b.png"],
    "interval_secs": 300,
    "shuffle": true
  },
  "time_rules": [
    { "start": "07:00", "end": "19:00", "image": "/wallpapers/day.png" },
    { "start": "19:00", "end": "07:00", "image": "/wallpapers/night.png" }
  ],
  "random": false,
  "metadata": { "author": "me", "source": "unsplash" }
}
```

Resolution order: the first matching `time_rules` range, the workspace image,
the playlist slot, random scene image, then `default`. Time ranges wrap around
midnight when `end` is before `start`. `random` changes at most once per
five-minute slot, so redraws do not flicker. `easing` is `linear`, `in`, `out`
or `in-out`.

### Per-session scenes

```json
{
  "active": "default",
  "session_scenes": { "work": "office", "game": "dark" }
}
```

bind sessions with `scene session work office` or edit the state file. The
session name comes from `--session`, `LOOM_WALLPAPER_SESSION`,
`LOOM_SESSION_CONTEXT` (set by loom-session restore hooks) or the first line
of `LOOM_WALLPAPER_SESSION_FILE`. Unknown sessions use the active scene. The
selected scene is resolved at startup; restart `watch` after changing the
mapping.

`--socket PATH` overrides `$NIRI_SOCKET`. `--dry-run` queries niri and prints
the image resolved for each output without creating surfaces or spawning
anything (`apply --dry-run` with `--backend=command` keeps printing the
single-workspace result).

## Native backend

Each output gets one layer-shell surface on the **background** layer, anchored
to all four edges with an exclusive zone of `0`, so it covers the output without
reserving space for windows. The configured image is scaled to **cover** the
surface and centered (aspect-fill: no letterboxing, edges cropped); the buffer
is allocated at `logical size × output scale` and advertised with
`wl_surface.set_buffer_scale`, and the compositor applies output rotation.

Requirements: a Wayland session whose compositor implements
`zwlr_layer_shell_v1` (niri does), plus `wl_compositor` and `wl_shm`. `probe`
checks those globals before anything is drawn; `status` reports the result:

```console
$ loom-wallpaper status
{
  "backend": {
    "requested": "auto",
    "native_available": true,
    "native_error": null
  },
  ...
}
```

Behaviour:

| Situation | Result |
| --- | --- |
| `--backend=native` but no layer-shell | error explaining why, suggests `--backend=command` |
| `--backend=auto` but no layer-shell | warning, command backend is used |
| New output / monitor plugged in | background surface created for it |
| Output mode or scale change | buffer is re-rendered at the new geometry |
| Output HDR mode toggles (video playing) | decoder restarts within a second in 10-bit PQ or SDR |
| Output removed | surface is dropped; the process keeps running |
| Focused workspace changes (`watch`) | each output re-resolves its workspace image |
| Output has no named workspace | scene `default` is used |
| Image file modified | re-decoded and re-rendered within a second (mtime+size) |
| Image switches (workspace/playlist/time rule) | cross-fades when `transition` is set; immediate swap otherwise |
| niri IPC drops or restarts | event watcher reconnects every second |
| Compositor disconnects | session is rebuilt every second until it is back |
| Image missing or undecodable | dark fallback colour, retried when the file changes |

The native backend commits 8-bit `Xrgb8888` for images and SDR video. HDR video
gets a 10-bit `Xrgb2101010` surface with a parametric PQ/BT.2020 image
description (BT.2408 203-nit reference white) attached through
`wp_color_management_surface_v1`, but only when niri reports the output as
HDR-active, the compositor advertises the global, and the compositor accepts
10-bit shm buffers. Any of those missing keeps the SDR path: HDR video is
tone-mapped to SDR and the compositor treats the surface like any other SDR
client. Images always stay SDR, which the compositor encodes into its blend
space itself.

`apply` with the native backend renders every output and **keeps running** so
the surfaces stay mapped (like `swaybg`); use `watch` to also follow workspace
focus. Set `LOOM_WALLPAPER_DEBUG=1` to log surface creation, configure sizes
and buffer commits.

Limitations (native backend):

- Transitions cross-fade only between two decoded images of the same geometry;
  a resize or output mode change swaps immediately. Frames are CPU-blended and
  the frame timer only runs while a transition is active (no idle redraw loop).
- Images are decoded and scaled on the CPU when they change, which can take a
  moment for large photos on very high-resolution outputs; steady-state CPU use
  is idle (no frame callbacks, one maintenance tick per second).
- Playlists rotate on the one-second maintenance tick; the first image after a
  restart depends on the wall clock slot. Time rules use local time.
- The active scene/profile is read at startup; after `scene use`/`scene set`,
  restart `watch` to pick it up (same as the command backend).
- Per-output images come from the scene of each output's active workspace; there
  is no per-output override file yet.
- HDR video plays as PQ/BT.2020 only on an HDR-active output with colour
  management; everything else stays SDR and is tone-mapped. A video is decoded
  at most once per output, so toggling HDR restarts its decoder.
- Video wallpapers need `ffmpeg` (and `ffprobe` for HDR detection) in `PATH` or
  `LOOM_WALLPAPER_FFMPEG`/`LOOM_WALLPAPER_FFPROBE`. Decoding is CPU-side at
  30 fps; a 4K video costs more than a static image.
- Wayland only. Under X11 or a compositor without layer-shell, use
  `--backend=command`.

## Resolution

For an output whose active workspace is named `main`: first matching time rule,
exact `wallpapers["main"]`, playlist slot, random pick, else the scene
`default`, else nothing (the native backend paints a dark fallback). Unnamed
workspaces only use the time/default/playlist path. The command backend
resolves for the globally focused workspace; the native backend resolves per
output and re-evaluates playlists each maintenance tick.

## Test

```sh
cargo test
cargo fmt --check
cargo clippy --all-targets
```

The 42 unit tests cover scene resolution (including time rules, playlists,
random selection and session mappings), scene import/export and metadata,
JSON parsing, aspect-fill geometry, rotation/scale, image reload stamps,
rendering to XRGB8888, cross-fade/easing math, backend selection and
degradation, and the command watcher/`watch` IPC loop against a fake niri
socket. No test needs a running compositor: Wayland is only probed by `status`,
and the native session is exercised manually.
