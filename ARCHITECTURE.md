# Flick — architecture

Flick is a Honeyview-class image viewer in Rust: instant to open,
frameless, tabbed, fast to page through large JPEGs, cheap in RAM however
many images are open. It runs on Windows; the design (one resident process,
screen-sized levels, a preload ring) does not depend on Windows, but every
platform call does (§2, principle 10).

Measured numbers below are on Windows 11, an RTX 4080 at 144 Hz and a
3440×1440 display, with 17 MP camera JPEGs unless stated otherwise. Method
and raw data: `bench/README.md`.

## 1. Goals and non-goals

Goals, in priority order. When two conflict, the higher one wins.

1. **Open speed.** Double-click a JPEG in Explorer and see the final
   picture; nothing on screen before it (no placeholder, thumbnail or blank
   window). The process is normally resident (§3), so the usual open is
   warm. Targets (17–24 MP JPEG, SSD, warm OS file cache): warm open
   < 120 ms, the decode plus one present; cold open (first after login or
   after quitting from the tray) < 250 ms. Hard budgets in §3a. Honeyview
   takes 303 ms.
2. **Paging speed.** Mouse-wheel through a folder of 24–45 MP JPEGs with no
   visible wait. Target: next image on screen in < 50 ms when preloaded,
   < 150 ms when not.
3. **Memory.** Baseline process < 100 MB working set. One open 24 MP image
   at fit adds ~35 MB. Twenty open tabs across several windows cost less
   than one Honeyview window, because inactive tabs give up their pictures
   (§8).
4. **Idle cost.** Any number of idle windows and tabs: 0 % CPU, no timers,
   no redraws.
5. **Frameless, tabbed window.** No title bar or border; move, resize and
   snap still work. All chrome, including the tab strip, is an overlay that
   appears on hover and fades. Tabs must not cost anything in goals 1–4; if
   they do, the feature shrinks, not the goals.
6. **Quick favourites.** Star a picture in one keystroke, with no dialog and
   without modifying the file (§11).
7. **Formats.** JPEG first-class; PNG, WebP, JPEG XL, HEIC, AVIF, TIFF,
   GIF/APNG/animated WebP, SVG, camera RAW, OpenEXR/Radiance HDR (§7).

Non-goals: editing pixels, a library/catalog database beyond stars, a
thumbnail browser grid, printing, colour management beyond conversion to
sRGB, plugins loaded at runtime.

Where it stands (`bench/quick.ps1` and the milestone runs, §15): cold open
212 ms, warm open 115–133 ms, page turn 16 ms median key → screen, 0 blank
or flicker frames; 20 tabs in 99 MB working set.

## 2. Principles

0. **Nothing before the picture that the picture doesn't need.** Opening a
   file is a critical path, measured per build. Folder listing, overlay UI,
   fonts, metadata, the library and the tray are all deferred until the
   picture is on screen. Only final pixels are shown on open.
1. **The screen is the unit of work.** Decode and keep what a window needs
   to draw at its current zoom, not what the file contains. Pixels sharper
   than the screen level exist only for the picture on screen, and only
   once a zoom needs them (§5).
2. **One cache, shared.** Every window and tab draws from one cache of
   screen levels; it holds what some window's preload ring or a warm tab
   needs and drops the rest at each navigation (§8). Nothing is "per
   window" except the surface it draws into; a tab is a file, a folder
   position and a view.
3. **Never block the UI thread.** Decoding, folder listing, metadata reads,
   the clipboard copy and folder watching run on worker threads. The UI
   thread only uploads, transforms and draws.
4. **Newest request wins.** Each navigation replaces the decode queue
   wholesale; pictures scrolled past are dropped before they start. Work is
   coalesced, not queued.
5. **Never a blank or frozen frame.** Paging keeps the previous picture on
   screen until the next one is ready; a zoom draws the screen level until
   the sharper level fades in over it.
6. **One process, many windows, many tabs.** Later launches hand their
   file to the running instance (a warm start, the fastest cold start there
   is). One GPU device, one decode pool, one cache.
7. **Measure before optimising, and measure the right counters.** Every
   build shows its open timeline and memory in the F12 overlay;
   `flick-bench ext-viewer` times Flick and Honeyview the same way, from
   outside, by watching the screen (§14).
8. **Decoders are chosen by measurement.** Each format's decoder sits
   behind one dispatch (`formats::kind`); swapping one is a change in
   `decode.rs` or `formats.rs` and a benchmark, not a redesign.
9. **Files are never modified.** Stars live in Flick's own library (§11).
   The only file operations are ones the user asks for: delete, and the
   starred folder's links or copies.
10. **Windows first.** Rendering (Direct2D, DirectComposition), decoding
    (WIC), the single-instance pipe and the shell integration are Windows
    APIs called directly. A port means replacing those modules; nothing in
    the caching, paging or tab model would change.

## 3. Process and thread model

```
┌──────────────── process ─────────────────────────────────────────┐
│  UI thread (winit event loop, ControlFlow::Wait)                 │
│    ├─ Window 1: swap chain + DComp visual, hover bar, tabs [A* B]│
│    ├─ Window 2: swap chain + DComp visual, hover bar, tabs [D*]  │
│    ├─ Window N                       (* = active tab)            │
│    └─ Tray icon (hidden top-level window), right-click menus     │
│                                                                  │
│  Shared (viewer::Shared, owned by App)                           │
│    ├─ GPU: one D3D11 device + D2D device + DComp device          │
│    ├─ Cache: screen levels on the GPU, by path                   │
│    └─ Decode pool: 3 preload threads (below normal priority),    │
│       plus an urgent thread per destination picture (§6)        │
│                                                                  │
│  Threads started for one job: open-path decode, detail/tiles     │
│  decode, animation frames, folder listing, info-panel metadata,  │
│  clipboard copy; long-lived: one watcher per folder in use,      │
│  device-removed waiter, single-instance pipe, update check       │
└──────────────────────────────────────────────────────────────────┘
```

- Workers produce CPU pixels and send them to the UI thread
  (`EventLoopProxy::send_event`), which uploads them on its next wakeup.
  The one exception is tiles (§5), which a worker copies into GPU bitmaps
  through its own device context.
- The UI thread never waits on a worker. If a picture isn't ready it keeps
  drawing what it has.
- A **tab** (`viewer::Tab`) is a file, its folder position, a view and its
  history; its pixels belong to the cache. A **window** (`Viewer`) is a
  surface plus an ordered list of tabs and an active index. Only the active
  tab of a window is drawn.
- **The process stays resident** when the last window closes, keeping the
  GPU device ready, so the next open is a warm start: no process start and
  no ~170 ms driver start, only the decode. Resident with no window: 0 ms
  CPU over 5 s; ~150–165 MB private commit (mostly the graphics driver's
  reservation) and ~71 MB working set, trimmed to ~0 by `EmptyWorkingSet`
  after 30 s with no window. A warm open after the trim takes ~135–175 ms
  instead of ~120 ms while driver and compositor pages come back; the trim
  stays at 30 s. The tray icon shows the resident process and quits it
  (`flick --quit` does the same, for the installer).
- **Tray icon after an Explorer restart.** The tray's window is a hidden
  top-level window (message-only windows miss broadcasts), which re-adds
  the icon on `TaskbarCreated`.
- **Single instance** (`instance.rs`). The first Flick in a session claims
  a mutex and, once its first picture is on screen, listens on a named
  pipe. A later launch sends its file (and its foreground right, via
  `AllowSetForegroundWindow`) and exits.
- **Newer builds take over.** Each pipe request carries the launcher's
  build (its exe's timestamp). A resident instance older than the launcher
  releases the mutex, answers "outdated", stops listening and drops its
  tray icon; it keeps its open windows and exits after the last one. The
  launcher then claims the instance and opens the file itself.
  `tools\build.ps1` renames a running `flick.exe` (and its hard-linked twin
  in `deps\`) before building, so the registered path always holds the
  newest build. A resident that has begun quitting sends any later launch
  on to take over instead of taking its file and losing it.
- **Elevated launches** (`elevation.rs`). A launcher running as
  administrator (a file search tool, say) starts Flick elevated too, and an
  elevated resident can't be reached by normal launches: its pipe answers
  "access denied" (no writing up an integrity level). So Flick never stays
  resident while elevated: an elevated launch that finds a resident hands
  its file over (writing down is allowed, so warm opens are unchanged), and
  one that would become resident restarts itself through Explorer at the
  desktop's level and exits. Image decoding therefore never runs as
  administrator either. A resident left elevated by an older build makes
  launches fail at once with a message to quit it.

## 3a. Opening a file: the critical path

Measured end to end by `flick-bench ext-viewer` (the same way as Honeyview)
and as an internal timeline (`--timeline-out`, `FLICK_TRACE`, the F12
overlay). Targets in §1.

```
warm open (process resident, GPU device ready):
  launch → hand path to resident process over the pipe (~15 ms to main)
   ├─ decode:    screen level (WIC, DCT-scaled to cover the display)
   │                                          ~75 ms for a 17 MP JPEG
   └─ UI thread: create window hidden (frameless, placed), swap chain,
                 DComp visual → upload → draw → present → show
  picture = decode + ~5 ms

cold open (no resident process):
t=0  process entry (no console, no runtime, no logger)
 ├─ decode thread: screen decode starts at main              done at ~110 ms
 ├─ GPU thread:    D3D11 + D2D + DComp devices (driver start) done at ~205 ms
 └─ UI thread:     event loop, window created hidden          ~30 ms
picture = when both are done: draw, present, show             ~210–240 ms

after the picture is on screen (DwmFlush), each on its own wakeup:
 ├─ folder listing (worker thread), position in the list
 ├─ single-instance listener, tray icon, update check
 ├─ decode pool and preload ring (threads below normal priority)
 └─ text resources, the info panel's metadata, the library: on first use
```

Rules that keep this honest:

- **The window appears with its picture.** It is created hidden, placed at
  its final size (§10), and shown only after the first present of the final
  picture; DWM's open animation is disabled for it
  (`DWMWA_TRANSITIONS_FORCEDISABLED`: it hides content for ~170 ms). A
  swap-chain present replaces the frame atomically, so there is no blank
  frame.
- **No placeholders on open.** A file being opened shows its final screen
  level first. Previews (§5) appear only while paging.
- **The binary is the startup.** C runtime linked in, no async runtime, no
  logger, no font database or locale scan at startup. DirectWrite, the
  info panel, the debug overlay and the library are created on first use.
  Shared objects (text factory, library connection) are created once per
  process, never per window.
- **Warm start is the normal case.** A second launch hands the path over
  the pipe and exits; the resident process opens it as the first tab of
  the window used last, or in a new window (§10a). The launching process
  does nothing else, so its own start (~15 ms to `main`) is the only
  overhead on top of the decode.
- **Preloading waits for the picture.** The folder listing and the decode
  pool start only after the compositor has shown the first picture
  (`DwmFlush`), and pool threads run below normal priority, so nothing
  competes with the open path.
- **Hard budgets, checked every milestone.** Time to the final picture on a
  17 MP burst JPEG, end to end with `flick-bench ext-viewer`: **warm open
  ≤ 150 ms, cold open ≤ 300 ms**, 0 blank and 0 flicker frames. Over budget
  fails the milestone, however useful the feature that caused it.
  `bench/quick.ps1` runs 5 cold opens, 5 warm opens and 20 page turns
  against a saved baseline and fails on a regression past a set margin.
- **Features stay off the open path as they grow.** The tray, folder
  watching, fonts, the tab strip, the library, session saving and the
  update check are all initialised after the picture, off the UI thread
  where they do work. The debug overlay's open timeline makes a creeping
  stage visible.

## 4. Crates and modules

| Crate | Responsibility |
|---|---|
| `flick-app` | The viewer, `flick.exe`. Depends on `windows` (D3D11, Direct2D, DirectComposition, DirectWrite, WIC, WinHTTP, CNG), `winit`, `zune-jpeg`, `jxl-oxide`, `image` (PNG, WebP, HDR, EXR), `resvg`, `rusqlite` (bundled SQLite), `serde_json`. |
| `flick-bench` | CLI: decoder timings and peak memory per file (`decode`), end-to-end timing of any viewer by watching the screen (`ext-viewer`), renderer start-up (`startup`), the disk-cache experiment (`disk-cache`), sample generators (`gen-sample`, `gen-formats`). |
| `flick-startup-probe` | Milestone 0's probe: one renderer's cold start to the first real picture. |
| `flick-icon` | Dev tool: renders `assets/icon/flick.svg` (32 px and up) and `flick-small.svg` (16–24 px, heavier lines for the tray) into the committed `assets/icon/flick.ico`. Building Flick never needs it. |

`flick-app`'s modules:

| Module | Role |
|---|---|
| `main.rs` | `App`: the event loop, opening files, the preload plan (`plan`), folder watchers, device-loss recovery, session saving, command line |
| `viewer.rs` | One window: its tabs, picture, drawing, input, animation playback, slideshow |
| `view.rs` | Zoom/pan state, view modes, orientation (`Turn`), the bitmap-to-window transform |
| `gpu.rs` | The device, per-window surfaces, uploads, device-loss detection |
| `decode.rs`, `formats.rs`, `raw.rs`, `tiles.rs` | Decoding: WIC; the formats WIC doesn't cover; RAW's embedded JPEGs; tiles for pictures over one bitmap |
| `pool.rs` | Preload queue and urgent decodes |
| `folder.rs`, `watch.rs`, `history.rs` | Folder listing, sorting and shuffling; folder change watching; a tab's Back/Forward history |
| `chrome.rs`, `osd.rs`, `debug.rs`, `menu.rs` | Hover bar and tab strip; info panel and notes; F12 overlay; the right-click menu |
| `keys.rs`, `settings.rs`, `placement.rs`, `session.rs` | Hotkey table; saved settings; window placement; the close history |
| `library.rs`, `meta.rs`, `clipboard.rs` | Stars (SQLite); EXIF for the info panel; Ctrl+C |
| `instance.rs`, `elevation.rs`, `tray.rs`, `icon.rs` | Single instance; elevated launches; tray icon; the app icon |
| `register.rs`, `update.rs`, `build.rs` | File associations; updates; the resource script (icon, version details) written at build time |

## 5. Image model: levels

Each picture has up to three levels, each a GPU bitmap (`ID2D1Bitmap1`,
premultiplied BGRA). The CPU pixels are dropped right after upload.

| Level | Source | Size | When produced | Lifetime |
|---|---|---|---|---|
| **Screen** | scaled decode covering the display (JPEG: the smallest DCT scale 1/2–1/8 that covers it; other formats: WIC's Fant scaler to the fit size) | ~2–8 MP | on navigate (urgent) and preload | in the cache while a ring or a warm tab wants it (§8) |
| **Preview** | a progressive JPEG's first scans at ⅛ (~90 ms), or a RAW's small embedded JPEG | small | for pictures paging passes, and as the destination's first frame while paging | replaced by the screen level when it lands, the view kept |
| **Detail** | the sharpest level the GPU holds: full size (one bitmap, or Tiles), else the finest of 1/2, 1/4, 1/8 by JPEG's DCT scaling that one bitmap holds | native, or that fraction | on zoom past the screen level; with Preferences ▸ Load Full Resolution Ahead (off by default), 500 ms after a picture settles | the current picture's; dropped on page-away and when its tab leaves the screen |
| **Tiles** (`tiles.rs`) | full resolution of a WIC picture over one bitmap's 16 384 px, decoded top to bottom in 256-row bands (WIC rectangles) straight into the tiles: the whole picture is never in memory | 4096² cells, each tile 8 px into its neighbours, drawn clipped to its cell (aliased), so smoothing and sharpening show no seam; only tiles in view are drawn | on zoom past 1/2 of a picture over 16 384 px, up to 1 GiB of GPU (~268 MP); larger pictures get the finest fraction one bitmap holds. 240 MP JPEG: 0.62 s | as Detail; Load Full Resolution Ahead never prefetches tiles |

- **No levels between screen and detail.** They decode nearly as slowly:
  Huffman decoding dominates, and 1/2 took ~85 % of full's time on a
  17 MP, a 176 MP and a 240 MP JPEG; stepping through them only popped and
  competed for the CPU. The detail level fades in over 120 ms (a D2D layer)
  over what was drawn before.
- **No mipmaps.** Direct2D's high-quality cubic filter pre-shrinks.
- **No thumbnail level.** On real camera files the ⅛ decode costs ~60 ms
  against ~74 ms for the screen level, and burst JPEGs carry no EXIF
  thumbnail, so a thumbnail path buys almost nothing; the tab strip shows
  names, not thumbnails.
- The screen level is tied to the display, not the file: it covers the
  primary display (or `--size` for benchmarks). On a 3440×1440 display only
  1/2 scaling covers a 24–45 MP JPEG.
- Load Full Resolution Ahead costs, for every picture paused on, a full
  decode (~170 ms of CPU for 24 MP) and its GPU bitmap (~100 MB for 24 MP)
  while the picture is on screen; in exchange the first zoom is sharp at
  once.

## 6. Loading and paging

```
navigate(target)
  ├─ urgent decode of the destination (Pool::urgent): its own thread,
  │   normal priority; a progressive JPEG gets its preview first
  ├─ pictures the walk passes: preview only (passing_paths)
  ├─ preload queue replaced (Pool::set_queue): the ring, nearest first
  ├─ cache pruned to what some ring or warm tab keeps (App::plan)
  └─ on zoom past the screen level → decode the detail level (§5)
```

- **The ring.** 3 ahead and 1 behind, in the direction of travel (toward
  the target, else the last turn); reversing swaps the ring within one
  navigation. The focused window's ring is queued first, then the others'
  in focus order. Paging in order, the two random neighbours join the ring;
  paging randomly, it runs 3 ahead along the shuffled order instead (with
  only the next random picture preloaded, a held random key waited on a
  fresh decode at every step and stuttered).
- **Coalescing, not cancellation.** The queue is replaced wholesale on every
  navigation, so fast wheeling never builds a backlog. A decode already
  started runs to completion; its result is cached only if still wanted.
- **No skipping.** Paging past images shows them: someone scanning for a
  picture needs to see each one go by. A wheel notch moves a *target*; the
  picture walks there one image at a time, each on screen at least 25 ms
  (40/s, about what three decode threads sustain on 17 MP JPEGs). If the
  wheel gets more than 8 images ahead (a free-spinning wheel), the walk
  jumps to keep the lag under ~0.2 s. Home/End jump directly. Rejected:
  skipping undecoded pictures, and giving up waiting on one after 250 ms.
- **Heavy files, fast wheel.** On 24 MP top-quality progressive JPEGs
  (~1 s per decode) a plain walk queued: the ring decoded every picture
  passed in full, and the destination waited behind them (1.1 s after a
  fast spin stopped, 0.7 s after steady scrolling). Hence the urgent
  destination decode (a stale one stops after its preview), and previews
  only for pictures passed. Measured (`ext-viewer --wheel-bursts`): steady
  scrolling shows the destination in 11–124 ms (sharp ~0.65 s later, one
  full decode); a fast 12-notch spin shows 5–6 pictures on the way and the
  destination in 45–133 ms. Key paging on the same files: first turn
  ~0.38 s, then 8–18 ms.
- **Held keys page as fast as pictures appear** (as Honeyview): a key's
  auto-repeats are dropped while the previous picture is still decoding or
  the walk is still under way, so no "render debt" builds up and paging
  stops when the key is released. Separate presses always count.
- **RAW preloads stop at the embedded JPEG** (§7): only the picture a
  window heads for gets Windows' RAW decode.

## 7. Formats and decoders

There is no decoder trait: `formats::kind` picks the decoder from the
file's first bytes (SVG by extension), and `decode_screen`,
`decode_preload`, `decode_level`, `decode_full` and `animate` dispatch on
it. WIC was the fastest JPEG decoder measured (~25 % faster than zune-jpeg
and image-rs at full size, 2.5 ms one-time init; turbojpeg unmeasured).

| Format | Decoder | Screen level |
|---|---|---|
| JPEG | WIC | DCT scale 1/2–1/8 |
| JPEG, progressive (YCbCr, greyscale) | `zune-jpeg`, colour-managed and scaled through WIC | full decode, Fant scaler, then the colour profile on the small picture. WIC takes twice as long on these (24 MP at top quality: 1.9 s against 1.0 s; its DCT scaling saves nothing, every scan is read anyway); pixels match WIC's within rounding (a test). Cold open 1.97 → 1.06 s, first page turn 1.39 → 0.38 s. Each decode holds ~300 MB while it runs (coefficients, file, output), so three preloads at once peak near 0.9 GB, briefly. zune failing falls back to WIC |
| JPEG, CMYK / Adobe YCCK | WIC, scaled while decoding, colours converted after on the small picture by WIC's own converter | zune refuses YCCK to BGRA, and its YCCK to RGBA, though 2.4× faster, comes out far from WIC's colours (mean difference 40–50 of 255). Scaled decoding takes an 18 MP, 17-scan file from 3.0 to 2.0 s, with a preview in ~115 ms, matching WIC's whole-picture colours within rounding (a test) |
| PNG, TIFF (8/16-bit), BMP, ICO, JPEG XR, static WebP, HEIC/HEIF, AVIF | WIC (HEIC/AVIF through Windows' HEIF + HEVC / AV1 extensions) | Fant scaler to the fit size, after converting to premultiplied BGRA (a 20 000 px PNG panorama opens) |
| APNG, animated WebP | `image` crate (composes frames) | first frame, full size, then played like a GIF |
| GIF | WIC + `decode::compose` | first composed frame |
| JPEG XL | `jxl-oxide` (WIC's JPEG XL component may be installed but fail to initialise) | full decode, then the Fant scaler |
| SVG, SVGZ | `resvg` (system fonts, loaded on the first SVG) | rendered at the fit size; detail = rendered at its own size raised to at least 4096 px on the longer side (max 16 384) |
| OpenEXR, Radiance HDR | `image` crate, tone-mapped on the CPU | extended Reinhard on luminance, white point at the 99.9th percentile, sRGB; then the Fant scaler |
| Camera RAW | `raw.rs` + WIC | the embedded JPEG (below) |

Decode times for 12 MP samples (screen level, 3377×1440 target): PNG
183 ms, WebP lossy 264 ms, HEIC 220 ms, AVIF 153 ms, JPEG XL lossless
268 ms; 16 stills open cold with 0 blank and 0 flicker frames, and the
cold JPEG open stays at 212 ms with these decoders linked in (none does
anything until a file of its format opens). Heavy decodes in parallel
barely slow each other on a 16-thread CPU (4 at once: +5–10 %,
`decode_contention`): waits came from queueing behind busy workers, which
the urgent destination decode removed (§6).

A picture whose full size exceeds the GPU's bitmap limit gets tiles or a
fraction (§5), never a full decode that would only be refused.

**Camera RAW** (`raw.rs`). Cameras store a JPEG inside each RAW; Flick
never demosaics itself. `raw.rs` finds the JPEGs with a few small reads
(under 1 ms): the TIFF IFDs and sub-IFDs (CR2, NEF, ARW, DNG, PEF, RW2's
JpgFromRaw, ORF's maker note), CR3's tracks and PRVW box, RAF's header.
Each candidate is checked by its frame header, so lossless-JPEG sensor data
is never taken; orientation comes from the RAW's IFD0. WIC's RAW decoder
(the Raw Image Extension) is only opened for the RAW's size, 2–5 ms. A
JPEG at least 90 % of that size stands for the RAW at every level; a
smaller one (Sony before ~2021: 1616×1080; Panasonic, Olympus, Fujifilm)
gives the screen level where it covers the screen, else a quick preview,
and WIC decodes the RAW for the picture a window heads for and for zooming
in. Preloads stop at that preview (an A6000 ARW preloaded: 13 ms and 10 MB
at its peak, where WIC's decode is 0.6 s and 347 MB). 12 public samples
(raw.pixls.us, `bench/samples/raw`): screen level 12–80 ms where WIC's
demosaic takes 0.6–2.5 s; detail 47–106 ms from a full-size JPEG. Phone
DNGs: Windows' DNG codec shows the DNG's own preview too (a Pixel's is
1280×964), so nothing larger exists to zoom into.

**Animations.** WIC's GIF frames are composed by `decode::compose`
(logical screen, frame offsets, transparency, disposal 0–3; delays under
20 ms play at 100 ms, as browsers do). The first composed frame is the
screen level, so an animation opens like any picture. Once it has been on
screen for 150 ms (so paging past animations costs nothing extra), a
thread decodes the remaining frames in order; each goes to the GPU as it
arrives, and playback loops over them on the viewer's deadline timer (no
frame due: nothing runs). Paging away stops the thread. Frames are kept on
the GPU while they fit in 256 MB per animation. A bigger one (a video saved
as GIF or WebP) streams: the thread stays 2–8 frames ahead (up to 64 MB,
credits in `Ring`), each frame is dropped once shown and its bitmap
refilled with a later one, and every loop decodes the file again. 1080p,
300 frames: GIF decodes at 6.8 ms per frame, animated WebP at 25.9 ms, both
under their 40 ms per frame of playback. Pictures are uploaded as
premultiplied alpha, so transparency shows the window background.

**Colour management.** A scan of 164 000 real-world image files: 90 % carry
no profile, 6 % sRGB, ~2 % wide gamut (Display P3, Adobe RGB, a few
others). WIC formats read the frame's colour contexts; an ICC profile that
isn't sRGB (by its description) or grey, or EXIF's Adobe RGB, is converted
to sRGB with WIC's colour transform (`decode::to_srgb`); everything else
costs one `GetColorContexts` call. On a 24 MP Display P3 JPEG: DCT-scaled
screen level +25 ms (converted after decoding, on the small picture),
Fant-scaled none (converted after scaling), full resolution +390 ms (zoom
and Ctrl+C only). JPEG XL renders to sRGB already. Not converted:
APNG/animated WebP and GIF. The display's own profile isn't applied (sRGB
output assumed).

## 8. Memory

- **One cache** (`Shared::cache`): screen levels on the GPU, by path. After
  every navigation `App::plan` keeps exactly the paths some window's ring
  or warm tab needs (`keep_paths`) and drops the rest. Paging in order,
  memory is flat by construction: 5 screen levels (17 MB each for a 17 MP
  JPEG) whatever the folder size; working set ~70 MB while paging, where
  Honeyview grows 66 MB per turn to 396 MB.
- **Tab states.** The active tab of each window is drawn. An inactive tab
  is *warm*: its screen level stays on the GPU, so switching back is a
  texture swap (0.6 ms key → present). After 3 minutes unviewed
  (`COOL_AFTER`; `FLICK_COOL_AFTER_S` for measuring) it is *cold*: it keeps
  only its file and view, and decodes again (~85 ms) when activated, the
  same path as opening a file. A tab leaving the screen drops its detail
  level and animation at once (`park`).
- **Measured, 20 tabs in one window:** working set 99 MB (Honeyview: 153–416
  MB for one window); with all 20 warm, 491 MB private and 410 MB GPU
  dedicated (~17 MB per warm tab); with 19 cooled, 252 MB private and 172 MB
  GPU, one window's worth: the active tab and its ring.
- **Freeing GPU memory.** D3D11 destroys released objects only when its
  immediate context is flushed, and nothing flushes it while no window
  draws; Direct2D also keeps scratch textures. Closing a window and cooling
  tabs call `ID2D1Device::ClearResources` and `ClearState` + `Flush`
  (`Gpu::release_unused`). Without this, 10 warm opens took private commit
  from 166 to 189 MB, still climbing; with it, memory levels off after ~5.
- **Private commit vs working set.** Private commit counts the graphics
  driver's reserve (~100 MB appears as soon as a D3D11 device exists); the
  working set is what is actually in use. Budgets and comparisons use the
  working set; both are reported.
- No byte budget and no disk tier: with screen-sized levels and tab
  cooling, nothing has needed them (§16, §18).

## 9. Rendering

- One D3D11 device with a Direct2D device and a DirectComposition device on
  top, created at process start on a background thread and kept for the
  life of the resident process.
- Per window: a flip-model swap chain (2 buffers, flip-discard,
  `Present(1)`) as a DirectComposition visual, and a D2D device context.
  The window has no GDI redirection surface (`WS_EX_NOREDIRECTIONBITMAP`):
  91 MB private after a cold open, against 118 MB for the milestone 0
  probe's Direct2D window. The swap chain is
  premultiplied-alpha though every frame is opaque: an opaque one may be
  put on a hardware overlay plane (multiplane overlay), which showed black
  on screen while the window was resized and added ~35 ms between present
  and the picture on screen. Composed by DWM instead, page turns went from
  ~50 ms to 16 ms median key → screen (p90 25 ms). A window also draws
  inside `Resized` (resized buffers are empty until presented).
- Draw = `DrawBitmap` per visible level (screen, detail, or the visible
  tiles) with the view transform. The render filter is the window's
  setting: No Filter (nearest neighbour), Soften (high-quality cubic when
  shrinking, linear when enlarging), Soften+Sharpen (high-quality cubic
  both ways, then Direct2D's Sharpen effect in window space).
- The hover bar, tab strip, info panel and notes are drawn with Direct2D +
  DirectWrite in the picture's pass: hand-drawn chrome is a small surface
  (one bar, a tab strip, text), with native text and no toolkit cost on
  memory or startup. A separate visual for the overlay, to avoid redrawing
  the picture during fades, is unneeded so far.
- Redraw only on: input that changes the view, a level arriving, chrome
  animation (fades and tab glides request frames only while they run),
  an animation frame due, OS expose/resize. Idle: 0 ms CPU over 5 s with
  the cursor away, on the picture or on the bar.
- **Device loss** (driver update, GPU reset or hang, adapter removed) is
  noticed three ways: a call failing with `DXGI_ERROR_DEVICE_REMOVED`/
  `_RESET` or `D2DERR_RECREATE_TARGET`, `GetDeviceRemovedReason` before
  each frame, and Windows' device-removed event (`ID3D11Device4`), waited on
  by a thread so an idle window recovers too. The app then drops every
  window's surface, every tab's picture and the cache, makes a new device
  (on the UI thread: a rare ~170 ms), gives each shown window a new surface
  and decodes its picture again through the pool, back at the zoom and pan
  it had; windows stay open. Shift+F12 simulates a loss.
- Not used: wgpu (DX12: +55 ms and ~2.9× the memory of Direct2D; GL: 35 ms
  faster but ~1.8× the memory and OpenGL on Windows; Vulkan: +140 ms) and a
  GDI first frame (§16).

## 10. Windows and frameless UX

- `winit` window, `with_decorations(false)`, no redirection bitmap. Within
  8 px of an edge (DPI-scaled) a press resizes (`drag_resize_window`);
  dragging the title or the tab strip moves the window (`drag_window`),
  which gives native Aero Snap, Win+arrow, shake and snap layouts. The move
  starts only once the cursor passes the system drag threshold, so a click
  never enters Windows' move loop; buttons act on release over the same
  button, like native ones.
- **Hover bar** (`chrome.rs`), as in Honeyview: a single top bar, drawn
  over the picture (never shrinking it), fades in (100 ms) when the cursor
  is within **2× the bar height** of the window's top edge, and fades out
  (200 ms) when it leaves the zone or the window. Contents: the file's full
  path and its [index/count] as the title (cut in the middle when too long,
  keeping the file name), the tab strip (§10a), and minimize /
  maximize-restore / close buttons on the right (46 px each, close hovers
  red). A tab, or a cut-short title, shows the whole path in a tip after a
  second's hover, then at once on the next tab. Double-clicking the title
  maximizes. Text resources are created on first hover. There is no bottom
  bar.
- Windows shown by Flick's own code (a remembered maximized placement) are
  still reported to winit with `set_visible(true)`: winit rebuilds the
  style from its own flags on every maximize/restore, and with its VISIBLE
  flag unset it hides the window.
- **Placement** (`placement.rs`): on close, the window's `WINDOWPLACEMENT`
  (normal rect + maximized) is saved to `%LOCALAPPDATA%\Flick\window-placement`.
  The next first window is placed there while still hidden (a maximized one
  sized to the monitor's work area), so the first frame is drawn at its
  final size and the window appears maximized in one step (235 ms, 0
  blank/flicker frames). For a snapped window Windows' normal rect is the
  one before snapping, so `placement::save` stores the rect on screen
  instead (`IsWindowArranged`). No saved placement: sized to the picture
  (≤ 90 % of the monitor), centred. `--size` (benchmarks) neither reads nor
  writes it.
- **New windows open beside the window they come from** (Move to New
  Window, Ctrl+N, files opened with tabs off): maximized if it is; on the
  other side of the screen if it is snapped to one; else the same size,
  right of it, else left, below or above, wherever it fits without
  overlapping. Only the first window uses the remembered placement.
- **Least window size** (`chrome::MIN_WIDTH` × `MIN_HEIGHT`, 202 × 150
  logical px): half the bar with the padlock, one full tab and the three
  buttons across, less high than wide, so a window never shrinks out of
  sight. Windows keeps to it on resizes and display changes (winit's min
  inner size, at each display's scaling); a saved placement or a picture's
  size is grown to it.
- Each window holds its own tabs; the cache and pool are shared. Closing
  the last window leaves the process resident (§3).

## 10a. Tabs

Tabs replace Honeyview's one-image-per-window without adding cost to the
goals above, because a tab owns no pixels (§8).

- Opening a file puts it in the **first tab of the window used last**; that
  window's other tabs shift right. **Preferences ▸ Open Files in Tabs**
  (app-wide, saved; on by default): off opens every file in its own window
  and removes Move to New Window; windows that already have tabs keep them.
- A window with a single picture shows **no tab strip**, just the title.
- **Window colours.** Each window has its own colour on its hover bar
  (`chrome::PALETTE`: eight deep jewel tones with light text; the active
  tab is the tone brightened by a fifth, inactive tab text the ink part-way
  into the tone, tab fills rounded 4 px). A new window takes the first
  colour no open window uses. Each window's background behind the picture
  is a slightly different dark grey (`chrome::BACKGROUNDS`, 0.055–0.135,
  paired with its colour), so overlapping windows are told apart even
  where the bar is hidden.
- **Move to Window** (right-click menu) lists the other windows by colour,
  then New Window (absent for a window's only tab). Up to three
  destinations stand in the main menu as "Move to …" items; four or more go
  into a submenu. A window left without tabs closes. The moved tab keeps
  its picture, place and zoom, and pages in the receiving window's sort
  order. Dragging a tab out to another window is optional backlog (§18).
- **Reordering:** Ctrl+left-drag or right-drag on a tab (a right click
  without moving still opens the menu), Chrome style: the dragged tab
  follows the cursor and takes the cell nearest its centre; the others
  glide aside (exponential easing, 50 ms time constant, redrawing only
  while something moves). One drag at a time.
- **Multi-row strip:** tabs are 220 px wide at most and wrap onto another
  row once they would be narrower than 150 px; every row is a bar high and
  all tabs are one width, set by filling the first row's room between the
  padlock and the window buttons (which stay on the first row). The rows
  below run the window's full width. **Preferences ▸ Multi-row Tabs**
  (app-wide, saved; on by default): off keeps one row, the tabs narrowing
  to fit.
- **Tab Lock** (`L`, or the menu; per window, not saved; only with two or
  more tabs, and it turns off when a window is down to one): the keys and
  the wheel switch tabs instead of pictures. Left/Right, A/D and the wheel
  go along the strip (wrapping); Down/S and Up/W go forward and back in a
  random order of the tabs, made like random paging's (`folder::shuffled`):
  each tab once per round, back retracing forward, shuffled afresh when
  tabs come or go and kept when they are reordered. PgUp/PgDn and
  Space/Backspace still page. A padlock shows at the bar's left; clicking
  it unlocks.
- A tab switch from the keys or wheel, Tab Lock toggled, or a change in the
  window's tab count shows the bar for 2 s (`Chrome::reveal`, on the
  viewer's deadline timer).
- **Opening a selection from Explorer** (one launch per file, all at once):
  files arriving within 500 ms of the previous one form a burst. The first
  opens as usual; the others become background tabs after it, in arrival
  order, and are decoded (and their folders listed) only when first viewed.
  A cold start counts as the first of a burst.
- Switching to a tab without its picture yet (a background or cold tab)
  keeps the previous tab's picture on screen until it decodes, as paging
  does.
- **Keys:** `Ctrl+Tab`/`Ctrl+Shift+Tab` cycle, `Ctrl+1..9` jump, `Ctrl+T`
  duplicates the tab into the background beside it (as does a double-click
  on the picture), `Ctrl+N` opens the file again in a new window, `Ctrl+W`
  or a middle click on a tab closes it (the last one closes the window),
  `Ctrl+Shift+T` reopens the last close.
- **The menu and stray clicks.** Menu items are chosen with the left button
  only. Clicks that close the menu can leave a press without its release,
  or presses queued during its modal loop: Flick drops drag state when the
  menu closes, ignores presses for 50 ms after in every window, acts on a
  release only if it saw the press, and keeps a drag only while the button
  is physically down (`GetAsyncKeyState`, mapped through swapped buttons).
  A menu closed by a right click reopens where that click was, in the Flick
  window under the cursor (brought forward), as Explorer's menus do.

**The close history** (`session.rs`). Restoring is undoing closes, newest
first: `Ctrl+Shift+T` (or a click on the tray icon with no window open)
reopens the last close, each press an earlier one, up to 50 back. A close
is a tab (Ctrl+W, a middle click on it) or whole windows: the close button
or a window's last tab (one window, all its tabs), Quit (every window at
once), and a crash, kill or shutdown (every window the process had open,
found at the next start). Moving a tab to another window, deleting its
file, and a tab dropped because its file vanished aren't closes.

- A tab goes back to its window, at its place in the strip, if that window
  is still open (window ids count only within one run); else after the
  active tab of the window used last, or into a window of its own. A window
  comes back at its placement, with its tabs, active tab, menu settings,
  colour (if free) and Tab Lock. Each tab keeps its Rotate ▸ turn and its
  zoom and pan (`View::custom_zoom`, if set by hand), cold tabs included.
- The active tab of a reopened window decodes on the open path; the others,
  and a tab reopened into a window, wait as background tabs do.
- A reopened tab whose file can't be shown (gone, or its drive) is
  *unavailable*: just the background, its name on the hover bar. The folder
  is listed with the missing file in its sorted place, so the arrows and the
  wheel go on from there; if the folder is gone too, only `[` `]` work, the
  gone folder sorting among its siblings where it was.
- Files in `%LOCALAPPDATA%\Flick`: `closed.json` (the history, read and
  rewritten on each change, since other Flick processes may share it) and
  `open-<process id>.json`, this process's open windows, written a second
  after a change (at once on a close; nothing while idle), removed when the
  last window closes or at Quit. At start, files of processes no longer
  running each become one close. One file per process, so a newer build
  taking over from a running one isn't a crash. Writes go through a
  temporary file and a rename. `--standalone` and `--size` runs keep no
  history.

Non-goals: tab groups, pinned tabs, tab search.

## 11. Stars and the library

Stars replace Honeyview's "Save to photo folder" (a copy per favourite,
and only starred / not starred).

- `Ctrl+F` adds a star to the picture on screen, `Shift+F` takes one away;
  any number of stars. A brief "★★★" note confirms; the info panel shows
  them and since when. Sort by ▸ Star count orders a folder by them (most
  first); Sort by ▸ Date starred by when each got its first star (oldest
  first, or newest with Descending; unstarred files last).
- **Files are never modified.** Stars live in Flick's library
  (`library.rs`, SQLite, `%LOCALAPPDATA%\Flick\library.db`), keyed by the
  file's NTFS ID (volume serial + 128-bit file ID: survives renames and
  moves on its drive, never reused), with its last known path and folder
  for looking a whole folder up in one query (a file moved since starring
  is found by ID when viewed, and its path updated then). The date starred
  is kept in the library (schema version 2; version 1 libraries are
  upgraded on open, their stars undated), because hard links share their
  file's dates. Opened on first use (a star, the info panel, sorting by
  stars), never at startup.
- **The starred folder** (Preferences ▸ Choose Starred Folder…, asked for
  on the first star): the first star puts a **hard link** there (another
  name for the same file: no copy; a file and its link share their ID, so
  their stars), or a **copy** for a file on another drive; the last star
  taken away removes it (a link only while the file has another name, never
  a file's only name). Name clashes get "name (2)". Deleting the original
  keeps the star (the file lives on under the link's name).
- **The starred folder decides what is starred:** an entry deleted in
  Explorer unstars its file. Seen at once by a folder watcher if a tab is
  in that folder, otherwise by the daily sweep (`sweep_if_due`), run when
  the resident goes idle: records whose entry is gone are dropped, records
  whose original is gone point at their entry. Renames seen by watchers
  update records.

Keywords (tags) would fit the same library (§18): `tags (id, name)` and
`file_tags (volume, file_id, tag)` keyed like `files`, and a tag could get
its own folder of links with the same link and cleanup code. Writing them
into files (XMP via the Windows Property System, or sidecars) was the
original plan and is set aside by principle 9.

## 12. Folder navigation

- The folder is listed on a worker thread after the first picture is on
  screen (§3a): its files with a supported extension (`folder.rs`, plus
  `raw::EXTENSIONS`), with dates and sizes from the same directory scan.
  The position in the list is resolved when the listing arrives. Nothing
  reads EXIF for every file.
- **Sort by** Name (natural order, as Explorer: img2 before img10), Date
  modified, Size, Type, Star count or Date starred; Descending reverses.
  Re-sorting 100 000 files takes ~40–70 ms.
- **Random order:** a shuffle of the folder made on the first random step
  (SplitMix64 Fisher–Yates, ~0.4 ms and 8 bytes per file for 100 000), new
  for each window. Back exactly retraces forward from any picture, every
  picture comes once per round, and a re-sort doesn't change the walk.
- **Folders:** `[` and `]` go to the folder before or after this one among
  its siblings, ordered as the pictures are (by name, natural order, or
  date modified; by name for keys folders don't have; Descending too),
  skipping hidden folders and ones without pictures; listed on a worker
  thread, at its first picture, with a note naming it. Paging past the
  first or last picture says so in a note.
- **History** (`history.rs`): each tab remembers every picture it showed,
  the ones the wheel walks through too, up to 50 back and 50 forward, a few
  KB at most. The mouse's side buttons and the keyboard's Back and Forward
  keys walk it as a browser does: moving on any other way drops the way
  forward; a picture in another folder lists that folder; deleted files are
  skipped; at either end a note says there's nothing further. No other UI.
- **Watched folders** (`watch.rs`). Windows can't count a folder's files
  without listing them, but it can report changes: one watcher per folder
  some tab pages through, shared by every tab and window in it
  (`App::sync_watchers`, after each `plan`), is a thread waiting on
  `ReadDirectoryChangesW` (new, removed, renamed files; size or date
  changes), costing nothing while the folder is quiet. A folder's changes
  are applied once it has been quiet for 250 ms (a file being written
  reports many), to every tab in it, each with its own sort order:
  `Folder::apply` inserts new images where the order puts them (binary
  search), removes and re-sorts in place, keeps the current file current
  (a removed one is followed by its successor; a renamed one is followed),
  and the paging target stays on its file. A rewritten file is dropped from
  the cache and decoded again, the one on screen too (its old picture stays
  until then). If Windows' change buffer overflows, every tab in that
  folder lists it again. Random paging starts a new shuffle after a change.
  Not covered: a file created between listing a folder and its watcher
  starting.

## 13. Input and settings

- **No config file of tunables.** Ring depth, budgets, fades, zoom step and
  the like are constants in the code: most were set by measuring against
  §1's goals, and a file would invite changes that quietly break them (and
  add a read to startup). A tunable someone needs becomes a menu choice,
  saved by the settings rule below, as the slideshow interval is.
- **Settings rule:** each window keeps its own settings; new windows start
  with the latest chosen in any window; that is saved in
  `%LOCALAPPDATA%\Flick\settings` (`settings.rs`) for the next run. A view
  mode applies to each new picture in the window; resizes re-apply it until
  the user zooms or pans. A sort change re-sorts the window's folder at
  once and the preload ring follows. Preferences ▸ items are app-wide.
- **Hotkeys are one table** (`keys.rs`): `BINDINGS` pairs each chord with a
  `Command`. The key handler looks a key up there and runs the command
  (`Viewer::run`, which the menu's items call too), and the menu shows each
  command's keys from the same table, so the two can't disagree; changing a
  key is one line. Honeyview's keys where sensible, so muscle memory
  carries over. A chord names its key by position (letters, digits,
  brackets: any layout), as a named key (arrows, Tab, Enter, F12) or as a
  character ("+", which takes Shift on most layouts, so Shift doesn't count
  for characters). Modifiers match exactly: Ctrl+Shift+C is not Ctrl+C.
  Each command says whether a held key repeats it (paging, tab switching,
  zoom, closing tabs) or runs once per press, whether it pages (a held
  paging key waits for the picture on screen, §6) and whether it stops a
  slideshow. With Tab Lock on, the paging commands switch tabs, from the
  keys and the menu alike.
- **Keys:** ←/→, A/D, PgUp/PgDn, Space/Backspace page in sort order; ↓/S
  and ↑/W step forward and back in the window's random order (§12);
  Home/End; `[` `]` folders; Back/Forward history; +/− zoom; `Tab` the info
  panel; F12 the debug overlay; Del / Shift+Del delete; Ctrl+C copy;
  Ctrl+Enter shows the file in Explorer; tab keys in §10a. Esc does
  nothing. A plain hotkey works on its own only: with Ctrl or Alt, keys
  without a Ctrl/Alt binding do nothing. The wheel pages without modifiers
  only (Ctrl+wheel zooms).
- **Right-click menu** (`menu.rs`; also the Menu key). A native popup menu
  (no code runs until it opens): Honeyview's View items minus the
  double-page modes — Stretch Small Images (`Z`), and the view modes in
  middle click's order: Fit in Window (`9`), Fill Window (`6`: whichever of
  Fit to Width and Fit to Height fills the window, so the picture overflows
  one way), Fit to Width (`8`), Fit to Height (`7`), Original Size (`0`);
  Sort by ▸; Filter ▸ No Filter (`U`), Soften (`I`), Soften+Sharpen (no
  key: `S` is random paging here); Rotate ▸; Slideshow ▸; Navigate and Tabs
  submenus listing every hotkey beside its command; Move to Window; File
  Info; Preferences ▸ (Open Files in Tabs, Multi-row Tabs, Load Full
  Resolution Ahead, Update Automatically, Choose Starred Folder…, and a
  Mouse reference). Every view mode centres the picture, on the axis it
  overflows too, so paging doesn't jump between edge and centre.
- **Mouse.** Wheel pages (walking through every picture passed); Ctrl+wheel
  zooms. Left-drag pans (nothing moves while the picture fits; only the
  title bar and tab strip move the window, so dragging means one thing).
  Double-click on the picture duplicates the tab, as Ctrl+T. Right-drag
  zooms by horizontal travel (right: in, left: out; 2× per 200 px) around
  where it started: the picture point pressed stays under the cursor (a
  picture smaller than the window grows from there, not from the centre,
  until it meets an edge). During the drag nothing holds the picture's
  edges to the window's; on release the picture glides (150 ms, eased) to
  where the edge rules put it. (Held to them during the drag, an edge that
  met the window's took over as the zoom's centre and the picture
  jiggled.) Right-drag holds at 100 % for 24 px of travel as it passes, and
  Ctrl+wheel and `+`/`−` stop at 100 % on the way through. A right click
  without moving opens the menu. A picture left off-centre by a zoom around
  a point stays there: panning moves it only on axes where it is larger
  than the window, and zooming or resizing only keeps it inside; the view
  modes centre it again.
- **Middle click** on the picture steps through the view modes in the
  menu's order, each set exactly as the menu sets it (the window's mode, so
  new windows take it and it is saved), around the point clicked where the
  picture overflows (taken before the mode changes the view); then, as one
  more stop after Original Size, the last zoom set by hand, which applies
  only to the picture shown and leaves the mode alone (remembered per tab,
  as scale and centre relative to the picture; panning in a mode isn't a
  zoom by hand). Stops that look the same as the picture now or as an
  earlier stop are skipped. Middle click on a tab closes it.
- **On-screen text** (`osd.rs`), just below the hover bar's first row (the
  bar draws over it), Segoe UI 14 px (Honeyview's size; 17 px read as too
  big). Notes at the top right on a faint pill for 1.2 s: the zoom after any
  zoom, named by the mode when it is the mode's ("Fill window · 47 %", else
  "Zoom · 150 %"), "Copying…"/"Copied to clipboard", stars, first/last
  picture. `Tab` toggles the **info panel** at the top left (a click on it
  closes it; a setting like the View menu's; also in the menu as File
  Info): file name and position in the folder, picture size, format details
  (JPEG chroma subsampling and progressive scan, 16-bit, animation frames,
  colour profile), file size and date, stars, and EXIF camera, lens,
  exposure, Software, date taken and the 35 mm equivalent focal length
  where it differs. Read through WIC's photo metadata names on a worker
  thread (`meta.rs`) the first time the panel shows a picture. 1.65× line
  spacing on a dark box (an outlined box-less version read badly). Where the
  picture leaves a border on the left at least 240 px wide, the panel fits
  in it, off the picture; else it wraps at 400 px. Labels make a column and
  long values wrap within theirs (a hanging indent); a file name may break
  after every `-` and `_`, and its `[i/n]` stays with the name's end.
- **Rotate** (menu, `Alt+↑/→/↓/←` for 0/90° clockwise/180°/90°
  counter-clockwise, `Ctrl+R` the next of those in turn, `Alt+H`/`Alt+V`
  mirror, `Alt+F` cycles not mirrored → horizontally → vertically; both
  ways is a 180° turn, which `Ctrl+R` reaches): per tab, for every picture
  paged to in it, on top of EXIF orientation (`view::Turn::after` composes
  the two into one of the eight orientations, so drawing, sizing and copying
  need nothing new). Not kept by default; Rotate ▸ Persist makes it a
  window setting like the others.
- **Delete** (`Del`, menu: to the Recycle Bin; `Shift+Del`: for good)
  always asks first in Windows' own message box, then shows the next
  picture; a folder's only picture closes its tab.
- **Slideshow** (`Ctrl+G` starts and stops; Slideshow ▸ with 250 ms–30 s
  presets; `Ctrl+[` `Ctrl+]` adjust the interval by about a tenth, 10 ms
  steps at the least, 50 ms–60 s; saved like a setting): moves the way the
  last move went (in order, wrapping at the end, or random; through the
  tabs under Tab Lock, without revealing the bar). Each picture stays the
  interval from when it is on screen (`slide_due`), so a slow decode delays
  the next step instead of shortening it or letting steps pile up;
  preloading already runs ahead in the slideshow's direction. Only moving
  by hand (the paging keys, Home/End, the wheel, tab switching, `[` `]`,
  Back/Forward) or `Ctrl+G` stops it.
- **Ctrl+C** copies the picture at full resolution, upright (EXIF
  orientation applied), as a 32-bit DIBv5 with straight alpha; decoded on a
  worker thread. **Ctrl+Enter** opens the folder in Explorer with the file
  selected.
- **Command line:** `flick <file>`; `--standalone` (neither joins nor
  becomes the resident instance; exits when its window closes: cold-open
  benchmarks), `--size WxH` and `--timeline-out <file>` (benchmarks);
  `--register`, `--unregister`, `--quit` (for the installer, exit code 0 or
  1); `--reopen`, `--tray`, `--after <pid>` (after an update, §17).

## 14. Observability and benchmarks

- **Debug overlay** (`F12`, `debug.rs`): the open timeline (each stage
  from process creation to the picture on screen), process private commit
  and working set, the view state and what the detail level is doing. Shift+F12 simulates a lost graphics device.
- **Switches for measuring:** `--timeline-out` writes the timeline and
  per-page-turn timestamps as JSON; `FLICK_TRACE` logs stages;
  `FLICK_GPU_SYNC=1` waits for the GPU after each present, to time it;
  `FLICK_COOL_AFTER_S` shortens tab cooling; `FLICK_UPDATE_URL` points the
  update check elsewhere.
- **`flick-bench ext-viewer`** launches a viewer (Flick or Honeyview) and
  samples the centre of its window every ~7 ms: time to the window, to the
  first picture and to the final one, blank and flicker frames, memory,
  page turns by real key presses (`--page`), wheel flicks
  (`--wheel-bursts`), warm opens against a resident instance
  (`--resident`). It is the judge of §1's open and paging goals.
- **`bench/quick.ps1`**: the regression check run before each milestone
  commit (5 cold opens, 5 warm opens, 20 page turns against
  `bench/results/quick-baseline.json`; history in
  `bench/results/quick-history.csv`). It takes over the screen while it
  runs.
- **Tests:** unit tests in `flick-app` for the view math, folder sorting
  and change application, shuffles, hotkeys, the close history, the
  library, decode paths (pixels from zune vs WIC, CMYK colours, RAW
  JPEG finding, tiles) and update manifest signatures. Ignored tests double
  as measuring tools over a folder of samples (`formats_report`,
  `inspect_jpegs`, `decode_contention`, RAW memory per decode step).

## 15. Milestones

| # | Deliverable | Result |
|---|---|---|
| 0 | `flick-bench`: decoders, renderers, Honeyview baseline | WIC for JPEG; Direct2D on D3D11 + DirectComposition (§16); `bench/README.md` |
| 1 | One frameless window, one JPEG, zoom/pan, debug overlay, the open path (§3a) | Cold open 234 ms, 0 blank/flicker, 91 MB private |
| 2 | Folder paging with screen levels, preload ring | Working set flat at ~70 MB (Honeyview: 416 MB); every turn a preloaded swap |
| 3a | Resident process, tray icon, single instance, windows sharing GPU/pool/cache | Warm open 120 ms; resident idle 0 % CPU, working set ~0 after trim |
| 3b | Tabs, tab cooling, window colours, Move to Window, reordering | 20 tabs in 99 MB working set; tab switch 0.6 ms |
| 4 | Detail level, tiles for oversize pictures | Sharpest level on zoom; pictures up to ~268 MP at full resolution |
| 5 | Tab strip, hotkey table, hover chrome | Never redraws idle; not on the open path |
| 6 | Rating | Done as stars (§11); tags are optional backlog |
| 7 | Format long tail | 16 stills and 12 RAW cameras; cold JPEG open unchanged |
| 8 | File associations, installer, updates, session restore | Done (§10a, §17); Linux/macOS is backlog |

Every milestone re-runs `bench/quick.ps1` (§14): cold start is the easiest
goal to lose by accident. Latest: cold open ~212 ms, warm 115–133 ms, page
turn 16 ms median, 0 blank/flicker frames.

## 16. Decisions

Settled choices and why; each was measured or argued once and need not be
revisited without new evidence.

- **Renderer: Direct2D on Direct3D 11, presented through DirectComposition,
  in a resident process that keeps the device ready** (§3, §9). Cold open
  to the final picture at 3377×1440: Direct2D 241 ms / 118 MB; wgpu GL
  205 ms / 209 MB; wgpu DX12 296 ms / 338 MB; GDI 190 ms / 20 MB; Honeyview
  303 ms / 133 MB. Any GPU device costs ~170 ms of driver start, paid once
  per resident process. A GDI first frame with a flicker-free GPU takeover
  was built and tested (40 runs, 0 blank or flicker frames, picture at
  ~170 ms) and dropped: with a resident process it only speeds up the rare
  cold open, at the cost of two renderers.
- **Chrome is hand-drawn with Direct2D + DirectWrite**, not a toolkit:
  with no wgpu, egui would need an unproven D3D11 backend, and the surface
  is small (§9).
- **The window-open animation is off** (`DWMWA_TRANSITIONS_FORCEDISABLED`):
  DWM's fade/zoom-in delays a new window's content by ~170 ms.
- **No placeholders, no thumbnail level** (§3a, §5): only final pixels on
  open; on real files a thumbnail path saves ~14 ms.
- **No skipping while paging** (§6): every picture passed is shown.
- **The detail level is decoded on zoom**, not after a dwell; Load Full
  Resolution Ahead is the opt-in for a sharp first zoom (§5).
- **No config file of tunables** (§13); a user keys file is optional
  backlog.
- **Disk cache: shelved.** Measured (`flick-bench disk-cache`): a raw BGRA
  screen level reads back cold in ~6 ms against ~75 ms to decode the JPEG
  again; LZ4 is 22 % smaller but twice the read time and 30 ms to compress.
  Not built: it means constant disk writes while browsing, and with tab
  cooling nothing needs it. Revisit only if a RAM budget can't be met
  otherwise.
- **Trim after 30 s idle** (§3): a warm open after the trim is ~15–60 ms
  slower, while the resident's working set drops from ~71 MB to ~0.
- **Start with Windows: not offered.** A tray toggle for a `Run` key entry
  would make the first open after login warm too; it isn't built.
- **Stars, not in-file tags** (§11): files are never modified; the library
  is keyed by NTFS file ID.
- **Colour: convert to sRGB on decode** (§7); the display's profile is not
  applied.
- **Licence: GPL-3.0-or-later**, so nobody ships a closed fork. `LICENSE`
  is the FSF's text. Every crate built in is
  MIT/Apache/BSD/Zlib/Unicode/Unlicense, all compatible; their notices ship
  as `THIRD-PARTY-NOTICES.txt` (`tools\notices.ps1`).

Open:

- **Flick started without a file** shows its usage as an error. It needs a
  meaning (the close history's last windows? an Open dialog?) before the
  installer adds a Start menu shortcut.
- **Code signing.** Unsigned, the browser-downloaded setup.exe meets
  SmartScreen once (§17), and Smart App Control (Windows 11, on some clean
  installs) blocks it outright; only a signature passes that.

## 17. Distribution: icon, associations, installer, updates

- **App icon.** An aperture in a colour sweep. `assets/icon/flick.ico` (16,
  20, 24 px from the tray-tuned artwork; 32, 40, 48, 64, 256) is resource 1
  of `flick.exe`, linked with the version details through a resource script
  that `build.rs` writes. Explorer and the `DefaultIcon` registration use
  it, windows set it as their small and taskbar icons, the tray loads it at
  the system's small icon size (`icon.rs`). The version details name the
  program "Flick" in Task Manager, Settings and notifications.
- **File associations** (`register.rs`, `flick --register` /
  `--unregister`): "Open with" and a choice under Settings ▸ Default apps,
  only under HKEY_CURRENT_USER (no admin rights) and only Flick's own keys:
  a `Flick.Image` file type, the application entry, one `OpenWithProgids`
  value per extension, and `RegisteredApplications`. It never changes which
  program is the default; Windows leaves that to the user. The extensions
  come straight from `folder.rs` and `raw.rs`, so a build that learns a
  format registers it. `tools\register.ps1` / `unregister.ps1` call these
  for a dev build.
- **Installer** (`tools\installer.ps1` → `target\installer\Flick-<version>-setup.exe`,
  ~5 MB; Inno Setup 6, `installer\flick.iss`): per user, no admin, into
  `%LOCALAPPDATA%\Programs\Flick`, with an uninstall entry; it quits a
  running Flick first (`--quit`), registers the file types and ends
  offering Settings ▸ Default apps at Flick. `flick.exe` does the rest
  itself, so updates can too. The C runtime is linked in
  (`.cargo\config.toml`, +0.2 MB; page turns measured alike either way), so
  the exe runs without the Visual C++ Redistributable. Uninstalling removes
  everything but `%LOCALAPPDATA%\Flick` (settings, library, history).
- **Updates, Chrome-style** (`update.rs`; on by default, Preferences ▸
  Update Automatically).
  - *Look:* only an installed copy updates (the uninstall entry's
    InstallLocation is its folder; a dev build never replaces itself). At
    start, off the open path, and then daily, it fetches
    `https://github.com/Timsonrobl/Flick/releases/latest/download/update.txt`
    through WinHTTP (system proxy; no new crate). The manifest (version,
    exe URL, size, SHA-256) is signed ECDSA P-256; the public key is built
    in (`src/update.key`), and verification is Windows' CNG (BCrypt), also
    no crate. Forged or edited manifests are refused (a test), and only a
    version newer than this build is taken.
  - *Stage:* the exe downloads in the background to `{app}\update\`,
    checked for size and hash; a newer release replaces one waiting.
  - *Offer:* the tray icon gets a dot, its menu and the right-click menu
    "Relaunch to Update Flick (x.y.z)", and a Windows notification says so
    once; clicking it relaunches.
  - *Apply:* Relaunch quits (windows to the close history), renames the
    running exe aside, moves the new one in and starts it with
    `--reopen --after <pid>`: it waits for the old process, then starts
    resident without a picture and reopens the windows. Otherwise the swap
    happens at Quit, or at the next start (a launch that finds one waiting
    swaps, starts the new build with its arguments and exits: ~0.7 s,
    once). The new build's first start registers its file types again,
    sets the uninstall entry's version and deletes renamed copies.
  - *One warning per machine:* SmartScreen judges files a browser marked
    as downloaded (Mark of the Web); Flick's own download carries no mark,
    and a per-user folder needs no admin, so only the first setup.exe is
    ever warned about.
- **Releasing:** bump `crates/flick-app/Cargo.toml`'s version, run
  `tools\release.ps1` (installer, exe and signed manifest in
  `target\release-upload`), then `gh release create v<version> …`. The
  signing key's private half is DPAPI-encrypted for one Windows account in
  `%USERPROFILE%\.flick` (`tools\update-key.ps1`); **back it up with
  `-Backup`**: without it no installed Flick takes another update.

## 18. Backlog

Known gaps that block nothing but must not be forgotten, each with what it
would take. A commit that closes one says so and folds the result into the
sections above.

- [ ] **Version hand-off test.** A newer build taking over from an older
  resident (§3) is checked by hand only. Script it: an old resident with
  and without windows, a newer launcher; check who serves the next open
  and that the old process exits after its last window.
- [ ] **Installer edge cases.** Untested: installing over a running
  installed copy (its `--quit` before the files are replaced) and a clean
  PC. Then code signing (§16).
- [ ] **RAW+JPEG pairs.** A camera set to save both leaves `X.ARW` beside
  `X.JPG`, and paging shows each picture twice. Likely fix: list the RAW
  only where no same-name JPEG (or HEIF) sits beside it; the JPEG opens
  faster and is the camera's own rendering. A RAW opened directly still
  shows (in a folder of its pair).
- [ ] **A portrait RAW through WIC.** Every sample is landscape, or its
  embedded JPEG covers the screen. Whether the Raw Image Extension turns a
  portrait RAW's pixels itself (Flick then turning them again, by the
  RAW's IFD0 orientation) is unchecked: needs a portrait RAW with a small
  JPEG (a Sony before ~2021), zoomed in past the JPEG.
- [ ] **HDR on HDR displays.** EXR/Radiance HDR are tone-mapped to 8-bit
  sRGB on the CPU. Showing them as HDR means an RGBA16F level, a
  scRGB/HDR10 swap chain and tone mapping in a shader.
- [ ] **Format gaps:** multi-page TIFF (first page only), animated AVIF/HEIC
  sequences and animated JPEG XL (first frame), SVG detail only up to
  16 384 px, colour conversion for APNG/animated WebP and GIF, and a
  pixel-count limit (a huge JPEG XL or EXR is decoded whole before being
  scaled down; reject absurd headers before allocating).
- [ ] **Decode memory admission.** A progressive JPEG decode holds ~300 MB
  while it runs, so three preloads peak near 0.9 GB. Admission by
  estimated peak bytes (run jobs together only while their sum fits a
  budget) would cap it.
- [ ] **Multiple monitors.** The screen level covers the primary display;
  a larger secondary display gets it enlarged until a zoom decodes detail.
  Cover the largest display instead.
- [ ] **Display colour profile.** Pictures are converted to sRGB; the
  monitor's profile isn't applied (a 3D LUT in a shader would do it).
- [ ] **Linux/macOS.** Replace the Windows modules (§2, principle 10):
  renderer, decoding, single instance, tray, shell integration.

**Optional backlog** (build only if a real need shows up):

- [ ] **Tags.** Keywords per file in the library (§11), a folder of links
  per tag, an inline field to edit them.
- [ ] **Tab drag-out.** Drag a tab off the strip into a new window, or onto
  another Flick window. Move to Window covers the need.
- [ ] **A keys file.** Remapping hotkeys without a rebuild: a file read
  over `keys.rs`'s table (reloaded on change by the folder watcher), menu
  labels following it.
- [ ] **Disk tier for screen levels** (§16): only if a RAM budget can't be
  met otherwise.
- [ ] **Archives** (zip/cbz) as folders.
- [ ] **Background and minimized windows** giving up their detail level and
  swap chain.
