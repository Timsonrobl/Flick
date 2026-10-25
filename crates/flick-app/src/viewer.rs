//! One viewer window: its picture, view, folder position and hover bar.
//! What windows share (the GPU device, the decode pool and the cache of
//! screen levels) lives in `Shared`, owned by the app (ARCHITECTURE.md §3).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{
    D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, ID2D1Bitmap1,
};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::platform::windows::WindowAttributesExtWindows;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{CursorIcon, ResizeDirection, Window};

use crate::Event;
use crate::chrome::{self, Chrome};
use crate::clock::{self, Timeline, ms_since_start};
use crate::debug::DebugText;
use crate::decode::{self, Image, Picture};
use crate::folder::{self, Folder, Sort};
use crate::gpu::{Gpu, Sharpen, Surface};
use crate::instance::Request;
use crate::placement::{self, Placement};
use crate::pool::Pool;
use crate::clipboard;
use crate::library;
use crate::menu;
use crate::meta;
use crate::osd::Osd;
use crate::settings::{Global, Prefs};
use crate::history::History;
use crate::view::{self, Affine, Filter, Fit, Mode, Stop, Turn, View};

pub type ViewerId = u64;

/// Resize band at the window edge, in pixels at 100 % display scaling.
const EDGE: f64 = 8.0;
const ZOOM_STEP: f32 = 1.25;
/// How long the tab strip shows after a tab switch from the keys or wheel.
const REVEAL: Duration = Duration::from_secs(2);
/// Left-drag zoom: this much horizontal travel (at 100 % scaling) doubles
/// or halves the picture.
const ZOOM_DRAG: f64 = 200.0;
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
    /// The user changed a menu setting: new windows start with it, it is
    /// saved, and (sort order) preloading follows the new order.
    PrefsChanged(Prefs),
    /// Right click: the app opens the menu (`show_menu`), which needs the
    /// other windows for Move to Window.
    Menu,
    /// Move the active tab to that window, or to a new one (None).
    MoveTab(Option<ViewerId>),
    /// Preferences ▸ changed (app-wide, saved).
    GlobalChanged(Global),
    /// The menu closed on a right click elsewhere: open it in the window
    /// under the cursor (this one or another).
    ReopenMenu,
    /// Ctrl+D (a background tab beside this one) or Ctrl+N (a new window):
    /// the same file again.
    Duplicate { file: PathBuf, new_window: bool },
}

/// A `Screen` level on the GPU. Cloning shares the texture.
#[derive(Clone)]
pub struct Screen {
    bitmap: ID2D1Bitmap1,
    size: (u32, u32),
    /// Stored (pre-orientation) native size.
    full_size: (u32, u32),
    orientation: u16,
    /// More than one: an animation, and this is its first frame, at full size.
    frames: u32,
}

impl Screen {
    pub fn upload(gpu: &Gpu, picture: &Picture) -> Result<Screen, String> {
        Ok(Screen {
            bitmap: gpu.upload(&picture.screen)?,
            size: (picture.screen.width, picture.screen.height),
            full_size: picture.full_size,
            orientation: picture.orientation,
            frames: picture.frames,
        })
    }

    fn animated(&self) -> bool {
        self.frames > 1
    }

    /// One frame's size on the GPU (animation frames are full size).
    fn frame_bytes(&self) -> usize {
        self.size.0 as usize * self.size.1 as usize * 4
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
    /// The info panel's details (`meta`), read when the panel first shows it.
    info: Option<meta::Info>,
    info_requested: bool,
    /// Its file was rewritten: shown until the new version is decoded.
    stale: bool,
    /// How it's drawn: its EXIF orientation with the tab's turn on top.
    orientation: u16,
}

impl Current {
    fn new(path: PathBuf, screen: Screen, window: (u32, u32), mode: Mode, turn: Turn) -> Current {
        let orientation = turn.after(screen.orientation);
        let view = View::new(view::oriented_size(orientation, screen.full_size), window, mode);
        // An animation's frames are already full size: nothing to decode for zooming.
        let (full_requested, full_note) = if screen.animated() { (true, "animation frames are full size") } else { (false, "not needed yet") };
        Current { path, screen, full: None, full_requested, full_note, view, anim: None, info: None, info_requested: false, stale: false, orientation }
    }

    /// The same picture, drawn as it is, while another tab's decodes.
    fn stand_in(&self) -> Current {
        let full_note = "the previous tab's picture, until this one decodes";
        let (path, screen, view, orientation) = (self.path.clone(), self.screen.clone(), self.view, self.orientation);
        Current { path, screen, full: None, full_requested: true, full_note, view, anim: None, info: None, info_requested: true, stale: false, orientation }
    }

    /// Turned another way: the view starts again in `mode`.
    fn set_turn(&mut self, turn: Turn, mode: Mode) {
        self.orientation = turn.after(self.screen.orientation);
        let window = (self.view.window.0 as u32, self.view.window.1 as u32);
        self.view = View::new(view::oriented_size(self.orientation, self.screen.full_size), window, mode);
    }
}

/// What the animation thread sends (Event::Anim).
pub enum AnimUpdate {
    Frame(decode::Frame),
    /// All frames sent: playback loops over them.
    End,
}

/// An animation playing: its composed frames on the GPU, decoded on their
/// own thread as playback starts. Dropping it (paging away) stops the thread.
///
/// One whose frames fit in ANIM_BUDGET keeps them all and loops over them.
/// A bigger one (a video saved as GIF or WebP) streams: the thread stays a
/// few frames ahead (`Ring`), each frame is dropped once shown and its
/// bitmap refilled with a later one, and the thread decodes the file again
/// from the start for every loop.
struct Anim {
    frames: VecDeque<(ID2D1Bitmap1, Duration)>,
    /// Kept: the index shown. Streaming: always 0, the front.
    shown: usize,
    /// When the next frame is due (or decoding starts); None while it is
    /// still decoding.
    due: Option<Instant>,
    /// Frames are being decoded (from ANIM_START after it appears).
    decoding: bool,
    /// Kept: all frames are in. (Streaming never completes.)
    complete: bool,
    stop: Arc<AtomicBool>,
    /// Streaming: how far ahead the thread may decode.
    ring: Option<Arc<Ring>>,
    /// Streaming: bitmaps of frames already shown, to refill.
    spare: Vec<ID2D1Bitmap1>,
}

/// An animation's frames start decoding once it has been on screen this
/// long, so paging past animations doesn't spend CPU nobody watches.
const ANIM_START: Duration = Duration::from_millis(150);

impl Drop for Anim {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(ring) = &self.ring {
            ring.ready.notify_all();
        }
    }
}

/// GPU memory one animation's frames may take, all kept.
const ANIM_BUDGET: usize = 256 << 20;
/// A streaming animation's frames on the GPU at once: up to this much
/// (2 to 8 frames; ~8 at 1080p).
const ANIM_STREAM_BYTES: usize = 64 << 20;

/// Credits for frames the streaming thread may send ahead of playback: it
/// takes one per frame, playback gives one back per frame it drops.
struct Ring {
    free: Mutex<usize>,
    ready: Condvar,
}

impl Ring {
    /// Waits for a credit; false once the animation stopped.
    fn take(&self, stop: &AtomicBool) -> bool {
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        while *free == 0 {
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            // Timed, so a stop that lands just before the wait isn't missed.
            free = self.ready.wait_timeout(free, Duration::from_millis(100)).unwrap_or_else(|e| e.into_inner()).0;
        }
        *free -= 1;
        !stop.load(Ordering::Relaxed)
    }

    fn give(&self) {
        *self.free.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.ready.notify_one();
    }
}

/// One page turn: how it was served and when (wall-clock ms, comparable with
/// `flick-bench ext-viewer`'s key-press times).
struct PageTurn {
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

/// Whether a second click this long after the first, at `to` from `from`,
/// makes a double-click (the system's time and rectangle, 500 ms and 4 px
/// by default).
fn is_double_click(after: Duration, from: (f64, f64), to: (f64, f64)) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXDOUBLECLK, SM_CYDOUBLECLK};
    let (time, w, h) = unsafe { (GetDoubleClickTime(), GetSystemMetrics(SM_CXDOUBLECLK), GetSystemMetrics(SM_CYDOUBLECLK)) };
    after.as_millis() <= u128::from(time) && (to.0 - from.0).abs() <= f64::from(w) / 2.0 && (to.1 - from.1).abs() <= f64::from(h) / 2.0
}

/// Whether a key moves by hand through pictures or tabs (which stops a
/// slideshow): the paging keys, Home/End, Ctrl+Tab and Ctrl+1–9.
fn is_navigation(event: &KeyEvent, modifiers: ModifiersState) -> bool {
    if modifiers.alt_key() || (modifiers.shift_key() && !modifiers.control_key()) {
        return false; // Alt+arrows turn the picture; Shift+paging keys do nothing
    }
    if modifiers.control_key() {
        let digit = matches!(
            event.physical_key,
            PhysicalKey::Code(
                KeyCode::Digit1 | KeyCode::Digit2 | KeyCode::Digit3 | KeyCode::Digit4 | KeyCode::Digit5 | KeyCode::Digit6 | KeyCode::Digit7 | KeyCode::Digit8 | KeyCode::Digit9
            )
        );
        return digit || matches!(event.logical_key.as_ref(), Key::Named(NamedKey::Tab));
    }
    let letter = matches!(
        event.physical_key,
        PhysicalKey::Code(KeyCode::KeyA | KeyCode::KeyD | KeyCode::KeyW | KeyCode::KeyS | KeyCode::BracketLeft | KeyCode::BracketRight)
    );
    letter
        || matches!(
            event.logical_key.as_ref(),
            Key::Named(
                NamedKey::ArrowLeft
                    | NamedKey::ArrowRight
                    | NamedKey::ArrowUp
                    | NamedKey::ArrowDown
                    | NamedKey::PageUp
                    | NamedKey::PageDown
                    | NamedKey::Space
                    | NamedKey::Backspace
                    | NamedKey::Home
                    | NamedKey::End
                    | NamedKey::BrowserBack
                    | NamedKey::BrowserForward
            )
        )
}

/// Moves a file to the Recycle Bin (silently: the caller asked already).
fn recycle(hwnd: HWND, path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::{FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, SHFILEOPSTRUCTW, SHFileOperationW};
    // A list of paths, each NUL-ended, the list ended by another NUL.
    let from: Vec<u16> = path.as_os_str().encode_wide().chain([0, 0]).collect();
    let mut operation = SHFILEOPSTRUCTW {
        hwnd,
        wFunc: FO_DELETE,
        pFrom: windows::core::PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI).0 as u16,
        ..Default::default()
    };
    let result = unsafe { SHFileOperationW(&mut operation) };
    if result != 0 || operation.fAnyOperationsAborted.as_bool() {
        return Err(format!("error {result:#x}"));
    }
    Ok(())
}

/// "★★★", or "★ 14" past ten.
fn star_text(stars: u32) -> String {
    if stars > 10 { format!("★ {stars}") } else { "★".repeat(stars as usize) }
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

pub type TabId = u64;

/// One opened file and the folder it pages through (ARCHITECTURE.md §10a).
/// A window draws its active tab; the others keep their picture on the GPU
/// ("warm") until COOL_AFTER, then only their file ("cold").
#[derive(Default)]
pub struct Tab {
    pub id: TabId,
    pub request: Request,
    pub timeline: Timeline,
    /// The first picture, until the GPU and window are ready for it.
    pub picture: Option<Picture>,
    current: Option<Current>,
    /// The file the user navigated to; shown as soon as it is decoded.
    pub wanted: Option<PathBuf>,
    folder: Option<Folder>,
    /// Where paging is headed; `folder.index` walks there one image at a time.
    target: usize,
    /// The sign of the last page turn.
    direction: i64,
    /// Paging randomly: +1 forward, -1 back; 0 when paging in order.
    random_heading: i64,
    /// When `wanted` went on screen (or its error notice); None while it decodes.
    shown_at: Option<Instant>,
    /// Shown instead of a picture that can't be decoded.
    notice: Option<String>,
    turn_started: Option<Instant>,
    turn_started_wall: f64,
    last_turn: Option<(f64, &'static str)>,
    turns: Vec<PageTurn>,
    pub last_decode_ms: Option<f64>,
    /// This tab's shuffle (random paging); a new tab, a new order.
    seed: u64,
    /// When it stopped being the window's active tab.
    parked: Option<Instant>,
    /// Its folder listing was asked for.
    listing: bool,
    /// The last zoom set by hand (scale, centre as a fraction of the
    /// picture), for the middle-click cycle.
    custom_zoom: Option<(f32, (f32, f32))>,
    /// Rotate ▸ chosen in this tab (else the window's persisted turn, if any).
    turn: Option<Turn>,
    /// The picture's zoom and pan when the graphics device was lost, for
    /// when it is decoded again (`restore_view`).
    lost_view: Option<(PathBuf, View)>,
    /// The pictures looked at, for the Back and Forward keys.
    history: History,
    /// The next move is Back or Forward's: it isn't a visit to record.
    by_history: bool,
}

impl Tab {
    pub fn new(id: TabId, request: Request, timeline: Timeline) -> Tab {
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64) ^ id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        Tab { id, request, timeline, direction: 1, seed, ..Default::default() }
    }

    fn label(&self) -> String {
        file_name(self.wanted.as_deref().unwrap_or(&self.request.file))
    }

    /// A tab opened in the background (the rest of a burst): its file is
    /// decoded only when the tab is first viewed.
    pub fn waiting(id: TabId, request: Request, timeline: Timeline) -> Tab {
        let mut tab = Tab::new(id, request, timeline);
        tab.wanted = Some(tab.request.file.clone());
        tab
    }

    /// Marks its folder listing as asked for. Returns whether it already
    /// was, or the tab already has its folder (e.g. moved in from another
    /// window): then there's nothing to list.
    pub fn mark_listing(&mut self) -> bool {
        std::mem::replace(&mut self.listing, true) || self.folder.is_some()
    }

    /// Its picture is back after a lost graphics device: the zoom and pan
    /// it had then (at the window's size now), unless it has paged on.
    fn restore_view(&mut self) {
        let Some((path, view)) = self.lost_view.take() else { return };
        if let Some(current) = self.current.as_mut().filter(|c| c.path == path) {
            let window = (current.view.window.0 as u32, current.view.window.1 as u32);
            current.view = view;
            current.view.resize(window);
        }
    }
}

/// An inactive tab keeps its picture on the GPU this long (switching back
/// is then a texture swap), then drops it and decodes again when activated.
const COOL_AFTER: Duration = Duration::from_secs(180);

/// COOL_AFTER, or `FLICK_COOL_AFTER_S` seconds (for measuring eviction).
fn cool_after() -> Duration {
    static AFTER: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *AFTER.get_or_init(|| {
        std::env::var("FLICK_COOL_AFTER_S").ok().and_then(|s| s.parse().ok()).map_or(COOL_AFTER, Duration::from_secs)
    })
}

/// A tab leaving the screen keeps its picture, not what only drawing needs:
/// the full-resolution level and the animation (which restarts on return).
fn park(mut tab: Tab) -> Tab {
    tab.parked = Some(Instant::now());
    if let Some(current) = &mut tab.current {
        current.anim = None;
        current.full = None;
        current.full_requested = current.screen.animated();
    }
    tab
}

pub struct Viewer {
    pub id: ViewerId,
    /// The window's colour (chrome::PALETTE index), shown on its hover bar.
    pub color: usize,
    /// Where to open (the remembered placement), unless `--size` is given.
    placement: Option<Placement>,
    pub window: Option<Arc<Window>>,
    hwnd: Option<HWND>,
    surface: Option<Surface>,
    /// The tab drawn in the window.
    pub tab: Tab,
    /// The previous tab's picture, drawn while the active tab's decodes.
    holdover: Option<Current>,
    /// The other tabs, in strip order; `tab` sits at `active_at` among them.
    others: Vec<Tab>,
    active_at: usize,
    wheel: f32,
    modifiers: ModifiersState,
    debug: Option<DebugText>,
    show_debug: bool,
    chrome: Chrome,
    cursor: (f64, f64),
    panning: Option<(f64, f64)>,
    /// When, where and on what the last left click landed.
    last_click: Option<(Instant, (f64, f64), Option<chrome::Hit>)>,
    /// The left button is down on this, and nothing has happened yet.
    pressed: Option<Press>,
    /// This window's menu settings; each picture shown starts in its mode.
    prefs: Prefs,
    /// The right button is down: a drag zooms, a click opens the menu.
    right_press: Option<DragZoom>,
    /// Tells the current animation's events from those of pictures passed.
    anim_serial: u64,
    /// Tab Lock: the wheel and arrows switch tabs (this window only, not saved).
    locked: bool,
    /// Tab Lock's random order of the tabs (by id), for S/W and Up/Down;
    /// shuffled afresh when tabs come or go, kept when they are reordered.
    tab_shuffle: Vec<TabId>,
    /// The info panel and notes, made when first shown.
    osd: Option<Osd>,
    /// A brief note (zoom level, "Copied to clipboard") and when it goes.
    note: Option<(String, Instant)>,
    /// Where the info panel was last drawn: a click there closes it.
    info_rect: Option<D2D_RECT_F>,
    /// Ctrl+G: pages (or tabs) turn by themselves.
    slideshow: Option<Slideshow>,
    /// Tab Lock's last step was random (a slideshow through tabs follows it).
    tab_random: bool,
}

/// A running slideshow. Each picture stays `Prefs::slide_ms` from when it
/// is on screen, so a slow decode delays the next step instead of cutting
/// it short or letting steps pile up.
#[derive(Clone, Copy)]
struct Slideshow {
    /// In the window's random order (the last move was random).
    random: bool,
    /// Through the tabs (Tab Lock was on).
    tabs: bool,
    /// When it last moved on.
    stepped: Instant,
}

/// How long a note stays.
const NOTE_FOR: Duration = Duration::from_millis(1200);

/// When a right-click menu (of any window) last closed.
static MENU_CLOSED: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

/// A left press whose meaning waits for the release or for movement.
#[derive(Clone, Copy)]
enum Press {
    /// A hover-bar button: acts on release over the same button, like
    /// native caption buttons.
    Button(chrome::Hit),
    /// Moves the window once the cursor travels past the drag threshold, so
    /// a click (or a double-click) never enters Windows' move loop.
    Move((f64, f64)),
    /// A tab: dragging moves the window (as the title does); a click,
    /// released without dragging, switches to it.
    Tab(usize, (f64, f64)),
    /// Ctrl+left or right on a tab: dragging moves the tab along the strip,
    /// held `grab` px from its top-left corner (`at` is where it is now).
    /// Without moving, a left click switches to it, a right click opens the
    /// menu.
    Reorder { at: usize, from: (f64, f64), grab: (f64, f64), moved: bool, right: bool },
}

/// A right press: zooms by horizontal travel from `from`, starting at
/// `scale`, once past the drag threshold (`active`), keeping the image point
/// that was under `from` (`anchor`) there.
#[derive(Clone, Copy)]
struct DragZoom {
    from: (f64, f64),
    scale: f32,
    anchor: (f32, f32),
    active: bool,
}

impl Viewer {
    pub fn new(id: ViewerId, tab: Tab, placement: Option<Placement>, prefs: Prefs, global: Global, color: usize) -> Viewer {
        let placement = if tab.request.size.is_none() { placement } else { None };
        Viewer {
            id,
            color,
            placement,
            window: None,
            hwnd: None,
            surface: None,
            tab,
            others: Vec::new(),
            holdover: None,
            active_at: 0,
            wheel: 0.0,
            modifiers: ModifiersState::default(),
            debug: None,
            show_debug: false,
            chrome: Chrome::new(1.0, color, global.multirow),
            cursor: (0.0, 0.0),
            panning: None,
            last_click: None,
            pressed: None,
            prefs,
            right_press: None,
            anim_serial: 0,
            locked: false,
            tab_shuffle: Vec::new(),
            osd: None,
            note: None,
            info_rect: None,
            slideshow: None,
            tab_random: false,
        }
    }

    /// Preferences changed: the strip follows Multi-row Tabs.
    pub fn set_global(&mut self, global: Global) {
        self.chrome.set_multirow(global.multirow);
        self.redraw();
    }

    pub fn is_shown(&self) -> bool {
        self.surface.is_some()
    }

    pub fn hwnd(&self) -> Option<HWND> {
        self.hwnd
    }

    pub fn has_tab(&self, id: TabId) -> bool {
        self.tab.id == id || self.others.iter().any(|t| t.id == id)
    }

    pub fn tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        if self.tab.id == id { Some(&mut self.tab) } else { self.others.iter_mut().find(|t| t.id == id) }
    }

    /// All tabs' ids and labels, in strip order.
    fn tab_labels(&self) -> Vec<(TabId, String)> {
        let mut labels: Vec<(TabId, String)> = self.others.iter().map(|t| (t.id, t.label())).collect();
        labels.insert(self.active_at, (self.tab.id, self.tab.label()));
        labels
    }

    /// A newly opened file: the first tab, made active; the others shift
    /// right. Its picture follows when decoded (`on_decoded`).
    pub fn add_tab(&mut self, tab: Tab) {
        let old = park(std::mem::replace(&mut self.tab, tab));
        self.others.insert(self.active_at, old);
        self.active_at = 0;
        if let Some(window) = &self.window {
            window.set_title(&self.tab.label());
            window.set_minimized(false);
            window.focus_window();
        }
        self.redraw();
    }

    /// Before the drawn tab changes: keep its picture on screen until the
    /// next tab's is decoded (a background or cooled tab), as paging does,
    /// so a switch never shows an empty window.
    fn hold_picture(&mut self) {
        if let Some(stand_in) = self.tab.current.as_ref().or(self.holdover.as_ref()).map(Current::stand_in) {
            self.holdover = Some(stand_in);
        }
    }

    /// The rest of a burst of opened files: a background tab, the
    /// `count`th after the active one; the active tab stays.
    pub fn add_background_tab(&mut self, tab: Tab, count: usize) {
        let at = (self.active_at + count).min(self.others.len());
        self.others.insert(at, tab);
        self.redraw();
    }

    /// Ctrl+D: the copy goes right after the active tab, in the background;
    /// the bar shows for a moment so the new tab is seen arriving.
    pub fn add_duplicate(&mut self, tab: Tab) {
        self.add_background_tab(tab, 0);
        self.chrome.reveal(REVEAL);
    }

    /// The active tab shows a picture but its folder was never listed (a
    /// background tab, now viewed): the app lists it.
    pub fn take_listing(&mut self) -> Option<(TabId, PathBuf)> {
        let tab = &mut self.tab;
        if tab.current.is_none() || tab.listing || tab.folder.is_some() {
            return None;
        }
        tab.listing = true;
        Some((tab.id, tab.request.file.clone()))
    }

    /// Makes the tab at strip position `at` the drawn one.
    fn activate(&mut self, at: usize, shared: &Shared) -> Action {
        if at > self.others.len() || at == self.active_at {
            return Action::None;
        }
        let started = Instant::now();
        self.hold_picture();
        let mut tabs = std::mem::take(&mut self.others);
        tabs.insert(self.active_at, park(std::mem::take(&mut self.tab)));
        self.tab = tabs.remove(at);
        self.others = tabs;
        self.active_at = at;
        self.wake(shared);
        // Drawn now rather than on the next redraw, for the measurement below.
        let warm = self.tab.current.is_some();
        if self.render(shared).is_ok() {
            clock::trace(format_args!("tab switch: {:.2} ms to present ({})", started.elapsed().as_secs_f64() * 1000.0, if warm { "warm" } else { "cold" }));
        }
        Action::Navigated
    }

    /// The active tab came back: its picture (still on the GPU, or from
    /// the cache), at the window's current size; else it decodes again.
    fn wake(&mut self, shared: &Shared) {
        self.tab.parked = None;
        let size = self.surface.as_ref().map_or((1, 1), |s| s.size);
        let turn = self.turn_of(&self.tab);
        match &mut self.tab.current {
            Some(current) => current.view.resize(size),
            None => {
                if let Some(screen) = self.tab.wanted.as_ref().and_then(|p| shared.cache.get(p)) {
                    let path = self.tab.wanted.clone().unwrap_or_default();
                    self.tab.current = Some(Current::new(path, screen.clone(), size, self.prefs.mode, turn));
                    self.tab.restore_view();
                    self.tab.shown_at = Some(Instant::now());
                }
            }
        }
        if let Some(window) = &self.window {
            window.set_title(&self.tab.label());
        }
        self.start_animation();
        self.redraw();
    }

    /// Closes the tab at strip position `at`; the last one closes the window.
    fn close_tab(&mut self, at: usize, shared: &Shared) -> Action {
        if self.others.is_empty() {
            return Action::Close;
        }
        if at == self.active_at {
            let next = if at < self.others.len() { at } else { at - 1 };
            self.hold_picture();
            self.tab = self.others.remove(next);
            self.active_at = next;
            self.wake(shared);
        } else if at <= self.others.len() {
            self.others.remove(if at < self.active_at { at } else { at - 1 });
            if at < self.active_at {
                self.active_at -= 1;
            }
            self.redraw();
        }
        if self.others.is_empty() {
            self.set_locked(false); // one picture left: nothing to lock to
        }
        Action::Navigated
    }

    /// Moves the tab at strip position `from` to `to` (Ctrl-drag).
    fn move_tab(&mut self, from: usize, to: usize) {
        let count = self.others.len() + 1;
        if from == to || from >= count || to >= count {
            return;
        }
        let active = self.tab.id;
        let mut tabs = std::mem::take(&mut self.others);
        tabs.insert(self.active_at, std::mem::take(&mut self.tab));
        let moved = tabs.remove(from);
        tabs.insert(to, moved);
        self.active_at = tabs.iter().position(|t| t.id == active).unwrap_or(0);
        self.tab = tabs.remove(self.active_at);
        self.others = tabs;
        self.redraw();
    }

    /// Takes the active tab out, to move it to another window. This window
    /// shows its neighbour, or is left empty (true) if it was the only tab.
    pub fn take_active(&mut self, shared: &Shared) -> (Tab, bool) {
        if self.others.is_empty() {
            return (park(std::mem::take(&mut self.tab)), true);
        }
        let next = if self.active_at < self.others.len() { self.active_at } else { self.active_at - 1 };
        self.hold_picture();
        let tab = std::mem::replace(&mut self.tab, self.others.remove(next));
        self.active_at = next;
        self.wake(shared);
        if self.others.is_empty() {
            self.set_locked(false); // one picture left: nothing to lock to
        }
        (park(tab), false)
    }

    /// A tab moved here from another window: the first tab, made active,
    /// paging in this window's sort order.
    pub fn adopt(&mut self, mut tab: Tab, shared: &Shared) {
        if let Some(folder) = tab.folder.as_mut().filter(|f| f.sort != self.prefs.sort) {
            folder.sort(self.prefs.sort);
            tab.target = folder.index;
        }
        let old = park(std::mem::replace(&mut self.tab, tab));
        self.others.insert(self.active_at, old);
        self.active_at = 0;
        self.wake(shared);
        if let Some(window) = &self.window {
            window.set_minimized(false);
            window.focus_window();
        }
    }

    pub fn tab_count(&self) -> usize {
        self.others.len() + 1
    }

    /// Inactive tabs past COOL_AFTER give up their picture, and the GPU
    /// memory goes back at once: Direct3D frees released textures only on
    /// a flush, which nothing else does while no window draws.
    fn cool_tabs(&mut self, shared: &Shared) {
        let mut cooled = 0;
        for tab in &mut self.others {
            if tab.parked.is_some_and(|t| t.elapsed() >= cool_after()) {
                cooled += usize::from(tab.current.take().is_some());
                tab.parked = None;
            }
        }
        if cooled > 0 {
            if let Some(gpu) = &shared.gpu {
                gpu.release_unused();
            }
            clock::trace(format_args!("cooled {cooled} tab(s)"));
        }
    }

    /// The graphics device is lost: everything made on it goes (the
    /// window's surface, every tab's picture); each tab keeps the file it
    /// shows, to decode it again.
    pub fn drop_gpu(&mut self) {
        self.surface = None;
        self.holdover = None;
        for tab in std::iter::once(&mut self.tab).chain(self.others.iter_mut()) {
            if let Some(current) = tab.current.take() {
                tab.lost_view = Some((current.path.clone(), current.view));
                tab.wanted.get_or_insert(current.path);
            }
        }
    }

    /// A new device, for a window that was shown: a new surface at the
    /// window's size. The picture comes back through the preload pool
    /// (`on_preloaded`), as after paging.
    pub fn restore_gpu(&mut self, shared: &Shared) -> Result<(), String> {
        let (Some(gpu), Some(window), Some(hwnd)) = (&shared.gpu, &self.window, self.hwnd) else { return Ok(()) };
        let size = window.inner_size();
        self.surface = Some(Surface::new(gpu, hwnd, (size.width, size.height))?);
        self.redraw();
        Ok(())
    }

    /// A tab's first picture is decoded. Returns true once it is on screen
    /// (the app then lists its folder): with the window itself for its
    /// first tab, else straight away.
    pub fn on_decoded(&mut self, tab: TabId, picture: Picture, shared: &mut Shared) -> Result<bool, String> {
        if self.surface.is_none() {
            if self.tab.id == tab {
                self.tab.picture = Some(picture);
            }
            return self.try_show(shared);
        }
        let Some(gpu) = &shared.gpu else { return Ok(false) };
        let screen = Screen::upload(gpu, &picture)?;
        drop(picture);
        let (size, mode, active) = (self.surface.as_ref().map_or((1, 1), |s| s.size), self.prefs.mode, self.tab.id == tab);
        let default_turn = self.default_turn();
        let Some(target) = self.tab_mut(tab) else { return Ok(false) };
        target.timeline.mark("uploaded");
        let path = target.request.file.clone();
        shared.cache.insert(path.clone(), screen.clone());
        target.wanted = Some(path.clone());
        target.current = Some(Current::new(path, screen, size, mode, target.turn.unwrap_or(default_turn)));
        target.shown_at = Some(Instant::now());
        if active {
            self.render(shared)?;
            self.tab.timeline.mark("picture");
            self.start_animation();
        } else {
            target.parked = Some(Instant::now());
        }
        Ok(true)
    }

    pub fn create_window(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let (small_icon, taskbar_icon) = crate::icon::window_icons();
        let attrs = Window::default_attributes()
            .with_title(file_name(&self.tab.request.file))
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
        self.tab.timeline.mark("window");
        Ok(())
    }

    /// Size and place the window for a picture of `display` pixels.
    fn place_window(&self, window: &Window, display: (u32, u32)) -> (u32, u32) {
        let monitor = window.current_monitor().or_else(|| window.primary_monitor());
        let (origin, area) = monitor.map_or(((0, 0), (1920, 1080)), |m| ((m.position().x, m.position().y), (m.size().width, m.size().height)));
        let size = self.tab.request.size.unwrap_or_else(|| {
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
        // A first picture just decoded, or a tab moved here already showing one.
        let ready = self.tab.picture.is_some() || self.tab.current.is_some();
        if self.surface.is_some() || !ready || shared.gpu.is_none() || self.window.is_none() {
            return Ok(false);
        }
        let (Some(gpu), Some(window), Some(hwnd)) = (shared.gpu.as_ref(), self.window.clone(), self.hwnd) else {
            return Ok(false);
        };
        let turn = self.turn_of(&self.tab);
        let display = match (&self.tab.picture, &self.tab.current) {
            (Some(picture), _) => view::oriented_size(turn.after(picture.orientation), picture.full_size),
            (None, Some(current)) => view::oriented_size(current.orientation, current.screen.full_size),
            (None, None) => return Ok(false),
        };
        // The remembered placement (also maximized), else sized to the picture.
        let size = match &self.placement {
            Some(saved) => placement::prepare(hwnd, saved),
            None => self.place_window(&window, display),
        };
        self.surface = Some(Surface::new(gpu, hwnd, size)?);
        if let Some(picture) = self.tab.picture.take() {
            let screen = Screen::upload(gpu, &picture)?;
            drop(picture); // the CPU copy of the screen level isn't kept
            self.tab.timeline.mark("uploaded");
            let path = self.tab.request.file.clone();
            shared.cache.insert(path.clone(), screen.clone());
            self.tab.wanted = Some(path.clone());
            self.tab.current = Some(Current::new(path, screen, size, self.prefs.mode, turn));
        } else if let Some(current) = &mut self.tab.current {
            current.view.resize(size);
        }
        self.tab.parked = None;
        self.tab.shown_at = Some(Instant::now());
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
        self.tab.timeline.mark("picture");
        self.start_animation();
        Ok(true)
    }

    /// Remembers where the window is (not for `--size` benchmark runs).
    pub fn save_placement(&self) -> Option<Placement> {
        if self.tab.request.size.is_some() || self.surface.is_none() {
            return None;
        }
        placement::save(self.hwnd?)
    }

    /// The sort order this window's folder listing should use.
    pub fn sort(&self) -> Sort {
        self.prefs.sort
    }

    /// A tab's folder is listed (possibly a tab no longer active).
    pub fn on_listed(&mut self, tab: TabId, mut folder: Folder, shared: &mut Shared) {
        // Chosen while the listing ran.
        if folder.sort != self.prefs.sort {
            folder.sort(self.prefs.sort);
        }
        let Some(tab) = self.tab_mut(tab) else { return };
        // Re-key the first picture under the folder's own path for it.
        let path = folder.files[folder.index].clone();
        if let Some(current) = &mut tab.current {
            if let Some(screen) = shared.cache.remove(&current.path) {
                shared.cache.insert(path.clone(), screen);
            }
            current.path = path.clone();
        }
        tab.wanted = Some(path);
        tab.target = folder.index;
        tab.folder = Some(folder);
        self.redraw(); // the title gains its [i/n]
    }

    /// The way paging is going: toward the target, else the last turn's way.
    fn heading(&self) -> i64 {
        match self.tab.folder.as_ref().map(|f| (self.tab.target as i64 - f.index as i64).signum()) {
            Some(0) | None => self.tab.direction,
            Some(sign) => sign,
        }
    }

    /// Ring indices around the current image, most wanted first. Paging in
    /// order: current, next, previous, next+1, ... in the heading's
    /// direction, reaching AHEAD past the target so the images on the way
    /// decode in order, then the two random neighbours. Paging randomly: the
    /// same shape along the shuffled order, then the two sequential
    /// neighbours.
    fn ring(&self) -> Vec<usize> {
        let Some(folder) = &self.tab.folder else { return Vec::new() };
        let (i, n, d) = (folder.index as i64, folder.files.len() as i64, self.heading());
        let mut ring = vec![folder.index];
        let mut add = |j: Option<usize>| {
            if let Some(j) = j.filter(|j| !ring.contains(j)) {
                ring.push(j);
            }
        };
        let in_order = |j: i64| (0..n).contains(&j).then_some(j as usize);
        if self.tab.random_heading != 0 {
            let h = self.tab.random_heading;
            add(folder.random_at(h));
            add(folder.random_at(-h));
            for k in 2..=AHEAD as i64 {
                add(folder.random_at(k * h));
            }
            add(in_order(i + 1));
            add(in_order(i - 1));
        } else {
            let ahead = (self.tab.target as i64 - i).abs() + AHEAD as i64;
            for k in 1..=ahead.max(BEHIND as i64) {
                if k <= ahead {
                    add(in_order(i + k * d));
                }
                if k <= BEHIND as i64 {
                    add(in_order(i - k * d));
                }
            }
            add(folder.random_at(1));
            add(folder.random_at(-1));
        }
        ring
    }

    pub fn ring_paths(&self) -> Vec<PathBuf> {
        // Not listed (yet): just the wanted picture, e.g. a woken cold tab.
        let Some(folder) = &self.tab.folder else { return self.tab.wanted.iter().cloned().collect() };
        self.ring().into_iter().map(|i| folder.files[i].clone()).collect()
    }

    /// Paths this window needs kept in the cache.
    pub fn keep_paths(&self) -> Vec<PathBuf> {
        let mut keep = self.ring_paths();
        keep.extend(self.tab.current.as_ref().map(|c| c.path.clone()));
        keep.extend(self.tab.wanted.clone());
        keep
    }

    /// Wheel and arrow keys: move the target, then walk toward it.
    fn step(&mut self, delta: i64, shared: &Shared) -> Action {
        let Some(folder) = &self.tab.folder else { return Action::None };
        let (index, last) = (folder.index as i64, folder.files.len() as i64 - 1);
        let target = (self.tab.target as i64 + delta).clamp(0, last);
        if target == self.tab.target as i64 {
            // Nowhere further that way: say so (the folders go on with [ ]).
            self.show_note(if delta > 0 { "Last picture" } else { "First picture" }.into());
            return Action::None;
        }
        self.tab.target = target as usize;
        self.tab.direction = delta.signum();
        self.tab.random_heading = 0;
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
        self.tab.target = index;
        self.tab.random_heading = 0;
        self.go(index, shared)
    }

    /// The `deadline` came: the next paging step and/or animation frame.
    /// Returns whether the folder position moved.
    pub fn tick(&mut self, shared: &Shared) -> bool {
        if self.chrome.end_reveal() {
            self.redraw(); // the bar fades out
        }
        if self.note.as_ref().is_some_and(|(_, until)| *until <= Instant::now()) {
            self.note = None;
            self.redraw();
        }
        let slid = self.slide_due().is_some_and(|due| due <= Instant::now()) && self.slide(shared);
        self.cool_tabs(shared);
        self.next_frame(shared);
        self.advance(shared) || slid
    }

    /// One image toward the target, once the current one has been on screen
    /// for DWELL. Returns whether it moved.
    fn advance(&mut self, shared: &Shared) -> bool {
        let Some(folder) = &self.tab.folder else { return false };
        let index = folder.index as i64;
        let due = self.tab.shown_at.is_some_and(|t| t.elapsed() >= DWELL);
        if self.tab.target as i64 == index || !due {
            return false;
        }
        let next = index + (self.tab.target as i64 - index).signum();
        self.go(next as usize, shared);
        true
    }

    /// When the next paging step or animation frame is due (the app wakes
    /// for it; otherwise nothing runs).
    pub fn deadline(&self) -> Option<Instant> {
        let frame = self.tab.current.as_ref().and_then(|c| c.anim.as_ref()).and_then(|a| a.due);
        let step = self.tab.folder.as_ref().filter(|f| f.index != self.tab.target).and_then(|_| self.tab.shown_at).map(|t| t + DWELL);
        let cool = self.others.iter().filter_map(|t| t.parked).min().map(|t| t + cool_after());
        let note = self.note.as_ref().map(|(_, until)| *until);
        [frame, step, cool, self.chrome.reveal_deadline(), note, self.slide_due()].into_iter().flatten().min()
    }

    /// If the current picture is animated, schedules its playback: frames
    /// start decoding ANIM_START later (`next_frame`).
    fn start_animation(&mut self) {
        let Some(current) = &mut self.tab.current else { return };
        if current.screen.animated() {
            // Frames still arriving for an animation passed earlier are told apart by this.
            self.anim_serial += 1;
            let stop = Arc::new(AtomicBool::new(false));
            let due = Some(Instant::now() + ANIM_START);
            let frame_bytes = current.screen.frame_bytes();
            let ring = (current.screen.frames as usize * frame_bytes > ANIM_BUDGET).then(|| {
                let ahead = (ANIM_STREAM_BYTES / frame_bytes.max(1)).clamp(2, 8);
                Arc::new(Ring { free: Mutex::new(ahead), ready: Condvar::new() })
            });
            current.anim = Some(Anim { frames: VecDeque::new(), shown: 0, due, decoding: false, complete: false, stop, ring, spare: Vec::new() });
        }
    }

    /// Decodes the current animation's frames on their own thread: once
    /// (kept), or over and over, a few frames ahead of playback (streaming).
    fn decode_frames(&mut self, shared: &Shared) {
        let Some(current) = &mut self.tab.current else { return };
        let Some(anim) = &mut current.anim else { return };
        anim.decoding = true;
        anim.due = None;
        let (stop, ring) = (anim.stop.clone(), anim.ring.clone());
        let (viewer, serial, path, proxy) = (self.id, self.anim_serial, current.path.clone(), shared.proxy.clone());
        std::thread::spawn(move || {
            let send = |update| proxy.send_event(Event::Anim { viewer, serial, update }).is_ok();
            loop {
                let mut sent = 0;
                let result = decode::animate(&path, |frame| {
                    let room = ring.as_ref().is_none_or(|ring| ring.take(&stop));
                    sent += 1;
                    room && !stop.load(Ordering::Relaxed) && send(AnimUpdate::Frame(frame))
                });
                if let Err(e) = &result {
                    clock::trace(format_args!("animation: {e}"));
                }
                if ring.is_none() || result.is_err() || sent == 0 || stop.load(Ordering::Relaxed) {
                    break;
                }
            }
            if ring.is_none() {
                send(AnimUpdate::End);
            }
        });
    }

    /// A frame (or the end) from the animation thread.
    pub fn on_anim(&mut self, serial: u64, update: AnimUpdate, shared: &Shared) {
        let (Some(current), Some(gpu)) = (&mut self.tab.current, &shared.gpu) else { return };
        let Some(anim) = current.anim.as_mut().filter(|_| serial == self.anim_serial) else { return };
        let now = Instant::now();
        match update {
            AnimUpdate::Frame(frame) => {
                // Streaming: refill a bitmap already shown rather than allocate.
                let bitmap = match anim.spare.pop() {
                    Some(bitmap) if gpu.refill(&bitmap, &frame.image).is_ok() => bitmap,
                    _ => match gpu.upload(&frame.image) {
                        Ok(bitmap) => bitmap,
                        Err(_) => return,
                    },
                };
                anim.frames.push_back((bitmap, frame.delay));
                if anim.frames.len() == 1 {
                    // The first frame is the one already shown: time it.
                    anim.due = Some(now + frame.delay);
                } else if anim.due.is_none() && anim.frames.len() > anim.shown + 1 {
                    // Playback was waiting for this one.
                    self.advance_frame(now);
                }
            }
            AnimUpdate::End if anim.frames.len() < 2 => current.anim = None,
            AnimUpdate::End => {
                anim.complete = true;
                if anim.due.is_none() {
                    anim.due = Some(now);
                }
            }
        }
    }

    /// Shows the animation's next frame if it is due (or starts decoding).
    fn next_frame(&mut self, shared: &Shared) {
        let Some(anim) = self.tab.current.as_mut().and_then(|c| c.anim.as_mut()) else { return };
        let now = Instant::now();
        let Some(due) = anim.due.filter(|&due| due <= now) else { return };
        if !anim.decoding {
            return self.decode_frames(shared);
        }
        // Keep the animation's own pace, unless far behind (a stall): then restart it.
        let base = if now.duration_since(due) > Duration::from_secs(1) { now } else { due };
        self.advance_frame(base);
    }

    /// Moves to the next frame, timed from `base`; if it hasn't been decoded
    /// yet, waits for it (`on_anim` calls back).
    fn advance_frame(&mut self, base: Instant) {
        let Some(anim) = self.tab.current.as_mut().and_then(|c| c.anim.as_mut()) else { return };
        if let Some(ring) = &anim.ring {
            // Streaming: the front is shown; drop it once the next is in.
            if anim.frames.len() < 2 {
                anim.due = None;
                return;
            }
            if let Some((bitmap, _)) = anim.frames.pop_front() {
                anim.spare.push(bitmap);
                ring.give();
            }
            anim.shown = 0;
        } else {
            let next = anim.shown + 1;
            if next < anim.frames.len() {
                anim.shown = next;
            } else if anim.complete {
                anim.shown = 0;
            } else {
                anim.due = None; // still decoding: on_anim resumes
                return;
            }
        }
        anim.due = Some(base + anim.frames[anim.shown].1);
        self.redraw();
    }

    fn go(&mut self, index: usize, shared: &Shared) -> Action {
        let Some(folder) = &self.tab.folder else { return Action::None };
        if index == folder.index || index >= folder.files.len() {
            return Action::None;
        }
        self.show_index(index, shared)
    }

    /// Shows the folder's file at `index` (decoded, or once it is), even
    /// if the position didn't change (the file there did: a deletion).
    fn show_index(&mut self, index: usize, shared: &Shared) -> Action {
        let Some(folder) = &mut self.tab.folder else { return Action::None };
        folder.index = index;
        let path = folder.files[index].clone();
        // Leaving a picture: a visit, unless Back or Forward left it.
        if let Some(left) = self.tab.current.as_ref().filter(|c| c.path != path && !self.tab.by_history) {
            self.tab.history.visited(left.path.clone());
        }
        self.tab.by_history = false;
        self.tab.turn_started = Some(Instant::now());
        self.tab.turn_started_wall = clock::wall_ms();
        self.tab.wanted = Some(path.clone());
        self.tab.shown_at = None;
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
        let turn = self.turn_of(&self.tab);
        self.tab.current = Some(Current::new(path, screen, window, self.prefs.mode, turn));
        self.tab.restore_view();
        self.tab.notice = None;
        self.tab.shown_at = Some(Instant::now());
        if self.render(shared).is_err() {
            self.redraw();
        }
        self.start_animation();
        if let Some(started) = self.tab.turn_started.take() {
            let presented_wall = clock::wall_ms();
            self.tab.last_turn = Some((started.elapsed().as_secs_f64() * 1000.0, how));
            let composition_hz = self.surface.as_ref().and_then(|s| s.composition_hz());
            let gpu_done_wall = self.surface.as_ref().and_then(|s| s.wait_for_gpu()).map(|_| clock::wall_ms());
            self.tab.turns.push(PageTurn {
                index: self.tab.folder.as_ref().map_or(0, |f| f.index),
                how,
                handled_wall: self.tab.turn_started_wall,
                presented_wall,
                gpu_done_wall,
                composition_hz,
            });
        }
    }

    /// A screen level arrived from the pool: show it if this window waits for it.
    pub fn on_preloaded(&mut self, path: &Path, screen: &Screen, shared: &Shared) {
        let shown = self.tab.current.as_ref().filter(|c| c.path == path && !c.stale).is_some();
        if self.tab.wanted.as_deref() == Some(path) && !shown {
            self.display(path.to_path_buf(), screen.clone(), "decoded", shared);
        }
    }

    /// The folders this window's tabs page through (each watched once).
    pub fn folder_dirs(&self) -> Vec<PathBuf> {
        std::iter::once(&self.tab).chain(&self.others).filter_map(|t| t.folder.as_ref()).map(|f| f.dir.clone()).collect()
    }

    /// The tabs paging through `dir`, with the file each is on (to list
    /// the folder again from).
    pub fn files_shown_in(&self, dir: &Path) -> Vec<(TabId, PathBuf)> {
        std::iter::once(&self.tab)
            .chain(&self.others)
            .filter_map(|t| t.folder.as_ref().filter(|f| f.dir == dir).map(|f| (t.id, f.files[f.index].clone())))
            .collect()
    }

    /// Changes to a watched folder, for every tab paging through it: the
    /// list updates in place, the position and paging target stay on their
    /// files, a renamed current file is followed, and a rewritten one on
    /// screen is decoded again (the old picture stays until then).
    pub fn on_folder_changed(&mut self, dir: &Path, changes: &[folder::Change]) {
        let active = self.tab.id;
        let mut touched = false;
        for tab in std::iter::once(&mut self.tab).chain(self.others.iter_mut()) {
            let Some(folder) = tab.folder.as_mut().filter(|f| f.dir == dir) else { continue };
            let target = folder.files.get(tab.target).cloned();
            if !folder.apply(changes) {
                continue;
            }
            tab.target = target.and_then(|t| folder.position(&t)).unwrap_or(folder.index);
            for change in changes {
                match change {
                    folder::Change::Renamed(old, new) => {
                        if tab.wanted.as_ref() == Some(old) {
                            tab.wanted = Some(new.clone());
                        }
                        if let Some(current) = tab.current.as_mut().filter(|c| &c.path == old) {
                            current.path = new.clone();
                        }
                    }
                    folder::Change::Modified(path) if tab.id == active => {
                        if let Some(current) = tab.current.as_mut().filter(|c| &c.path == path) {
                            current.stale = true;
                            tab.wanted = Some(path.clone());
                        }
                    }
                    _ => {}
                }
            }
            touched |= tab.id == active;
        }
        if touched {
            if let (Some(window), Some(wanted)) = (&self.window, &self.tab.wanted) {
                window.set_title(&file_name(wanted));
            }
            self.redraw(); // the position [i/n]
        }
    }

    pub fn on_failed(&mut self, path: &Path, error: &str) {
        if self.tab.wanted.as_deref() == Some(path) {
            self.show_notice(path, error);
        }
    }

    /// A new tab's file couldn't be decoded: the tab shows why.
    pub fn on_decode_failed(&mut self, tab: TabId, error: &str) {
        if self.tab.id == tab {
            let path = self.tab.request.file.clone();
            self.show_notice(&path, error);
        } else if let Some(t) = self.tab_mut(tab) {
            t.notice = Some(format!("Can't show {}\n{error}", file_name(&t.request.file)));
            t.shown_at = Some(Instant::now());
        }
    }

    fn show_notice(&mut self, path: &Path, error: &str) {
        self.tab.current = None;
        self.tab.notice = Some(format!("Can't show {}\n{error}", file_name(path)));
        self.tab.shown_at = Some(Instant::now());
        if self.debug.is_none() {
            self.debug = DebugText::new().ok();
        }
        self.redraw();
    }

    pub fn on_full_decoded(&mut self, path: PathBuf, image: Result<Image, String>, shared: &Shared) {
        let (Some(current), Some(gpu)) = (&mut self.tab.current, &shared.gpu) else { return };
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
        let path = self.tab.wanted.as_deref().unwrap_or(&self.tab.request.file);
        let folder = path.parent().and_then(|p| p.file_name()).map(|f| f.to_string_lossy().into_owned());
        let position = self.tab.folder.as_ref().map(|f| format!("   [{}/{}]", f.index + 1, f.files.len())).unwrap_or_default();
        match folder {
            // In a tab of the strip: no room for the folder.
            Some(folder) if self.others.is_empty() => format!("{folder}  ›  {}{position}", file_name(path)),
            _ => format!("{}{position}", file_name(path)),
        }
    }

    /// The info panel's lines for the picture on screen. Its details are read
    /// on a worker thread the first time (`on_info`); until then, the file
    /// name and size only.
    fn info_lines(&mut self, shared: &Shared) -> Option<Vec<(&'static str, String)>> {
        let position = self.tab.folder.as_ref().map(|f| format!("   [{}/{}]", f.index + 1, f.files.len())).unwrap_or_default();
        let current = self.tab.current.as_mut()?;
        if !current.info_requested {
            current.info_requested = true;
            let (viewer, path, proxy) = (self.id, current.path.clone(), shared.proxy.clone());
            std::thread::spawn(move || {
                let info = meta::read(&path);
                let _ = proxy.send_event(Event::Info { viewer, path, info });
            });
        }
        let (w, h) = view::oriented_size(current.orientation, current.screen.full_size);
        let mut image = format!("{w} × {h}");
        if let Some(info) = &current.info {
            image += &format!(", {}", info.format);
        }
        if current.screen.animated() {
            image += &format!(", {} frames", current.screen.frames);
        }
        let mut lines = vec![("File", format!("{}{position}", file_name(&current.path))), ("Image", image)];
        if let Some(info) = &current.info {
            if info.stars.count > 0 {
                let since = (info.stars.since > 0)
                    .then(|| meta::local_time(std::time::UNIX_EPOCH + Duration::from_secs(info.stars.since as u64)))
                    .flatten()
                    .map(|t| format!("  (since {})", &t[..16]))
                    .unwrap_or_default();
                lines.push(("Stars", format!("{}{since}", star_text(info.stars.count))));
            }
            lines.extend(info.lines.iter().cloned());
            if !info.exif.is_empty() {
                lines.push(("", String::new())); // a gap before the shot's details
                lines.extend(info.exif.iter().cloned());
            }
        }
        Some(lines)
    }

    /// The info panel's details for `path`, read on a worker thread.
    pub fn on_info(&mut self, path: PathBuf, info: meta::Info) {
        if let Some(current) = self.tab.current.as_mut().filter(|c| c.path == path) {
            current.info = Some(info);
            self.redraw();
        }
    }

    /// The turn a tab starts with: the persisted one, else none.
    fn default_turn(&self) -> Turn {
        if self.prefs.persist_turn { self.prefs.turn } else { Turn::default() }
    }

    fn turn_of(&self, tab: &Tab) -> Turn {
        tab.turn.unwrap_or_else(|| self.default_turn())
    }

    /// Rotate ▸ (Alt+arrows, Alt+H, Alt+V): this tab's pictures turned,
    /// the one on screen at once. Persisted, it's saved for new tabs too.
    fn set_turn(&mut self, turn: Turn) -> Action {
        self.tab.turn = Some(turn);
        let mode = self.prefs.mode;
        if let Some(current) = &mut self.tab.current {
            current.set_turn(turn, mode);
        }
        let angle = match turn.quarter {
            0 => "Not rotated",
            1 => "Rotated 90° clockwise",
            2 => "Rotated 180°",
            _ => "Rotated 90° counter-clockwise",
        };
        let flips = match (turn.flip_h, turn.flip_v) {
            (true, true) => ", flipped both ways",
            (true, false) => ", flipped horizontally",
            (false, true) => ", flipped vertically",
            _ => "",
        };
        self.show_note(format!("{angle}{flips}"));
        if self.prefs.persist_turn { self.set_prefs(Prefs { turn, ..self.prefs }) } else { Action::None }
    }

    /// Rotate ▸ Persist: on, this tab's turn becomes the window's (saved,
    /// for new tabs); off, nothing is kept.
    fn toggle_persist_turn(&mut self) -> Action {
        let persist = !self.prefs.persist_turn;
        let turn = if persist { self.turn_of(&self.tab) } else { Turn::default() };
        self.tab.turn = Some(self.turn_of(&self.tab)); // this tab keeps what it shows
        self.set_prefs(Prefs { persist_turn: persist, turn, ..self.prefs })
    }

    /// The file on screen (not one still decoding after a page turn).
    fn shown_file(&self) -> Option<PathBuf> {
        self.tab.current.as_ref().map(|c| c.path.clone())
    }

    /// Ctrl+C: the picture at full resolution and upright, decoded on a
    /// worker thread, then put on the clipboard (`on_copy`).
    fn copy(&mut self, shared: &Shared) -> Action {
        let Some(current) = &self.tab.current else { return Action::None };
        let (viewer, path, orientation, proxy) = (self.id, current.path.clone(), current.orientation, shared.proxy.clone());
        std::thread::spawn(move || {
            let image = decode::decode_full(&path).map(|image| decode::oriented(image, orientation));
            let _ = proxy.send_event(Event::Copy { viewer, image });
        });
        self.show_note("Copying…".into());
        Action::None
    }

    pub fn on_copy(&mut self, image: Result<Image, String>) {
        let hwnd = self.hwnd;
        let result = image.and_then(|image| hwnd.ok_or_else(|| "no window".to_string()).and_then(|hwnd| clipboard::put(hwnd, &image)));
        self.show_note(match result {
            Ok(()) => "Copied to clipboard".into(),
            Err(e) => format!("Couldn't copy: {}", e.rsplit(": ").next().unwrap_or(&e)),
        });
    }

    /// Ctrl+Enter: Explorer opens the file's folder with the file selected.
    fn show_in_explorer(&mut self) -> Action {
        use std::os::windows::process::CommandExt;
        if let Some(path) = self.shown_file() {
            let _ = std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{}\"", path.display())).spawn();
        }
        Action::None
    }

    /// F / Shift+F: a star more or less for the picture on screen (the
    /// first star asks for the favourites folder if none was chosen). The
    /// library work, a hard link or a copy among it, runs on a worker thread.
    fn star(&mut self, delta: i32, shared: &Shared) -> Action {
        let Some(path) = self.shown_file() else { return Action::None };
        let favorites = match library::favorites() {
            Some(folder) => folder,
            None if delta > 0 => match self.choose_favorites() {
                Some(folder) => folder,
                None => return Action::None,
            },
            None => return Action::None, // nothing starred yet
        };
        let (viewer, proxy) = (self.id, shared.proxy.clone());
        std::thread::spawn(move || {
            let result = library::add_stars(&path, delta, &favorites);
            let _ = proxy.send_event(Event::Starred { viewer, path, result });
        });
        Action::None
    }

    /// Stars changed for `path`: every tab's folder notes it (and re-sorts
    /// by it), the info panel shows it; the window that starred it says so.
    pub fn on_starred(&mut self, path: &Path, result: &Result<library::Stars, String>, starred_here: bool) {
        if let Ok(stars) = *result {
            for tab in std::iter::once(&mut self.tab).chain(self.others.iter_mut()) {
                if let Some(folder) = tab.folder.as_mut() {
                    let target = folder.files.get(tab.target).cloned();
                    folder.set_stars(path, stars);
                    tab.target = target.and_then(|t| folder.position(&t)).unwrap_or(folder.index);
                }
                if let Some(info) = tab.current.as_mut().filter(|c| c.path == path).and_then(|c| c.info.as_mut()) {
                    info.stars = stars;
                }
            }
            self.redraw();
        }
        if starred_here {
            self.show_note(match result {
                Ok(stars) if stars.count == 0 => "☆ No stars".into(),
                Ok(stars) => star_text(stars.count),
                Err(e) => format!("Couldn't star: {e}"),
            });
        }
    }

    /// Preferences ▸ Favorites Folder…, or the first star with none chosen:
    /// Windows' folder picker. Returns the folder (None if cancelled).
    fn choose_favorites(&mut self) -> Option<PathBuf> {
        let hwnd = self.hwnd?;
        let start = library::favorites().or_else(|| self.shown_file().and_then(|f| f.parent().map(Path::to_path_buf)));
        let folder = menu::pick_folder(hwnd, "Favorites folder (starred pictures are linked here)", start.as_deref())?;
        match library::set_favorites(&folder) {
            Ok(()) => {
                self.show_note(format!("Favorites: {}", folder.display()));
                Some(folder)
            }
            Err(e) => {
                self.show_note(format!("Couldn't set favorites: {e}"));
                None
            }
        }
    }

    /// When the slideshow moves on: `slide_ms` after the picture is on
    /// screen (and not before the last step). None while it still decodes.
    fn slide_due(&self) -> Option<Instant> {
        let show = self.slideshow?;
        let shown = self.tab.shown_at?.max(show.stepped);
        Some(shown + Duration::from_millis(u64::from(self.prefs.slide_ms)))
    }

    /// The slideshow's next step: the next picture (wrapping to the first
    /// at the end), the next random one, or the next tab. Returns whether
    /// the folder position moved.
    fn slide(&mut self, shared: &Shared) -> bool {
        let Some(show) = self.slideshow.as_mut() else { return false };
        show.stepped = Instant::now();
        let show = *show;
        let count = self.tab_count();
        if show.tabs && count > 1 {
            // Quietly: no bar reveal per step, as switch_tab does for keys.
            let at = if show.random { self.next_random_tab(true) } else { Some((self.active_at + 1) % count) };
            if let Some(at) = at {
                self.activate(at, shared);
            }
            return true;
        }
        if show.random {
            return matches!(self.random(true, shared), Action::Navigated);
        }
        let Some(folder) = &self.tab.folder else { return false };
        let action = if folder.index + 1 >= folder.files.len() { self.jump(0, shared) } else { self.step(1, shared) };
        matches!(action, Action::Navigated)
    }

    /// Ctrl+G: starts (the way the last move went: random or in order;
    /// through tabs under Tab Lock) or stops.
    fn toggle_slideshow(&mut self) -> Action {
        if self.slideshow.take().is_some() {
            self.show_note("Slideshow stopped".into());
            return Action::None;
        }
        self.start_slideshow();
        Action::None
    }

    fn start_slideshow(&mut self) {
        let tabs = self.locked && self.tab_count() > 1;
        let random = if tabs { self.tab_random } else { self.tab.random_heading != 0 };
        self.slideshow = Some(Slideshow { random, tabs, stepped: Instant::now() });
        let how = match (tabs, random) {
            (true, true) => " · tabs, random",
            (true, false) => " · tabs",
            (false, true) => " · random",
            (false, false) => "",
        };
        self.show_note(format!("Slideshow · {} ms{how}", self.prefs.slide_ms));
    }

    /// `[` / `]`: the folder before or after this one among its siblings
    /// (ordered as the pictures are), listed on a worker thread
    /// (`on_entered`), at its first picture.
    fn sibling(&mut self, forward: bool, shared: &Shared) -> Action {
        let dir = match &self.tab.folder {
            Some(folder) => folder.dir.clone(),
            None => match self.shown_file().and_then(|f| f.parent().map(Path::to_path_buf)) {
                Some(dir) => dir,
                None => return Action::None,
            },
        };
        let (viewer, tab, sort, proxy) = (self.id, self.tab.id, self.prefs.sort, shared.proxy.clone());
        std::thread::spawn(move || {
            let result = folder::sibling(&dir, sort, forward);
            let _ = proxy.send_event(Event::Entered { viewer, tab, result, at: None });
        });
        Action::None
    }

    /// The Back and Forward keys or mouse buttons: the picture looked at
    /// before (or after, having gone back), in its folder, wherever that is.
    fn history(&mut self, back: bool, shared: &Shared) -> Action {
        let Some(here) = self.tab.wanted.clone().or_else(|| self.tab.current.as_ref().map(|c| c.path.clone())) else { return Action::None };
        let to = if back { self.tab.history.back(&here) } else { self.tab.history.forward(&here) };
        clock::trace(format_args!("history: from {here:?} to {to:?}"));
        let Some(to) = to else {
            self.show_note(if back { "Nothing further back" } else { "Nothing further forward" }.into());
            return Action::None;
        };
        self.tab.random_heading = 0;
        if let Some(index) = self.tab.folder.as_ref().and_then(|f| f.position(&to)) {
            self.tab.target = index;
            self.tab.by_history = true;
            return self.show_index(index, shared);
        }
        // In another folder: listed on a worker thread, then shown (`on_entered`).
        let (viewer, tab, sort, proxy) = (self.id, self.tab.id, self.prefs.sort, shared.proxy.clone());
        std::thread::spawn(move || {
            let result = folder::list(&to, sort);
            let _ = proxy.send_event(Event::Entered { viewer, tab, result, at: Some(to) });
        });
        Action::None
    }

    /// A folder listed for `[` / `]` (a sibling: from its first picture) or
    /// Back / Forward (`at`: that picture): the tab pages through it (if the
    /// tab is still the one shown), or says why not.
    pub fn on_entered(&mut self, tab: TabId, result: Result<Folder, String>, at: Option<PathBuf>, shared: &Shared) -> Action {
        if self.tab.id != tab {
            return Action::None;
        }
        let mut folder = match result {
            Ok(folder) => folder,
            Err(e) => {
                self.show_note(e);
                return Action::None;
            }
        };
        if folder.sort != self.prefs.sort {
            folder.sort(self.prefs.sort);
        }
        let index = at.as_ref().and_then(|p| folder.position(p)).unwrap_or(0);
        folder.index = index;
        let name = folder.dir.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let count = folder.files.len();
        self.tab.folder = Some(folder);
        self.tab.target = index;
        self.tab.random_heading = 0;
        self.tab.listing = true;
        self.tab.by_history = at.is_some();
        self.show_note(format!("{name}  ({count})"));
        self.show_index(index, shared)
    }

    /// Ctrl+[ / Ctrl+]: the slideshow interval shorter or longer, in steps of
    /// about a tenth (10 ms at the least), 50 ms to 60 s.
    fn adjust_slide(&mut self, longer: bool) -> Action {
        let ms = self.prefs.slide_ms;
        let step = ((ms / 10).max(10) / 10) * 10;
        let ms = if longer { ms + step } else { ms.saturating_sub(step) }.clamp(50, 60_000);
        let action = self.set_prefs(Prefs { slide_ms: ms, ..self.prefs });
        self.show_note(format!("Slideshow · {ms} ms{}", if self.slideshow.is_some() { "" } else { " (Ctrl+G starts)" }));
        action
    }

    /// Slideshow ▸ an interval: set, and the slideshow runs.
    fn set_slide_interval(&mut self, ms: u32) -> Action {
        let action = self.set_prefs(Prefs { slide_ms: ms, ..self.prefs });
        self.start_slideshow();
        action
    }

    /// A key or the wheel moved by hand: the slideshow stops.
    fn stop_slideshow_by_hand(&mut self) {
        if self.slideshow.take().is_some() {
            self.show_note("Slideshow stopped".into());
        }
    }

    /// Del (to the Recycle Bin) or Shift+Del (for good), always asking
    /// first, in Windows' own message box. Then the next picture shows (the
    /// folder's watcher would get there too, a moment later); the folder's
    /// only picture closes its tab.
    fn delete(&mut self, permanently: bool, shared: &Shared) -> Action {
        use windows::Win32::UI::WindowsAndMessaging::{IDYES, MB_DEFBUTTON2, MB_ICONQUESTION, MB_ICONWARNING, MB_YESNO, MessageBoxW};
        let (Some(path), Some(hwnd)) = (self.shown_file(), self.hwnd) else { return Action::None };
        let name = file_name(&path);
        let (text, style) = if permanently {
            (format!("Permanently delete \"{name}\"?\n\nIt won't go to the Recycle Bin, and this can't be undone."), MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2)
        } else {
            (format!("Move \"{name}\" to the Recycle Bin?"), MB_YESNO | MB_ICONQUESTION)
        };
        let answer = unsafe { MessageBoxW(Some(hwnd), &windows::core::HSTRING::from(text), windows::core::w!("Delete"), style) };
        if answer != IDYES {
            return Action::None;
        }
        let deleted = if permanently { std::fs::remove_file(&path).map_err(|e| e.to_string()) } else { recycle(hwnd, &path) };
        if let Err(e) = deleted {
            self.show_note(format!("Couldn't delete: {e}"));
            return Action::None;
        }
        let Some(folder) = self.tab.folder.as_mut() else { return self.close_tab(self.active_at, shared) };
        if folder.files.len() <= 1 {
            return self.close_tab(self.active_at, shared);
        }
        let target = folder.files.get(self.tab.target).cloned();
        folder.apply(&[folder::Change::Removed(path)]);
        let index = folder.index;
        self.tab.target = target.and_then(|t| folder.position(&t)).unwrap_or(index);
        self.show_index(index, shared)
    }

    /// Ctrl+D (a background tab) or Ctrl+N (a new window) with this file.
    fn duplicate(&mut self, new_window: bool) -> Action {
        self.shown_file().map_or(Action::None, |file| Action::Duplicate { file, new_window })
    }

    /// Middle click on the picture steps through the view modes in the
    /// menu's order, each set as the menu sets it (around the click), then
    /// the last zoom set by hand, for this picture only; stops that look
    /// the same here are skipped.
    fn cycle_zoom(&mut self, at: (f32, f32)) -> Action {
        let Some(current) = &mut self.tab.current else { return Action::None };
        let view = &mut current.view;
        let from = if view.hand_zoomed() {
            self.tab.custom_zoom = Some(view.custom_zoom()); // leaving it
            Stop::Hand
        } else {
            Stop::Mode(self.prefs.mode.fit)
        };
        match view.next_stop(from, self.tab.custom_zoom.map(|(scale, _)| scale)) {
            Some(Stop::Mode(fit)) => {
                let action = self.set_prefs(Prefs { mode: Mode { fit, ..self.prefs.mode }, ..self.prefs });
                if let Some(current) = &mut self.tab.current {
                    current.view.apply_mode_at(at);
                }
                self.show_zoom();
                action
            }
            Some(Stop::Hand) => {
                view.set_custom_zoom(self.tab.custom_zoom.unwrap_or_else(|| view.custom_zoom()));
                self.show_zoom();
                Action::None
            }
            None => Action::None,
        }
    }

    fn on_middle_press(&mut self) -> Action {
        // On the bar, middle click closes a tab (on release); elsewhere it zooms.
        if self.chrome.hit(self.cursor_f32(), self.width()).is_none() {
            return self.cycle_zoom(self.cursor_f32());
        }
        Action::None
    }

    /// Shows `text` briefly at the bottom of the window.
    fn show_note(&mut self, text: String) {
        self.note = Some((text, Instant::now() + NOTE_FOR));
        self.redraw();
    }

    /// The zoom level, as a brief note after any zoom, named by the mode
    /// when it is the mode's: "Fill window · 47 %", "Zoom · 150 %".
    fn show_zoom(&mut self) {
        let Some(view) = self.tab.current.as_ref().map(|c| &c.view) else { return };
        let name = if view.hand_zoomed() { "Zoom" } else { self.prefs.mode.fit.name() };
        let scale = view.scale * 100.0;
        self.show_note(if scale >= 10.0 { format!("{name} · {scale:.0} %") } else { format!("{name} · {scale:.1} %") });
    }

    pub fn render(&mut self, shared: &Shared) -> Result<(), String> {
        // Nothing can be drawn on a lost device: the error takes the app
        // to its recovery (`App::fail_viewer`).
        if self.surface.is_some() && shared.gpu.as_ref().is_some_and(Gpu::lost) {
            return Err("graphics device lost".into());
        }
        let debug_text = self.show_debug.then(|| self.debug_text(shared));
        let title = self.title();
        let (tabs, active) = (self.tab_labels(), self.active_at);
        self.chrome.set_tabs(tabs.len());
        let maximized = self.window.as_ref().is_some_and(|w| w.is_maximized());
        let scale = self.window.as_ref().map_or(1.0, |w| w.scale_factor() as f32);
        let info = if self.prefs.info { self.info_lines(shared) } else { None };
        let note = self.note.as_ref().filter(|(_, until)| *until > Instant::now()).map(|(text, _)| text.clone());
        if (info.is_some() || note.is_some()) && self.osd.as_ref().is_none_or(|osd| osd.scale() != scale) {
            self.osd = Osd::new(scale).ok();
        }
        let Some(surface) = &mut self.surface else { return Ok(()) };
        let window = (surface.size.0 as f32, surface.size.1 as f32);

        // The previous tab's picture stays until this tab has its own (or a notice).
        if self.tab.current.is_some() || self.tab.notice.is_some() {
            self.holdover = None;
        }
        let picture = match self.tab.current.as_mut().or(self.holdover.as_mut()) {
            Some(current) => {
                let magnified = current.screen.magnified(current.view.scale);
                let max_bitmap = shared.gpu.as_ref().map_or(u32::MAX, Gpu::max_bitmap_size);
                if magnified && !current.full_requested && current.screen.full_size.0.max(current.screen.full_size.1) > max_bitmap {
                    // Not decoded only to be refused by the GPU (a 20000 px panorama).
                    current.full_requested = true;
                    current.full_note = "too large for one bitmap (tiles: milestone 4)";
                }
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
                    .then(view::orientation(current.orientation, fw, fh))
                    .then(current.view.transform());
                let shrinking = current.view.scale * fw / (size.0 as f32) < 1.0;
                let interpolation = match self.prefs.filter {
                    Filter::Nearest => D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                    Filter::Soften if !shrinking => D2D1_INTERPOLATION_MODE_LINEAR,
                    Filter::Soften | Filter::Sharpen => D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
                };
                Some((bitmap, to_window, interpolation))
            }
            None => None,
        };
        // Soften+Sharpen draws through an effect chain, made on first use.
        let sharpen = match self.prefs.filter {
            Filter::Sharpen if picture.is_some() => surface.sharpen.take().or_else(|| Sharpen::new(&surface.context).ok()),
            _ => None,
        };
        let (debug, notice, osd, chrome) = (self.debug.as_ref(), self.tab.notice.as_deref(), self.osd.as_ref(), &mut self.chrome);
        let mut info_rect = None;
        let fading = chrome.animate();
        let grey = chrome::BACKGROUNDS[self.color % chrome::BACKGROUNDS.len()];
        let background = D2D1_COLOR_F { r: grey, g: grey, b: grey, a: 1.0 };
        surface.frame(background, |context| unsafe {
            if let Some((bitmap, to_window, interpolation)) = &picture {
                match &sharpen {
                    Some(sharpen) => {
                        let _ = sharpen.draw(context, bitmap, to_window.to_d2d());
                    }
                    None => {
                        context.SetTransform(&to_window.to_d2d());
                        context.DrawBitmap(bitmap, None, 1.0, *interpolation, None, None);
                    }
                }
                context.SetTransform(&Affine::IDENTITY.to_d2d());
            }
            if let (Some(debug), Some(notice)) = (debug, notice) {
                debug.draw(context, notice, window.1 / 2.0 - 30.0);
            }
            if let (Some(debug), Some(text)) = (debug, &debug_text) {
                debug.draw(context, text, 48.0 * scale);
            }
            if let (Some(osd), Some(lines)) = (osd, &info) {
                info_rect = osd.draw_info(context, lines, window);
            }
            if let (Some(osd), Some(text)) = (osd, &note) {
                osd.draw_note(context, text, window);
            }
            chrome.draw(context, window.0, &title, &tabs, active, maximized);
        })?;
        self.info_rect = info_rect;
        if sharpen.is_some() {
            surface.sharpen = sharpen;
        }
        // The bar fading, or tabs gliding into place: one more frame.
        if fading || self.chrome.moving() {
            self.redraw();
        }
        Ok(())
    }

    fn debug_text(&self, shared: &Shared) -> String {
        let mut text = String::from("open timeline (ms since process start)\n");
        for (stage, ms) in &self.tab.timeline.stages {
            text += &format!("  {stage:<14} {ms:>7.1}\n");
        }
        if let Some(current) = &self.tab.current {
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
                if current.view.follows_mode { format!(" ({:?})", self.prefs.mode) } else { String::new() },
            );
        }
        if let Some(folder) = &self.tab.folder {
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
        if let Some((ms, how)) = self.tab.last_turn {
            text += &format!("turn    {ms:.1} ms ({how})\n");
        }
        if let Some(ms) = self.tab.last_decode_ms {
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
        if let Some(current) = &mut self.tab.current {
            let at = at.unwrap_or((current.view.window.0 / 2.0, current.view.window.1 / 2.0));
            current.view.zoom_at(at, factor);
            self.show_zoom();
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
        let cursor = self.cursor_f32();
        // A double-click is two clicks on the same thing (the same tab, the
        // title, the picture), close together in time and place, as Windows
        // counts them; two quick clicks on different tabs are two switches.
        let (now, target) = (Instant::now(), self.chrome.hit(cursor, self.width()));
        let double = self.last_click.is_some_and(|(t, at, hit)| hit == target && is_double_click(now - t, at, self.cursor));
        self.last_click = if double { None } else { Some((now, self.cursor, target)) };

        if let Some(hit) = self.chrome.hit(cursor, self.width()) {
            match hit {
                chrome::Hit::Lock => self.set_locked(false),
                chrome::Hit::Title | chrome::Hit::Tab(_) if double => window.set_maximized(!window.is_maximized()),
                chrome::Hit::Title => self.pressed = Some(Press::Move(self.cursor)),
                chrome::Hit::Tab(i) if self.modifiers.control_key() => self.pressed = Some(self.reorder_press(i, false)),
                chrome::Hit::Tab(i) => self.pressed = Some(Press::Tab(i, self.cursor)),
                button => self.pressed = Some(Press::Button(button)),
            }
            return Action::None;
        }

        // A click on the info panel closes it (as Tab does).
        let on_info = self.info_rect.is_some_and(|r| (r.left..r.right).contains(&cursor.0) && (r.top..r.bottom).contains(&cursor.1));
        if self.prefs.info && on_info {
            self.last_click = None;
            return self.set_prefs(Prefs { info: false, ..self.prefs });
        }

        let Some(current) = &mut self.tab.current else {
            self.pressed = Some(Press::Move(self.cursor));
            return Action::None;
        };
        if double {
            current.view.toggle_zoom(cursor);
            self.show_zoom();
        } else {
            // Pans (nothing moves while the picture fits); only the title
            // bar moves the window.
            self.panning = Some(self.cursor);
            self.update_cursor_icon();
        }
        Action::None
    }

    fn on_right_press(&mut self) {
        // On a tab: a drag reorders (as Ctrl+left does).
        if let Some(chrome::Hit::Tab(i)) = self.chrome.hit(self.cursor_f32(), self.width()) {
            self.pressed = Some(self.reorder_press(i, true));
            return;
        }
        let cursor = self.cursor_f32();
        let (scale, anchor) = self.tab.current.as_ref().map_or((1.0, (0.0, 0.0)), |c| (c.view.scale, c.view.to_image(cursor)));
        self.right_press = Some(DragZoom { from: self.cursor, scale, anchor, active: false });
    }

    /// A right drag zooms (or reorders tabs); a right click (no drag) opens the menu.
    fn on_right_release(&mut self) -> Action {
        if let Some(Press::Reorder { moved, right: true, .. }) = self.pressed {
            self.pressed = None;
            return if moved { self.end_reorder() } else { Action::Menu };
        }
        // A release whose press Flick never saw (e.g. the click that closed
        // the menu) does nothing.
        let Some(press) = self.right_press.take() else { return Action::None };
        if press.active { Action::None } else { Action::Menu }
    }

    /// Not a click left over from closing the menu (queued during its modal
    /// loop, handled right after it), in this window or any other: a
    /// right-click on another window closes the menu too, and that window
    /// gets the press.
    fn fresh_press(&self) -> bool {
        MENU_CLOSED.lock().map_or(true, |closed| closed.is_none_or(|t| t.elapsed() > Duration::from_millis(50)))
    }

    /// A drag lasts only while its button is down: a release can go missing
    /// (a menu's modal loop takes it), and the next move must not zoom or
    /// pan on a button nobody holds.
    fn drop_released_drags(&mut self) {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SWAPBUTTON};
        // The buttons as they are now (not as of the messages handled so far,
        // which lags when a menu swallowed a release). GetAsyncKeyState
        // reports the physical buttons: swap them back for left-handed setups.
        let held = |vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| unsafe { GetAsyncKeyState(i32::from(vk.0)) } < 0;
        let (physical_left, physical_right) = (held(VK_LBUTTON), held(VK_RBUTTON));
        let swapped = unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0;
        let (left, right) = if swapped { (physical_right, physical_left) } else { (physical_left, physical_right) };
        if !right {
            self.right_press = None;
        }
        let pressed_by_right = matches!(self.pressed, Some(Press::Reorder { right: true, .. }));
        if self.pressed.is_some() && !(if pressed_by_right { right } else { left }) {
            self.pressed = None;
            self.chrome.set_drag(None);
        }
        if self.panning.is_some() && !left {
            self.panning = None;
            self.update_cursor_icon();
        }
    }

    /// A press on tab `i` that may become a drag along the strip.
    fn reorder_press(&self, i: usize, right: bool) -> Press {
        let (x, y) = self.chrome.strip(self.width()).map_or((0.0, 0.0), |strip| strip.origin(i));
        let grab = (self.cursor.0 - f64::from(x), self.cursor.1 - f64::from(y));
        Press::Reorder { at: i, from: self.cursor, grab, moved: false, right }
    }

    /// The tab id at strip position `at`.
    fn tab_id_at(&self, at: usize) -> Option<TabId> {
        match at.cmp(&self.active_at) {
            std::cmp::Ordering::Equal => Some(self.tab.id),
            std::cmp::Ordering::Less => self.others.get(at).map(|t| t.id),
            std::cmp::Ordering::Greater => self.others.get(at - 1).map(|t| t.id),
        }
    }

    /// The dragged tab follows the cursor; it takes the cell its centre is
    /// nearest to, in any row (the others glide aside, chrome.rs).
    fn drag_tab(&mut self) {
        let Some(Press::Reorder { at, from, grab, moved, right }) = self.pressed else { return };
        if !moved && !beyond_drag_threshold(from, self.cursor) {
            return;
        }
        let Some(strip) = self.chrome.strip(self.width()) else { return };
        let corner = ((self.cursor.0 - grab.0) as f32, (self.cursor.1 - grab.1) as f32);
        let slot = strip.nearest(corner, self.tab_count());
        self.move_tab(at, slot);
        self.pressed = Some(Press::Reorder { at: slot, from, grab, moved: true, right });
        self.chrome.set_drag(self.tab_id_at(slot).map(|id| (id, corner)));
        self.redraw();
    }

    /// Dropped: the tab glides into its slot.
    fn end_reorder(&mut self) -> Action {
        self.chrome.set_drag(None);
        self.redraw();
        Action::None
    }

    /// A right drag on the picture: horizontal travel sets the zoom, around
    /// where it started (twice or half the size per ZOOM_DRAG px; right: in).
    fn drag_zoom(&mut self) {
        let Some(DragZoom { from, scale, anchor, active }) = self.right_press else { return };
        if !active && !beyond_drag_threshold(from, self.cursor) {
            return;
        }
        self.right_press = Some(DragZoom { from, scale, anchor, active: true });
        let per_doubling = ZOOM_DRAG * self.window.as_ref().map_or(1.0, |w| w.scale_factor());
        let target = scale * 2f32.powf(((self.cursor.0 - from.0) / per_doubling) as f32);
        if let Some(current) = &mut self.tab.current {
            current.view.zoom_anchored(anchor, (from.0 as f32, from.1 as f32), target);
            self.show_zoom();
        }
    }

    fn on_left_release(&mut self, shared: &Shared) -> Action {
        if self.panning.take().is_some() {
            self.update_cursor_icon();
        }
        let pressed = self.pressed.take();
        match pressed {
            Some(Press::Tab(i, _) | Press::Reorder { at: i, moved: false, .. }) => return self.activate(i, shared),
            Some(Press::Reorder { .. }) => return self.end_reorder(),
            _ => {}
        }
        let (Some(Press::Button(button)), Some(window)) = (pressed, self.window.clone()) else { return Action::None };
        // Released somewhere else: cancelled, as with native buttons.
        if self.chrome.hit(self.cursor_f32(), self.width()) != Some(button) {
            return Action::None;
        }
        match button {
            chrome::Hit::Close => return Action::Close,
            chrome::Hit::Minimize => window.set_minimized(true),
            chrome::Hit::Maximize => window.set_maximized(!window.is_maximized()),
            chrome::Hit::Title | chrome::Hit::Tab(_) | chrome::Hit::Lock => {}
        }
        Action::None
    }

    /// Middle click: closes the tab under the cursor.
    fn on_middle_release(&mut self, shared: &Shared) -> Action {
        match self.chrome.hit(self.cursor_f32(), self.width()) {
            Some(chrome::Hit::Tab(i)) => self.close_tab(i, shared),
            _ => Action::None,
        }
    }

    /// Applies menu settings to this window: the view mode to its current
    /// picture, the filter to drawing, the sort order to its folder.
    fn set_prefs(&mut self, prefs: Prefs) -> Action {
        let old = std::mem::replace(&mut self.prefs, prefs);
        if prefs.mode != old.mode {
            if let Some(current) = &mut self.tab.current {
                current.view.set_mode(prefs.mode);
                self.show_zoom();
            }
        }
        if prefs.sort != old.sort {
            if let Some(folder) = &mut self.tab.folder {
                folder.sort(prefs.sort);
                self.tab.target = folder.index;
            }
        }
        self.redraw();
        Action::PrefsChanged(prefs)
    }

    /// The right-click menu; `others` are the other windows (id, colour,
    /// the file each shows) for Move to Window.
    pub fn show_menu(&mut self, others: &[(ViewerId, usize, String)], global: Global, shared: &Shared) -> Action {
        let Some(hwnd) = self.hwnd else { return Action::None };
        let targets: Vec<menu::Target> = others.iter().map(|(_, color, label)| menu::Target { color: *color, label: label.clone() }).collect();
        // A tab can leave for a new window if it isn't the only one, and tabs are on.
        let can_split = self.tab_count() > 1 && global.tabs;
        // Tab Lock only means something with tabs.
        let lock = (self.tab_count() > 1).then_some(self.locked);
        let favorites = library::favorites();
        let turn = self.turn_of(&self.tab);
        let chosen = menu::show(hwnd, self.prefs, &targets, can_split, lock, global, favorites.as_deref(), turn, self.slideshow.is_some());
        // Whatever the clicks that closed the menu left half-done (a press
        // without its release) is dropped, and presses queued while it was
        // open are ignored when they arrive just after (`fresh_press`).
        self.pressed = None;
        self.right_press = None;
        self.panning = None;
        if let Ok(mut closed) = MENU_CLOSED.lock() {
            *closed = Some(Instant::now());
        }
        match chosen {
            Some(menu::Choice::Prefs(prefs)) => self.set_prefs(prefs),
            Some(menu::Choice::MoveTo(i)) => Action::MoveTab(others.get(i).map(|(id, _, _)| *id)),
            Some(menu::Choice::NewWindow) => Action::MoveTab(None),
            Some(menu::Choice::Lock) => self.toggle_lock(),
            Some(menu::Choice::Global(global)) => Action::GlobalChanged(global),
            Some(menu::Choice::Reopen) => Action::ReopenMenu,
            Some(menu::Choice::ChooseFavorites) => {
                self.choose_favorites();
                Action::None
            }
            Some(menu::Choice::Turn(turn)) => self.set_turn(turn),
            Some(menu::Choice::PersistTurn) => self.toggle_persist_turn(),
            Some(menu::Choice::Slideshow) => self.toggle_slideshow(),
            Some(menu::Choice::SlideInterval(ms)) => self.set_slide_interval(ms),
            Some(menu::Choice::Delete) => self.delete(false, shared),
            None => Action::None,
        }
    }

    /// Tab Lock (L): the wheel and arrows switch tabs instead of pages. Only
    /// with tabs (L does nothing in a single-picture window).
    fn toggle_lock(&mut self) -> Action {
        self.set_locked(!self.locked && self.tab_count() > 1);
        Action::None
    }

    fn set_locked(&mut self, locked: bool) {
        if locked != self.locked {
            self.locked = locked;
            self.chrome.set_locked(locked);
            self.redraw();
        }
    }

    /// Tab Lock's paging: `delta` tabs along the strip, wrapping around.
    fn step_tab(&mut self, delta: i64, shared: &Shared) -> Action {
        self.tab_random = false;
        let count = self.tab_count() as i64;
        self.switch_tab((self.active_at as i64 + delta).rem_euclid(count) as usize, shared)
    }

    /// Tab Lock's random paging: the next (or previous) tab in the window's
    /// shuffled order, so back retraces forward and a round visits each tab
    /// once, as random paging does with pictures.
    fn random_tab(&mut self, forward: bool, shared: &Shared) -> Action {
        self.tab_random = true;
        match self.next_random_tab(forward) {
            Some(at) => self.switch_tab(at, shared),
            None => Action::None,
        }
    }

    /// The strip position of the next (or previous) tab in the random order.
    fn next_random_tab(&mut self, forward: bool) -> Option<usize> {
        let count = self.tab_count();
        let ids: Vec<TabId> = (0..count).filter_map(|at| self.tab_id_at(at)).collect();
        if self.tab_shuffle.len() != count || !ids.iter().all(|id| self.tab_shuffle.contains(id)) {
            let mut sorted = ids.clone();
            sorted.sort_unstable();
            let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64) ^ self.id;
            self.tab_shuffle = folder::shuffled(count, seed).into_iter().map(|i| sorted[i as usize]).collect();
        }
        let here = self.tab_shuffle.iter().position(|&id| id == self.tab.id)?;
        let next = self.tab_shuffle[if forward { (here + 1) % count } else { (here + count - 1) % count }];
        ids.iter().position(|&id| id == next)
    }

    /// A tab switch from the keys or the wheel: the strip shows for a
    /// moment, so it is clear which tab this is.
    fn switch_tab(&mut self, at: usize, shared: &Shared) -> Action {
        self.chrome.reveal(REVEAL);
        self.redraw();
        self.activate(at, shared)
    }

    /// What Move to Window shows for this window: its colour and file.
    pub fn menu_entry(&self) -> (ViewerId, usize, String) {
        (self.id, self.color, self.tab.label())
    }

    /// One step in the window's shuffled order (S/Down forward, W/Up back).
    fn random(&mut self, forward: bool, shared: &Shared) -> Action {
        let Some(index) = self.tab.folder.as_mut().and_then(|f| f.random_step(forward, self.tab.seed)) else { return Action::None };
        self.jump(index, shared);
        // Preloading now runs ahead along the shuffled order, this way.
        self.tab.random_heading = if forward { 1 } else { -1 };
        Action::Navigated
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
        // Pages on its own only (Ctrl zooms, above; Shift or Alt: nothing).
        if self.modifiers.shift_key() || self.modifiers.alt_key() {
            return Action::None;
        }
        self.stop_slideshow_by_hand();
        if self.locked {
            return self.step_tab(-whole as i64, shared);
        }
        self.step(-whole as i64, shared)
    }

    /// Still getting a picture on screen: the one wanted is decoding, or
    /// paging is walking toward its target.
    fn busy(&self) -> bool {
        self.tab.shown_at.is_none() || self.tab.folder.as_ref().is_some_and(|f| f.index != self.tab.target)
    }

    fn on_key(&mut self, event: &KeyEvent, shared: &Shared) -> Action {
        // A held key pages as fast as pictures get on screen, as in
        // Honeyview: its repeats are dropped while one is still on its way,
        // so nothing piles up and paging stops when the key is released.
        // (Separate presses always count; the wheel walks through everything.)
        let page = !(event.repeat && self.busy());
        // Moving by hand stops a slideshow (other keys, zoom say, don't).
        if self.slideshow.is_some() && is_navigation(event, self.modifiers) {
            self.stop_slideshow_by_hand();
        }
        // Rotate ▸ Alt+arrows (this tab's turn), Alt+H / Alt+V (mirrors).
        if self.modifiers.alt_key() && !event.repeat {
            let turn = self.turn_of(&self.tab);
            let quarter = |quarter| Some(Turn { quarter, ..turn });
            let chosen = match (event.physical_key, event.logical_key.as_ref()) {
                (_, Key::Named(NamedKey::ArrowUp)) => quarter(0),
                (_, Key::Named(NamedKey::ArrowRight)) => quarter(1),
                (_, Key::Named(NamedKey::ArrowDown)) => quarter(2),
                (_, Key::Named(NamedKey::ArrowLeft)) => quarter(3),
                (PhysicalKey::Code(KeyCode::KeyH), _) => Some(Turn { flip_h: !turn.flip_h, ..turn }),
                (PhysicalKey::Code(KeyCode::KeyV), _) => Some(Turn { flip_v: !turn.flip_v, ..turn }),
                // Alt+F: not flipped → horizontally → vertically → not flipped.
                // (Both ways is a 180° turn, which Ctrl+R reaches.)
                (PhysicalKey::Code(KeyCode::KeyF), _) => Some(match (turn.flip_h, turn.flip_v) {
                    (false, false) => Turn { flip_h: true, flip_v: false, ..turn },
                    (true, false) => Turn { flip_h: false, flip_v: true, ..turn },
                    _ => Turn { flip_h: false, flip_v: false, ..turn },
                }),
                _ => None,
            };
            if let Some(turn) = chosen {
                return self.set_turn(turn);
            }
        }
        // Tabs: Ctrl+Tab / Ctrl+Shift+Tab cycle, Ctrl+1..9 jump, Ctrl+W closes.
        if self.modifiers.control_key() {
            let count = self.others.len() + 1;
            let action = match (event.physical_key, event.logical_key.as_ref()) {
                (_, Key::Named(NamedKey::Tab)) => {
                    let step = if self.modifiers.shift_key() { count - 1 } else { 1 };
                    Some(self.switch_tab((self.active_at + step) % count, shared))
                }
                (PhysicalKey::Code(KeyCode::KeyW), _) => Some(self.close_tab(self.active_at, shared)),
                // Once per press, not per key repeat.
                (PhysicalKey::Code(KeyCode::KeyC | KeyCode::KeyD | KeyCode::KeyN | KeyCode::KeyF | KeyCode::KeyG | KeyCode::KeyR), _) | (_, Key::Named(NamedKey::Enter))
                    if event.repeat =>
                {
                    Some(Action::None)
                }
                // The slideshow's interval: finer steps than the menu's.
                (PhysicalKey::Code(KeyCode::BracketLeft), _) => Some(self.adjust_slide(false)),
                (PhysicalKey::Code(KeyCode::BracketRight), _) => Some(self.adjust_slide(true)),
                // Ctrl+R: the next quarter turn clockwise (four make normal again).
                (PhysicalKey::Code(KeyCode::KeyR), _) => {
                    let turn = self.turn_of(&self.tab);
                    Some(self.set_turn(Turn { quarter: (turn.quarter + 1) % 4, ..turn }))
                }
                (PhysicalKey::Code(KeyCode::KeyC), _) => Some(self.copy(shared)),
                (PhysicalKey::Code(KeyCode::KeyF), _) => Some(self.star(1, shared)),
                (PhysicalKey::Code(KeyCode::KeyG), _) => Some(self.toggle_slideshow()),
                (PhysicalKey::Code(KeyCode::KeyD), _) => Some(self.duplicate(false)),
                (PhysicalKey::Code(KeyCode::KeyN), _) => Some(self.duplicate(true)),
                (_, Key::Named(NamedKey::Enter)) => Some(self.show_in_explorer()),
                (PhysicalKey::Code(code), _) => {
                    let digits = [KeyCode::Digit1, KeyCode::Digit2, KeyCode::Digit3, KeyCode::Digit4, KeyCode::Digit5, KeyCode::Digit6, KeyCode::Digit7, KeyCode::Digit8, KeyCode::Digit9];
                    digits.iter().position(|&d| d == code).filter(|&i| i < count).map(|i| self.switch_tab(i, shared))
                }
                _ => None,
            };
            if let Some(action) = action {
                return action;
            }
        }
        // A plain hotkey works on its own only: with Ctrl or Alt, a key not
        // bound above does nothing; with Shift, only the Shift bindings here
        // do (and "+", which takes Shift on most layouts).
        if self.modifiers.control_key() || self.modifiers.alt_key() {
            return Action::None;
        }
        if self.modifiers.shift_key() {
            return match (event.physical_key, event.logical_key.as_ref()) {
                // Stars: Ctrl+F adds one (above), Shift+F takes one away.
                (PhysicalKey::Code(KeyCode::KeyF), _) if !event.repeat => self.star(-1, shared),
                // Delete for good (always asks first).
                (_, Key::Named(NamedKey::Delete)) if !event.repeat => self.delete(true, shared),
                // Debugging, beside F12: the next frame finds the graphics
                // device lost, and the app recovers as from a real loss.
                (_, Key::Named(NamedKey::F12)) if !event.repeat => {
                    crate::gpu::simulate_loss();
                    self.redraw();
                    Action::None
                }
                (_, Key::Character("+")) => {
                    self.zoom(None, ZOOM_STEP);
                    Action::None
                }
                _ => Action::None,
            };
        }
        // Letters and digits by position, so they work in any keyboard
        // layout (as Honeyview's do): Z is Z on a Russian keyboard too.
        if let PhysicalKey::Code(code) = event.physical_key {
            let prefs = self.prefs;
            let action = match code {
                // Tab Lock: what pages pictures switches tabs instead.
                KeyCode::KeyD if self.locked => Some(self.step_tab(1, shared)),
                KeyCode::KeyA if self.locked => Some(self.step_tab(-1, shared)),
                KeyCode::KeyS if self.locked => Some(self.random_tab(true, shared)),
                KeyCode::KeyW if self.locked => Some(self.random_tab(false, shared)),
                KeyCode::KeyF => Some(Action::None), // stars take Ctrl or Shift
                // The folder before or after this one.
                KeyCode::BracketLeft | KeyCode::BracketRight if event.repeat => Some(Action::None),
                KeyCode::BracketLeft => Some(self.sibling(false, shared)),
                KeyCode::BracketRight => Some(self.sibling(true, shared)),
                KeyCode::KeyL => {
                    // The bar shows for a moment, padlock and all, so it is
                    // clear whether L locked or unlocked.
                    let was = self.locked;
                    let action = self.toggle_lock();
                    if self.locked != was {
                        self.chrome.reveal(REVEAL);
                    }
                    Some(action)
                }
                KeyCode::KeyD if page => Some(self.step(1, shared)),
                KeyCode::KeyA if page => Some(self.step(-1, shared)),
                KeyCode::KeyS if page => Some(self.random(true, shared)),
                KeyCode::KeyW if page => Some(self.random(false, shared)),
                KeyCode::KeyD | KeyCode::KeyA | KeyCode::KeyS | KeyCode::KeyW => Some(Action::None),
                // The menu's keys, as in Honeyview (Soften+Sharpen's S is random paging here).
                KeyCode::Digit0 => Some(self.set_prefs(Prefs { mode: Mode { fit: Fit::Original, ..prefs.mode }, ..prefs })),
                KeyCode::Digit9 => Some(self.set_prefs(Prefs { mode: Mode { fit: Fit::Window, ..prefs.mode }, ..prefs })),
                KeyCode::Digit8 => Some(self.set_prefs(Prefs { mode: Mode { fit: Fit::Width, ..prefs.mode }, ..prefs })),
                KeyCode::Digit7 => Some(self.set_prefs(Prefs { mode: Mode { fit: Fit::Height, ..prefs.mode }, ..prefs })),
                KeyCode::Digit6 => Some(self.set_prefs(Prefs { mode: Mode { fit: Fit::Fill, ..prefs.mode }, ..prefs })),
                KeyCode::KeyZ => Some(self.set_prefs(Prefs { mode: Mode { stretch_small: !prefs.mode.stretch_small, ..prefs.mode }, ..prefs })),
                KeyCode::KeyU => Some(self.set_prefs(Prefs { filter: Filter::Nearest, ..prefs })),
                KeyCode::KeyI => Some(self.set_prefs(Prefs { filter: Filter::Soften, ..prefs })),
                _ => None,
            };
            if let Some(action) = action {
                return action;
            }
        }
        match event.logical_key.as_ref() {
            // To the Recycle Bin (Shift+Del, above: for good). Always asks first.
            Key::Named(NamedKey::Delete) if !event.repeat => self.delete(false, shared),
            // The info panel, on or off (a setting: saved, new windows follow).
            Key::Named(NamedKey::Tab) if event.repeat => Action::None,
            Key::Named(NamedKey::Tab) => self.set_prefs(Prefs { info: !self.prefs.info, ..self.prefs }),
            Key::Named(NamedKey::ArrowRight) if self.locked => self.step_tab(1, shared),
            Key::Named(NamedKey::ArrowLeft) if self.locked => self.step_tab(-1, shared),
            Key::Named(NamedKey::ArrowDown) if self.locked => self.random_tab(true, shared),
            Key::Named(NamedKey::ArrowUp) if self.locked => self.random_tab(false, shared),
            Key::Named(NamedKey::ArrowRight | NamedKey::PageDown | NamedKey::Space) if page => self.step(1, shared),
            Key::Named(NamedKey::ArrowLeft | NamedKey::PageUp | NamedKey::Backspace) if page => self.step(-1, shared),
            Key::Named(NamedKey::ArrowDown) if page => self.random(true, shared),
            Key::Named(NamedKey::ArrowUp) if page => self.random(false, shared),
            // The keyboard's Back and Forward (media) keys: the tab's history.
            Key::Named(NamedKey::BrowserBack) => self.history(true, shared),
            Key::Named(NamedKey::BrowserForward) => self.history(false, shared),
            Key::Named(NamedKey::Home) => self.jump(0, shared),
            Key::Named(NamedKey::End) => match self.tab.folder.as_ref().map(|f| f.files.len().saturating_sub(1)) {
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
            Key::Named(NamedKey::ContextMenu) => Action::Menu,
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
                    for current in self.tab.current.iter_mut().chain(self.holdover.iter_mut()) {
                        current.view.resize((size.width.max(1), size.height.max(1)));
                    }
                    // Resized buffers hold nothing until the next present, and
                    // the compositor shows them as they are (black): draw now,
                    // not on a later redraw request. (Not while still hidden:
                    // the first frame shown must have the picture.)
                    if self.window.as_ref().and_then(|w| w.is_visible()) == Some(true) {
                        self.render(shared)?;
                    } else {
                        self.redraw();
                    }
                }
                Action::None
            }
            WindowEvent::CursorMoved { position, .. } => {
                let previous = self.cursor;
                self.cursor = (position.x, position.y);
                self.drop_released_drags();
                if let (Some(Press::Move(from) | Press::Tab(_, from)), Some(window)) = (self.pressed, &self.window) {
                    if beyond_drag_threshold(from, self.cursor) {
                        self.pressed = None;
                        let _ = window.drag_window();
                    }
                }
                self.drag_tab();
                self.drag_zoom();
                if self.panning.is_none() && self.chrome.pointer(Some(self.cursor_f32()), self.width()) {
                    self.redraw();
                }
                if self.panning.is_some() {
                    if let Some(current) = &mut self.tab.current {
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
                ElementState::Pressed if self.fresh_press() => self.on_left_press(),
                ElementState::Pressed => Action::None,
                ElementState::Released => self.on_left_release(shared),
            },
            // The mouse's side buttons, as the keyboard's Back and Forward.
            WindowEvent::MouseInput { state: ElementState::Pressed, button: button @ (MouseButton::Back | MouseButton::Forward), .. } => {
                self.stop_slideshow_by_hand();
                self.history(button == MouseButton::Back, shared)
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Middle, .. } => self.on_middle_press(),
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Middle, .. } => self.on_middle_release(shared),
            WindowEvent::MouseInput { state, button: MouseButton::Right, .. } => match state {
                ElementState::Pressed => {
                    if self.fresh_press() {
                        self.on_right_press();
                    }
                    Action::None
                }
                // The menu opens on release, like Windows' own context menus.
                ElementState::Released => self.on_right_release(),
            },
            WindowEvent::MouseWheel { delta, .. } => self.on_wheel(delta, shared),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => self.on_key(&event, shared),
            _ => Action::None,
        })
    }

    /// Writes the open timeline and page-turn log, if `--timeline-out` asked for it.
    pub fn write_timeline(&self, error: Option<&str>) {
        let Some(path) = &self.tab.request.timeline_out else { return };
        let stages = self.tab.timeline.stages.iter().map(|(name, ms)| format!("[\"{name}\",{ms:.3}]")).collect::<Vec<_>>().join(",");
        let turns = self
            .tab
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
