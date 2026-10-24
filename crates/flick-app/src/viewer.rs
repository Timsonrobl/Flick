//! One viewer window: its picture, view, folder position and hover bar.
//! What windows share (the GPU device, the decode pool and the cache of
//! screen levels) lives in `Shared`, owned by the app (ARCHITECTURE.md §3).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::Graphics::Direct2D::{D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, D2D1_INTERPOLATION_MODE_LINEAR, ID2D1Bitmap1};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::windows::WindowAttributesExtWindows;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{CursorIcon, ResizeDirection, Window};

use crate::Event;
use crate::chrome::{self, Chrome};
use crate::clock::{self, Timeline, ms_since_start};
use crate::debug::DebugText;
use crate::decode::{self, Image, Picture};
use crate::folder::Folder;
use crate::gpu::{Gpu, Surface};
use crate::instance::Request;
use crate::placement::{self, Placement};
use crate::pool::Pool;
use crate::view::{self, Affine, Fit, Mode, View};

pub type ViewerId = u64;

const BACKGROUND: D2D1_COLOR_F = D2D1_COLOR_F { r: 0.08, g: 0.08, b: 0.08, a: 1.0 };
/// Resize band at the window edge, in pixels at 100 % display scaling.
const EDGE: f64 = 8.0;
const ZOOM_STEP: f32 = 1.25;
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// The preload ring: this many images ahead of and behind the current one
/// (ahead in the direction of travel).
pub const AHEAD: usize = 3;
pub const BEHIND: usize = 1;
/// Paging faster than images decode walks through every image passed, each
/// on screen at least this long (40 a second, what the pool decodes)...
const DWELL: Duration = Duration::from_millis(25);
/// ...trailing the wheel by at most this many; past that, images are skipped.
const MAX_LAG: usize = 8;

/// Owned by the app, borrowed by every viewer.
pub struct Shared {
    pub proxy: EventLoopProxy<Event>,
    pub gpu: Option<Gpu>,
    pub pool: Option<Pool>,
    /// Screen levels of every window's ring, by path.
    pub cache: HashMap<PathBuf, Screen>,
    pub failed: HashMap<PathBuf, String>,
    /// What screen levels are decoded to cover (the monitor, or `--size`).
    pub target: (u32, u32),
}

/// What the app must do after a viewer handled an event.
pub enum Action {
    None,
    Close,
    /// The folder position changed: re-plan preloading.
    Navigated,
    /// The user chose a view mode: new windows start with it, and it is saved.
    ModeChanged(Mode),
}

/// A `Screen` level on the GPU. Cloning shares the texture.
#[derive(Clone)]
pub struct Screen {
    bitmap: ID2D1Bitmap1,
    size: (u32, u32),
    /// Stored (pre-orientation) native size.
    full_size: (u32, u32),
    orientation: u16,
    /// An animated GIF: this is its first frame, at full size.
    animated: bool,
}

impl Screen {
    pub fn upload(gpu: &Gpu, picture: &Picture) -> Result<Screen, String> {
        Ok(Screen {
            bitmap: gpu.upload(&picture.screen)?,
            size: (picture.screen.width, picture.screen.height),
            full_size: picture.full_size,
            orientation: picture.orientation,
            animated: picture.frames > 1,
        })
    }

    /// Whether drawing it at `scale` (display px per full-res px) magnifies it.
    fn magnified(&self, scale: f32) -> bool {
        scale * self.full_size.0 as f32 / self.size.0 as f32 > 1.001
    }
}

/// The picture on screen.
struct Current {
    path: PathBuf,
    screen: Screen,
    full: Option<(ID2D1Bitmap1, (u32, u32))>,
    /// Full resolution is decoded only once a zoom needs it.
    full_requested: bool,
    full_note: &'static str,
    view: View,
    /// An animated GIF's playback (its first frame is `screen`).
    anim: Option<Anim>,
}

impl Current {
    fn new(path: PathBuf, screen: Screen, window: (u32, u32), mode: Mode) -> Current {
        let view = View::new(view::oriented_size(screen.orientation, screen.full_size), window, mode);
        // An animation's frames are already full size: nothing to decode for zooming.
        let (full_requested, full_note) = if screen.animated { (true, "animation frames are full size") } else { (false, "not needed yet") };
        Current { path, screen, full: None, full_requested, full_note, view, anim: None }
    }
}

/// What the animation thread sends (Event::Anim).
pub enum AnimUpdate {
    Frame(decode::Frame),
    /// All frames sent: playback loops.
    End,
    /// Over ANIM_BUDGET: the first frame stays, still.
    TooLarge,
}

/// An animated GIF playing: its composed frames on the GPU, decoded on their
/// own thread as playback starts. Dropping it (paging away) stops the thread.
struct Anim {
    frames: Vec<(ID2D1Bitmap1, Duration)>,
    shown: usize,
    /// When the next frame is due; None while it is still decoding.
    due: Option<Instant>,
    complete: bool,
    stop: Arc<AtomicBool>,
}

impl Drop for Anim {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// GPU memory one animation's frames may take.
const ANIM_BUDGET: usize = 256 << 20;

/// One page turn: how it was served and when (wall-clock ms, comparable with
/// `flick-bench ext-viewer`'s key-press times).
struct Turn {
    index: usize,
    how: &'static str,
    handled_wall: f64,
    presented_wall: f64,
    /// With `FLICK_GPU_SYNC`: when the GPU had finished the frame.
    gpu_done_wall: Option<f64>,
    /// The compositor's frame rate at the time (Hz).
    composition_hz: Option<f64>,
}

/// Whether the cursor moved far enough from a press to make it a drag
/// (the system's drag rectangle, 4 px by default).
fn beyond_drag_threshold(from: (f64, f64), to: (f64, f64)) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXDRAG, SM_CYDRAG};
    let (dx, dy) = unsafe { (GetSystemMetrics(SM_CXDRAG).max(1), GetSystemMetrics(SM_CYDRAG).max(1)) };
    (to.0 - from.0).abs() >= dx as f64 || (to.1 - from.1).abs() >= dy as f64
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

pub struct Viewer {
    pub id: ViewerId,
    pub request: Request,
    /// Where to open (the remembered placement), unless `--size` is given.
    placement: Option<Placement>,
    pub timeline: Timeline,
    pub window: Option<Arc<Window>>,
    hwnd: Option<HWND>,
    /// The first picture, until the GPU and window are ready for it.
    pub picture: Option<Picture>,
    surface: Option<Surface>,
    current: Option<Current>,
    /// The file the user navigated to; shown as soon as it is decoded.
    pub wanted: Option<PathBuf>,
    folder: Option<Folder>,
    /// Where paging is headed; `folder.index` walks there one image at a time.
    target: usize,
    /// The sign of the last page turn.
    direction: i64,
    /// When `wanted` went on screen (or its error notice); None while it decodes.
    shown_at: Option<Instant>,
    /// Shown instead of a picture that can't be decoded.
    notice: Option<String>,
    turn_started: Option<Instant>,
    turn_started_wall: f64,
    last_turn: Option<(f64, &'static str)>,
    turns: Vec<Turn>,
    pub last_decode_ms: Option<f64>,
    wheel: f32,
    modifiers: ModifiersState,
    debug: Option<DebugText>,
    show_debug: bool,
    chrome: Chrome,
    cursor: (f64, f64),
    panning: Option<(f64, f64)>,
    last_click: Option<Instant>,
    /// The left button is down on this, and nothing has happened yet.
    pressed: Option<Press>,
    /// This window's view mode; each picture shown starts in it.
    mode: Mode,
    /// Tells the current animation's events from those of pictures passed.
    anim_serial: u64,
}

/// A left press whose meaning waits for the release or for movement.
#[derive(Clone, Copy)]
enum Press {
    /// A hover-bar button: acts on release over the same button, like
    /// native caption buttons.
    Button(chrome::Hit),
    /// Moves the window once the cursor travels past the drag threshold, so
    /// a click (or a double-click) never enters Windows' move loop.
    Move((f64, f64)),
}

impl Viewer {
    pub fn new(id: ViewerId, request: Request, placement: Option<Placement>, mode: Mode, timeline: Timeline) -> Viewer {
        let placement = if request.size.is_none() { placement } else { None };
        Viewer {
            id,
            request,
            placement,
            timeline,
            window: None,
            hwnd: None,
            picture: None,
            surface: None,
            current: None,
            wanted: None,
            folder: None,
            target: 0,
            direction: 1,
            shown_at: None,
            notice: None,
            turn_started: None,
            turn_started_wall: 0.0,
            last_turn: None,
            turns: Vec::new(),
            last_decode_ms: None,
            wheel: 0.0,
            modifiers: ModifiersState::default(),
            debug: None,
            show_debug: false,
            chrome: Chrome::new(1.0),
            cursor: (0.0, 0.0),
            panning: None,
            last_click: None,
            pressed: None,
            mode,
            anim_serial: 0,
        }
    }

    pub fn is_shown(&self) -> bool {
        self.surface.is_some()
    }

    pub fn create_window(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let (small_icon, taskbar_icon) = crate::icon::window_icons();
        let attrs = Window::default_attributes()
            .with_title(file_name(&self.request.file))
            .with_window_icon(small_icon)
            .with_taskbar_icon(taskbar_icon)
            .with_decorations(false)
            .with_visible(false)
            // DirectComposition draws everything; no GDI surface to flash.
            .with_no_redirection_bitmap(true);
        let window = Arc::new(event_loop.create_window(attrs).map_err(|e| e.to_string())?);
        let hwnd = match window.window_handle().map_err(|e| e.to_string())?.as_raw() {
            RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut _),
            _ => return Err("not a Win32 window".into()),
        };
        // DWM's open animation hides new content for ~170 ms (milestone 0).
        unsafe {
            use windows::Win32::Graphics::Dwm::{DWMWA_TRANSITIONS_FORCEDISABLED, DwmSetWindowAttribute};
            let disable = windows::core::BOOL::from(true);
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_TRANSITIONS_FORCEDISABLED, (&raw const disable).cast(), size_of_val(&disable) as u32);
        }
        self.chrome.set_scale(window.scale_factor() as f32);
        self.window = Some(window);
        self.hwnd = Some(hwnd);
        self.timeline.mark("window");
        Ok(())
    }

    /// Size and place the window for a picture of `display` pixels.
    fn place_window(&self, window: &Window, display: (u32, u32)) -> (u32, u32) {
        let monitor = window.current_monitor().or_else(|| window.primary_monitor());
        let (origin, area) = monitor.map_or(((0, 0), (1920, 1080)), |m| ((m.position().x, m.position().y), (m.size().width, m.size().height)));
        let size = self.request.size.unwrap_or_else(|| {
            let max = (area.0 as f32 * 0.9, area.1 as f32 * 0.9);
            let scale = f32::min(1.0, f32::min(max.0 / display.0 as f32, max.1 / display.1 as f32));
            (((display.0 as f32 * scale) as u32).max(320), ((display.1 as f32 * scale) as u32).max(240))
        });
        let x = origin.0 + (area.0.saturating_sub(size.0) / 2) as i32;
        let y = origin.1 + (area.1.saturating_sub(size.1) / 2) as i32;
        window.set_outer_position(PhysicalPosition::new(x, y));
        let _ = window.request_inner_size(PhysicalSize::new(size.0, size.1));
        size
    }

    /// Once the first picture, the GPU and the window are all ready: draw the
    /// picture, then show the window with it. Returns true when it did.
    pub fn try_show(&mut self, shared: &mut Shared) -> Result<bool, String> {
        if self.surface.is_some() || self.picture.is_none() || shared.gpu.is_none() || self.window.is_none() {
            return Ok(false);
        }
        let (Some(picture), Some(gpu), Some(window), Some(hwnd)) = (self.picture.take(), shared.gpu.as_ref(), self.window.clone(), self.hwnd) else {
            return Ok(false);
        };
        let display = view::oriented_size(picture.orientation, picture.full_size);
        // The remembered placement (also maximized), else sized to the picture.
        let size = match &self.placement {
            Some(saved) => placement::prepare(hwnd, saved),
            None => self.place_window(&window, display),
        };
        let surface = Surface::new(gpu, hwnd, size)?;
        let screen = Screen::upload(gpu, &picture)?;
        drop(picture); // the CPU copy of the screen level isn't kept
        self.timeline.mark("uploaded");
        self.surface = Some(surface);
        let path = self.request.file.clone();
        shared.cache.insert(path.clone(), screen.clone());
        self.wanted = Some(path.clone());
        self.current = Some(Current::new(path, screen, size, self.mode));
        self.shown_at = Some(Instant::now());
        self.render(shared)?;
        if let Some(saved) = self.placement.as_ref().filter(|p| p.maximized) {
            placement::show_maximized(hwnd, saved);
        }
        // Also after showing it ourselves: winit rebuilds the window style
        // from its own flags on every change (maximize, restore), and with
        // VISIBLE unset there it hid the window again.
        window.set_visible(true);
        // A warm open's launcher passed its foreground right on to us.
        window.focus_window();
        self.timeline.mark("picture");
        self.start_animation(shared);
        Ok(true)
    }

    /// Remembers where the window is (not for `--size` benchmark runs).
    pub fn save_placement(&self) -> Option<Placement> {
        if self.request.size.is_some() || self.surface.is_none() {
            return None;
        }
        placement::save(self.hwnd?)
    }

    pub fn on_listed(&mut self, folder: Folder, shared: &mut Shared) {
        // Re-key the first picture under the folder's own path for it.
        let path = folder.files[folder.index].clone();
        if let Some(current) = &mut self.current {
            if let Some(screen) = shared.cache.remove(&current.path) {
                shared.cache.insert(path.clone(), screen);
            }
            current.path = path.clone();
        }
        self.wanted = Some(path);
        self.target = folder.index;
        self.folder = Some(folder);
        self.redraw(); // the title gains its [i/n]
    }

    /// The way paging is going: toward the target, else the last turn's way.
    fn heading(&self) -> i64 {
        match self.folder.as_ref().map(|f| (self.target as i64 - f.index as i64).signum()) {
            Some(0) | None => self.direction,
            Some(sign) => sign,
        }
    }

    /// Ring indices around the current image, most wanted first: current,
    /// next, previous, next+1, ... in the heading's direction, reaching
    /// AHEAD past the target so the images on the way decode in order.
    fn ring(&self) -> Vec<usize> {
        let Some(folder) = &self.folder else { return Vec::new() };
        let (i, n, d) = (folder.index as i64, folder.files.len() as i64, self.heading());
        let ahead = (self.target as i64 - i).abs() + AHEAD as i64;
        let mut ring = vec![folder.index];
        for k in 1..=ahead.max(BEHIND as i64) {
            for (j, wanted) in [(i + k * d, k <= ahead), (i - k * d, k <= BEHIND as i64)] {
                if wanted && (0..n).contains(&j) {
                    ring.push(j as usize);
                }
            }
        }
        ring
    }

    pub fn ring_paths(&self) -> Vec<PathBuf> {
        let Some(folder) = &self.folder else { return Vec::new() };
        self.ring().into_iter().map(|i| folder.files[i].clone()).collect()
    }

    /// Paths this window needs kept in the cache.
    pub fn keep_paths(&self) -> Vec<PathBuf> {
        let mut keep = self.ring_paths();
        keep.extend(self.current.as_ref().map(|c| c.path.clone()));
        keep.extend(self.wanted.clone());
        keep
    }

    /// Wheel and arrow keys: move the target, then walk toward it.
    fn step(&mut self, delta: i64, shared: &Shared) -> Action {
        let Some(folder) = &self.folder else { return Action::None };
        let (index, last) = (folder.index as i64, folder.files.len() as i64 - 1);
        let target = (self.target as i64 + delta).clamp(0, last);
        if target == self.target as i64 {
            return Action::None;
        }
        self.target = target as usize;
        self.direction = delta.signum();
        // Too far behind the wheel: skip ahead to stay within MAX_LAG.
        let lag = target - index;
        if lag.unsigned_abs() as usize > MAX_LAG {
            let jump = target - lag.signum() * MAX_LAG as i64;
            self.go(jump as usize, shared);
            return Action::Navigated;
        }
        self.advance(shared);
        Action::Navigated
    }

    /// Home, End: straight there.
    fn jump(&mut self, index: usize, shared: &Shared) -> Action {
        self.target = index;
        self.go(index, shared)
    }

    /// The `deadline` came: the next paging step and/or animation frame.
    /// Returns whether the folder position moved.
    pub fn tick(&mut self, shared: &Shared) -> bool {
        self.next_frame();
        self.advance(shared)
    }

    /// One image toward the target, once the current one has been on screen
    /// for DWELL. Returns whether it moved.
    fn advance(&mut self, shared: &Shared) -> bool {
        let Some(folder) = &self.folder else { return false };
        let index = folder.index as i64;
        let due = self.shown_at.is_some_and(|t| t.elapsed() >= DWELL);
        if self.target as i64 == index || !due {
            return false;
        }
        let next = index + (self.target as i64 - index).signum();
        self.go(next as usize, shared);
        true
    }

    /// When the next paging step or animation frame is due (the app wakes
    /// for it; otherwise nothing runs).
    pub fn deadline(&self) -> Option<Instant> {
        let frame = self.current.as_ref().and_then(|c| c.anim.as_ref()).and_then(|a| a.due);
        let step = self.folder.as_ref().filter(|f| f.index != self.target).and_then(|_| self.shown_at).map(|t| t + DWELL);
        match (frame, step) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Starts decoding the current picture's animation, if it is one.
    fn start_animation(&mut self, shared: &Shared) {
        let Some(current) = &mut self.current else { return };
        if !current.screen.animated {
            return;
        }
        self.anim_serial += 1;
        let stop = Arc::new(AtomicBool::new(false));
        current.anim = Some(Anim { frames: Vec::new(), shown: 0, due: None, complete: false, stop: stop.clone() });
        let (viewer, serial, path, proxy) = (self.id, self.anim_serial, current.path.clone(), shared.proxy.clone());
        std::thread::spawn(move || {
            let send = |update| proxy.send_event(Event::Anim { viewer, serial, update }).is_ok();
            let mut bytes = 0;
            let mut over = false;
            let result = decode::animate(&path, |frame| {
                bytes += frame.image.pixels.len();
                over = bytes > ANIM_BUDGET;
                !stop.load(Ordering::Relaxed) && !over && send(AnimUpdate::Frame(frame))
            });
            if let Err(e) = result {
                clock::trace(format_args!("animation: {e}"));
            }
            send(if over { AnimUpdate::TooLarge } else { AnimUpdate::End });
        });
    }

    /// A frame (or the end) from the animation thread.
    pub fn on_anim(&mut self, serial: u64, update: AnimUpdate, shared: &Shared) {
        let (Some(current), Some(gpu)) = (&mut self.current, &shared.gpu) else { return };
        let Some(anim) = current.anim.as_mut().filter(|_| serial == self.anim_serial) else { return };
        let now = Instant::now();
        match update {
            AnimUpdate::Frame(frame) => {
                let Ok(bitmap) = gpu.upload(&frame.image) else { return };
                anim.frames.push((bitmap, frame.delay));
                if anim.frames.len() == 1 {
                    // The first frame is the one already shown: time it.
                    anim.due = Some(now + frame.delay);
                } else if anim.due.is_none() && anim.frames.len() == anim.shown + 2 {
                    // Playback was waiting for this one.
                    anim.shown += 1;
                    anim.due = Some(now + frame.delay);
                    self.redraw();
                }
            }
            AnimUpdate::End if anim.frames.len() < 2 => current.anim = None,
            AnimUpdate::End => {
                anim.complete = true;
                if anim.due.is_none() {
                    anim.due = Some(now);
                }
            }
            AnimUpdate::TooLarge => {
                current.anim = None;
                current.full_note = "animation too large to play (first frame shown)";
                self.redraw();
            }
        }
    }

    /// Shows the animation's next frame if it is due.
    fn next_frame(&mut self) {
        let Some(anim) = self.current.as_mut().and_then(|c| c.anim.as_mut()) else { return };
        let now = Instant::now();
        let Some(due) = anim.due.filter(|&due| due <= now) else { return };
        let next = anim.shown + 1;
        let next = if next < anim.frames.len() {
            next
        } else if anim.complete {
            0
        } else {
            anim.due = None; // still decoding: on_anim resumes
            return;
        };
        anim.shown = next;
        // Keep the GIF's own pace, unless far behind (a stall): then restart it.
        let base = if now.duration_since(due) > Duration::from_secs(1) { now } else { due };
        anim.due = Some(base + anim.frames[next].1);
        self.redraw();
    }

    fn go(&mut self, index: usize, shared: &Shared) -> Action {
        let Some(folder) = &mut self.folder else { return Action::None };
        if index == folder.index || index >= folder.files.len() {
            return Action::None;
        }
        folder.index = index;
        let path = folder.files[index].clone();
        self.turn_started = Some(Instant::now());
        self.turn_started_wall = clock::wall_ms();
        self.wanted = Some(path.clone());
        self.shown_at = None;
        if let Some(window) = &self.window {
            window.set_title(&file_name(&path));
        }
        if let Some(screen) = shared.cache.get(&path).cloned() {
            self.display(path, screen, "preloaded", shared);
        } else if let Some(error) = shared.failed.get(&path).cloned() {
            self.show_notice(&path, &error);
        } else {
            // Not decoded yet: the previous picture stays until it is (no
            // blank frame); the title already names the new file.
            self.redraw();
        }
        Action::Navigated
    }

    fn display(&mut self, path: PathBuf, screen: Screen, how: &'static str, shared: &Shared) {
        let window = self.surface.as_ref().map_or((1, 1), |s| s.size);
        self.current = Some(Current::new(path, screen, window, self.mode));
        self.notice = None;
        self.shown_at = Some(Instant::now());
        if self.render(shared).is_err() {
            self.redraw();
        }
        self.start_animation(shared);
        if let Some(started) = self.turn_started.take() {
            let presented_wall = clock::wall_ms();
            self.last_turn = Some((started.elapsed().as_secs_f64() * 1000.0, how));
            let composition_hz = self.surface.as_ref().and_then(|s| s.composition_hz());
            let gpu_done_wall = self.surface.as_ref().and_then(|s| s.wait_for_gpu()).map(|_| clock::wall_ms());
            self.turns.push(Turn {
                index: self.folder.as_ref().map_or(0, |f| f.index),
                how,
                handled_wall: self.turn_started_wall,
                presented_wall,
                gpu_done_wall,
                composition_hz,
            });
        }
    }

    /// A screen level arrived from the pool: show it if this window waits for it.
    pub fn on_preloaded(&mut self, path: &Path, screen: &Screen, shared: &Shared) {
        if self.wanted.as_deref() == Some(path) && self.current.as_ref().map(|c| c.path.as_path()) != Some(path) {
            self.display(path.to_path_buf(), screen.clone(), "decoded", shared);
        }
    }

    pub fn on_failed(&mut self, path: &Path, error: &str) {
        if self.wanted.as_deref() == Some(path) {
            self.show_notice(path, error);
        }
    }

    fn show_notice(&mut self, path: &Path, error: &str) {
        self.current = None;
        self.notice = Some(format!("Can't show {}\n{error}", file_name(path)));
        self.shown_at = Some(Instant::now());
        if self.debug.is_none() {
            self.debug = DebugText::new().ok();
        }
        self.redraw();
    }

    pub fn on_full_decoded(&mut self, path: PathBuf, image: Result<Image, String>, shared: &Shared) {
        let (Some(current), Some(gpu)) = (&mut self.current, &shared.gpu) else { return };
        if current.path != path {
            return; // paged away meanwhile
        }
        match image {
            Ok(image) if image.width.max(image.height) <= gpu.max_bitmap_size() => match gpu.upload(&image) {
                Ok(bitmap) => current.full = Some((bitmap, (image.width, image.height))),
                Err(_) => current.full_note = "upload failed",
            },
            Ok(_) => current.full_note = "too large for one bitmap (tiles: milestone 4)",
            Err(_) => current.full_note = "decode failed",
        }
        self.redraw();
    }

    /// "folder › file  [i/n]" for the hover bar.
    fn title(&self) -> String {
        let path = self.wanted.as_deref().unwrap_or(&self.request.file);
        let folder = path.parent().and_then(|p| p.file_name()).map(|f| f.to_string_lossy().into_owned());
        let position = self.folder.as_ref().map(|f| format!("   [{}/{}]", f.index + 1, f.files.len())).unwrap_or_default();
        match folder {
            Some(folder) => format!("{folder}  ›  {}{position}", file_name(path)),
            None => format!("{}{position}", file_name(path)),
        }
    }

    pub fn render(&mut self, shared: &Shared) -> Result<(), String> {
        let debug_text = self.show_debug.then(|| self.debug_text(shared));
        let title = self.title();
        let maximized = self.window.as_ref().is_some_and(|w| w.is_maximized());
        let scale = self.window.as_ref().map_or(1.0, |w| w.scale_factor() as f32);
        let Some(surface) = &mut self.surface else { return Ok(()) };
        let window = (surface.size.0 as f32, surface.size.1 as f32);

        let picture = match &mut self.current {
            Some(current) => {
                let magnified = current.screen.magnified(current.view.scale);
                if magnified && !current.full_requested {
                    // Zoomed past the screen level: decode full resolution,
                    // drawing the screen level (slightly soft) until it lands.
                    current.full_requested = true;
                    current.full_note = "decoding";
                    let (viewer, path, proxy) = (self.id, current.path.clone(), shared.proxy.clone());
                    std::thread::spawn(move || {
                        let image = decode::decode_full(&path);
                        let _ = proxy.send_event(Event::FullDecoded { viewer, path, image, at: ms_since_start() });
                    });
                }
                let frame = current.anim.as_ref().and_then(|a| a.frames.get(a.shown)).map(|(bitmap, _)| bitmap.clone());
                let (bitmap, size) = match (&current.full, frame) {
                    // Animation frames are the screen level's size (both full size).
                    (_, Some(frame)) => (frame, current.screen.size),
                    (Some((bitmap, size)), _) if magnified => (bitmap.clone(), *size),
                    _ => (current.screen.bitmap.clone(), current.screen.size),
                };
                let (fw, fh) = (current.screen.full_size.0 as f32, current.screen.full_size.1 as f32);
                let to_window = Affine::scale(fw / size.0 as f32, fh / size.1 as f32)
                    .then(view::orientation(current.screen.orientation, fw, fh))
                    .then(current.view.transform());
                let interpolation = if current.view.scale * fw / (size.0 as f32) < 1.0 {
                    D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC
                } else {
                    D2D1_INTERPOLATION_MODE_LINEAR
                };
                Some((bitmap, to_window, interpolation))
            }
            None => None,
        };
        let (debug, notice, chrome) = (self.debug.as_ref(), self.notice.as_deref(), &mut self.chrome);
        let fading = chrome.animate();
        surface.frame(BACKGROUND, |context| unsafe {
            if let Some((bitmap, to_window, interpolation)) = &picture {
                context.SetTransform(&to_window.to_d2d());
                context.DrawBitmap(bitmap, None, 1.0, *interpolation, None, None);
                context.SetTransform(&Affine::IDENTITY.to_d2d());
            }
            if let (Some(debug), Some(notice)) = (debug, notice) {
                debug.draw(context, notice, window.1 / 2.0 - 30.0);
            }
            if let (Some(debug), Some(text)) = (debug, &debug_text) {
                debug.draw(context, text, 48.0 * scale);
            }
            chrome.draw(context, window.0, &title, maximized);
        })?;
        if fading {
            self.redraw();
        }
        Ok(())
    }

    fn debug_text(&self, shared: &Shared) -> String {
        let mut text = String::from("open timeline (ms since process start)\n");
        for (stage, ms) in &self.timeline.stages {
            text += &format!("  {stage:<14} {ms:>7.1}\n");
        }
        if let Some(current) = &self.current {
            let s = &current.screen;
            let full = match &current.full {
                Some((_, (w, h))) => format!("{w}x{h}"),
                None => current.full_note.to_string(),
            };
            let level = match (s.magnified(current.view.scale), current.full.is_some()) {
                (false, _) => "screen",
                (true, true) => "full",
                (true, false) => "screen (soft, full not ready)",
            };
            text += &format!(
                "image   {}x{} stored, orientation {}\nlevels  screen {}x{}, full {full}\ndrawing {level}\nzoom    {:.1} %{}\n",
                s.full_size.0,
                s.full_size.1,
                s.orientation,
                s.size.0,
                s.size.1,
                current.view.scale * 100.0,
                if current.view.follows_mode { format!(" ({:?})", self.mode) } else { String::new() },
            );
        }
        if let Some(folder) = &self.folder {
            let i = folder.index;
            let mut ring = String::new();
            for offset in -(BEHIND as i64)..=AHEAD as i64 {
                let j = i as i64 + offset;
                let state = match folder.files.get(j.max(0) as usize).filter(|_| j >= 0) {
                    None => "  ",
                    Some(p) if shared.cache.contains_key(p) => "ok",
                    Some(p) if shared.failed.contains_key(p) => "x ",
                    Some(p) if shared.pool.as_ref().is_some_and(|pool| pool.is_busy_with(p)) => "..",
                    Some(_) => "- ",
                };
                ring += &format!(" {offset:+}:{state}");
            }
            text += &format!("folder  {}/{}\nring   {ring}\ncache   {} screen levels (all windows)\n", i + 1, folder.files.len(), shared.cache.len());
        }
        if let Some((ms, how)) = self.last_turn {
            text += &format!("turn    {ms:.1} ms ({how})\n");
        }
        if let Some(ms) = self.last_decode_ms {
            text += &format!("decode  {ms:.0} ms (last screen level)\n");
        }
        let (private, working) = clock::memory_mb();
        text += &format!("memory  private {private:.0} MB, working set {working:.0} MB (process)");
        text
    }

    pub fn redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Which window edge the cursor is on, for frameless resizing.
    fn edge(&self) -> Option<ResizeDirection> {
        let window = self.window.as_ref()?;
        if window.is_maximized() {
            return None;
        }
        let size = window.inner_size();
        let band = EDGE * window.scale_factor();
        let (x, y) = self.cursor;
        let (left, right) = (x < band, x >= size.width as f64 - band);
        let (top, bottom) = (y < band, y >= size.height as f64 - band);
        Some(match (left, right, top, bottom) {
            (true, _, true, _) => ResizeDirection::NorthWest,
            (_, true, true, _) => ResizeDirection::NorthEast,
            (true, _, _, true) => ResizeDirection::SouthWest,
            (_, true, _, true) => ResizeDirection::SouthEast,
            (true, ..) => ResizeDirection::West,
            (_, true, ..) => ResizeDirection::East,
            (_, _, true, _) => ResizeDirection::North,
            (_, _, _, true) => ResizeDirection::South,
            _ => return None,
        })
    }

    fn update_cursor_icon(&self) {
        let Some(window) = &self.window else { return };
        let icon = match self.edge() {
            Some(ResizeDirection::East | ResizeDirection::West) => CursorIcon::EwResize,
            Some(ResizeDirection::North | ResizeDirection::South) => CursorIcon::NsResize,
            Some(ResizeDirection::NorthEast | ResizeDirection::SouthWest) => CursorIcon::NeswResize,
            Some(ResizeDirection::NorthWest | ResizeDirection::SouthEast) => CursorIcon::NwseResize,
            None if self.chrome.hit(self.cursor_f32(), self.width()).is_some() => CursorIcon::Default,
            None if self.panning.is_some() => CursorIcon::Grabbing,
            None => CursorIcon::Default,
        };
        window.set_cursor(icon);
    }

    fn zoom(&mut self, at: Option<(f32, f32)>, factor: f32) {
        if let Some(current) = &mut self.current {
            let at = at.unwrap_or((current.view.window.0 / 2.0, current.view.window.1 / 2.0));
            current.view.zoom_at(at, factor);
            self.redraw();
        }
    }

    fn cursor_f32(&self) -> (f32, f32) {
        (self.cursor.0 as f32, self.cursor.1 as f32)
    }

    fn width(&self) -> f32 {
        self.window.as_ref().map_or(0.0, |w| w.inner_size().width as f32)
    }

    fn on_left_press(&mut self) -> Action {
        let Some(window) = self.window.clone() else { return Action::None };
        if let Some(direction) = self.edge() {
            let _ = window.drag_resize_window(direction);
            return Action::None;
        }
        let now = Instant::now();
        let double = self.last_click.is_some_and(|t| now - t < DOUBLE_CLICK);
        self.last_click = if double { None } else { Some(now) };
        let cursor = self.cursor_f32();

        if let Some(hit) = self.chrome.hit(cursor, self.width()) {
            match hit {
                chrome::Hit::Title if double => window.set_maximized(!window.is_maximized()),
                chrome::Hit::Title => self.pressed = Some(Press::Move(self.cursor)),
                button => self.pressed = Some(Press::Button(button)),
            }
            return Action::None;
        }

        let Some(current) = &mut self.current else {
            self.pressed = Some(Press::Move(self.cursor));
            return Action::None;
        };
        if double {
            current.view.toggle_zoom(cursor);
            self.redraw();
        } else if current.view.fits() {
            // Nothing to pan: dragging moves the window (with Aero Snap).
            self.pressed = Some(Press::Move(self.cursor));
        } else {
            self.panning = Some(self.cursor);
            self.update_cursor_icon();
        }
        Action::None
    }

    fn on_left_release(&mut self) -> Action {
        self.panning = None;
        self.update_cursor_icon();
        let (Some(Press::Button(button)), Some(window)) = (self.pressed.take(), self.window.clone()) else { return Action::None };
        // Released somewhere else: cancelled, as with native buttons.
        if self.chrome.hit(self.cursor_f32(), self.width()) != Some(button) {
            return Action::None;
        }
        match button {
            chrome::Hit::Close => return Action::Close,
            chrome::Hit::Minimize => window.set_minimized(true),
            chrome::Hit::Maximize => window.set_maximized(!window.is_maximized()),
            chrome::Hit::Title => {}
        }
        Action::None
    }

    /// Applies a view mode to this window (and its current picture).
    fn set_mode(&mut self, mode: Mode) -> Action {
        self.mode = mode;
        if let Some(current) = &mut self.current {
            current.view.set_mode(mode);
        }
        self.redraw();
        Action::ModeChanged(mode)
    }

    fn show_menu(&mut self) -> Action {
        let Some(hwnd) = self.hwnd else { return Action::None };
        match crate::menu::show(hwnd, self.mode) {
            Some(mode) => self.set_mode(mode),
            None => Action::None,
        }
    }

    fn on_wheel(&mut self, delta: MouseScrollDelta, shared: &Shared) -> Action {
        let steps = match delta {
            MouseScrollDelta::LineDelta(_, y) => y,
            MouseScrollDelta::PixelDelta(p) => p.y as f32 / 120.0,
        };
        if self.modifiers.control_key() {
            self.zoom(Some(self.cursor_f32()), ZOOM_STEP.powf(steps));
            return Action::None;
        }
        // Wheel down = next image, as in Honeyview. Touchpads send small
        // deltas, so whole steps are taken from an accumulator.
        self.wheel += steps;
        let whole = self.wheel.trunc();
        if whole == 0.0 {
            return Action::None;
        }
        self.wheel -= whole;
        self.step(-whole as i64, shared)
    }

    fn on_key(&mut self, key: Key<&str>, shared: &Shared) -> Action {
        match key {
            Key::Named(NamedKey::Escape) => Action::Close,
            Key::Named(NamedKey::ArrowRight | NamedKey::ArrowDown | NamedKey::PageDown | NamedKey::Space) => self.step(1, shared),
            Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowUp | NamedKey::PageUp | NamedKey::Backspace) => self.step(-1, shared),
            Key::Named(NamedKey::Home) => self.jump(0, shared),
            Key::Named(NamedKey::End) => match self.folder.as_ref().map(|f| f.files.len().saturating_sub(1)) {
                Some(last) => self.jump(last, shared),
                None => Action::None,
            },
            Key::Named(NamedKey::F12) => {
                if self.debug.is_none() {
                    self.debug = DebugText::new().ok();
                }
                self.show_debug = !self.show_debug;
                self.redraw();
                Action::None
            }
            // The View menu's keys, as in Honeyview.
            Key::Character("0") => self.set_mode(Mode { fit: Fit::Original, ..self.mode }),
            Key::Character("9") => self.set_mode(Mode { fit: Fit::Window, ..self.mode }),
            Key::Character("8") => self.set_mode(Mode { fit: Fit::Width, ..self.mode }),
            Key::Character("z" | "Z") => self.set_mode(Mode { stretch_small: !self.mode.stretch_small, ..self.mode }),
            Key::Named(NamedKey::ContextMenu) => self.show_menu(),
            Key::Character("+" | "=") => {
                self.zoom(None, ZOOM_STEP);
                Action::None
            }
            Key::Character("-") => {
                self.zoom(None, 1.0 / ZOOM_STEP);
                Action::None
            }
            _ => Action::None,
        }
    }

    pub fn window_event(&mut self, event: WindowEvent, shared: &Shared) -> Result<Action, String> {
        Ok(match event {
            WindowEvent::CloseRequested => Action::Close,
            WindowEvent::RedrawRequested => {
                self.render(shared)?;
                Action::None
            }
            WindowEvent::Resized(size) => {
                if let Some(surface) = &mut self.surface {
                    surface.resize((size.width, size.height))?;
                    if let Some(current) = &mut self.current {
                        current.view.resize((size.width.max(1), size.height.max(1)));
                    }
                    self.redraw();
                }
                Action::None
            }
            WindowEvent::CursorMoved { position, .. } => {
                let previous = self.cursor;
                self.cursor = (position.x, position.y);
                if let (Some(Press::Move(from)), Some(window)) = (self.pressed, &self.window) {
                    if beyond_drag_threshold(from, self.cursor) {
                        self.pressed = None;
                        let _ = window.drag_window();
                    }
                }
                if self.panning.is_none() && self.chrome.pointer(Some(self.cursor_f32()), self.width()) {
                    self.redraw();
                }
                if self.panning.is_some() {
                    if let Some(current) = &mut self.current {
                        current.view.pan(((position.x - previous.0) as f32, (position.y - previous.1) as f32));
                        self.redraw();
                    }
                } else {
                    self.update_cursor_icon();
                }
                Action::None
            }
            WindowEvent::CursorLeft { .. } => {
                if self.chrome.pointer(None, self.width()) {
                    self.redraw();
                }
                Action::None
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.chrome.set_scale(scale_factor as f32);
                Action::None
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
                Action::None
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => match state {
                ElementState::Pressed => self.on_left_press(),
                ElementState::Released => self.on_left_release(),
            },
            // On release, like Windows' own context menus.
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Right, .. } => self.show_menu(),
            WindowEvent::MouseWheel { delta, .. } => self.on_wheel(delta, shared),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => self.on_key(event.logical_key.as_ref(), shared),
            _ => Action::None,
        })
    }

    /// Writes the open timeline and page-turn log, if `--timeline-out` asked for it.
    pub fn write_timeline(&self, error: Option<&str>) {
        let Some(path) = &self.request.timeline_out else { return };
        let stages = self.timeline.stages.iter().map(|(name, ms)| format!("[\"{name}\",{ms:.3}]")).collect::<Vec<_>>().join(",");
        let turns = self
            .turns
            .iter()
            .map(|t| {
                let gpu = t.gpu_done_wall.map_or_else(|| "null".into(), |g| format!("{g:.3}"));
                let hz = t.composition_hz.map_or_else(|| "null".into(), |h| format!("{h:.1}"));
                format!(
                    "{{\"index\":{},\"how\":\"{}\",\"handled_wall\":{:.3},\"presented_wall\":{:.3},\"gpu_done_wall\":{gpu},\"composition_hz\":{hz}}}",
                    t.index, t.how, t.handled_wall, t.presented_wall
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let (private, working) = clock::memory_mb();
        let error = error.map_or_else(|| "null".to_string(), |e| format!("{:?}", e));
        let json = format!(
            "{{\"adapter\":\"flick\",\"stages\":[{stages}],\"turns\":[{turns}],\"private_mb\":{private:.1},\"working_set_mb\":{working:.1},\"error\":{error}}}"
        );
        let _ = std::fs::write(path, json);
    }
}
