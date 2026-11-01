# Benchmarks

How Flick is measured, and what the measurements decided. The images the
benchmarks need by default are committed in `bench/images` (CC0 and public
domain, `bench/images/CREDITS.md`); larger sets are fetched
(`fetch-mixed.ps1`) or generated (`gen-formats.ps1`).

Two tools decide Flick's decoder choice and how much of the cold-start budget
the window itself costs (ARCHITECTURE.md §15, milestone 0).

- `flick-bench` (`crates/flick-bench`): decoder timings and peak memory per
  JPEG, plus a driver for the startup probe.
- `flick-startup-probe` (`crates/flick-startup-probe`): opens an empty
  frameless winit + wgpu window, presents one frame, and reports each stage
  in milliseconds since **process creation** (DLL loading included), plus
  memory at exit.

## Quick check for regressions: `bench/quick.ps1`

One command, about 30 s of measuring (plus the build): 5 cold opens, 5 warm
opens and 20 page turns on `bench/images/pixel-7-2.jpg`, compared with a saved
baseline. It closes any running Flick first and watches the screen, so
keep off the keyboard and mouse while it runs.

```
.\bench\quick.ps1 -Note "what changed"     # run, compare, log
.\bench\quick.ps1 -SetBaseline             # this run becomes the baseline
.\bench\quick.ps1 -NoBuild                 # skip the build
```

- **Table:** each figure's baseline, previous run, now, change, and ok/FAIL.
  A time or memory figure fails above the baseline by more than an
  absolute margin or a fraction, whichever is larger (cold median +15 ms or
  8 %, warm median +15 ms or 10 %, page turn median +8 ms or 25 %, memory
  +8–15 MB or 8–12 %); blank or flicker frames and failed runs fail at any
  count. Exit code 1 on any failure.
- **History:** `bench/results/quick-history.csv`, one line per run (date,
  commit, `+changes` if uncommitted, the note, every figure): what each
  iteration cost. The baseline is `bench/results/quick-baseline.json`; raw
  ext-viewer output stays in `bench/quick-runs/` (git-ignored).
- **Noise:** two back-to-back runs (2026-10-25) agreed within 1 ms on every
  median and 1 MB on memory; worst cases moved up to 15 ms.

## Running

```
cargo build --release
target/release/flick-bench.exe decoders
target/release/flick-bench.exe gen-sample bench/samples
target/release/flick-bench.exe decode <folder-or-files> --json bench/results/<name>.json
target/release/flick-bench.exe startup --runs 10 --backends dx12,vulkan,gl --json bench/results/startup.json
target/release/flick-bench.exe ext-viewer --exe "C:/Program Files/Honeyview/Honeyview.exe" --runs 5 --page 1 --json bench/results/honeyview.json bench/samples
```

`ext-viewer` launches the viewer on your screen and sends it real key presses
(`--page`), so don't type or move windows while it runs. The viewer must not
already be running, because a single-instance hand-off would hide the cold
start.

`decode` options: `--decoders wic,zune-jpeg`, `--modes full,screen,thumb`,
`--runs 5`, `--screen 2560x1440` (default: the primary display's resolution),
`--recursive`, `--limit N`, `--in-process`.

The project must not live under `%APPDATA%`: the Claude desktop app's package
virtualisation hides rustc's output from `link.exe` there and every build
fails with LNK1104.

## What is measured

- **Each measurement is a fresh child process.** Peak memory is the process's
  peak private commit minus the commit just before the decoder was created, so
  it is the decoder's own allocation. The file is memory-mapped, so its pages
  don't count, and the first decode pays the page faults a cold open would
  (the OS file cache is warm; reading from the SSD itself is not measured yet).
- **All decoders produce 4-channel output** (RGBA/BGRA), because that is what a
  GPU upload needs; any RGB-to-RGBA expansion a decoder forces is part of its
  time and memory.
- **Modes:** `full` = native resolution. `screen` = the smallest DCT scale
  (1/2, 1/4, 1/8) whose output still covers the image fitted to the screen;
  `wic-scaler` goes straight to the exact fit size. `thumb` = the EXIF IFD1
  thumbnail.
- **Columns:** `first` is the first decode in the process, `median`/`min` are
  over `--runs`, `init` is decoder construction (WIC: COM + factory).
- **turbojpeg** is behind `--features turbojpeg`. It builds libjpeg-turbo from
  source and needs `cmake` and `nasm` on PATH, which this machine doesn't
  have, so it is untested.

## Results: synthetic samples (2026-10-23)

Machine: Windows 11, RTX 4080, display 3440×1440. Samples are from
`gen-sample`: q90, **4:4:4 chroma, noisy content**, 12/24/45 MP, 7/14/26 MB.
That is heavier per pixel than typical camera JPEGs (usually 4:2:0 or 4:2:2),
so treat absolute times as pessimistic and the decoder ranking as indicative.
Raw data: `bench/results/synthetic.json`.

| decoder | mode | median ms (12 / 24 / 45 MP) | peak MB (24 MP) | output (24 MP) |
|---|---|---|---|---|
| image-rs | full | 104 / 208 / 389 | 161 | 6000×4000 |
| zune-jpeg | full | 102 / 198 / 373 | 92 | 6000×4000 |
| jpeg-decoder | full | 122 / 246 / 448 | 163 | 6000×4000 |
| jpeg-decoder | screen | 97 / 196 / 366 | 42 | 3000×2000 (1/2) |
| **wic** | full | 86 / 170 / 318 | 92 | 6000×4000 |
| **wic** | **screen** | **75 / 145 / 278** | **23** | 3000×2000 (1/2) |
| wic-scaler | screen | 82 / 156 / 289 | 13 | 2160×1440 |
| exif-thumb | thumb | 0.1 / 0.1 / 0.1 | 0 | 160×106 |

## Results: startup probe (2026-10-23)

Empty frameless window, hidden until the first present, 10 runs per backend,
milliseconds since process creation (medians). Raw data:
`bench/results/startup.json`.

| stage | DX12 | Vulkan | GL |
|---|---|---|---|
| `main` entered | 15 | 15 | 16 |
| window created | 29 | 28 | 28 |
| instance | 43 | 152 | 158 |
| adapter | **224** | 161 | 158 |
| device | 257 | 186 | 159 |
| surface configured | 260 | **359** | 163 |
| first present | 262 | 359 | 169 |
| shown + second present | 275 | 372 | 182 |
| private commit at exit | **319 MB** | 334 MB | 94 MB |
| working set at exit | 146 MB | 121 MB | 68 MB |

## Results: Honeyview 5.53 on the same samples (2026-10-23)

`flick-bench ext-viewer` launches a viewer as `<exe> <file>` and watches the
screen. It samples the centre half of the viewer's window (shrunk to 128×128)
about every 7 ms, so times are ±7 ms. All times are measured from the viewer's
process creation, the same clock as the startup probe:

- `window`: its first visible top-level window appears.
- `image`: the first frame that already looks like the final picture. A preview
  counts. Before the viewer paints, the screen DC still shows whatever was
  behind the window, so "something not flat" is not enough.
- `final`: the last change before the frame holds still for 700 ms, for example
  a refinement pass after the first draw.

`--dump <dir>` saves every distinct frame, which is how these definitions were
checked. Memory is read from Honeyview's process once the image has settled.
Its window opens at 3377×1440 (it remembers its last size). Each file got 5
cold launches. Raw data: `bench/results/honeyview.json`.

| | 12 MP | 24 MP | 45 MP |
|---|---|---|---|
| window visible | 94 ms | 94 ms | 92 ms |
| image on screen | 282 ms | 333 ms | 327 ms |
| final | 282 ms | 472 ms | 571 ms |
| private commit after open | 114 MB | 159 MB | 110 MB |
| working set after open | 135 MB | 182 MB | 133 MB |

Page turns (→ key, measured once per file after the image settled):

| | image | final |
|---|---|---|
| 12 → 24 MP | 175 ms | 320 ms |
| 24 → 45 MP | 21 ms (already preloaded) | 418 ms |

What the frame dumps and numbers suggest about Honeyview. These are inferences,
not documented behaviour:

- **It paints a dark background about 60 ms after the window appears, then the
  image about 230 ms later.** The 45 MP image appears as early as the 24 MP one,
  so the first draw is a reduced decode. The refinement that follows on 24 and
  45 MP is not visible at 128×128 sampling (likely a higher-quality resample).
- **It keeps about one RGBA copy at the decoded scale, on top of a ~65 MB base.**
  12 MP full size is 48 MB (114 MB total); 24 MP full size is 96 MB (159 MB);
  45 MP comes in *below* 24 MP (110 MB), which fits a 1/2-scale decode (45 MB).
- **It preloads the next file**, but the 12 → 24 MP turn wasn't ready even
  700 ms after the first image settled.
- **The "500 MB per image" complaint doesn't reproduce with one cold open of
  these files.** Real photos, several windows, or memory growing while paging
  are the next things to test.

## Honeyview vs. Flick's parts

Flick has no viewer yet, so this compares Honeyview end to end with the sum of
Flick's measured parts. These are projections, not measurements; milestone 1
has to confirm them. Times are for the 24 MP sample, from process creation.

| | Honeyview | Flick projection (DX12 wgpu) | Flick projection (light first frame) |
|---|---|---|---|
| window | 94 ms (visible) | 29 ms (created hidden) | 29 ms |
| first picture | 333 ms | ~265 ms: wgpu is ready at 262 ms, the WIC screen decode at ~165 ms | **~35 ms** if a GDI/D2D path can draw the 0.1 ms EXIF thumbnail (unmeasured) |
| full screen-quality picture | 472 ms | ~265 ms (screen decode is already final quality at fit) | ~170 ms (WIC screen decode, 1/2 scale) |
| memory | 159 MB private | ≥ 340 MB (319 MB empty DX12 window + 23 MB screen level) | depends on the renderer: GL was 94 MB empty |

So Flick's decode pipeline already beats Honeyview's time to the final picture,
even with wgpu's slow start. The remaining gaps are the **first** picture, which
needs the light first-frame path to win clearly, and **memory**, where wgpu on
DX12 alone costs twice Honeyview's total.

## Results: real "burst" set (2026-10-23)

22 real camera JPEGs from one burst (`bench/photos/burst`, git-ignored):
17.2 MP (4800×3584), 7.5–9.4 MB, baseline 4:2:0, **no EXIF thumbnail, no
restart markers**. Raw data: `bench/results/burst.json`.

| decoder | full | screen (1/2) | preview (1/8) | peak MB (full / screen) |
|---|---|---|---|---|
| image-rs | 114 ms | – | – | 116 |
| zune-jpeg | 109 ms | – | – | 67 |
| jpeg-decoder | 121 ms | 94 ms | 78 ms | 118 / 31 |
| **wic** | 86 ms | **74 ms** | 60 ms | 66 / **17** |
| wic-scaler | – | 84 ms | – | 11 |

On these files the first pixel can't come from an EXIF thumbnail, and a 1/8
decode saves only 14 ms over the screen decode. So the first picture *is* the
WIC screen decode, at ~75 ms.

### Honeyview vs. Flick on the burst set

Honeyview 5.53, via `ext-viewer`: 5 cold opens of the first file, then 9 page
turns (→) in one window, with memory read after each turn. The 10th turn timed
out, probably at the end of Honeyview's own folder order. Raw data:
`bench/results/honeyview-burst.json`.

| page turn | 1 | 2 | 3 | 4 | 5–9 |
|---|---|---|---|---|---|
| time to new picture | 140 ms | 139 ms | 133 ms | 147 ms | 138–154 ms |
| Honeyview private memory | 198 MB | 264 MB | 330 MB | 396 MB | 396 MB |

**Honeyview keeps about five full-resolution decodes.** Each page turn adds
66 MB, exactly one 4800×3584 RGBA image, until it plateaus at 396 MB. With
24 MP files the same cache would be about 550 MB, which matches the
"500 MB" Honeyview is known to reach. It never had the next image preloaded: each turn took about
140 ms even after the previous one had settled for 0.7 s.

Flick still has no working viewer, so its column is projected from measured
parts: WIC screen decode 74 ms (+2.5 ms init), the startup probe's window and
renderer times, and screen-size levels of 17 MB each.

| | Honeyview (measured) | Flick, wgpu DX12 | Flick, light first frame |
|---|---|---|---|
| cold open → first picture | 303 ms | ~265 ms (waits for wgpu) | **~95 ms** (decode done at ~92 ms) |
| page turn, not preloaded | ~145 ms | ~80 ms (decode + upload) | ~80 ms |
| page turn, preloaded | never observed | < 1 frame | < 1 frame |
| memory after open | 133 MB | ~340 MB (319 MB is the empty DX12 window) | renderer + 17 MB |
| memory after 5 images | 396 MB | renderer + ~85 MB (5 screen levels) | renderer + ~85 MB |

So for real camera files the decode and cache design beats Honeyview on
every count. The renderer decides the rest: wgpu on DX12 would give away the
cold-start lead and most of the memory win.

### Renderers: time to the real picture (burst file, measured)

`flick-startup-probe --image` starts the WIC screen-scale decode on a worker
thread at `main`. Meanwhile it creates the window hidden and initialises one
renderer. It shows the window only when it can draw the decoded picture
fitted at final quality: no placeholder frames. It was measured two ways on
the same burst file at Honeyview's window size (3377×1440):

- **Inside:** `flick-bench startup --image ... --size 3377x1440`, 10 runs.
  Raw data: `bench/results/startup-burst.json`.
- **Outside, exactly like Honeyview:** `ext-viewer --exe flick-startup-probe.exe
  --args "--backend X --stay --size 3377x1440 --out NUL --image {file}"`,
  5 runs. Raw data: `bench/results/probe-*-burst.json`.

| renderer | renderer ready (inside) | frame drawn (inside) | **final picture on screen (outside)** | private MB |
|---|---|---|---|---|
| **GDI `StretchDIBits`** | 32 ms | 142 ms | **191 ms** | **20** |
| wgpu GL | 174 ms | 191 ms | 206 ms | 209 |
| Direct2D on D3D11 swap chain | 207 ms | 227 ms | 240 ms | 118 |
| Direct2D `HwndRenderTarget` | 205 ms | 226 ms | (not run outside) | 115 |
| wgpu DX12 | 271 ms | 288 ms | 296 ms | 338 |
| wgpu Vulkan | 367 ms | 381 ms | (not run outside) | 348 |
| *Honeyview 5.53* | | | *303 ms* | *133* |

- **The GPU driver is the floor, not wgpu.** A bare `D3D11CreateDevice`
  takes ~170 ms on this machine (NVIDIA RTX 4080), and Direct2D creates the
  same device underneath. Every GPU path pays this before its first frame.
  wgpu DX12 adds another ~70 ms on top, and Vulkan more.
- **GDI needs no device**, so its frame waits only on the decode (~110 ms).
  Its 32 ms of drawing is CPU `HALFTONE` scaling of 2400×1792 → 1929×1440.
  Decoding straight to the exact fit size (`wic-scaler`, ~10 ms slower to
  decode) would make that a 1:1 blit of a few ms.
- **The GPU driver also costs memory:** about 100 MB of private commit
  appears as soon as a D3D11 device exists, against 20 MB for the whole GDI
  process with its picture.
- **Window position doesn't change the result.** The first outside runs
  used Windows' default cascade, which pushed some probe windows partly
  below the screen edge, inside the sampled region. Re-running with the
  window pinned fully on-screen at (0, 0), now the probe's default, gave the
  same medians within 1 ms: GDI 191, wgpu GL 206, Direct2D on D3D11 240,
  wgpu DX12 296 ms. The table above shows the pinned runs.
- **DWM's window-open animation adds ~170 ms** before a new window's content
  shows at full opacity. With it on, the GDI probe's picture settled at
  361 ms instead of 190 ms. The probe now disables it
  (`DWMWA_TRANSITIONS_FORCEDISABLED`); `--animate` restores it. Honeyview
  hides the animation by showing an empty dark window first. A mixed set fetched from Wikimedia Commons
(`fetch-mixed.ps1`, manifest `mixed-sources.json`) is defined but was left
unfinished (9 of 22 files) because of Commons rate limits; it isn't needed for
the decisions above.

### Handover: GDI first frame, then the GPU takes the window (2026-10-23)

The question: can GDI show the first picture (fast, no driver start) and a
Direct3D 11 + Direct2D renderer then take over the same window, for zoom and
pan, without the user ever seeing a non-final frame?

Probe backends (`flick-startup-probe --backend ...`):

- `gdi+hwnd`: GDI draws the picture. A background thread, started at
  process start, creates the D3D11 + D2D device. The GPU renderer then
  creates a flip-model swap chain on the window (`CreateSwapChainForHwnd`)
  and presents the same picture.
- `gdi+dcomp`: the same, but the swap chain is a DirectComposition visual
  above the window's GDI content and goes live with `Commit()` after its
  first `Present`.

`ext-viewer` gained two checks:
- **flicker:** any frame after the picture first appeared that doesn't match
  the final picture;
- **blank:** how long a flat (black or white) window was visible before the
  picture.

The display runs at 144 Hz, so a one-frame flash lasts about 7 ms, the same as
one sampling interval. Each variant ran 20 times, and every distinct frame was
saved (`--dump`) and spot-checked by eye. Raw data:
`bench/results/handover-*.json`.

| variant | runs | blank before picture | flicker after | picture on screen | GPU took over | private MB |
|---|---|---|---|---|---|---|
| GDI drawing straight into the window | 10 | **10/10, ~50 ms black** | 0 | 186 ms | – | 20 |
| same, window cloaked until drawn | 10 | 10/10, ~25 ms | 0 | 181 ms | – | 20 |
| **GDI composed off-screen, one blit** | 10 | **0** | 0 | **171 ms** | – | 20 |
| same, cloaked | 10 | 0 | 0 | 179 ms | – | 20 |
| **buffered GDI → swap chain on window** | 20 | **0** | **0** | **169 ms** | 219 ms | 97 |
| **buffered GDI → DirectComposition** | 20 | **0** | **0** | **170 ms** | 220 ms | 97 |
| Direct2D only (for comparison) | 10 | 0 | 0 | 241 ms | – | 118 |

- **The handover is invisible with both attach methods.** In 40 runs, no
  frame between the GDI picture and the GPU picture failed to match. The GPU
  frame differs only by scaling filter (HALFTONE vs. linear), well under the
  "same picture" threshold.
- **The real flicker was in the first GDI frame, not the handover.** Drawing
  into a visible window (clear to black, then a 32 ms `StretchDIBits`) let
  DWM compose the half-drawn state, so every open showed ~50 ms of black.
  Composing the frame in an off-screen bitmap before showing the window, then
  copying it with one `BitBlt`, removed the black entirely and brought the
  picture 15 ms earlier. Cloaking then adds nothing; it only delays
  the picture by ~8 ms.
- **Starting the GPU device in the background doesn't delay the first
  picture** (169–170 ms with the handover, 171 ms without). The device still
  takes ~170 ms of driver start, so it is ready ~50 ms after the picture is
  up.
- **The hybrid shows the picture 70 ms sooner than Direct2D alone** (170 vs.
  241 ms) and ends in the same state. Memory reaches ~97 MB once the GPU
  exists, against 20 MB for GDI alone.
- **The 718 ms outlier** in an earlier run was the GPU device starting late;
  the GDI picture was on screen throughout. One run where the first picture
  was seen only at 309 ms was a ~200 ms stall in the measuring process: its
  first sample came at 309 ms.

Not tested: creating the GPU device lazily, on the first zoom instead of at
startup. That would keep an idle fit-view window at ~20 MB, at the cost of
~170 ms on the first zoom.

## Milestone 1: `flick.exe` end to end (2026-10-23)

The real viewer (`crates/flick-app`), measured exactly like Honeyview:
`ext-viewer --exe target/release/flick.exe --args "--size 3377x1440 {file}"`,
10 cold launches on the same burst file. Raw data:
`bench/results/m1-flick-burst.json`.

| | flick.exe (cold) | Honeyview 5.53 |
|---|---|---|
| final picture on screen | **234 ms** (227–263) | 303 ms |
| blank / flicker frames | 0 / 0 in 10 runs | dark window for ~200 ms |
| private commit after open | **91 MB** | 133 MB |
| working set after open | **61 MB** | 153 MB |

- **Within budget:** cold open ≤ 300 ms hard and < 250 ms target
  (ARCHITECTURE.md §3a).
- **91 MB, not the probe's 118 MB:** the window has no GDI redirection
  surface (`WS_EX_NOREDIRECTIONBITMAP`); DirectComposition draws everything.
- **Full resolution is decoded only when a zoom needs it.** Decoding it
  right after the picture (the first version) raised memory after open to
  161 MB private, with a 227 MB peak. With the full-resolution texture
  loaded, the process sits at ~168 MB private but only ~77 MB working set:
  the driver reserves far more commit than it touches. To look into with
  the Full level in milestone 4.
- **With the hover bar** (top bar, Honeyview-style): open time unchanged
  (238 ms, 5 runs, 0 blank/flicker; `bench/results/m1-flick-burst-chrome.json`).
  Idle CPU over 5 s: 0.0 ms with the cursor away, on the picture, or on the
  visible bar. One earlier run showed 94 ms, most likely DirectWrite's
  one-time font loading on the first hover.
- Checked by screenshot: a frameless window sized to the picture's aspect
  (1735×1296 for 4800×3584 at default size), centred; zoom with keys; the
  full-resolution level replaces the screen level after zooming; F12
  overlay with the open timeline. Mouse wheel zoom, drag to pan, window
  move/resize at the edges, Aero Snap and double-click are implemented but
  not checked here; they need a manual test.

## Milestone 2: paging through the burst folder (2026-10-24)

`flick.exe` lists the folder after the first picture and keeps a ring of
screen levels (current, +1, −1, +2, +3) as GPU textures, decoded by a 3-thread
pool whose queue is replaced on every turn. Measured like Honeyview:
`ext-viewer --exe target/release/flick.exe --args "--size 3377x1440 {file}"
--runs 5 --page 21` on the burst folder. Raw data:
`bench/results/m2-flick-burst-paging.json`.

| | flick.exe | Honeyview 5.53 |
|---|---|---|
| cold open | 249 ms (232–284), 0 blank/flicker | 303 ms |
| page turn, key → picture on screen | **7–90 ms**, median ~42 ms | ~140 ms |
| private commit while paging | 144 → **flat at ~200 MB** after 3 turns | 133 → 396 MB (+66 MB per turn) |
| working set while paging | **flat at ~70 MB** | 153 → 416 MB |

- **Memory is flat by construction:** 5 screen levels (17 MB each on the
  GPU), whatever the folder size. Private commit counts the driver's reserve
  for those textures; the working set barely moves.
- **Every turn was a preloaded texture swap.** Flick's own turn log
  (`--timeline-out`, per-turn timestamps) shows all 21 turns "preloaded", with
  0.4–0.8 ms from key to `Present`.
- **Where the remaining 30–90 ms goes**, with both processes stamping a
  system-wide clock (`GetSystemTimePreciseAsFileTime`):

  | stage | time |
  |---|---|
  | key sent → Flick handles it | 0.6–1.2 ms |
  | Flick swaps texture, presents | 0.4–0.8 ms |
  | GPU finishes the frame (`FLICK_GPU_SYNC=1`) | 0.4–20 ms |
  | **GPU done → visible on screen** | **22–78 ms** (6 ms on the first turn) |

  Almost all of it happens after Flick's frame is finished, in composition
  or the display path. Honeyview's page turns go through the same stage.
- **Present settings tried (one 21-turn pass each), with no effect:**

  | variant | Flick → GPU done | GPU done → on screen | key → on screen (median / max) | compositor rate |
  |---|---|---|---|---|
  | baseline: `Present(1)`, default queue | 3.9 ms | 36 ms | 42 / 83 ms | 144 Hz |
  | `Present(0)` (no vsync wait) | 10.7 ms | 36 ms | 42 / 70 ms | 144 Hz |
  | max frame latency 1 | 9.0 ms | 38 ms | 49 / 83 ms | 144 Hz |
  | both | 5.5 ms | 33 ms | 42 / 90 ms | 144 Hz |

  Linear instead of high-quality cubic filtering changed nothing either.
  The switches were removed again. The compositor reported a steady 144 Hz
  on every turn (`IDCompositionDevice::GetFrameStatistics`): it does not drop
  its rate while idle, so variable refresh (G-SYNC on this machine) does not
  show up at that level. Still open: whether the ~35 ms after composition is
  real display latency or lag in `ext-viewer`'s screen reading (occasional
  6–8 ms turns show it can be fast). DXGI Desktop Duplication timestamps
  would tell.
- **Preloading now waits for the first picture to be on screen:** a
  `DwmFlush` after showing the window, then the folder listing, then the
  pool, whose threads run below normal priority. The cold open measured
  248 ms median over 10 runs (235–256) before and after this change. Its
  internal timeline matches milestone 1 stage for stage (decoded 113 ms,
  device 199, uploaded 216, shown 236, on screen 250), so the earlier
  ~10 ms difference to milestone 1 is run-to-run variance on a machine in
  use, not the preload.

## Milestone 3a: resident process, warm opens (2026-10-24)

The first Flick in a session stays resident (tray icon) and keeps its GPU
device. Later launches hand the file over a named pipe (and their
foreground right via `AllowSetForegroundWindow`) and exit. Measured with the
new `ext-viewer --resident`: it starts Flick once, closes that window, then
times each launcher from *its* process creation until the resident
process's new window shows the final picture. Burst file, `--size
3377x1440`. Raw data: `bench/results/m3-*.json`.

| | time to final picture | blank / flicker | notes |
|---|---|---|---|
| cold, `--standalone` (5 runs) | 241 ms | 0 / 0 | unchanged from milestone 2 |
| **warm** (10 runs) | **120 ms** (116–154) | 0 / 0 | target < 120, budget ≤ 150 |
| warm after the 30 s memory trim (3 runs) | 137 ms | 0 / 0 | +~15 ms while trimmed pages come back |
| *Honeyview, every open* | *303 ms* | dark window | |

Resident memory (one window opened and closed):

| | private | working set |
|---|---|---|
| with the window open | 149 MB | 71 MB |
| 2 s after closing | 149 MB | 71 MB |
| after the 30 s trim (`EmptyWorkingSet`) | 149 MB | **~0 MB** |

Idle CPU while resident: 0.0 ms over 5 s.

- **The first version leaked GPU memory across windows:** 10 warm opens took
  private commit from 166 to 189 MB and the working set from 72 to 95 MB,
  still climbing. D3D11 destroys released objects (closed windows' swap
  chains, evicted textures) only when its immediate context is flushed, and
  nothing flushes it while no window draws. Direct2D also keeps scratch
  textures. Closing a window now calls `ID2D1Device::ClearResources` and
  `ClearState` + `Flush`. Memory then levels off after ~5 opens (private
  145 → 158 MB, working set 68 → 82 MB, flat for the last 5 of 10).
- The private commit (~149 MB) is mostly the graphics driver's reservation
  for the device and is not trimmed. The working set, i.e. RAM actually in
  use, drops to ~0 after the trim.

## Baseline before milestone 3b (2026-10-24)

The milestone 2/3a runs again after the features added since (app icon,
elevation hand-off, View menu and settings, render filters, sorting, random
paging, animated GIF, held-key paging), same burst file, `--size 3377x1440`,
`ext-viewer` as above. Benchmark runs use default settings, not the saved
ones. Raw data: `bench/results/pre3b-*.json`.

| | milestone 3a | now | budget |
|---|---|---|---|
| cold open, `--standalone` (10 runs) | 241 ms | **220 ms** (213–259) | ≤ 300 |
| warm open (10 runs) | 120 ms | **116 ms** (112–132) | ≤ 150 |
| warm open after the 30 s trim (3 + 3 runs) | 137 ms | **135–176 ms**, see below | ≤ 150 |
| page turn, key → screen (median / p90 / max) | 50 / 83 / 90 ms | **42 / 56 / 85 ms** | |
| random page turn (↓, new) | | **42 / 56 / 88 ms** | |
| blank / flicker frames | 0 | **0 in every run** | 0 |
| resident idle CPU over 5 s | 0.0 ms | **0.0 ms** | 0 |
| resident private / working set, window closed | 149 / 71 MB | 165 / 71 MB | |
| working set after the trim | ~0 MB | **0 MB** | |

- **Warm open after the trim varies between sessions:** 170 / 168 / 176 ms
  in the first run, 159 / 135 / 132 ms in a second one minutes later
  (milestone 3a: 133–138). The window appears on time in all of them
  (105–110 ms). Flick's own timeline (`--timeline-out`) puts the extra
  time after decoding, which takes the same ~85 ms trimmed or not: the
  upload and first draw take ~15 ms instead of ~9, and the wait for the
  compositor to show the first frame (`DwmFlush`) 28 ms instead of 9, i.e.
  driver and compositor memory coming back in. Not clearly caused by the
  new features; to settle with an A/B against the 3a build if it matters.
- **Random paging levels off higher:** private commit 290 → 303 MB and
  working set 81 → 94 MB over 80 turns (`pre3b-random-long.json`), flat for
  the last 30. Paging in order sits at 199 / 72 MB. A random step replaces
  more of the ring than an in-order one, and the ring keeps 7 screen
  levels instead of 5 while paging randomly. Not a leak.
- **Private commit while resident** is 16 MB above milestone 3a, all in
  the graphics driver's reserve (the working set is unchanged).

## Milestone 3b: 20 tabs in one window (2026-10-24, first cut)

Flick started on the first burst file, then 19 more opened the normal way
(each a warm open, so each became the first tab of that window), then
Ctrl+Tab through all 20. Memory from the process and the GPU's per-process
counters (`Win32_PerfFormattedData_GPUPerformanceCounters_GPUProcessMemory`);
times from `FLICK_TRACE`. Script: one run, by hand-off from a scratch script.

| | 1 tab | 20 tabs | after Ctrl+Tab through all 20 |
|---|---|---|---|
| working set (RAM in use) | 71 MB | **99 MB** | 100 MB |
| private commit | 166 MB | 473 MB | 589 MB |
| GPU dedicated | 114 MB | 393 MB | 509 MB |

| | |
|---|---|
| switch to a warm tab, key → frame presented | **0.60 ms median, 0.79 ms max** (20 switches) |
| open into a tab, request → on screen | **83–93 ms** (19 opens; a warm open into a new window: ~102 ms by the same measure) |

- **RAM:** 20 tabs use 99 MB working set against Honeyview's 153–416 MB
  for one window. **Commit and GPU memory** grow ~17 MB per warm tab (its
  2400×1792 screen level stays on the GPU so switching back is a texture
  swap), plus the active tab's preload ring wherever it is; after cycling
  through all 20, 589 MB private / 509 MB on the GPU.
- Tabs left unviewed for 3 minutes drop their picture (cold): switching to
  one then decodes again (~85 ms), like opening it.
- **Eviction measured** with `FLICK_COOL_AFTER_S=20` (same 20 tabs): every
  inactive tab cooled on schedule, and cooling now flushes Direct3D
  (`release_unused`), which otherwise frees released textures only on its
  next flush:

  | | private | working set | GPU dedicated |
  |---|---|---|---|
  | 20 tabs, warm | 491 MB | 99 MB | 410 MB |
  | 20 tabs, 19 cooled | **252 MB** | 99 MB | **172 MB** |
  | after 5 switches to cooled tabs | 277 MB | 99 MB | 147 MB |

  What remains is one window's worth: the active tab and its preload ring.
  The 5 switches were still 0.5–0.7 ms, because the cooled tabs' pictures
  were in the active tab's ring (all 20 tabs are neighbours in one folder);
  a cooled tab from another folder decodes again on return.

## After milestone 3b: the overlay finding (2026-10-25)

The baseline runs again after tabs, multi-row strip, Tab Lock and the
black-on-resize fix (premultiplied swap chain, so DWM composes the window
instead of putting it on a hardware overlay plane). Same burst file,
`--size 3377x1440`. Raw data: `bench/results/post3b-*.json`.

| | before 3b | now | budget |
|---|---|---|---|
| cold open, `--standalone` (10 runs) | 220 ms | **212 ms** (206–266) | ≤ 300 |
| warm open (10 runs, see below) | 116 ms | **115–133 ms** | ≤ 150 |
| page turn, key → screen (median / p90 / max) | 42 / 56 / 85 ms | **16 / 25 / 28 ms** | |
| blank / flicker frames | 0 | **0 in every run** | 0 |

- **Page turns are 2.5× faster, because of the overlay.** The unexplained
  22–78 ms between "GPU done" and "visible" in milestone 2 was the overlay
  path. A/B in one session, 20 turns each, twice: opaque swap chain (as
  before) 56 / 50 ms median, p90 90 / 77 ms; premultiplied 16 / 16 ms, p90
  25 / 28 ms (`post3b-paging-opaque.json`, `-premultiplied.json`).
- **Warm opens: no regression in Flick, a slower machine today.** The first
  runs gave 132 ms median, with memory creeping up over the 10 opens. The
  pre-3b commit, built and run alternately in the same session, gave 121 /
  126 ms and the same memory creep (working set 91–94 MB); the current build
  115 / 117 ms (`post3b-warm-pre3b-build.json`, `-current-build.json`).
  Flick's own timeline is steady (decode ~83, upload 4, draw 8 ms); what
  varies is the compositor showing the first frame (`DwmFlush`, 2–54 ms).
  Premultiplied vs. opaque on warm opens: 131–133 vs. 121–125 ms in one
  A/B, within the noise of the build A/B above.

## Milestone 7: formats (2026-10-25)

`bench/gen-formats.ps1` writes one sample per format into
`bench/samples/formats` (ffmpeg; `flick-bench gen-formats` for JPEG XL,
lossless, and HEIC through WIC's HEIF encoder). Decode times in-process
(`cargo test --release -p flick-app formats_report -- --ignored
--nocapture`, 3377×1440 target), then cold opens like every other run
(`ext-viewer --standalone`, 2 runs each). Raw data:
`bench/results/m7-formats-cold.json`.

| sample | size | Screen level | screen decode | cold open → final picture |
|---|---|---|---|---|
| JPEG (burst, for reference) | 4800×3584 | 2400×1792 (DCT ½) | ~75 ms | 212 ms |
| PNG photo | 4032×3024 | 1920×1440 (Fant) | 183 ms | 240 ms |
| PNG panorama | 20000×1200 | 3377×203 | 55 ms | 202 ms (failed before) |
| PNG 16-bit + alpha | 1600×1200 | full | 11 ms | 212 ms |
| TIFF 16-bit | 3000×2250 | 1920×1440 | 34 ms | 212 ms |
| WebP lossy | 4032×3024 | 1920×1440 | 264 ms | 360 ms |
| WebP lossless + alpha | 1600×1200 | full | 29 ms | 226 ms |
| HEIC | 4032×3024 | 1920×1440 | 220 ms | 437 ms |
| AVIF | 4032×3024 | 1920×1440 | 153 ms | 263 ms |
| JPEG XL (lossless photo) | 4032×3024 | 1920×1440 | 268 ms | 324 ms |
| SVG | 800×600 → 4096×3072 | 1920×1440 | 295 ms first (system fonts), then ~60 | 219 ms |
| OpenEXR (values to 4.0) | 1600×1200 | full, tone-mapped | 98 ms | 215 ms |
| Radiance HDR | 1600×1200 | full, tone-mapped | 65 ms | 208 ms |
| BMP | 1600×1200 | full | 4 ms | 205 ms |
| GIF / APNG / animated WebP | 640×480, 24 frames | first frame | 3–5 ms | plays (95 screen changes in 8 s) |

- **0 blank and 0 flicker frames** in all 32 cold opens of the 16 stills.
- **The cold JPEG open is unchanged** at 212 ms (10 runs,
  `m7-cold-jpeg.json`) with three more decoder crates linked in (the exe
  is 7.4 MB). None of them does anything until a file of its format opens.
- **Memory after open** in the table's run reflects the neighbours being
  preloaded (this folder mixes formats). Each file alone in its folder:
  71–76 MB private, except HEIC, which shows ~170 MB for about 3 s after
  decoding (Windows' HEVC decoders, one per decoding thread) and then
  settles at 106 MB private / 74 MB working set.
- **Paging through all 16 stills in one window** (`m7-formats-paging.json`):
  every turn a preloaded swap, 8–17 ms key → screen; working set ~75 MB,
  briefly ~160 MB while the HEIC neighbours were decoding.
- **Slowest: HEIC and lossy WebP** (WIC's decoders, ~220–265 ms for 12 MP).
  JPEG XL lossless is 268 ms; typical lossy (VarDCT) files decode faster.

## Disk cache of screen levels (2026-10-24)

`flick-bench disk-cache --screen 3377x1440 bench/photos/burst`: decode each
burst file's screen level with WIC, write it raw and LZ4-compressed, flush
everything to the SSD, then read it back warm (OS file cache) and cold
(`FILE_FLAG_NO_BUFFERING`). GPU upload (a few ms) is not included.

| per screen level (2400×1792, 17.2 MB) | median |
|---|---|
| WIC decode of the JPEG | 75.4 ms |
| **raw BGRA, cold read** | **6.1 ms** (~2.8 GB/s) |
| raw BGRA, warm read | 4.6 ms |
| raw BGRA, write | 4.5 ms |
| LZ4 (13.5 MB), cold read + decompress | 11.8 ms |
| LZ4 compress | 29.7 ms |

- **A raw disk cache is ~12× faster than decoding again.** That makes a
  revisit of a cached image (warm open, paging back, a tab restored, a jump
  past the ring) roughly a 10 ms job instead of 80 ms.
- **LZ4 is not worth it** for photos: 22 % smaller, but twice the read time
  and 30 ms to compress.
- "Cold" here means flushed to the SSD just before, so the drive's own cache
  may still help. Reads after a reboot will be slower, but are not expected
  to come near the decode time.
- The first version read while Windows was still flushing the writes and
  measured ~150 ms; the benchmark now flushes first.
- 1 GB holds ~58 screen levels at this size.

## Findings

1. **The first pixel is a presentation problem, not a decode problem.** The
   EXIF thumbnail decodes in 0.1 ms, and the window exists at ~29 ms. The
   design's 80 ms first-pixel target is reachable only if something cheap can
   draw that thumbnail into the window.
2. **wgpu blows the cold-start budget on this machine.** Even the fastest
   backend (GL) takes ~170 ms to its first present; DX12, the natural Windows
   choice, takes ~260 ms, most of it in the adapter request. wgpu must come off
   the first-pixel path (ARCHITECTURE.md §16 is updated).
3. **wgpu also threatens the memory goal.** DX12 commits ~320 MB for an empty
   window (working set 146 MB) against a 100 MB baseline target. Part of that
   is likely driver reservation rather than touched memory; not yet broken
   down.
4. **WIC is the fastest JPEG decoder available here**, ~25 % faster than
   zune-jpeg and image-rs at full size, with 2.5 ms one-time init. That is in
   line with the plan (WIC for the long tail, a fast dedicated JPEG decoder for
   the hot path), except that so far WIC *is* the fastest dedicated path:
   turbojpeg is still unmeasured.
5. **DCT scaling saves memory far more than time on these files.** 1/2-scale
   decoding is only 10–15 % faster than full, but cuts peak memory ~4× (WIC:
   92 → 23 MB at 24 MP). On noisy 4:4:4 content entropy decoding, which scaling
   can't skip, dominates. Real camera files should show a larger time saving;
   unverified.
6. **On a 3440×1440 screen only 1/2 scaling qualifies** for 24–45 MP images:
   1/4 of 6000×4000 (1500×1000) no longer covers the fit size (2160×1440).
   The "decode at 1/4 or 1/8" path mostly helps smaller displays or preview
   tiles.
7. **image-rs doubles peak memory** relative to zune-jpeg (which it uses
   internally) because `into_rgba8` makes a second full-size copy.
8. **Screen-ready on a cold open is ~150 ms (24 MP) and ~280 ms (45 MP) with
   WIC** on these pessimistic samples: within the 200 ms target at 24 MP, not
   at 45 MP. The thumbnail covers the gap visually.
9. **Honeyview's baseline to beat** on these samples: first picture ~330 ms,
   final picture 470–570 ms, ~110–160 MB private memory with one image open,
   and page turns of ~20 ms when preloaded or ~175 ms when not.

## Not done yet

(Real camera files, Honeyview's memory while paging and first-frame paths
without wgpu are measured in the sections above.)

- Install `cmake` and `nasm`, then measure turbojpeg (scaled decode is its
  strength).
- Disk reads after a reboot, with the drive's own cache cold too.
