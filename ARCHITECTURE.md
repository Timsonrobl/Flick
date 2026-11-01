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
   dialog. (Rating is built as stars, §11; keywords are optional backlog, §17.)
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
| `Full` (built as "detail") | the sharpest level the GPU holds, at once: full size (one bitmap, or `Tiles`), else the finest of 1/2, 1/4, 1/8 by JPEG's DCT scaling one bitmap holds. No levels in between: they decode nearly as slowly (Huffman decoding dominates: 1/2 took ~85 % of full's time on a 17 MP photo, a 176 MP and a 240 MP JPEG; 2026-10-25), so stepping through them only popped and competed for the CPU. Fades in over 120 ms (a D2D layer) over what was drawn. No mipmaps (Direct2D's high-quality cubic pre-shrinks) | native, or the finest fraction the GPU holds | on zoom past `Screen`; with Preferences ▸ Load Full Resolution Ahead (off by default), 500 ms after a picture settles | the current picture's; dropped on page-away and with a parked tab |
| `Tiles` (built: `tiles.rs`) | full resolution of a WIC picture over one bitmap's 16 384 px, decoded top to bottom in 256-row bands (WIC rectangles; 240 MP JPEG in 0.62 s) copied straight into the tiles on a worker thread's device context: the whole picture is never in memory | 4096² cells, each tile 8 px into its neighbours, drawn clipped to its cell (aliased) so smoothing and sharpening show no seam; only tiles in view are drawn | on zoom past 1/2 of a picture over 16 384 px, up to 1 GiB of GPU (~268 MP); larger pictures get the finest fraction one bitmap holds | the current picture's, as `Full`; Load Full Resolution Ahead never prefetches tiles |

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
- **Heavy files, fast wheel (2026-10-25).** On 24 MP top-quality
  progressive JPEGs (~1 s per decode) the walk above queued: it waited for
  each picture passed, the ring decoded them all in full, and the
  destination waited behind those on busy threads (1.1 s after a fast spin
  stopped, 0.7 s after steady scrolling; `flick-bench ext-viewer
  --wheel-bursts`). Now: the destination decodes at once on its own thread
  (`Pool::urgent`), a stale one stopping after its preview; a progressive
  JPEG gets a preview first, its first scans at ⅛ size via WIC's
  progressive levels (~90 ms), swapped for the real level when that lands
  (the view kept; no zoom decode over a preview); pictures passed get the
  preview only (`passing_paths`). The walk itself is unchanged: every
  picture passed is shown and waited for (no skipping is not negotiable:
  user, 2026-10-25; a version that skipped undecoded pictures, and one
  that gave up waiting after 250 ms, were dropped). Measured: steady
  scrolling shows the destination in 11–124 ms (sharp ~0.65 s, one full
  decode); a fast 12-notch spin shows 5–6 pictures on the way and the
  destination in 45–133 ms. Key paging unchanged (first turn ~0.38 s,
  then 8–18 ms).
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
| RAW | `raw.rs` (own parser) + WIC | the embedded JPEG, DCT scaled | the JPEG if ≥ 90 % of the RAW's size, else WIC's RAW decode | Flick never demosaics itself (built 2026-10-29, §17) |
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
thread. Frames are kept on the GPU while they fit in 256 MB per animation;
a bigger one streams through a ring of 2–8 frames instead, decoding the
file again for every loop (§17). Pictures are now uploaded as premultiplied alpha, so transparency
shows the window background.
| EXR/HDR/16-bit | `exr`, `image` | convert to RGBA16F while decoding | same | tone-map in shader |
| anything else | `image-rs`, then WIC | full → scale | full | long tail |

Pixel-count limit (default 500 MP, config) rejects absurd headers before any
allocation.

**As built (milestone 7, 2026-10-25).** No `Decoder` trait yet: one
function, `formats::kind`, picks the decoder from the file's first bytes
(SVG by extension), and `decode_screen`/`decode_full`/`animate` dispatch on
it. The table above is the plan; what each format actually uses:

| Format | Decoder | Screen level |
|---|---|---|
| JPEG | WIC | DCT scale 1/2–1/8 (unchanged) |
| JPEG, progressive (YCbCr, greyscale) | `zune-jpeg`, colour-managed and scaled through WIC (2026-10-25) | full decode, Fant scaler, then the colour profile on the small picture. WIC took twice as long on these (24 MP at top quality: 1.9 s against 1.0 s; its DCT scaling saves nothing, every scan is read anyway); pixels match WIC's within rounding (a test). Heavy set paging: cold open 1.97 → 1.06 s, first turn 1.39 → 0.38 s. Each such decode holds ~300 MB while it runs (coefficients, file, output), so three preloads at once peak near 0.9 GB, briefly. CMYK and anything WIC can't call YCbCr stay with WIC; zune failing falls back to WIC. Adobe CMYK (YCCK) progressive JPEGs (the user's slowest: 18 MP, 17 scans, ~30 MB; WIC 2.1 s alone, ~5 s with preloads beside it) don't get zune: it refuses YCCK to BGRA, and its YCCK to RGBA, though 2.4× faster, comes out far from WIC's colours (mean difference 40–50 of 255; 2026-10-25, `inspect_jpegs`). WIC scales them while decoding (CMYK accepted in `scaled`, its colours converted after, on the small picture, by WIC's own converter): screen level 3.0 → 2.0 s, and a preview in ~115 ms, matching WIC's whole-picture colours within rounding (mean 0.5–0.8, a test). Heavy decodes in parallel barely slow each other on this 16-thread machine (4 at once: +5–10 %, `decode_contention`): waits came from queueing behind busy workers, which the urgent destination decode removed |
| PNG, TIFF (8/16-bit), BMP, ICO, JPEG XR, static WebP, HEIC/HEIF, AVIF | WIC (HEIC/AVIF through Windows' HEIF + HEVC / AV1 extensions) | WIC's Fant scaler to the fit size, after converting to premultiplied BGRA (a PNG panorama of 20000 px now opens; before, its full size went to the GPU and was refused) |
| APNG, animated WebP | `image` crate (composes frames) | first frame, full size, then played like a GIF |
| GIF | WIC + `decode::compose` | unchanged |
| JPEG XL | `jxl-oxide` (WIC has a JPEG XL component on this machine that fails to initialise) | full decode, then the Fant scaler |
| SVG, SVGZ | `resvg` (system fonts, loaded on the first SVG) | rendered at the fit size; Full = rendered at the SVG's "pixel size": its own size raised to at least 4096 px on the longer side (max 16384) |
| OpenEXR, Radiance HDR | `image` crate, tone-mapped on the CPU | extended Reinhard on luminance, white point at the 99.9th percentile, sRGB; then the Fant scaler |

A picture whose full size exceeds the GPU's bitmap limit no longer gets a
Full decode that would only be refused.

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
  GPU reset; §17). Windows-only by decision; the renderer sits behind a small
  trait so another platform can bring its own.
- Per window: a flip-model swap chain as a DirectComposition visual (the
  picture), a second visual for the overlay UI, and a D2D device context.
  The swap chain is premultiplied-alpha though every frame is opaque: an
  opaque one may be put on a hardware overlay plane (multiplane overlay),
  and on an RTX 4080 that showed black on screen, never in captures, while
  the window was resized. Composed by DWM instead, it cannot. A window
  also draws inside `Resized` (resized buffers are empty until presented).
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
  Contents: the file's full path and its [index/count] as the title (cut
  in the middle when too long, keeping the file name; a tab, or a cut-short
  title, shows the whole path in a tip after a second's hover, then at once
  on the next tab), and minimize / maximize-restore / close buttons on the right
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
- Least window size (`chrome::MIN_WIDTH` × `MIN_HEIGHT`, 202 × 150 logical
  px): half the bar with the padlock, one full tab and the three buttons
  across, less high than wide, so a window never shrinks out of sight. Windows
  keeps to it on resizes and display changes (winit's min inner size, at
  each display's scaling); a placement saved smaller, or a picture's size,
  is grown to it.
- Hover bar colours: light, as in Honeyview (pale grey bar, dark text and
  glyphs, red close button with a white glyph on hover).
- Multi-window semantics: each window holds its own tabs; the cache and pool
  are shared. Closing the last window leaves the process resident (§3)
  unless "exit on last window close" is set.

## 10a. Tabs

Tabs replace Honeyview's one-image-per-window without adding cost to the
goals above, because a tab owns no pixels.

**Wanted behaviour (user, 2026-10-24; milestone 3b is the experiment that
decides whether tabs stay, so these are the goal, not all built in 3b):**
- Opening a file puts it in the **first tab of the last active window**;
  that window's other tabs shift right.
- A window with a single picture shows **no tab strip** at all.
- Each window has its own **colour** on its hover bar, so windows
  are easy to tell apart (since 2026-10-25 the "Jewel" palette: eight deep
  tones with light text, chosen over pastels, earth, Nord, stripe and vivid
  sets; the active tab is the tone brightened by a fifth, inactive tab text
  the ink part-way into the tone, tab fills rounded 4 px, the window
  square). The right-click menu gains **Move to Window ▸**,
  listing the other windows by colour plus one entry that makes a new
  window.
- Dragging the tab strip **moves the window**, like the title; **Ctrl-drag**
  reorders tabs.
- Built in 3b: the first two, tab switching and closing, per-tab memory
  tiers; measured against the 3b exit criterion (§15). Tabs stayed (user,
  2026-10-24), and the rest is built: eight colours (`chrome::PALETTE`),
  each new window taking the first one no open window uses; Move to Window
  lists the other windows with a colour swatch and the file each shows,
  then New Window (absent for a window's only tab). Up to three
  destinations stand in the main menu as a block of "Move to …" items; four
  or more go into the Move to Window submenu; with none, nothing shows. A window left without tabs
  closes. The moved tab keeps its picture, place and zoom, and pages in the
  receiving window's sort order. Tabs are reordered by Ctrl+left-drag or
  right-drag (a right click without moving still opens the menu), Chrome
  style: the dragged tab follows the cursor and takes a new slot when its
  centre passes a neighbour's; the others glide aside (exponential easing,
  50 ms time constant, redrawing only while something moves). Dragging a
  tab out to another window is not built (Move to Window covers it).
- **New windows open beside the window they come from** (Move to New
  Window; files opened with tabs off): maximized if it is; on the other
  side of the screen if it is snapped to one (`IsWindowArranged`); else the
  same size, right of it, else left, below or above, wherever it fits
  without overlapping. Only the first window uses the remembered placement.
- **Preferences ▸ Open Files in Tabs** (app-wide, saved; on by default): off
  opens every file in its own window and removes Move to New Window;
  windows that already have tabs keep them.
- **Tab Lock** (`L`, or the menu; per window, not saved; only with two or
  more tabs, else hidden and `L` ignored, and it turns off when a window is
  down to one): the keys and the wheel switch tabs instead of pictures.
  Left/Right, A/D and the wheel go along the strip (wrapping); Down/S and
  Up/W go forward and back in a random order of the tabs, made like random
  paging's (`folder::shuffled`): each tab once per round, back retracing
  forward. It is shuffled afresh when tabs come or go and kept when they
  are reordered. PgUp/PgDn and Space/Backspace still page. A padlock shows
  at the bar's left; clicking it unlocks.
- A tab switch from the keys or wheel (Tab Lock, Ctrl+Tab, Ctrl+1..9), and
  Tab Lock toggled with `L` (to see the padlock come or go), show the bar
  for 2 s (`Chrome::reveal`, on the viewer's deadline timer).
- Each window's background behind the picture is a slightly different dark
  grey (`chrome::BACKGROUNDS`, 0.055-0.135, paired with its colour), so
  overlapping windows are told apart.
- **Multi-row strip:** tabs are 220 px wide at most and wrap onto another
  row once they would be narrower than 150 px (`TAB_MIN`); every row is a
  bar high and all tabs are one width, set by filling the first row's room
  between the padlock and the window buttons (which stay on the first
  row). The rows below run the window's full width, so they hold more. Dragging reorders across
  rows (the dragged tab takes the cell nearest its centre); the others
  glide in both directions. **Preferences ▸ Multi-row Tabs** (app-wide,
  saved; on by default): off keeps one row, the tabs narrowing to fit.
- **Preferences ▸ Load Full Resolution Ahead** (app-wide, saved; off by
  default, added 2026-10-25): a picture on screen for 500 ms (paging past
  doesn't count) gets its sharpest level decoded before any zoom, so the
  first zoom is sharp at once. It costs, for every picture paused on,
  zoomed or not, a full decode (~170 ms of CPU for 24 MP) and its GPU
  bitmap (~100 MB for 24 MP) while the picture is on screen.
- **Remembered placement of a snapped window:** Windows' normal rect for a
  snapped window is the one before snapping, so `placement::save` stores
  the window's rect on screen instead (`IsWindowArranged`).
- **Opening a selection from Explorer** (one launch per file, all at once):
  files arriving within 500 ms of the previous one form a burst. The first
  opens as usual; the others become background tabs after it, in arrival
  order, and are decoded (and their folders listed) only when first viewed.
  A cold start counts as the first of a burst.
- Switching to a tab without its picture yet (a background or cooled tab)
  keeps the previous tab's picture on screen until it decodes, as paging
  does: no empty frame.
- The menu's items are chosen with the left button only. Clicks that close
  the menu can leave a press without its release, or presses queued during
  its modal loop: Flick drops drag state when the menu closes, ignores
  presses for 50 ms after in every window (a right-click on another window
  also closes the menu, and that window gets the press), acts on a release
  only if it saw the press, and keeps a drag only while the button is
  physically down (`GetAsyncKeyState`, mapped through swapped buttons). A
  menu closed by a right click reopens where that click was, in the Flick
  window under the cursor (brought forward), as Explorer's menus do.

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
`Ctrl+W` close, `Ctrl+Shift+T` reopen last closed, `Ctrl+1..9` jump. Dragging a
tab out of the window creates a new window (later milestone). In "clean"
mode there is no visible evidence tabs exist, which is the point.

**Session: the close history** (built; `session.rs`). Restoring is undoing
closes, newest first: `Ctrl+Shift+T` (or a click on the tray icon with no
window open) reopens the last close, each press an earlier one, up to 50
back; a new close becomes the last. A close is a tab (Ctrl+W, a middle click
on it) or whole windows: the close button or a window's last tab (one window,
all its tabs), Quit (every window at once), and a crash, kill or shutdown
(every window the process had open, found at the next start). Moving a tab
to another window, deleting its file, and a tab dropped because its file
vanished aren't closes.
- A tab goes back to its window, at its place in the strip, if that window
  is still open (window ids count only within one run); else after the
  active tab of the window used last, or into a window of its own. A window
  comes back at its placement, with its tabs, active tab, menu settings
  (stored in the settings file's form), colour (if free) and Tab Lock. Each
  tab keeps its Rotate ▸ turn and its zoom and pan (`View::custom_zoom`, if
  set by hand); a cooled tab now keeps its zoom too.
- The active tab of a reopened window decodes on the open path; the others,
  and a tab reopened into a window, wait as background tabs do (decoded when
  viewed).
- A reopened tab whose file can't be shown (gone, or its drive) is
  *unavailable*: just the background, its name on the hover bar. The folder
  is listed with the missing file in its sorted place, so the arrows and the
  wheel go on from there; if the folder is gone too, only `[` `]` work, the
  gone folder sorting among its siblings where it was.
- Files in `%LOCALAPPDATA%\Flick`: `closed.json` (the history, read and
  rewritten on each change: other Flick processes may share it) and
  `open-<process id>.json`, this process's open windows, written a second
  after a change (and at once on a close; nothing while idle), removed when
  the last window closes or at Quit. At start, files of processes no longer
  running each become one close. Writes go through a temporary file and a
  rename. `--standalone` and `--size` runs keep no history.

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

**Stars (built 2026-10-25, before tags; the plan above is for tags).**
Replaces Honeyview's "Save to photo folder" (a copy per favourite, and
only starred / not starred):

- `Ctrl+F` adds a star to the picture on screen, `Shift+F` takes one away; any
  number of stars. A brief "★★★" note confirms; the info panel shows them
  and since when; Sort by ▸ Stars (most first) orders a folder by them,
  Sort by ▸ Date starred by when each got its first star (oldest first,
  or newest with Descending; unstarred files last). Hard links share their
  file's dates, so the starred folder can't be sorted by when entries
  were made the way copies could (by their date modified): the library
  keeps the date (`starred`, schema version 2; version 1 libraries are
  upgraded on open, their earlier stars undated), and Date starred works
  in the starred folder too.
- **Files are never modified.** Stars live in Flick's library
  (`library.rs`, SQLite, `%LOCALAPPDATA%\Flick\library.db`), keyed by the
  file's NTFS ID (volume serial + 128-bit file ID: survives renames and
  moves on its drive, never reused), with its last known path and folder
  for looking a whole folder up in one query (a file moved since starring
  is found by ID when viewed, and its path updated then). Opened on first
  use, never at startup.
- **The starred folder** (Preferences ▸ Starred Folder…, asked for on
  the first star): the first star puts a **hard link** there (another name
  for the same file: no copy; a file and its link share their ID, so their
  stars), or a **copy** for a file on another drive; the last star taken
  away removes it (a link only while the file has another name, never a
  file's only name). Name clashes get "name (2)". Deleting the original
  keeps the star (the file lives on under that name).
- **The starred folder decides what is starred:** an entry deleted in
  Explorer unstars its file. Seen at once by a folder watcher if a tab is
  in that folder, otherwise by the daily sweep, run when the resident goes
  idle: records whose entry is gone are dropped, records whose original
  is gone point at their entry. Renames seen by watchers update records.
- **Tags later** fit the same library: `tags (id, name)` and `file_tags
  (volume, file_id, tag)` keyed like `files`; a tag can get its own folder
  of links with the same link and cleanup code.

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
- **Watched folders** (built 2026-10-25, `watch.rs`). Windows can't count a
  folder's files without listing them, but it can report changes: one
  watcher per folder some tab pages through, shared by every tab and window
  in it (`App::sync_watchers`, after each `plan`), is a thread waiting on
  `ReadDirectoryChangesW` (new, removed, renamed files; size or date
  changes), costing nothing while the folder is quiet. A folder's changes
  are applied once it has been quiet for 250 ms (a file being written
  reports many), to every tab in it, each with its own sort order:
  `Folder::apply` inserts new images where the order puts them (binary
  search), removes and re-sorts in place, keeps the current file current
  (a removed one is followed by its successor; a renamed one is
  followed), and the paging target stays on its file. A rewritten file is
  dropped from the cache and decoded again, the one on screen too (its old
  picture stays until then). If Windows' change buffer overflows, every tab
  in that folder lists it again. Random paging starts a new shuffle after
  a change. Not covered: a file created in the moment between listing a
  folder and its watcher starting.

## 13. Configuration and input

- **No config file of tunables** (decided 2026-10-27). Ring depth, dwell,
  budgets, fades, zoom step and the like stay constants in the code: most
  were set by measuring against §1's goals, and a file would invite
  changes that quietly break them (and add a read to startup). A tunable
  someone needs becomes a menu choice, saved by the settings rule below,
  as the slideshow interval is. With no file there is nothing to reload.
- **Hotkeys are one table** (`keys.rs`, built 2026-10-27): `BINDINGS`
  pairs each chord with a `Command`. The key handler looks a key up there
  and runs the command (`Viewer::run`, which the menu's items call too),
  and the menu shows each command's keys from the same table, so the two
  can't disagree; changing a key is one line. Honeyview's keys where
  sensible, so muscle memory carries over. A chord names its key by
  position (letters, digits, brackets: any layout), as a named key
  (arrows, Tab, Enter, F12) or as a character ("+", which takes Shift on
  most layouts, so Shift doesn't count for characters). Modifiers match
  exactly: Ctrl+Shift+C is not Ctrl+C. Each command says whether a held
  key repeats it (paging, tab switching, zoom, closing tabs) or runs once
  per press, whether it pages (a held paging key waits for the picture
  on screen) and whether it stops a slideshow. With Tab Lock on, the
  paging commands switch tabs, from the keys and the menu alike. A user
  keys file is in the optional backlog (§17).
- **Right-click menu (built).** A native popup menu (no code runs until it
  opens; also the Menu key) with Honeyview's View items minus the
  double-page modes: Stretch Small Images (`Z`), and the view modes in
  middle click's order: Fit in Window (`9`), Fill Window (`6`: whichever of
  Fit to Width and Fit to Height fills the window, so the picture overflows
  one way; added 2026-10-25), Fit to Width (`8`), Fit to Height (`7`;
  added 2026-10-25), Original Size (`0`); every mode centres the picture,
  on the axis it overflows and pans too (Fit to Width started at the top
  and Fit to Height at the left until 2026-10-25: paging then jumped
  between edge and centre); its render
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
  title bar moves the window, so dragging means one thing); double-click on
  the picture duplicates the tab, as Ctrl+T (it toggled 100 % until
  2026-10-25: redundant with middle click's cycle). Right-drag zooms by horizontal
  travel (right: in, left: out; 2× per 200 px) around where it started: the
  picture point pressed stays under the cursor (a picture smaller than the
  window grows from there, not from the centre, until it meets an edge).
  During the drag nothing holds the picture's edges to the window's: the
  zoom stays purely around the pressed point; on release the picture
  glides (150 ms, eased) to where the edge rules put it. Held to them
  during the drag, an edge that met the window's took over as the zoom's
  centre, the picture slid the other way, then the zoom moved back once
  the picture outgrew the window: a jiggle (2026-10-25, with a test; a
  first fix that zoomed each step from the last clamped place still did
  it). Right-drag holds at 100 % for 24 px of travel as it passes, and
  Ctrl+wheel and `+`/`−` stop at 100 % on the way through. A right click
  without moving opens the menu. A picture left off-centre by
  a zoom around a point stays there: panning moves it only on axes where
  it is larger than the window, and zooming or resizing only keeps it
  inside; the view modes (and Fit to Window below) centre it again.
  Middle click on the picture
  steps through the view modes in the menu's order, each set exactly as the
  menu sets it (the window's mode, so new windows take it and it is saved),
  around the click where the picture overflows (the point clicked taken
  before the mode changes the view: taken after, the zoom missed it); then, as one more stop
  after Original Size, the last zoom set by hand, which applies only to the
  picture shown and leaves the mode alone (remembered per tab, as scale and
  centre relative to the picture, when middle click leaves it; panning in a
  mode isn't a zoom by hand). Stops that look the same here as the picture
  now or as an earlier stop are skipped (Fit to Width where it is Fit in
  Window, say). (2026-10-25: middle click and the menu used to disagree,
  the cycle being temporary.) Middle click on a tab closes it.
- **On-screen text** (`osd.rs`), just below the hover bar's first row (the
  bar draws over it): the zoom level shows for 1.2 s, in the info panel's lettering (Segoe UI 17 px, near-white) on a faint pill, at
  the top right after any zoom, named by the mode when it is the mode's
  ("Fill window · 47 %", else "Zoom · 150 %"), as do "Copying…"/"Copied to
  clipboard". `Tab` toggles the info
  panel at the top left, and a click on it closes it (a setting like the View menu's: per window,
  latest wins, saved; also in the menu as File Info): file name and
  position in the folder, picture size, format details (JPEG chroma
  subsampling and progressive scan, 16-bit, animation frames), file size
  and date, and EXIF camera, lens, exposure and date taken. Read through
  WIC's photo metadata names on a worker thread (`meta.rs`) the first time
  the panel shows a picture. Text at 14 px (Honeyview's size; 17 px read
  as too big; the notes match), 1.65× line spacing, on the dark box (an
  outlined box-less version was tried and read badly). Where the picture
  leaves a border on the left at least 240 px wide, the panel fits in it,
  off the picture; else it wraps at 400 px. Labels make a column and long
  values wrap within theirs (a hanging indent); a file name may break
  after every `-` and `_` (Unicode keeps `-` with a following digit, which
  left a short line before a date), and its `[i/n]` stays with the name's
  end. Also shown, as Honeyview does: the colour profile on the Image line
  ("sRGB", "Display P3"), Software, and the 35 mm equivalent focal length
  where it differs.
- **Rotate** (menu, `Alt+↑/→/↓/←` for 0/90° clockwise/180°/90°
  counter-clockwise, `Ctrl+R` the next of those in turn, `Alt+H`/`Alt+V`
  mirror, `Alt+F` cycles not mirrored → horizontally → vertically; both
  ways is a 180° turn, which `Ctrl+R` reaches): per tab, for every picture
  paged to in it, on top of EXIF orientation (`view::Turn::after` composes
  the two into one of the eight orientations, so drawing, sizing and copying
  need nothing new). Not kept by default; Rotate ▸ Persist makes it a
  window setting like the others (new tabs and windows start with it,
  saved).
- **Delete** (`Del`, menu: to the Recycle Bin; `Shift+Del`: for good)
  always asks first in Windows' own message box (no UI of Flick's), then
  shows the next picture; a folder's only picture closes its tab.
- **Slideshow** (`Ctrl+G` starts and stops; Slideshow ▸ in the menu with
  250 ms–30 s presets; `Ctrl+[` `Ctrl+]` adjust the interval by about a tenth, 10 ms
  steps at the least, 50 ms–60 s; saved like a setting): moves the way the
  last move went (in order, wrapping at the end, or random; through the
  tabs under Tab Lock, quietly, without revealing the bar). Each picture
  stays the interval from when it is on screen (`slide_due`), so a slow
  decode delays the next step instead of shortening it or letting steps
  pile up; preloading already runs ahead in the slideshow's direction.
  Only moving by hand (the paging keys, Home/End, the wheel, Ctrl+Tab,
  Ctrl+1–9, `[` `]`) or `Ctrl+G` stops it. (Its interval keys are
  `Ctrl+[` `Ctrl+]`.)
- **Folders:** `[` and `]` go to the folder before or after this one among
  its siblings, ordered as the pictures are (by name, natural order, or
  date modified; by name for keys folders don't have; Descending too),
  skipping hidden folders and ones without pictures; listed on a worker
  thread, at its first picture, with a note naming it. Paging past the
  first or last picture says so in a note ("First picture" / "Last
  picture").
- **History** (`history.rs`, added 2026-10-25): each tab remembers every
  picture it showed, the ones the wheel walks through too (no dwell
  filter: 50 deep is plenty, and a push costs nothing), up to 50 back and
  50 forward, a few KB at most. The mouse's side
  buttons (Back, Forward) and the keyboard's Back and Forward keys walk it
  as a browser does: moving on any other way
  drops the way forward; a picture in another folder lists that folder
  (worker thread, as `[` `]`); deleted files are skipped; at either end a
  note says there's nothing further. No other UI. They stop a slideshow,
  as moving by hand does.
- **Ctrl+C** copies the picture at full resolution, upright (EXIF
  orientation applied), as a 32-bit DIBv5 with straight alpha; decoded on
  a worker thread. **Ctrl+Enter** opens the folder in Explorer with the file
  selected. **Ctrl+T** duplicates the tab into the background, right after
  this one (the bar shows for a moment); **Ctrl+N** opens the file again in
  a new window beside this one.
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
  Home/End, Ctrl+W (close the tab; the last one closes the window; Esc
  does nothing, by request), Ctrl+Shift+T (reopen the last close, §10a),
  F12 (debug), +/- zoom.
  A plain hotkey works on its own only: with Ctrl or Alt, keys without a
  Ctrl/Alt binding do nothing; with Shift, only Shift+F, Shift+Del,
  Ctrl+Shift+Tab, Ctrl+Shift+T and "+" work. The wheel pages without modifiers only
  (Ctrl+wheel zooms).
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
| 3b (first cut) | Tabs (with a minimal tab strip in the hover bar), tab cooldown (warm → cold after 3 min, no disk tier, no cap on warm tabs: user decision) | 20 tabs: working set **99 MB** (Honeyview: 153–416 MB for one window) ✓; GPU 410 MB warm → **172 MB** once cooled; warm tab switch **0.6 ms** to present ✓; open into a tab **83–93 ms** request → on screen (warm open into a new window ~102) ✓. Window colours, Move to Window, Ctrl-drag reorder: after the user's verdict on tabs |
| 4 ✓ | `Full` level with dwell, mips, tiles for oversize | Zoom-in sharp within dwell; > 16k px images open. Part A (2026-10-25): zooming past `Screen` decodes the sharpest level the GPU holds (intermediate levels tried and dropped: barely faster, they popped in steps), fading in; optional dwell prefetch; mips dropped (Direct2D's high-quality cubic pre-shrinks). Part B (2026-10-25): full resolution in tiles for WIC pictures over 16 384 px, up to 1 GiB of GPU (§5) |
| 5 ✓ | Tab strip in the hover bar, keymap, config (hover bar itself: done in milestone 1) | Chrome appears on hover, fades, never redraws idle; not on the open path. Keymap: one bindings table (§13, 2026-10-27); no config file of tunables (decided, §13); a user keys file is optional backlog (§17) |
| 6 ✓ | Tags/rating read + write, inline editor | Rating is done as stars (§11: Ctrl+F / Shift+F, kept in the library, files never modified). Tags: moved to the optional backlog (§17; user decision, 2026-10-27) |
| 7 ✓* | Format long tail (PNG/WebP/JXL/HEIC/AVIF/TIFF/RAW/SVG/animated/HDR) | Each via the trait; bench results recorded; none loaded at startup. 19 samples open with 0 blank/flicker, cold JPEG unchanged (212 ms); `bench/README.md`. RAW added 2026-10-29 (§17): 12 cameras' samples, screen level 12–80 ms. *No `Decoder` trait yet: `formats::kind` dispatches |
| 8 | Polish: file associations, installer, session restore (done: the close history, §10a), Linux/macOS build (tab drag-out: optional backlog, §17) | — |

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
- **Colour management.** Embedded profiles other than sRGB are converted to
  sRGB on decode (WIC colour transform; §17); the display's own profile is
  not applied (sRGB output assumed). The shader path is designed so a 3D
  LUT can be added for that.
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
- [x] **Graphics device loss** (closed 2026-10-25). A lost device (driver
  update, GPU reset or hang, adapter removed) is noticed three ways: a call
  failing with `DXGI_ERROR_DEVICE_REMOVED`/`_RESET` or
  `D2DERR_RECREATE_TARGET`, `GetDeviceRemovedReason` before each frame,
  and Windows' device-removed event (`ID3D11Device4`), waited on by a
  thread so an idle window recovers too. The app then drops every
  window's surface, every tab's picture and the cache, makes a new device
  (on the UI thread: a rare ~170 ms), gives each shown window a new
  surface and decodes its picture again through the pool, back at the zoom
  and pan it had (every tab's); windows stay open. Shift+F12 simulates a loss to try it.
- [x] **Tray icon after an Explorer restart** (closed 2026-10-25). The
  tray's window is a hidden top-level window (message-only windows miss
  broadcasts) that re-adds the icon on `TaskbarCreated`.
- [-] **Start with Windows** (not planned: user decision, 2026-10-25).
  Optional `Run` key entry so the first open of the day is warm too (a
  tray menu toggle).
- [ ] **Latency after composition** (§16): ~35 ms between present and the
  picture on screen during page turns. Separate display-path latency from
  measurement lag with DXGI Desktop Duplication timestamps.
- [x] **Large animations** (2026-10-25). An animation whose frames would
  take over 256 MB on the GPU (a video saved as GIF or WebP) used to play
  ~30 frames at 1080p and then stop on its first frame. It now streams:
  the decoding thread stays 2–8 frames ahead (up to 64 MB, credits in
  `Ring`), each frame is dropped once shown and its bitmap refilled with
  a later one, and every loop decodes the file again. 1080p, 300 frames:
  GIF decodes at 6.8 ms per frame, animated WebP at 25.9 ms, both under
  their 40 ms per frame of playback.
- [x] **RAW** (built 2026-10-29; milestone 7's last format). Cameras store
  a JPEG inside each RAW; `raw.rs` finds them with a few small reads (under
  1 ms): the TIFF IFDs and sub-IFDs (CR2, NEF, ARW, DNG, PEF, RW2's
  JpgFromRaw, ORF's maker note), CR3's tracks and PRVW box, RAF's header.
  Each candidate is checked by its frame header, so lossless-JPEG sensor
  data is never taken; orientation comes from the RAW's IFD0. WIC's RAW
  decoder (the Raw Image Extension) is only opened for the RAW's size,
  2–5 ms. A JPEG at least 90 % of that size stands for the RAW at every
  level; a smaller one (Sony before ~2021: 1616×1080; Panasonic, Olympus,
  Fujifilm) gives the screen level where it covers the screen, else a
  quick preview, and WIC decodes the RAW for zooming in. 12 public
  samples (raw.pixls.us, `bench/samples/raw`): screen level 12–80 ms
  where WIC's demosaic takes 0.6–2.5 s; full level 47–106 ms from a
  full-size JPEG. The info panel's EXIF works through WIC unchanged.
  Phone DNGs: Windows' DNG codec shows the DNG's own preview too (a
  Pixel's is 1280×964), so nothing larger exists to zoom into. Not yet
  verified: a portrait RAW through WIC (whether the Raw Image Extension
  turns the pixels itself); RAW+JPEG pairs in a folder show both.
- [x] **Colour management** (closed 2026-10-25, to sRGB). A scan of the
  user's 164 000 downloads: 90 % no profile, 6 % sRGB, ~2 % wide gamut
  (Display P3, Adobe RGB, a few others). WIC formats now read the frame's
  colour contexts; an ICC profile that isn't sRGB (by its description) or
  gray, or EXIF's Adobe RGB, is converted to sRGB with WIC's colour
  transform; everything else costs one `GetColorContexts` call. Measured
  on a 24 MP Display P3 JPEG: DCT-scaled screen level +25 ms (converted
  after decoding, on the small picture), Fant-scaled none (converted after
  scaling), full resolution +390 ms (zoom and Ctrl+C only; off screen).
  Not covered: APNG/animated WebP (image crate) and GIF; JPEG XL renders
  to sRGB already. The display's own profile isn't applied.
- [ ] **HDR on HDR displays.** EXR/Radiance HDR are tone-mapped to 8-bit
  sRGB on the CPU; §7's plan is RGBA16F and tone mapping in a shader.
- [ ] **Format details left out of milestone 7:** multi-page TIFF (first
  page only), animated AVIF/HEIC sequences and animated JPEG XL (first
  frame), SVG Full level only up to its 4096–16384 px size, and §6's
  pixel-count limit (a huge JPEG XL or EXR is decoded whole before being
  scaled down).
- [ ] **Version hand-off test.** A newer build taking over from an older
  resident has only been seen working by hand (2026-10-24); a stepped-down
  resident correctly lingers while it has windows. Add a scripted test:
  old resident with and without windows, newer launcher, check who serves
  the next open and that the old process exits after its last window.

**Optional backlog** (user decision, 2026-10-27: not sure they're needed;
build only if a real need shows up):

- [ ] **Tags.** Keywords per file, as planned in §11 (the library's
  `tags` / `file_tags` tables, a folder of links per tag). Rating is done
  as stars.
- [ ] **Tab drag-out.** Drag a tab off the strip into a new window, or onto
  another Flick window. Move to Window (menu) covers the need for now.
- [ ] **A keys file.** Remapping hotkeys without a rebuild: a file read
  over `keys.rs`'s table (and reloaded on change, with the folder watcher),
  menu labels following it. Worth it for a public release.
- [ ] **An installer** (for other people; the daily copy stays the latest
  dev build, registered by `tools\register.ps1`). Cheapest form: an
  unsigned per-user Inno Setup package (no admin; Start menu, uninstaller,
  file types, then Settings ▸ Default apps opened at Flick), with the C
  runtime linked statically and the dependencies' licence notices. Needs
  first: a licence (§16) and a public place for releases. Code signing
  (to avoid SmartScreen's warning) and update checks only for a real
  release. It registers the RAW types too, as `tools\register.ps1` now does.
