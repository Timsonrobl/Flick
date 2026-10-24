# Flick — architecture and principles (draft 2)

Flick is a Honeyview-class image viewer in Rust: instant to
open, frameless, tabbed, fast to page through large JPEGs, cheap in RAM
however many images are open, with quick tag editing. Windows first, nothing
that blocks Linux/macOS later.

## 1. Goals and non-goals

Goals, in priority order. When two conflict, the higher one wins.

1. **Open speed.** Double-click a JPEG in Explorer and see the final
   picture; nothing on screen before it (no placeholder, thumbnail or blank
   window). The process is normally resident (§3), so the usual open is
   warm. Targets (17–24 MP JPEG, SSD, warm OS file cache): warm open
   < 120 ms, the decode plus one present; cold open (first after login or
   after quitting from the tray) < 250 ms. Honeyview measured 303 ms.
2. **Paging speed.** Mouse-wheel through a folder of 24–45 MP JPEGs with no
   visible wait. Target: next image on screen in < 50 ms when preloaded,
   < 150 ms when not.
3. **Memory.** Baseline process < 100 MB. One open 24 MP image at fit adds
   ~35 MB. Twenty open tabs across several windows cost less than one
   Honeyview window today, because inactive tabs cool down to disk.
4. **Idle cost.** Any number of idle windows and tabs: 0 % CPU, no timers,
   no redraws.
5. **Frameless, tabbed window.** No title bar or border; move, resize and
   snap still work. All chrome, including the tab strip, is an overlay that
   appears on hover and fades. Tabs must not cost anything in goals 1–4; if
   they do, the feature shrinks, not the goals.
6. **Quick tags.** Edit keywords/rating in two keystrokes, saved without a
   dialog.
7. **Formats.** JPEG first-class; PNG, WebP, JXL, HEIC, AVIF, TIFF, GIF/APNG,
   SVG, camera RAW previews, 16-bit/HDR. New formats are plug-ins behind one
   trait.

Non-goals (for v1): editing pixels, a library/catalog database, a thumbnail
browser grid, printing, colour management beyond sRGB passthrough, plugins
loaded at runtime.

## 2. Principles

0. **Nothing before the picture that the picture doesn't need.** Opening a
   file is a critical path, measured per build. Folder listing, overlay UI,
   fonts, config beyond the essentials, metadata and decoder registries are
   all deferred until the picture is on screen. Only final pixels are ever
   shown.
1. **The screen is the unit of work.** Decode and keep what the active tab
   needs to draw at its current zoom, not what the file contains.
   Full-resolution pixels exist only for the image under the cursor, and only
   after the user has paused on it.
2. **Memory is budgeted in bytes, globally, in tiers.** One cache, one
   budget, shared by every window and tab. RAM holds the active and recently
   active tabs; disk holds the rest. Nothing is "per window" except the
   surface it draws into, and nothing is "per tab" except a path, a folder
   cursor and a view transform.
3. **Never block the UI thread.** All decoding, file listing, metadata reads
   and tag writes run on worker threads. The UI thread only transforms and
   draws textures it already has.
4. **Newest request wins.** Fast scrolling cancels everything the user has
   scrolled past. Work is coalesced, not queued.
5. **Always show something immediately.** Embedded thumbnail → screen-size
   decode → full resolution. Each step replaces the previous one in place;
   there is never a blank or frozen frame.
6. **One process, many windows, many tabs.** Second launches hand their
   arguments to the running instance (a warm start, which is the fastest
   cold start there is). One GPU device, one thread pool, one cache.
7. **Measure before optimising, and measure the right counters.** Every build
   can show its own private bytes, commit size, GPU dedicated/shared memory
   and per-stage timings in the overlay. Comparisons with Honeyview are done
   on the same counters.
8. **Decoders are replaceable.** Each format has a small adapter behind a
   trait. Swapping `image-rs` for `turbojpeg` for WIC is a one-file change and
   a benchmark, not a redesign.
9. **Files are read-only unless the user edits tags.** Tag writes are the
   only writes, they are atomic (write temp, rename) and never touch pixel
   data.
10. **Cross-platform by construction, Windows by default.** Platform code
    (hit-testing, WIC, Property System, single-instance pipe) lives behind
    `cfg(windows)` in a `platform` module with a trait the rest of the code
    uses.

## 3. Process and thread model

```
┌──────────────── process ─────────────────────────────────────────┐
│  UI thread (winit event loop, ControlFlow::Wait)                 │
│    ├─ Tray icon (resident process: open, settings, quit)         │
│    ├─ Window 1: swap chain + DComp visuals, overlay, tabs [A* B] │
│    ├─ Window 2: swap chain + DComp visuals, overlay, tabs [D*]   │
│    └─ Window N                       (* = active tab)            │
│                                                                  │
│  Shared                                                          │
│    ├─ GPU: one D3D11 device + D2D device + DComp device          │
│    ├─ ImageCache  (byte-budgeted, tiered RAM → disk)             │
│    ├─ Scheduler   (priority queue, cancellation tokens)          │
│    ├─ Decode pool (N-1 threads, byte-budgeted admission)         │
│    ├─ IO pool     (2–4 threads: dir listing, mmap, header probe, │
│    │               disk-cache read/write)                        │
│    ├─ Metadata worker (1 thread: EXIF/XMP read, tag write queue) │
│    └─ Janitor (wakes only on budget pressure or tab cooldown)    │
│                                                                  │
│  Single-instance listener (named pipe): later launches hand      │
│  their files to this process                                     │
└──────────────────────────────────────────────────────────────────┘
```

- Workers never touch the GPU device. They produce CPU buffers; the UI
  thread uploads them on its next wakeup (`EventLoopProxy::send_event`).
- The UI thread never waits on a worker. If a texture isn't ready it draws
  the best lower level it has.
- A **tab** is `{ folder cursor, ImageId, view transform, tab state }` — a
  few hundred bytes. Pixels belong to the cache, never to the tab. A
  **window** is a surface plus an ordered list of tabs and an active index.
  Only the active tab of a window is drawn; only active tabs pin cache
  entries.
- Windows are closed by dropping their surface; the device outlives them.
  **By default the process stays resident** when the last window closes,
  with the GPU device kept ready, so the next open is a warm start: no
  process start and no ~170 ms driver start, only the decode. Idle cost
  measured with the device ready: ~75 MB working set, ~97 MB private
  commit. After an idle period with no windows, the process trims its
  working set (`EmptyWorkingSet`); the next open then page-faults some of
  it back, a cost still to be measured. "Exit on last window close" is a
  setting. A tray icon shows the resident process and quits it; "start with
  Windows" (resident, no window) is an option that makes even the first open
  after login warm.

## 3a. Opening a file: the critical path

Measured end to end by `flick-bench ext-viewer` (the same way as Honeyview)
and as an internal timeline by `flick-bench startup`; shown in the debug
overlay. Targets in §1.

```
warm open (process resident, GPU device ready):
  launch → hand path to resident process over the pipe (or tray/menu open)
   ├─ IO pool:     open + mmap → header probe → disk-cache lookup for Screen
   ├─ decode pool: Screen decode (WIC, DCT-scaled to cover the window)
   │                                         ~75 ms for the user's 17 MP files
   └─ UI thread:   create window hidden (frameless, last size), swap chain,
                   DComp visual → upload → draw → present → show
  picture = decode + ~5 ms

cold open (no resident process):
t=0  process entry (no console, no runtime, no logger init yet)
 ├─ decode thread: Screen decode starts at main             done at ~110 ms
 ├─ GPU thread:    D3D11 + D2D device (driver start)        done at ~205 ms
 └─ UI thread:     event loop, window created hidden        ~30 ms
picture = when both are done: draw, present, show           ~240 ms measured

after the picture, each on its own wakeup:
 ├─ folder listing (IO pool), position in list
 ├─ overlay UI init (fonts etc.)
 ├─ full config, keymap, single-instance listener, tray icon
 ├─ preload ring, metadata read, arm dwell for Full
 └─ long-tail decoders registered lazily on first need
```

Rules that keep this honest:

- **The window appears with its picture.** It is created hidden and shown
  only after the first present of the final picture; DWM's open animation
  is disabled for it (`DWMWA_TRANSITIONS_FORCEDISABLED`, measured: it hides
  content for ~170 ms). A swap-chain present replaces the frame atomically,
  so no blank frame (measured: 0 of 10).
- **No placeholders.** Thumbnails and 1/8 previews feed the tab strip and
  paging previews, never the main view of a file being opened.
- **The binary is the startup.** Static CRT, no `tokio`, no `log` fanout at
  startup, no font database, no locale scan, no `std::env::current_dir`
  canonicalisation storms. Dependencies are checked for `ctor`/lazy-static
  work that runs at load.
- **Disk cache is a cold-start accelerator.** If the file was opened before
  on this display, its Screen level is read from the cache (LZ4-compressed
  BGRA, mmap) in parallel with the decode; whichever lands first wins.
- **Warm start is the normal case.** A second launch hands the path over a
  named pipe and exits; the resident process opens it as a new window or tab
  (by modifier/config). The launching process must do nothing else, so its
  own startup (~15 ms to `main`) is the only overhead on top of the decode.
- **Hard budgets, checked every milestone.** Time to the final picture on
  the user's burst files, measured end to end with `flick-bench ext-viewer`
  (same method as Honeyview): **warm open ≤ 150 ms, cold open ≤ 300 ms**,
  0 blank and 0 flicker frames. Over budget fails the milestone, however
  useful the feature that caused it. Between budgets, a > 10 % regression in
  the internal timeline is investigated before moving on.
- **Features stay off the open path as they grow.** Tagging (Windows
  Property System), clipboard/copy, the tray, file dialogs, fonts, the tab
  strip, folder listing and session restore are all initialised lazily,
  after the picture, off the UI thread. Heavy system DLLs (`shell32`,
  `propsys`, `comctl32`, DirectWrite) are delay-loaded. Shared objects
  (text factory, tag reader) are created once per process, never per window.
  The debug overlay's open timeline makes a creeping stage visible.

## 4. Crate layout (Cargo workspace)

| Crate | Depends on | Responsibility |
|---|---|---|
| `flick-core` | std, crossbeam | Image model (`ImageId`, `Level`, `Pixels`), cache, scheduler, budget, folder navigation, config. No GUI, no GPU, no decoder. Fully unit-testable. |
| `flick-codecs` | core + decoder crates | `Decoder` trait and adapters: turbojpeg, png, webp, jxl-oxide, gif, resvg, tiff, rawloader-preview, `image-rs` fallback, WIC fallback (windows). |
| `flick-meta` | core | EXIF/XMP/IPTC read; tag/rating write (Windows Property System, sidecar `.xmp`, exiftool fallback). |
| `flick-platform` | winit | Frameless hit-testing, snap, DPI, single-instance IPC, file associations, shell "open with". |
| `flick-app` | all of the above + `windows` (D3D11, Direct2D, DirectComposition, DirectWrite) | Event loop, windows, rendering, overlay UI, tray icon, keymap. |
| `flick-bench` | codecs, meta | CLI: decode timings and peak RSS per decoder per file; memory counters. Used in CI and by hand. |
| `flick-icon` | resvg, ico | Dev tool: renders `assets/icon/flick.svg` (32 px and up) and `flick-small.svg` (16–24 px, heavier lines for the tray) into the committed `assets/icon/flick.ico`, which `flick-app` links in via `flick.rc`. Building Flick never needs it. |

Rule: dependency arrows only point down the table. `flick-core` must build
with no C dependencies so tests run anywhere.

## 5. Image model: levels

Each open image (`ImageId` = canonical path + mtime + size) has up to four
*levels*, held independently in the cache:

| Level | Source | Size | When produced | Lifetime |
|---|---|---|---|---|
| `Thumb` | embedded EXIF/HEIC/JXL preview, else 1/8 decode | ≤ 512 px | on folder scan / first touch | cheap, kept long |
| `Screen` | scaled decode to ≥ largest window's fit size (rounded up to power-of-2 fraction) | ~2–8 MP | on navigate to / preload | the working set |
| `Full` | full decode, mipmapped texture | native | 300–500 ms after image becomes current, or on zoom > fit | one per *active* window, dropped on page-away (grace 2 s) |
| `Tiles` | region decode (JPEG DCT-domain crop, JXL crop, TIFF tiles) | 1024² tiles | only if native > GPU max texture size, or image > N MP | visible region + margin |

Each level has a CPU form (`Pixels`: RGBA8/BGRA8 or RGBA16F) and a GPU form
(a D3D11 texture / `ID2D1Bitmap1`). The CPU form is dropped right after upload except for
`Thumb`. Per principle 1, `Full`/`Tiles` for a non-current image are evicted
first; `Screen` for neighbours second; `Thumb` last.

`Screen` size is tied to the display, not the file: on a 4K monitor it is at
most 3840×2160 (×DPI); on a 1080p monitor, half that. Multi-monitor: the
largest.

## 6. Loading pipeline

```
navigate(path)
  │
  ├─ probe header (IO pool, mmap, < 1 ms): format, dimensions, orientation,
  │   has-embedded-thumb, estimated peak decode bytes
  ├─ if Thumb cached or embedded → show immediately (rotated by EXIF)
  ├─ schedule Screen decode  (priority: current > next > prev > next+1 …)
  ├─ on Screen ready → upload, replace Thumb on screen, animate nothing
  ├─ arm "dwell" timer (300–500 ms, config) → schedule Full for this window
  └─ on zoom > fit before Full is ready → draw Screen upscaled, mark "soft"
```

Scheduler rules:

- **Priorities** by distance from the current image, in the direction of
  travel: current=0, next=1, prev=2, next+1=3, next+2=4, prev-1=5 … Ring
  depth is configurable (default: 3 ahead, 1 behind).
- **Cancellation.** Every job carries a token. Navigating past an image
  cancels its pending jobs; running decodes check the token between rows /
  MCU rows and abort early where the decoder allows it.
- **Admission by bytes.** A job declares its estimated peak (from the header
  probe). The pool runs jobs in parallel only while the sum of running peaks
  ≤ the decode budget (default: 25 % of physical RAM, capped at 2 GB). A huge
  lossless WebP runs alone; four JPEG scaled decodes run together.
- **No skipping (built after milestone 3a, by request).** Paging past
  images must show them: someone scanning for a picture needs to see each
  one go by. A wheel notch moves a *target*; the picture walks there one
  image at a time, each on screen at least 25 ms (40/s, about what three
  decode threads sustain on 17 MP JPEGs). The ring spans current → target
  + 3, nearest first, so the images on the way decode in order. If the
  wheel gets more than 8 images ahead (a free-spinning wheel), the walk
  jumps to keep the lag under ~0.2 s. Home/End jump directly. An earlier
  plan showed thumbnails for passed images instead; measured, the ⅛-scale
  decode costs ~60 ms against ~74 ms for the Screen level, and the burst
  files carry no EXIF thumbnail, so a second path would buy almost nothing.
- **Direction memory.** Preload depth is biased toward the direction of
  travel (toward the target, else the last turn); reversing swaps the ring
  within one navigation.

## 7. Decoder abstraction

```
trait Decoder {
    fn probe(&self, bytes: &[u8]) -> Option<Probe>;          // dims, orient, thumb offset, peak estimate
    fn capabilities(&self) -> Caps;                          // SCALED | REGION | STREAMING | ANIMATED | EMBEDDED_THUMB
    fn decode(&self, src: Source, req: Request, cancel: &Token) -> Result<Pixels>;
}
Request = { target: ScaleHint | Region | Full, want_16bit: bool }
```

Per-format strategy (what `decode` does for `Screen`):

| Format | Primary crate | Screen strategy | Full | Notes |
|---|---|---|---|---|
| JPEG | `turbojpeg` (libjpeg-turbo) | DCT scaled 1/2–1/8 | full; restart-marker parallel later | the format that must be fastest |
| PNG | `png` / `zune-png` | stream rows → box/area downscale | full | interlaced → full then scale |
| WebP | `libwebp-sys` | built-in incremental scale | full | pure-Rust `image-webp` as fallback |
| JPEG XL | `jxl-oxide` | 1/8 preview layer, then crop/region | full | progressive by design |
| HEIC | WIC (win) / `libheif` | embedded thumb, then full → scale | full | tiled 512² internally |
| AVIF | WIC (win) / `dav1d` | full → scale | full | slowest path; thumb hides it |
| TIFF | `tiff` | reduced-resolution page if present, else strips → scale | full/tiles | |
| RAW | `rawloader` or WIC | embedded full-size JPEG preview | same | never demosaic in v1 |
| SVG | `resvg` | rasterise at target size | rasterise at zoom | vector: "Full" is per-zoom |
| GIF/APNG/aWebP | `gif`, `png`, libwebp | decode on play, ring of ≤ 8 frames | n/a | never pre-decode all frames |

**Animated GIF (built, before milestone 7).** WIC's GIF frames are composed
by `decode::compose` (logical screen, frame offsets, transparency,
disposal 0–3; delays under 20 ms play at 100 ms, as browsers do). The
first composed frame is the Screen level, so a GIF opens like any picture.
Once it has been on screen for 150 ms (so paging past GIFs costs nothing
extra), a thread decodes the remaining frames in order; each
goes to the GPU as it arrives, and playback loops over them on the
viewer's deadline timer (no frame due: nothing runs). Paging away stops the
thread. Simplification against the row above: frames are kept on the GPU
(budget 256 MB per animation); a GIF over it stays on its first frame
(§17). Pictures are now uploaded as premultiplied alpha, so transparency
shows the window background.
| EXR/HDR/16-bit | `exr`, `image` | convert to RGBA16F while decoding | same | tone-map in shader |
| anything else | `image-rs`, then WIC | full → scale | full | long tail |

Pixel-count limit (default 500 MP, config) rejects absurd headers before any
allocation.

## 8. Cache and memory budget

- `ImageCache` keys: `(ImageId, Level)` → `Entry { cpu: Option<Pixels>, gpu: Option<Texture>, bytes, last_use, pinned_by: Set<WindowId> }`.
- One **total budget** (default: 20 % of physical RAM, min 256 MB, max 1.5 GB)
  split into a CPU ledger and a GPU ledger. On an integrated GPU both ledgers
  count against RAM.
- Eviction order: unpinned `Full`/`Tiles` → unpinned `Screen` by distance
  from any window's current image → `Thumb` LRU. Pinned entries (currently
  displayed) are never evicted; if pins alone exceed budget, the budget is
  exceeded and a warning counter increments (surface it in the debug
  overlay).
- Background windows (not focused for > 5 s) unpin `Full`; minimized windows
  additionally release their swapchain.

### Tiers: RAM → disk

| Tier | Holds | Budget | Access cost |
|---|---|---|---|
| GPU | textures of pinned + warm entries | GPU ledger | 0 |
| RAM | CPU `Thumb`s; CPU `Screen`s awaiting upload or just evicted from GPU | CPU ledger | ~1 ms upload |
| Disk | `Screen` and `Thumb` for everything seen recently | size cap (default 2 GB) + age (default 30 days) | ~5–20 ms mmap + LZ4 |
| None | path, mtime, view transform (the tab itself) | — | full decode |

- Disk cache lives in the platform cache dir, keyed by
  `hash(ImageId, display-size bucket)`. Entries are LZ4-compressed BGRA with
  a small header (dims, orientation applied, colour tag). Writes happen on
  the IO pool after a `Screen` has been displayed and the UI is idle; never
  during paging.
- The **janitor** runs only when woken: by budget pressure (an admission
  that would exceed the ledger) or by a tab cooldown timer. It demotes
  entries one tier at a time. It never scans.
- Cache format is versioned; a mismatch wipes the directory. The cache is
  disposable: deleting it changes nothing but speed.
- Textures are created with no mipmaps except `Full` (mips generated on the
  GPU in one compute pass).
- Swapchain: 2 buffers, `PresentMode::Fifo`, resized lazily on the next
  draw, never speculatively.

## 9. Rendering

- One D3D11 device with a Direct2D device and a DirectComposition device on
  top, created at process start on a background thread and kept for the
  life of the resident process. Recreated on device loss (driver update,
  GPU reset). Windows-only by decision; the renderer sits behind a small
  trait so another platform can bring its own.
- Per window: a flip-model swap chain as a DirectComposition visual (the
  picture), a second visual for the overlay UI, and a D2D device context.
- Draw = `DrawBitmap` per visible level (Screen, or Full, or the visible
  Tiles) with the view transform; interpolation chosen by zoom:
  - zoom ≤ fit: draw `Screen`, linear filter.
  - fit < zoom < 1:1: draw `Full` with trilinear mips if present, else
    `Screen` linear (marked "soft").
  - zoom ≥ 1:1: `Full`/`Tiles`, linear or nearest (user toggle).
- Redraw only on: input that changes the view, a texture arriving, overlay
  animation frames (overlay fades request frames at 60 Hz for their 200 ms,
  then stop), animated image frame due, OS expose/resize.
- Checkerboard/transparency background and letterbox colour are drawn in
  the same D2D pass; no extra passes.
- Not used: wgpu (DX12: +55 ms and ~2.9× the memory of Direct2D; GL: 35 ms
  faster but ~1.8× the memory and OpenGL on Windows; Vulkan: +140 ms; see
  `bench/README.md`) and a GDI first frame (measured
  ~70 ms faster on a cold open, flicker-free handover, but two renderers;
  cold opens are rare with a resident process, so not worth the complexity).

## 10. Windows and frameless UX

- `winit` window, `with_decorations(false)`, `with_transparent(false)`.
- **Hit-testing (Windows):** subclass the HWND and answer `WM_NCHITTEST`:
  edges/corners → `HTLEFT`…`HTBOTTOMRIGHT` (8 px band, DPI-scaled), interior
  → `HTCLIENT` normally, `HTCAPTION` while the overlay's grab zone is hovered
  or while dragging with the image at fit. This gives native Aero Snap,
  Win+arrow, shake, and snap layouts for free. Other platforms:
  `drag_window()` / `drag_resize_window()`.
- `WM_NCCALCSIZE` returns the full window as client area so Windows draws no
  frame, but we keep the invisible resize border and the maximise/snap
  behaviour.
- **Hover bar** (built in milestone 1, `chrome.rs`), as in Honeyview: a
  single top bar, drawn over the picture (never shrinking it), fades in
  (100 ms) when the cursor is within **2× the bar height** of the window's
  top edge, and fades out (200 ms) when it leaves the zone or the window.
  Contents: "folder › file" title (ellipsised; index/count and tag chips
  later), and minimize / maximize-restore / close buttons on the right
  (46 px each, close hovers red). Dragging the title moves the window (with
  Snap); double-clicking it maximizes. The move starts only once the cursor
  passes the system drag threshold, so a click never enters Windows' move
  loop; buttons act on release over the same button, like native ones.
  Windows shown by our own code (the remembered maximized placement) must
  still be reported to winit with `set_visible(true)`: winit rebuilds the
  style from its own flags on every maximize/restore, and with its VISIBLE
  flag unset it hid the window, which looked like the double-click closing
  it. Drawn with Direct2D + DirectWrite
  in the picture's pass: a fade redraws the frame for its ~0.2 s, then
  nothing redraws (measured idle: 0 ms CPU over 5 s, cursor on the bar or
  on the picture). Text resources are created on first hover. **No bottom
  bar** until something truly needs one. A separate DirectComposition
  visual for the overlay, to avoid redrawing the picture during fades, is
  an optimisation for later, if ever needed.
- Window placement (`placement.rs`, milestone 1): on close, the window's
  `WINDOWPLACEMENT` (normal rect + maximized) is saved to
  `%LOCALAPPDATA%\Flick\window-placement`. The next open puts the still-hidden
  window there (for a maximized one: sized to the monitor's work area), so
  the first frame is drawn at its final size and the window appears
  maximized in one step. Measured: 235 ms, 0 blank/flicker frames. No saved
  placement: sized to the picture (≤ 90 % of the monitor), centred. `--size`
  (benchmarks) neither reads nor writes it. Later: new windows cascade from
  it; "open in new window" is a modifier on open.
- Hover bar colours: light, as in Honeyview (pale grey bar, dark text and
  glyphs, red close button with a white glyph on hover).
- Multi-window semantics: each window holds its own tabs; the cache and pool
  are shared. Closing the last window leaves the process resident (§3)
  unless "exit on last window close" is set.

## 10a. Tabs

Tabs replace Honeyview's one-image-per-window without adding cost to the
goals above, because a tab owns no pixels.

**Model.** `Tab { id, folder: FolderCursor, image: ImageId, view: Transform,
state: TabState, last_active: Instant }`. A window has `Vec<Tab>` and an
active index. Opening a file: new tab in the focused window (default), new
window with a modifier, or replace the current tab (Honeyview-style, config).

**Tab states** drive what the cache keeps for that tab's image:

| State | Entered when | Cache holds | Cost |
|---|---|---|---|
| **Active** | it is the drawn tab of a focused window | `Screen` pinned on GPU; `Full` after dwell; preload ring for its folder | full |
| **Warm** | just deactivated, or active in an unfocused window | `Screen` on GPU, unpinned; `Full` dropped after 2 s; no preload ring | ~35 MB GPU |
| **Cool** | warm for > `cool_after` (default 60 s), or budget pressure | `Screen` written to disk if not already; GPU/RAM copies evictable; `Thumb` in RAM | ~1 MB |
| **Cold** | cool for > `cold_after` (default 10 min), or pressure | only `Thumb`; restoring reads disk cache (~10 ms) or re-decodes (~100 ms) | ~0 |

Transitions downward are the janitor's job and happen only on its wakeups.
Transitions upward happen on activation: `Cold → Active` shows the
disk-cached `Screen` (or re-decodes it), then (after dwell) `Full`: the same
path as opening a file. The `Thumb` stays in the tab strip only.
Switching between two **warm** tabs is a texture swap: < 1 frame.

**Preload.** Only the active tab has a preload ring. Switching tabs
re-targets the ring to the new tab's folder position (cancellation and
coalescing as in §6). This is what keeps twenty tabs at the cost of one.

**Chrome.** The tab strip is part of the hover overlay: a row of thumbnails
(from `Thumb`, so it costs nothing new) with filename on hover. It fades
with the rest of the chrome. Keyboard: `Ctrl+Tab`/`Ctrl+Shift+Tab` cycle,
`Ctrl+W` close, `Ctrl+T` reopen last closed, `Ctrl+1..9` jump. Dragging a
tab out of the window creates a new window (later milestone). In "clean"
mode there is no visible evidence tabs exist, which is the point.

**Session.** Tabs (path, folder sort, view) are saved to the session file on
change, debounced, so a crash or "stay resident" restart restores them as
`Cold` tabs — nothing decodes until one is activated.

**Non-goals.** No tab groups, no pinned tabs, no tab search in v1.

## 11. Metadata and tags

- Read: EXIF (orientation, date, camera), XMP (`dc:subject` keywords,
  `xmp:Rating`, `xmp:Label`), IPTC keywords. Read lazily per image on the
  metadata worker; cached with the `ImageId`.
- **Write policy** (config, default = first available):
  1. In-file XMP via Windows Property System (`IPropertyStore`,
     `System.Keywords`, `System.Rating`) — what Explorer uses; JPEG/TIFF/
     HEIC/PNG supported by the OS handlers.
  2. Sidecar `<name>.xmp` (always available, never touches the image, works
     for RAW and read-only formats).
  3. `exiftool` subprocess if present on PATH.
- Writes are queued, debounced (so typing a tag doesn't rewrite the file per
  keystroke), atomic, and never block navigation. The `ImageId` includes
  mtime, so a tag write invalidates only metadata, not decoded levels —
  pixel levels are re-keyed, not re-decoded.
- UI: a key (default `T`) opens an inline tag field over the image; `1`–`5`
  set rating; recently used tags are one keystroke each. No modal dialog.
- Watch the file's directory for external changes (`notify`) to refresh
  metadata and the folder list.

## 12. Folder navigation

- Folder listing on the IO pool: directory entries filtered by supported
  extension, sorted by the configured key (name natural-order default; date,
  size, EXIF date optional — EXIF-date sort is lazy and progressive).
- The opened file is shown before the listing finishes; the position in the
  list is resolved when the listing arrives.
- Large folders (10k+ files): listing is incremental and the UI shows
  "n of ?" until done. Never read EXIF for every file up front.
- Archives (zip/cbz) are a later extension of the same `Source` abstraction
  (`Source::File`, `Source::Archive`, `Source::Bytes`).

## 13. Configuration and input

- TOML config in the platform config dir, hot-reloaded. Everything tunable
  mentioned above has a key: ring depth, dwell ms, budgets, overlay fade,
  tag write policy, sort key, zoom step.
- Keymap is data (action ↔ chord), Honeyview-compatible defaults where
  sensible so muscle memory carries over.
- **Right-click menu (built).** A native popup menu (no code runs until it
  opens; also the Menu key) with Honeyview's View items minus the
  double-page modes: Stretch Small Images (`Z`), Original Size (`0`), Fit
  in Window (`9`), Fit to Width (`8`, from the picture's top); its render
  filters: No Filter (`U`, nearest neighbour), Soften (`I`, high-quality
  cubic shrinking, linear enlarging), Soften+Sharpen (no key: `S` is random
  paging here; high-quality cubic both ways, then Direct2D's Sharpen effect
  in window space); and Sort by ▸ Name (natural) / Date modified / Size /
  Type, Descending.
- **Settings rule (user's default for every setting):** each window keeps
  its own; new windows start with the latest chosen in any window; that is
  saved in `%LOCALAPPDATA%\Flick\settings` (`settings.rs`) for the next run.
  A view mode applies to each new picture in the window; resizes re-apply
  it until the user zooms or pans. A sort change re-sorts the window's
  folder at once (100 000 files: ~40-70 ms) and the preload ring follows.
- **Mouse.** Wheel pages (walking through every picture passed); Ctrl+wheel
  zooms. Left-drag pans (nothing moves while the picture fits; only the
  title bar moves the window, so dragging means one thing); double-click
  toggles the mode's sizing and 100 %. Right-drag zooms by horizontal
  travel (right: in, left: out; 2× per 200 px) around where it started; a
  right click without moving opens the menu.
- **Held keys page as fast as pictures appear** (as Honeyview): a key's
  auto-repeats are dropped while the previous picture is still decoding or
  the walk is still under way, so no "render debt" builds up and paging
  stops when the key is released. Separate presses always count.
- **Keys** (letters and digits by position, so any keyboard layout works):
  ←/→, A/D, PgUp/PgDn, Space/Backspace page in sort order; ↓/S and ↑/W step
  forward and back in the window's random order: a shuffle of the folder
  made on the first random step (SplitMix64 Fisher–Yates, ~0.4 ms and 8
  bytes per file for 100 000), new for each window. Back exactly retraces
  forward from any picture, every picture comes once per round, and a
  re-sort doesn't change the walk. Paging in order, the two random
  neighbours join the preload ring; paging randomly, the ring runs 3 ahead
  along the shuffled order instead (with only the next random picture
  preloaded, a held random key waited on a fresh decode at every step and
  stuttered).
  Home/End, Esc, F12 (debug), +/- zoom.
- CLI: `flick <file|dir>…`, `--new-window`, `--slideshow`, `--bench`.

## 14. Observability and benchmarks

- Debug overlay (`F12`): private bytes, commit, GPU dedicated/shared (DXGI
  `QueryVideoMemoryInfo` on Windows), cache ledger per level, pool queue
  depth, last navigate → picture → full-ready timings.
- `flick-bench` CLI runs every registered decoder over a folder and prints
  per-file: decode ms (scaled and full), peak RSS delta, output dims. This is
  how decoder choices are made and re-validated; results for a reference
  image set are checked into `bench/results/`.
- Automated tests: `flick-core` (scheduler priorities, cancellation,
  coalescing, budget/eviction) are pure unit tests with fake decoders and
  a fake clock. Codec adapters have golden-image tests. The app has a
  headless smoke test (open, navigate, close) on a WARP (software D3D11)
  device. Open time and flicker are checked end to end with
  `flick-bench ext-viewer`, the same way Honeyview was measured.

## 15. Milestones

| # | Deliverable | Exit criterion |
|---|---|---|
| 0 ✓ | `flick-bench`: decoders, renderers, Honeyview baseline, handover | Done: WIC chosen for JPEG; Direct2D on D3D11 + DirectComposition chosen; see `bench/README.md` |
| 1 ✓* | One frameless window, one JPEG, zoom/pan, debug overlay, open path as in §3a | Cold open < 250 ms to the final picture, no blank or flicker frame (`ext-viewer`): **234 ms, 0/0, 91 MB**. *Mouse move/resize/snap/pan still to be checked by hand |
| 2 ✓* | Folder paging with Screen levels, preload ring, cancellation | RAM flat ✓ (~70 MB working set while paging 22 files; Honeyview grows to 416 MB); every turn a preloaded swap ✓ (< 1 ms in Flick); cold open 249 ms ✓. *Key → screen still 7–90 ms (compositor, see §16); fast mouse-wheel spinning to be judged by hand |
| 3a ✓ | Resident process + tray icon, single instance (named pipe, warm open), multiple windows sharing GPU/pool/cache, trim after 30 s idle, idle = 0 % CPU | Warm open **120 ms** ✓ (137 ms after trim), cold 241 ms unchanged, 0 blank/flicker; resident idle working set ~0 MB after trim ✓, 0 ms CPU ✓; memory levels off across opens (after the D3D11 flush fix) |
| 3b | Tabs (with a minimal tab strip in the hover bar), tab cooldown (Warm/Cool/Cold, no disk tier) | 20 tabs < 1 Honeyview window; tab switch < 1 frame for warm tabs; open path unchanged |
| 4 | `Full` level with dwell, mips, tiles for oversize | Zoom-in sharp within dwell; > 16k px images open |
| 5 | Tab strip in the hover bar, keymap, config (hover bar itself: done in milestone 1) | Chrome appears on hover, fades, never redraws idle; not on the open path |
| 6 | Tags/rating read + write, inline editor | Two-keystroke tag edit, file updated atomically |
| 7 | Format long tail (PNG/WebP/JXL/HEIC/AVIF/TIFF/RAW/SVG/animated/HDR) | Each via the trait; bench results recorded; none loaded at startup |
| 8 | Polish: file associations, installer, session restore, tab drag-out, Linux/macOS build | — |

Milestones 0–3 are the thesis; if 3 doesn't beat Honeyview on cold start,
paging and memory, stop and rethink before building chrome. Every milestone
after 1 re-runs the startup benchmark: cold start is the easiest goal to
lose by accident.

## 16. Open decisions

- **Overlay UI toolkit.** With no wgpu, egui needs a D3D11 backend
  (community crates exist; unproven here). The alternative is drawing the
  chrome directly with Direct2D + DirectWrite (native text, small surface:
  two bars, a tab strip, a tag field). Decide in milestone 5; prefer the
  hand-drawn path if the egui backend adds memory or startup cost.
- **Tag storage default.** In-file via Property System (Explorer-compatible,
  modifies the file) vs sidecar (never touches the file). Default to in-file
  for JPEG, sidecar for everything else, both switchable.
- **Dwell delay** before `Full` decode: 300 vs 500 ms. Config, tuned by use.
- **Renderer: decided (milestone 0; evidence in `bench/README.md`).**
  Direct2D on a Direct3D 11 device, presented through DirectComposition,
  in a resident process that keeps the device ready (§3, §9). Measured on
  the user's typical 17 MP file at 3377×1440, to the final picture on a
  cold open: Direct2D 241 ms / 118 MB; wgpu GL 205 ms / 209 MB; wgpu DX12
  296 ms / 338 MB; GDI 190 ms / 20 MB; Honeyview 303 ms / 133 MB. Any GPU
  device costs ~170 ms of driver start, paid once per resident process
  (+77 MB commit, +39 MB working set). A GDI first frame with a
  flicker-free GPU takeover was built and tested (40 runs, 0 blank or
  flicker frames, picture at ~170 ms). It was dropped for simplicity:
  with a resident process it only speeds up the rare cold open.
- **Window-open animation.** DWM's fade/zoom-in delays a new window's
  content by ~170 ms; Flick disables it per window
  (`DWMWA_TRANSITIONS_FORCEDISABLED`).
- **Development builds stay current** (user requirement). Each pipe request
  carries the launcher's build (its .exe's timestamp). A resident instance
  older than the launcher releases the instance mutex, answers "outdated",
  stops listening and drops its tray icon; it keeps its open windows and
  exits after the last one. The launcher then claims the instance and opens
  the file itself. `tools\build.ps1` renames a running `flick.exe` (and its
  hard-linked twin in `deps\`) before building, so the registered path
  (`tools\register.ps1`, HKCU only) always holds the newest build.
- **Elevated launches** (found 2026-10-24: Everything runs as
  administrator, so Flick opened from it did too). An elevated resident
  can't be reached by normal launches: its pipe answers "access denied"
  (no writing up an integrity level). So Flick never stays resident while
  elevated: an elevated launch that finds a resident hands its file over
  (writing down is allowed: warm opens unchanged), and one that would become
  resident restarts itself through Explorer's ShellExecute at the
  desktop's level and exits (once per resident start). Image decoding
  therefore never runs as administrator either (`elevation.rs`). A
  resident left elevated by an older build makes launches fail at once
  with a message to quit it, instead of retrying for 3 s.
- **Reopen after trimming: measured** (milestone 3a). A warm open after the
  30 s trim takes ~137 ms instead of ~120 ms, while the resident's idle
  working set drops from ~71 MB to ~0. Kept at 30 s.
- **Known gaps of the resident process:** tracked in §17.
- **Disk cache: paused** (user decision, 2026-10-24). Measured and shelved
  (`bench/README.md`): a raw BGRA screen level reads back cold in ~6 ms
  against ~75 ms to decode the JPEG again; LZ4 is not worth it on photos.
  Not built for now: it means constant disk writes while browsing, and
  nothing needs it yet. Revisit only if a RAM budget can't be met otherwise
  (e.g. many tabs, §8 tiers); until then the tab tiers end at "Cold"
  (thumbnail in RAM, re-decode on activation).
- **Latency after composition on page turns.** Flick swaps a preloaded
  texture and presents in < 1 ms, and the GPU is done in ~4 ms, but the
  picture is seen on screen ~35 ms later (median key → screen 42 ms,
  Honeyview ~140 ms). `Present(0)`, frame latency 1 and linear filtering
  were measured: no effect. The compositor runs at a steady 144 Hz
  throughout. Open: real display-path latency or lag in the screen-reading
  measurement; DXGI Desktop Duplication timestamps would separate them.
- **Preloading starts after the first picture is on screen** (`DwmFlush`
  after showing the window), on threads below normal priority, so nothing
  competes with the open path (user requirement).
- **Open-file default: new tab or replace current tab?** Honeyview replaces;
  browsers add. Default to new tab when the file is from another folder,
  replace when it is in the current folder (it's just navigation then).
- **Tab strip placement.** Top of the overlay, as thumbnails; or a
  side-docked strip for tall screens. Overlay-top first.
- **Colour management.** v1 passes sRGB through. Wide-gamut and ICC go in a
  later milestone; the shader path is designed so a 3D LUT can be added.
- **License.** Decided by the dependencies you accept: all-permissive is
  possible (turbojpeg, png, webp, jxl-oxide, resvg are MIT/BSD/Apache);
  libheif (LGPL) and dav1d (BSD) only affect bundling. GPL code from
  lightning-image-viewer or oculante is not copied.

## 17. Open gaps (backlog)

Known gaps that block no milestone but must not be forgotten. Each is
closed by a commit that says so; review this list at every milestone.

- [x] **App icon** (2026-10-24). The aperture, original colour sweep, chosen
  from twelve concepts. `assets/icon/flick.ico` (16, 20, 24 from the
  tray-tuned artwork; 32, 40, 48, 64, 256) is resource 1 of `flick.exe`:
  Explorer and `register.ps1`'s `DefaultIcon` use it, windows set it as
  their small and taskbar icons, the tray loads it at the system's small
  icon size.
- [ ] **Graphics device loss.** No recovery after a driver update or GPU
  reset (`D2DERR_RECREATE_TARGET`, `DXGI_ERROR_DEVICE_REMOVED`): recreate
  the device, surfaces and cached screen levels instead of failing the
  windows.
- [ ] **Tray icon after an Explorer restart.** Handle the
  `TaskbarCreated` message and re-add the icon.
- [ ] **Start with Windows.** Optional `Run` key entry so the first open of
  the day is warm too (a tray menu toggle).
- [ ] **Latency after composition** (§16): ~35 ms between present and the
  picture on screen during page turns. Separate display-path latency from
  measurement lag with DXGI Desktop Duplication timestamps.
- [ ] **Large animated GIFs.** Frames over 256 MB (e.g. long 1080p GIFs)
  aren't animated: stream them through a small ring instead of keeping
  them all (§7's plan). Also APNG and animated WebP, which WIC doesn't
  animate.
- [ ] **Version hand-off test.** A newer build taking over from an older
  resident has only been seen working by hand (2026-10-24); a stepped-down
  resident correctly lingers while it has windows. Add a scripted test:
  old resident with and without windows, newer launcher, check who serves
  the next open and that the old process exits after its last window.
