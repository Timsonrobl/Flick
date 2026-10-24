//! Flick: a frameless image viewer (ARCHITECTURE.md). Milestone 3: one
//! resident process with many windows.
//!
//! Open path (§3a): the screen-scale decode and the GPU device start on their
//! own threads at `main`. Each window is created hidden, placed, and shown
//! only after its final picture has been presented. Once it is on screen,
//! the folder is listed and neighbours are preloaded (§6).
//!
//! Single instance (§3): the first Flick in a session stays resident (tray
//! icon) and keeps its GPU device after the last window closes; later
//! launches hand their file over a named pipe and exit (a warm open).
//!
//! Usage: flick <image> [--size WxH] [--timeline-out file.json] [--standalone]
//! `--standalone` neither joins nor becomes the resident instance and exits
//! when its window closes (cold-open benchmarks).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod chrome;
mod clock;
mod debug;
mod decode;
mod elevation;
mod folder;
mod gpu;
mod icon;
mod instance;
mod menu;
mod placement;
mod pool;
mod settings;
mod tray;
mod view;
mod viewer;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

use clock::{Timeline, ms_since_start};
use decode::{Image, Picture};
use folder::Folder;
use gpu::Gpu;
use instance::Request;
use pool::Pool;
use viewer::{Action, Screen, Shared, Viewer, ViewerId};

const DECODE_THREADS: usize = 3;
/// With no windows open, the resident process trims its memory after this.
const TRIM_AFTER: Duration = Duration::from_secs(30);

pub enum Event {
    /// A window's first picture, decoded on its open path.
    Decoded { viewer: ViewerId, picture: Result<Picture, String>, at: f64 },
    GpuReady(Result<Gpu, String>, Vec<(&'static str, f64)>),
    /// The compositor has shown a window's first picture (after `DwmFlush`).
    OnScreen { viewer: ViewerId, at: f64 },
    Listed { viewer: ViewerId, folder: Result<Folder, String> },
    /// A screen level for some window's ring, from the decode pool.
    Preloaded { path: PathBuf, picture: Result<Picture, String>, decode_ms: f64 },
    FullDecoded { viewer: ViewerId, path: PathBuf, image: Result<Image, String>, at: f64 },
    /// An animated GIF's next frame (or its end), for one window.
    Anim { viewer: ViewerId, serial: u64, update: viewer::AnimUpdate },
    /// A later launch's request, from the pipe.
    Open(Request),
    /// A newer build took over as the resident instance.
    Superseded,
    Tray(tray::Command),
}

struct App {
    shared: Shared,
    viewers: HashMap<ViewerId, Viewer>,
    windows: HashMap<WindowId, ViewerId>,
    next_id: ViewerId,
    /// Most recently focused first: preloading follows this order.
    focus_order: Vec<ViewerId>,
    /// The remembered placement (updated whenever a window closes).
    placement: Option<placement::Placement>,
    /// The menu settings last chosen in any window: new windows start with them.
    prefs: settings::Prefs,
    /// The resident instance (not `--standalone`).
    resident: bool,
    serving: bool,
    tray: Option<tray::Tray>,
    last_file: Option<PathBuf>,
    /// Set when the last window closed; the working set is trimmed later.
    idle_since: Option<Instant>,
    error: Option<String>,
}

impl App {
    fn spawn_decode(&self, viewer: ViewerId, request: &Request) {
        let (path, proxy) = (request.file.clone(), self.shared.proxy.clone());
        let target = request.size.unwrap_or(self.shared.target);
        std::thread::spawn(move || {
            let picture = decode::decode_screen(&path, target);
            let _ = proxy.send_event(Event::Decoded { viewer, picture, at: ms_since_start() });
        });
    }

    /// Opens a window for `request` (a warm open: the GPU is usually ready).
    fn open(&mut self, event_loop: &ActiveEventLoop, request: Request) {
        let id = self.next_id;
        self.next_id += 1;
        let mut timeline = Timeline::default();
        timeline.mark("request");
        // Cascade from the remembered placement while other windows are open.
        let open = self.viewers.len() as i32;
        let placement = self.placement.map(|p| if p.maximized || open == 0 { p } else { p.offset(32 * open, 32 * open) });
        self.spawn_decode(id, &request);
        self.last_file = Some(request.file.clone());
        let mut viewer = Viewer::new(id, request, placement, self.prefs, timeline);
        if let Err(e) = viewer.create_window(event_loop) {
            return report(&e);
        }
        if let Some(window) = &viewer.window {
            self.windows.insert(window.id(), id);
        }
        self.viewers.insert(id, viewer);
        self.focus_order.insert(0, id);
        self.idle_since = None;
    }

    fn try_show(&mut self, id: ViewerId) {
        let Some(viewer) = self.viewers.get_mut(&id) else { return };
        match viewer.try_show(&mut self.shared) {
            Ok(true) => {
                // Nothing else may compete with the picture: wait until the
                // compositor has put it on screen before listing the folder.
                let proxy = self.shared.proxy.clone();
                std::thread::spawn(move || {
                    let _ = unsafe { windows::Win32::Graphics::Dwm::DwmFlush() };
                    let _ = proxy.send_event(Event::OnScreen { viewer: id, at: ms_since_start() });
                });
            }
            Ok(false) => {}
            Err(e) => self.fail_viewer(id, e),
        }
    }

    /// A window that can't open or draw: tell the user and drop it.
    fn fail_viewer(&mut self, id: ViewerId, error: String) {
        let first = self.viewers.len() == 1 && !self.viewers.values().any(Viewer::is_shown);
        if first && !self.resident {
            self.error = Some(error);
        } else {
            report(&error);
        }
        self.remove_viewer(id);
    }

    fn on_screen(&mut self, id: ViewerId, at: f64) {
        let Some(viewer) = self.viewers.get_mut(&id) else { return };
        viewer.timeline.push("on_screen", at);
        let (file, sort, proxy) = (viewer.request.file.clone(), viewer.sort(), self.shared.proxy.clone());
        std::thread::spawn(move || {
            let _ = proxy.send_event(Event::Listed { viewer: id, folder: folder::list(&file, sort) });
        });
        // Off the open path: listen for later launches, show the tray icon.
        if self.resident && !self.serving {
            self.serving = true;
            let (proxy, superseded) = (Mutex::new(self.shared.proxy.clone()), self.shared.proxy.clone());
            instance::serve(
                move |request| {
                    let _ = proxy.lock().unwrap_or_else(|e| e.into_inner()).send_event(Event::Open(request));
                },
                move || {
                    let _ = superseded.send_event(Event::Superseded);
                },
            );
            let proxy = Mutex::new(self.shared.proxy.clone());
            self.tray = tray::Tray::create(move |command| {
                let _ = proxy.lock().unwrap_or_else(|e| e.into_inner()).send_event(Event::Tray(command));
            })
            .ok();
        }
    }

    fn on_listed(&mut self, id: ViewerId, listed: Result<Folder, String>) {
        let Ok(folder) = listed else { return }; // single-image mode
        if self.shared.pool.is_none() {
            let proxy = Mutex::new(self.shared.proxy.clone());
            self.shared.pool = Some(Pool::new(DECODE_THREADS, self.shared.target, move |path, picture, decode_ms| {
                let _ = proxy.lock().unwrap_or_else(|e| e.into_inner()).send_event(Event::Preloaded { path, picture, decode_ms });
            }));
        }
        if let Some(viewer) = self.viewers.get_mut(&id) {
            viewer.on_listed(folder, &mut self.shared);
        }
        self.plan();
    }

    /// Queues the missing screen levels of every window's ring (the focused
    /// window first) and drops cached ones no window needs any more.
    fn plan(&mut self) {
        let mut queue = Vec::new();
        let mut keep = HashSet::new();
        for id in &self.focus_order {
            let Some(viewer) = self.viewers.get(id) else { continue };
            for path in viewer.ring_paths() {
                if !self.shared.cache.contains_key(&path) && !self.shared.failed.contains_key(&path) && !queue.contains(&path) {
                    queue.push(path);
                }
            }
            keep.extend(viewer.keep_paths());
        }
        if let Some(pool) = &self.shared.pool {
            pool.set_queue(queue);
        }
        self.shared.cache.retain(|path, _| keep.contains(path));
    }

    fn on_preloaded(&mut self, path: PathBuf, picture: Result<Picture, String>, decode_ms: f64) {
        let needed = self.viewers.values().any(|v| v.keep_paths().contains(&path));
        match picture {
            Ok(picture) if needed => {
                let Some(gpu) = &self.shared.gpu else { return };
                match Screen::upload(gpu, &picture) {
                    Ok(screen) => {
                        self.shared.cache.insert(path.clone(), screen.clone());
                        for viewer in self.viewers.values_mut() {
                            viewer.last_decode_ms = Some(decode_ms);
                            viewer.on_preloaded(&path, &screen, &self.shared);
                        }
                    }
                    Err(e) => {
                        self.shared.failed.insert(path, e);
                    }
                }
            }
            Ok(_) => {} // scrolled past while decoding
            Err(e) => {
                for viewer in self.viewers.values_mut() {
                    viewer.on_failed(&path, &e);
                }
                self.shared.failed.insert(path, e);
            }
        }
    }

    fn remove_viewer(&mut self, id: ViewerId) -> Option<Viewer> {
        let viewer = self.viewers.remove(&id)?;
        if let Some(window) = &viewer.window {
            self.windows.remove(&window.id());
        }
        self.focus_order.retain(|v| *v != id);
        Some(viewer)
    }

    fn close(&mut self, event_loop: &ActiveEventLoop, id: ViewerId) {
        let Some(viewer) = self.remove_viewer(id) else { return };
        if let Some(saved) = viewer.save_placement() {
            self.placement = Some(saved);
        }
        viewer.write_timeline(None);
        drop(viewer);
        self.plan();
        if !self.viewers.is_empty() {
            if let Some(gpu) = &self.shared.gpu {
                gpu.release_unused();
            }
            return;
        }
        if !self.resident {
            return event_loop.exit();
        }
        // Resident with no windows: keep only the GPU device, trim later.
        self.shared.cache.clear();
        self.shared.failed.clear();
        if let Some(gpu) = &self.shared.gpu {
            gpu.release_unused();
        }
        self.idle_since = Some(Instant::now()); // `about_to_wait` sets the timer
    }

    fn on_tray(&mut self, event_loop: &ActiveEventLoop, command: tray::Command) {
        match command {
            tray::Command::Quit => event_loop.exit(),
            tray::Command::Activate => match self.focus_order.first().and_then(|id| self.viewers.get(id)) {
                Some(viewer) => {
                    if let Some(window) = &viewer.window {
                        window.set_minimized(false);
                        window.focus_window();
                    }
                }
                None => {
                    if let Some(file) = self.last_file.clone() {
                        self.open(event_loop, Request { file, ..Default::default() });
                    }
                }
            },
        }
    }
}

impl ApplicationHandler<Event> for App {
    fn new_events(&mut self, _: &ActiveEventLoop, cause: StartCause) {
        if let StartCause::ResumeTimeReached { .. } = cause
            && self.viewers.is_empty()
            && self.idle_since.is_some_and(|t| t.elapsed() >= TRIM_AFTER)
        {
            self.idle_since = None;
            unsafe {
                use windows::Win32::System::ProcessStatus::EmptyWorkingSet;
                use windows::Win32::System::Threading::GetCurrentProcess;
                let _ = EmptyWorkingSet(GetCurrentProcess());
            }
        }
    }

    /// The only timers: paging steps that are due (`Viewer::deadline`) and
    /// the idle trim. Otherwise the loop sleeps until an event.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        let mut moved = false;
        for viewer in self.viewers.values_mut() {
            if viewer.deadline().is_some_and(|t| t <= now) {
                moved |= viewer.tick(&self.shared);
            }
        }
        if moved {
            self.plan();
        }
        let wake = self.viewers.values().filter_map(Viewer::deadline).chain(self.idle_since.map(|t| t + TRIM_AFTER)).min();
        event_loop.set_control_flow(wake.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let pending: Vec<ViewerId> = self.viewers.iter().filter(|(_, v)| v.window.is_none()).map(|(id, _)| *id).collect();
        for id in pending {
            let Some(viewer) = self.viewers.get_mut(&id) else { continue };
            match viewer.create_window(event_loop) {
                Ok(()) => {
                    if let Some(window) = &viewer.window {
                        self.windows.insert(window.id(), id);
                    }
                    self.try_show(id);
                }
                Err(e) => self.fail_viewer(id, e),
            }
        }
        self.exit_if_failed(event_loop);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Event) {
        match event {
            Event::Decoded { viewer, picture, at } => {
                let Some(v) = self.viewers.get_mut(&viewer) else { return };
                v.timeline.push("decoded", at);
                match picture {
                    Ok(picture) => {
                        v.picture = Some(picture);
                        self.try_show(viewer);
                    }
                    Err(e) => self.fail_viewer(viewer, e),
                }
            }
            Event::GpuReady(gpu, stages) => {
                // The device is started with the first window; its timeline gets the stages.
                if let Some(first) = self.viewers.values_mut().min_by_key(|v| v.id) {
                    for (stage, at) in stages {
                        first.timeline.push(stage, at);
                    }
                }
                match gpu {
                    Ok(gpu) => {
                        self.shared.gpu = Some(gpu);
                        let ids: Vec<ViewerId> = self.viewers.keys().copied().collect();
                        for id in ids {
                            self.try_show(id);
                        }
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            Event::OnScreen { viewer, at } => self.on_screen(viewer, at),
            Event::Listed { viewer, folder } => self.on_listed(viewer, folder),
            Event::Preloaded { path, picture, decode_ms } => self.on_preloaded(path, picture, decode_ms),
            Event::FullDecoded { viewer, path, image, at } => {
                if let Some(v) = self.viewers.get_mut(&viewer) {
                    v.timeline.push("full_decoded", at);
                    v.on_full_decoded(path, image, &self.shared);
                }
            }
            Event::Anim { viewer, serial, update } => {
                if let Some(v) = self.viewers.get_mut(&viewer) {
                    v.on_anim(serial, update, &self.shared);
                }
            }
            Event::Open(request) => self.open(event_loop, request),
            Event::Superseded => {
                // Keep open windows; exit when the last one closes.
                self.resident = false;
                self.tray = None;
                if self.viewers.is_empty() {
                    event_loop.exit();
                }
            }
            Event::Tray(command) => self.on_tray(event_loop, command),
        }
        self.exit_if_failed(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window: WindowId, event: WindowEvent) {
        let Some(&id) = self.windows.get(&window) else { return };
        if let WindowEvent::Focused(true) = event {
            self.focus_order.retain(|v| *v != id);
            self.focus_order.insert(0, id);
            self.plan();
        }
        let Some(viewer) = self.viewers.get_mut(&id) else { return };
        let traced = !matches!(event, WindowEvent::CursorMoved { .. } | WindowEvent::RedrawRequested);
        if traced {
            clock::trace(format_args!("viewer {id}: {event:?}"));
        }
        match viewer.window_event(event, &self.shared) {
            Ok(Action::None) => {}
            Ok(Action::Navigated) => self.plan(),
            Ok(Action::PrefsChanged(prefs)) => {
                self.prefs = prefs;
                settings::save(prefs);
                self.plan(); // a new sort order moves the preload ring
            }
            Ok(Action::Close) => {
                clock::trace(format_args!("viewer {id}: close"));
                self.close(event_loop, id)
            }
            Err(e) => {
                clock::trace(format_args!("viewer {id}: failed: {e}"));
                self.fail_viewer(id, e)
            }
        }
        self.exit_if_failed(event_loop);
    }
}

impl App {
    fn exit_if_failed(&self, event_loop: &ActiveEventLoop) {
        if self.error.is_some() {
            event_loop.exit();
        }
    }
}

fn primary_display_size() -> (u32, u32) {
    use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
    let mut mode = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
    if unsafe { EnumDisplaySettingsW(None, ENUM_CURRENT_SETTINGS, &mut mode) }.as_bool() {
        (mode.dmPelsWidth, mode.dmPelsHeight)
    } else {
        (1920, 1080)
    }
}

fn report(text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    unsafe { MessageBoxW(None, &windows::core::HSTRING::from(text), windows::core::w!("Flick"), MB_ICONERROR | MB_OK) };
}

/// What the command line asks for besides the request.
struct Flags {
    standalone: bool,
    /// Started by [`elevation::relaunch_unelevated`].
    relaunched: bool,
}

fn parse_args() -> Result<(Request, Flags), String> {
    let mut request = Request::default();
    let mut flags = Flags { standalone: false, relaunched: false };
    let mut it = std::env::args_os().skip(1);
    while let Some(arg) = it.next() {
        match arg.to_str() {
            Some("--size") => {
                let v = it.next().and_then(|v| v.into_string().ok()).ok_or("--size needs WxH")?;
                let (w, h) = v.split_once('x').ok_or("--size needs WxH")?;
                request.size = Some((w.parse().map_err(|_| "bad --size")?, h.parse().map_err(|_| "bad --size")?));
            }
            Some("--timeline-out") => {
                let path = PathBuf::from(it.next().ok_or("--timeline-out needs a path")?);
                request.timeline_out = Some(std::path::absolute(&path).unwrap_or(path));
            }
            Some("--standalone") => flags.standalone = true,
            Some(elevation::RELAUNCHED) => flags.relaunched = true,
            _ => request.file = std::path::absolute(&arg).unwrap_or_else(|_| PathBuf::from(&arg)),
        }
    }
    if request.file.as_os_str().is_empty() {
        return Err("usage: flick <image> [--size WxH] [--timeline-out file] [--standalone]".into());
    }
    Ok((request, flags))
}

/// This launch would become the resident instance while elevated: start
/// Flick unelevated instead (elevation.rs). True if that worked and this
/// process should exit.
fn hand_to_unelevated(request: &Request) -> bool {
    instance::release();
    match elevation::relaunch_unelevated(request) {
        Ok(()) => true,
        Err(e) => {
            clock::trace(format_args!("{e}; staying elevated"));
            // Couldn't: be the resident as before.
            instance::claim();
            false
        }
    }
}

fn main() {
    // Before anything measures the screen; winit would set the same later.
    unsafe {
        use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let mut timeline = Timeline::default();
    timeline.mark("main");
    let (mut request, flags) = match parse_args() {
        Ok(parsed) => parsed,
        Err(e) => return report(&e),
    };
    let standalone = flags.standalone;
    request.build = instance::build_id();

    // A warm open: Flick is already resident, so hand the file over and go
    // (unless the resident one is an older build: then this launch takes over).
    // An elevated launch never stays as the resident one (elevation.rs).
    let elevated = !standalone && !flags.relaunched && elevation::is_elevated();
    let mut resident = !standalone && instance::claim();
    if resident && elevated && hand_to_unelevated(&request) {
        return;
    }
    if !standalone && !resident {
        match instance::send(&request) {
            Ok(instance::Handoff::Delivered) => return,
            Ok(instance::Handoff::BecomeResident) if elevated && hand_to_unelevated(&request) => return,
            Ok(instance::Handoff::BecomeResident) => resident = true,
            Err(e) => report(&format!("{e}\nOpening in a separate window.")),
        }
    }

    let event_loop = match EventLoop::<Event>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(e) => return report(&e.to_string()),
    };
    timeline.mark("event_loop");
    let proxy = event_loop.create_proxy();
    let target = request.size.unwrap_or_else(primary_display_size);

    // The two long poles, in parallel from the start.
    let first: ViewerId = 1;
    let (path, decode_proxy) = (request.file.clone(), proxy.clone());
    std::thread::spawn(move || {
        let picture = decode::decode_screen(&path, target);
        let _ = decode_proxy.send_event(Event::Decoded { viewer: first, picture, at: ms_since_start() });
    });
    let gpu_proxy = proxy.clone();
    std::thread::spawn(move || {
        let mut stages = Vec::new();
        let gpu = Gpu::create(&mut |stage| stages.push((stage, ms_since_start())));
        let _ = gpu_proxy.send_event(Event::GpuReady(gpu, stages));
    });

    let placement = if request.size.is_none() { placement::load() } else { None };
    // `--size` benchmark runs keep the defaults, as they keep the default placement.
    let prefs = if request.size.is_none() { settings::load() } else { settings::Prefs::default() };
    let last_file = Some(request.file.clone());
    let mut app = App {
        shared: Shared { proxy, gpu: None, pool: None, cache: HashMap::new(), failed: HashMap::new(), target },
        viewers: HashMap::from([(first, Viewer::new(first, request, placement, prefs, timeline))]),
        windows: HashMap::new(),
        next_id: first + 1,
        focus_order: vec![first],
        placement,
        prefs,
        resident,
        serving: false,
        tray: None,
        last_file,
        idle_since: None,
        error: None,
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        app.error.get_or_insert(e.to_string());
    }
    for viewer in app.viewers.values() {
        viewer.write_timeline(app.error.as_deref());
    }
    if let Some(saved) = app.focus_order.first().and_then(|id| app.viewers.get(id)) {
        saved.save_placement();
    }
    drop(app.tray.take());
    if let Some(e) = &app.error {
        report(e);
    }
}
