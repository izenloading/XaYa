# macOS Metal renderer

The native Metal backend is the default on macOS. Other platforms use OpenGL.
To use OpenGL on macOS, select it in the configuration:

```toml
[debug]
renderer = "glsl3"
```

Or start a single instance with:

```sh
cargo run -p alacritty -- -o 'debug.renderer="glsl3"'
```

Backend selection applies when a window is created. Restart the application to
change all existing windows. Unsupported Metal initialization falls back to the automatically
selected OpenGL renderer and records the reason in the log. A runtime Metal
execution/allocation failure also recreates the window's renderer using OpenGL
and rebuilds its glyph cache. If both backends fail, the affected window closes
with an error in the log.

XaYa supports Apple Silicon Macs running macOS 11 or later, all of which support
Metal. No Swift rewrite or build-time Metal compiler is
required: the embedded MSL source is compiled through Metal at runtime.

## Implementation

The terminal/parser, PTY, input, font rasterization, search, selection, IME and
window event handling remain shared. `renderer/mod.rs` selects the backend and
owns presentation; the existing GL implementation lives in `renderer/opengl.rs`.
The native backend lives in `renderer/metal/`.

The main-thread resource cache shares the default Metal device and immutable
pipeline states across windows. Successful resources remain cached for the
process lifetime; transient initialization failures are not cached. Command
queues, layers, atlases and frame buffers remain per-window.

- `raw-window-metal` attaches an observer-backed Metal sublayer to the winit
  NSView. It preserves the existing root layer and native redraw callbacks.
- Glyphs keep the existing crossfont rasterization and cache transforms.
  Backend-local atlas handles replace GL-specific typing in the shared cache.
- Text uses instanced quads, linear texture sampling and dual-source blending,
  retaining RGB subpixel masks and premultiplied color Emoji behavior.
- Backgrounds, cursor shapes, decorations, footer/IME text and overlays use the
  same display inputs. Undercurl, dotted and dashed decoration shaders preserve
  the GL formulas and ordering.
- BGRA8 drawable values are tagged with an explicit sRGB layer color space.
  The layer's opaque flag follows runtime window opacity.
- Every drawable receives a full clear and full visible-frame redraw. A fresh
  drawable never relies on previous contents or damage-region preservation.
- Atlas uploads are coalesced into one staging buffer per frame, with 256-byte
  aligned offsets, and ordered blits on the window's command queue. Per-window
  instance/upload slots are reused only after their command buffer completes;
  in-flight contents remain immutable. Buffers grow geometrically. Cached GPU
  buffers are limited to 8 MiB each, two buffers per slot and two slots per window
  (32 MiB maximum for this pool, excluding atlases). Larger temporary buffers are
  discarded on completion. Font resets allocate new atlas textures while earlier
  frames retain their old textures. CPU glyph scratch storage is reused too.
- At most two command buffers are in flight per window. Temporary drawable
  unavailability uses the existing frame scheduler to retry without a busy loop.
- Destruction waits for queued GPU work and removes the sublayer before the
  native window is destroyed. GL ownership also handles unwinding during context
  recovery without double-destroying the old renderer.

The desktop tests also exposed an existing BSD/macOS IPC race: accepted sockets
could inherit nonblocking mode and discard delayed requests. IPC reads now wait
for a complete newline- or EOF-terminated request, with a one-second total
deadline and 1 MiB size bound so a stalled client cannot indefinitely block the
shared IPC/signal listener. Delayed, already-disconnected, legacy EOF and
stalled-client cases have regression tests.

## Reproduce validation

```sh
cargo test --workspace --locked

# Explicit GPU pixel-readback test; requires an available Metal device.
MTL_DEBUG_LAYER=1 MTL_SHADER_VALIDATION=1 \
  cargo test -p alacritty --locked renderer::metal::tests::gpu_ \
  -- --ignored --nocapture

# Real windows; requires a logged-in macOS desktop. Opens and closes its own
# test windows, retaining logs and a JSON report under target/metal-validation/.
cargo build -p alacritty --locked
python3 scripts/metal-smoke.py
```

The smoke test checks Metal initialization, font/opacity/padding hot reload,
a second Metal window with 10,000 lines of output, simultaneous OpenGL and Metal
windows, font-cache resets, individual window teardown and clean shutdown with
Metal API/GPU validation enabled. A stalled IPC client is also checked alongside
the real CLI's configuration reply. The test fails if a warning, rendering error
or unexpected fallback occurs.

For manual inspection, run with `MTL_DEBUG_LAYER=1 MTL_SHADER_VALIDATION=1` and
exercise native tabs, fullscreen transitions, selection, search, IME composition,
rapid resizing and moving between displays with different scale factors.

## Validation results and remaining coverage

Development validation used an Apple M4 Pro, macOS 27.2.1, Rust 1.98.1 and two
displays reporting scale factor 1. Results:

- Workspace tests passed, including glyph packing, RGB/RGBA upload conversion,
  buffer layouts and the IPC regressions.
- Actual GPU pixel readback verified RGB coverage blending, premultiplied Emoji,
  translucent cell backgrounds and untouched clear pixels. Both production
  pipelines compiled successfully.
- Real windows displayed ASCII, CJK, combining characters, Emoji, hidden text,
  box drawing, ANSI colors and decoration variants. Resize, minimize/restore,
  multiwindow, hot reload and mixed GL/Metal operation were exercised.
- Isolated fault-injection builds used an invalid shader for initialization
  failure and an injected presentation error for runtime failure. Both selected
  OpenGL 3.3 successfully and exited normally. Fault injection is absent from the
  production sources and binary.
- Touched Rust files pass formatting and scoped Clippy adds no warnings.
  Strict Clippy on the installed toolchain still rejects three pre-existing
  `clippy::question_mark` findings in `alacritty_terminal/src/tty/unix.rs`.
  A capped Clippy run (`-- --cap-lints warn`) reports these separately.

A preliminary interleaved comparison used three runs per backend, each writing
18.75 MiB through the PTY at the same 90 × 28 viewport, without GPU validation:

| Measurement (median) | OpenGL GLSL3 | Metal |
| --- | ---: | ---: |
| PTY write throughput | 17.32 MiB/s | 17.27 MiB/s |
| Process CPU time | 1.413 s | 1.412 s |

These results are effectively equal within observed variation. This measures
end-to-end PTY backpressure and processing, not isolated rendering throughput,
input-to-display latency, energy use or final-frame visibility. It does not
establish a Metal speedup.

Other Apple Silicon models and macOS releases, real Retina scale transitions,
wide-gamut display behavior and native tab/fullscreen/IME interactions still
require their own device/OS validation, as do latency and energy
measurements. Metal is the macOS default nonetheless, since initialization and
runtime failures fall back to OpenGL; set `debug.renderer = "glsl3"` if a
configuration misbehaves.

## macOS optimization diagnostics

Opt-in bounded telemetry is enabled with `ALACRITTY_RENDER_METRICS=1`. Use Info
logging (`-v`) and quit the application normally to see per-window JSON summaries
prefixed `Render metrics:` and `Metal metrics:`. Normal operation does not retain
timing samples. Each distribution retains at most the latest 4,096 samples;
counts/mean/max cover the complete window lifetime.

The summaries distinguish CPU frame work, drawable acquisition, CPU encoding,
GPU command execution, buffer allocation/reuse and pipeline cache hits. GPU
timestamps are read only after completion. Input samples cover observed
text/IME events to the next dirty-frame submission. Ignored/non-dirty and
occluded input is discarded. They do not correlate terminal echo with a specific
keystroke and do not measure physical display latency.

Run the repeatable benchmark without GPU validation:

```sh
cargo build -p alacritty --locked
python3 scripts/macos-bench.py --runs 3 --output target/metal-optimization/results.json
```

It alternates OpenGL/Metal, records binary hashes and logs, runs a fixed PTY
workload, measures additional-window initialization and a static nonblinking
window's parent-process CPU time, and exercises font-cache reloads. Idle CPU from
`ps` is quantized to centiseconds; zero does not establish zero wakeups or energy.
Use Instruments/energy measurements and visual/high-speed capture to establish
power and physical input-latency results separately.

The three-run comparison before/after buffer pooling and pipeline sharing used
nine Metal windows in each build, on the same Apple M4 Pro:

| Measurement | Before | After |
| --- | ---: | ---: |
| Glyph-upload GPU buffer allocations (total) | 17,096 | 9 |
| Instance GPU buffer allocations (total) | 244 | 12 |
| Windows reusing shared pipelines | 0 / 9 | 6 / 9 |
| Median Metal PTY throughput | 16.66 MiB/s | 17.10 MiB/s |
| Median OpenGL PTY throughput (control) | 16.60 MiB/s | 17.09 MiB/s |
| Median additional Metal window initialization | 69.45 ms | 69.91 ms |
| Idle parent CPU during each two-second sample | 0.00 s | 0.00 s |

The allocation reduction and cache reuse are directly verified. Both backends'
throughput changed similarly, and window initialization is effectively unchanged;
these runs do not establish a rendering-speed or energy improvement. The raw
local records are `target/metal-optimization/baseline.json` and `optimized.json`.
The buffer-pool GPU test verifies combined upload contents by readback, prevents
uncommitted-frame reuse and checks oversized-buffer eviction under GPU validation.

IME regressions cover UTF-8 selection offsets, wide/combining characters,
composition disable/cancel state, search-footer geometry and physical candidate
rectangles at simulated 1×, 1.5× and 2× font/DPI metrics. Geometry extraction
preserves existing behavior rather than changing native candidate positioning.

For an interactive native regression, on a desktop with enabled Apple Pinyin
and Accessibility authorization:

```sh
python3 scripts/macos-native-smoke.py
```

This helper creates a separate temporary signed test bundle, attempts to focus
only that process, then exercises real Pinyin composition/cancel/commit, search,
resize/font changes, tabs and fullscreen. It restores the original input source
and closes its test windows in cleanup. It refuses to send keys unless it can
confirm focus. **This interactive path has not passed in the development session:**
the session refused foreground activation and its input-source API did not expose
active Pinyin. No test keystrokes were sent. Candidate-popup appearance, real
Retina/display transitions and these interactive native steps still require
validation on a suitable desktop. Automated GPU/multiwindow tests do not stand in
for that coverage.
