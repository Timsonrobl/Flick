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
mod clipboard;
mod clock;
mod debug;
mod decode;
mod elevation;
mod folder;
mod formats;
mod gpu;
mod history;
mod icon;
mod instance;
mod library;
mod menu;
mod meta;
mod osd;
mod placement;
mod pool;
mod settings;
mod tray;
mod view;
mod viewer;
mod watch;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

use clock::{Timeline, ms_since_start};
use decode::{Image, Picture};
use folder::{Change, Folder};
use gpu::Gpu;
use instance::Request;
use pool::Pool;
use viewer::{Action, Screen, Shared, Tab, TabId, Viewer, ViewerId};

const DECODE_THREADS: usize = 3;
/// Files arriving to open within this of the previous one form a burst
/// (a selection opened from Explorer): only the first is shown.
const BURST: Duration = Duration::from_millis(500);
/// With no windows open, the resident process trims its memory after this.
const TRIM_AFTER: Duration = Duration::from_secs(30);

pub enum Event {
    /// A window's first picture, decoded on its open path.
    Decoded { viewer: ViewerId, tab: TabId, picture: Result<Picture, String>, at: f64 },
    GpuReady(Result<Gpu, String>, Vec<(&'static str, f64)>),
    /// Windows reported that device lost (`App::gpu_generation` when it was made).
    GpuLost(u64),
    /// The compositor has shown a window's first picture (after `DwmFlush`).
    OnScreen { viewer: ViewerId, tab: TabId, at: f64 },
    Listed { viewer: ViewerId, tab: TabId, folder: Result<Folder, String> },
    /// A screen level for some window's ring, from the decode pool.
    Preloaded { path: PathBuf, picture: Result<Picture, String>, decode_ms: f64 },
    /// A zoom's detail level (`decode::decode_level`): the image and its
    /// fraction of native size.
    DetailDecoded { viewer: ViewerId, path: PathBuf, level: Result<(Image, u32), String>, at: f64 },
    /// An animated GIF's next frame (or its end), for one window.
    Anim { viewer: ViewerId, serial: u64, update: viewer::AnimUpdate },
    /// The info panel's details for a file (`meta::read`).
    Info { viewer: ViewerId, path: PathBuf, info: meta::Info },
    /// Ctrl+C: the picture at full resolution, ready for the clipboard.
    Copy { viewer: ViewerId, image: Result<Image, String> },
    /// A watched folder changed (`watch.rs`).
    FolderChanged { dir: PathBuf, changes: Vec<Change> },
    /// `[` / `]`: a sibling folder, listed (or why there's none); or Back /
    /// Forward's picture (`at`) in another folder, its folder listed.
    Entered { viewer: ViewerId, tab: TabId, result: Result<Folder, String>, at: Option<PathBuf> },
    /// A file's stars changed (Ctrl+F / Shift+F in that window): the new count.
    Starred { viewer: ViewerId, path: PathBuf, result: Result<library::Stars, String> },
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
    /// Preferences ▸: app-wide.
    global: settings::Global,
    /// The resident instance (not `--standalone`).
    resident: bool,
    serving: bool,
    tray: Option<tray::Tray>,
    last_file: Option<PathBuf>,
    /// Set when the last window closed; the working set is trimmed later.
    idle_since: Option<Instant>,
    /// When the last file arrived to open, and the window its burst goes to
    /// with how many background tabs it has added so far.
    last_open: Option<Instant>,
    burst: Option<(ViewerId, usize)>,
    error: Option<String>,
    /// One watcher per folder some tab pages through (`sync_watchers`).
    watchers: HashMap<PathBuf, watch::Watcher>,
    /// Changes reported per folder, applied once it has been quiet for
    /// FOLDER_SETTLE (a file being written reports many).
    folder_changes: HashMap<PathBuf, (Vec<Change>, Instant)>,
    /// Counts graphics devices made after a loss (`Event::GpuLost`).
    gpu_generation: u64,
}

/// A folder's changes wait this long for the next one before they're applied.
const FOLDER_SETTLE: Duration = Duration::from_millis(250);

impl App {
    fn spawn_decode(&self, viewer: ViewerId, tab: TabId, request: &Request) {
        let (path, proxy) = (request.file.clone(), self.shared.proxy.clone());
        let target = request.size.unwrap_or(self.shared.target);
        std::thread::spawn(move || {
            let picture = decode::decode_screen(&path, target);
            let _ = proxy.send_event(Event::Decoded { viewer, tab, picture, at: ms_since_start() });
        });
    }

    fn new_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id - 1
    }

    /// Opens `request` (a warm open: the GPU is usually ready) as the first
    /// tab of the window used last (ARCHITECTURE.md §10a), or in a new
    /// window when there is none (and for `--size` benchmark runs).
    fn open(&mut self, event_loop: &ActiveEventLoop, request: Request) {
        let mut timeline = Timeline::default();
        timeline.mark("request");
        self.last_file = Some(request.file.clone());
        self.idle_since = None;
        let tab_id = self.new_id();
        let tabs = request.size.is_none() && self.global.tabs;
        // Explorer opens a selection of files with one launch per file, all
        // at once. The first opens as usual; the rest of the burst become
        // background tabs after it, decoded only when first viewed.
        let now = Instant::now();
        let in_burst = self.last_open.is_some_and(|t| now - t < BURST);
        self.last_open = Some(now);
        if let (true, Some((id, count))) = (tabs && in_burst, self.burst) {
            if let Some(viewer) = self.viewers.get_mut(&id) {
                viewer.add_background_tab(Tab::waiting(tab_id, request, timeline), count);
                self.burst = Some((id, count + 1));
                return;
            }
        }
        let last = self.focus_order.iter().copied().find(|id| self.viewers.get(id).is_some_and(Viewer::is_shown));
        if let (Some(id), true) = (last, tabs) {
            self.spawn_decode(id, tab_id, &request);
            if let Some(viewer) = self.viewers.get_mut(&id) {
                viewer.add_tab(Tab::new(tab_id, request, timeline));
            }
            self.burst = Some((id, 0));
            self.plan();
            return;
        }
        let id = self.next_id; // the id new_window gives it
        self.spawn_decode(id, tab_id, &request);
        // Tabs off: beside the window used last (benchmark runs keep their own placement).
        let beside = last.filter(|_| request.size.is_none());
        let id = self.new_window(event_loop, Tab::new(tab_id, request, timeline), beside).unwrap_or(id);
        self.burst = tabs.then_some((id, 0));
    }

    /// Lists the folder of every window's active tab that shows a picture
    /// but was never listed (a background tab of a burst, viewed at last).
    fn request_listings(&mut self) {
        let sort_of = |v: &Viewer| v.sort();
        let wanted: Vec<_> = self.viewers.values_mut().filter_map(|v| {
            let sort = sort_of(v);
            v.take_listing().map(|(tab, file)| (v.id, tab, file, sort))
        }).collect();
        for (id, tab, file, sort) in wanted {
            let proxy = self.shared.proxy.clone();
            std::thread::spawn(move || {
                let _ = proxy.send_event(Event::Listed { viewer: id, tab, folder: folder::list(&file, sort) });
            });
        }
    }

    /// A new (still hidden) window for `tab`, in the first colour no open
    /// window uses, placed `beside` the window it comes from (else where
    /// windows were last). It shows once its picture is drawn (`try_show`).
    fn new_window(&mut self, event_loop: &ActiveEventLoop, tab: Tab, beside: Option<ViewerId>) -> Option<ViewerId> {
        let id = self.new_id();
        let open = self.viewers.len() as i32;
        let placement = beside.and_then(|v| self.viewers.get(&v)).and_then(|v| v.hwnd()).and_then(placement::beside).or_else(|| {
            // Cascade from the remembered placement while other windows are open.
            self.placement.map(|p| if p.maximized || open == 0 { p } else { p.offset(32 * open, 32 * open) })
        });
        let used: Vec<usize> = self.viewers.values().map(|v| v.color).collect();
        let color = (0..chrome::PALETTE.len()).find(|c| !used.contains(c)).unwrap_or(self.viewers.len() % chrome::PALETTE.len());
        let mut viewer = Viewer::new(id, tab, placement, self.prefs, self.global, color);
        if let Err(e) = viewer.create_window(event_loop) {
            report(&e);
            return None;
        }
        if let Some(window) = &viewer.window {
            self.windows.insert(window.id(), id);
        }
        self.viewers.insert(id, viewer);
        self.focus_order.insert(0, id);
        Some(id)
    }

    /// Move to Window: the active tab of `from` goes to `to`, or to a new
    /// window; a window left without tabs closes.
    fn move_tab(&mut self, event_loop: &ActiveEventLoop, from: ViewerId, to: Option<ViewerId>) {
        let Some(source) = self.viewers.get_mut(&from) else { return };
        let (tab, emptied) = source.take_active(&self.shared);
        match to.and_then(|to| self.viewers.get_mut(&to)) {
            Some(target) => target.adopt(tab, &self.shared),
            None => {
                if let Some(id) = self.new_window(event_loop, tab, Some(from)) {
                    self.try_show(id);
                }
            }
        }
        if emptied {
            self.close(event_loop, from);
        }
        self.plan();
    }

    /// The window holding `tab` (a tab can move between windows while its
    /// decode or listing is under way).
    fn window_of(&self, tab: TabId) -> Option<ViewerId> {
        self.viewers.values().find(|v| v.has_tab(tab)).map(|v| v.id)
    }

    /// Shows a window whose first picture, GPU and window are all ready.
    fn try_show(&mut self, id: ViewerId) {
        let Some(viewer) = self.viewers.get_mut(&id) else { return };
        let (tab, shown) = (viewer.tab.id, viewer.try_show(&mut self.shared));
        self.after_show(id, tab, shown);
    }

    /// A tab's first picture arrived (`Event::Decoded`).
    fn on_decoded(&mut self, id: ViewerId, tab: TabId, picture: Picture) {
        let Some(viewer) = self.viewers.get_mut(&id) else { return };
        let shown = viewer.on_decoded(tab, picture, &mut self.shared);
        self.after_show(id, tab, shown);
    }

    fn after_show(&mut self, id: ViewerId, tab: TabId, shown: Result<bool, String>) {
        match shown {
            Ok(true) => {
                // Nothing else may compete with the picture: wait until the
                // compositor has put it on screen before listing the folder.
                let proxy = self.shared.proxy.clone();
                std::thread::spawn(move || {
                    let _ = unsafe { windows::Win32::Graphics::Dwm::DwmFlush() };
                    let _ = proxy.send_event(Event::OnScreen { viewer: id, tab, at: ms_since_start() });
                });
            }
            Ok(false) => {}
            Err(e) => self.fail_viewer(id, e),
        }
    }

    /// A window that can't open or draw: tell the user and drop it (unless
    /// the graphics device was lost: then every window comes back on a new one).
    fn fail_viewer(&mut self, id: ViewerId, error: String) {
        if self.shared.gpu.as_ref().is_some_and(Gpu::lost) {
            return self.recover_gpu();
        }
        let first = self.viewers.len() == 1 && !self.viewers.values().any(Viewer::is_shown);
        if first && !self.resident {
            self.error = Some(error);
        } else {
            report(&error);
        }
        self.remove_viewer(id);
    }

    fn on_screen(&mut self, id: ViewerId, tab: TabId, at: f64) {
        let Some(viewer) = self.viewers.get_mut(&id) else { return };
        let sort = viewer.sort();
        let Some(t) = viewer.tab_mut(tab) else { return };
        t.timeline.push("on_screen", at);
        if let Some(&(_, request)) = t.timeline.stages.iter().find(|(stage, _)| *stage == "request") {
            clock::trace(format_args!("open into a tab: {:.0} ms from request to on screen", at - request));
        }
        // Listed once: not again for a tab moved into a new window (it keeps
        // its folder and place) or one listed already.
        if !t.mark_listing() {
            let (file, proxy) = (t.request.file.clone(), self.shared.proxy.clone());
            std::thread::spawn(move || {
                let _ = proxy.send_event(Event::Listed { viewer: id, tab, folder: folder::list(&file, sort) });
            });
        }
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

    /// The graphics device is gone (a driver update or reset, a GPU hang, the
    /// adapter removed): every window lets go of what was on it, a new
    /// device is made, and each window draws again, its picture decoded anew
    /// through the pool. The windows stay where they are; nothing closes.
    fn recover_gpu(&mut self) {
        clock::trace(format_args!("graphics device lost: recovering"));
        let shown: Vec<ViewerId> = self.viewers.values().filter(|v| v.is_shown()).map(|v| v.id).collect();
        self.shared.cache.clear();
        self.shared.failed.clear();
        for viewer in self.viewers.values_mut() {
            viewer.drop_gpu();
        }
        // The old device's removal signal now means nothing.
        self.gpu_generation += 1;
        self.shared.gpu = None;
        match Gpu::create(&mut |_| {}) {
            Ok(gpu) => {
                self.watch_gpu(&gpu);
                self.shared.gpu = Some(gpu);
            }
            Err(e) => {
                self.error = Some(format!("The graphics device was lost and couldn't be started again.\n{e}"));
                return;
            }
        }
        for id in shown {
            let Some(viewer) = self.viewers.get_mut(&id) else { continue };
            if let Err(e) = viewer.restore_gpu(&self.shared) {
                report(&e);
                self.remove_viewer(id);
            }
        }
        // Windows still opening carry on as when the first device arrived.
        let ids: Vec<ViewerId> = self.viewers.keys().copied().collect();
        for id in ids {
            self.try_show(id);
        }
        self.ensure_pool();
        self.plan();
    }

    /// Recovers when Windows reports the device lost, without waiting for a
    /// window to fail drawing on it (an idle window would stay blank).
    fn watch_gpu(&self, gpu: &Gpu) {
        let Some(removed) = gpu.removed() else { return };
        let (proxy, generation) = (self.shared.proxy.clone(), self.gpu_generation);
        std::thread::spawn(move || {
            removed.wait();
            let _ = proxy.send_event(Event::GpuLost(generation));
        });
    }

    fn ensure_pool(&mut self) {
        if self.shared.pool.is_none() {
            let proxy = Mutex::new(self.shared.proxy.clone());
            self.shared.pool = Some(Pool::new(DECODE_THREADS, self.shared.target, move |path, picture, decode_ms| {
                let _ = proxy.lock().unwrap_or_else(|e| e.into_inner()).send_event(Event::Preloaded { path, picture, decode_ms });
            }));
        }
    }

    fn on_listed(&mut self, id: ViewerId, tab: TabId, listed: Result<Folder, String>) {
        let Ok(folder) = listed else { return }; // single-image mode
        self.ensure_pool();
        if let Some(viewer) = self.viewers.get_mut(&id) {
            viewer.on_listed(tab, folder, &mut self.shared);
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
        // A background tab woken from the cache needs its folder listed too.
        self.request_listings();
        self.sync_watchers();
    }

    /// Watches exactly the folders some tab (in any window) pages through:
    /// one watcher per folder, however many tabs share it.
    fn sync_watchers(&mut self) {
        let dirs: HashSet<PathBuf> = self.viewers.values().flat_map(Viewer::folder_dirs).collect();
        self.watchers.retain(|dir, _| dirs.contains(dir)); // dropping one stops it
        for dir in dirs {
            if self.watchers.contains_key(&dir) {
                continue;
            }
            let (proxy, reported) = (Mutex::new(self.shared.proxy.clone()), dir.clone());
            let watcher = watch::Watcher::start(&dir, move |changes| {
                let _ = proxy.lock().unwrap_or_else(|e| e.into_inner()).send_event(Event::FolderChanged { dir: reported.clone(), changes });
            });
            if let Some(watcher) = watcher {
                self.watchers.insert(dir, watcher);
            }
        }
    }

    /// A watched folder's settled changes: rewritten or removed files are
    /// decoded again (or dropped), every tab paging through the folder
    /// updates its list in place, or lists it again if too much changed.
    fn apply_folder_changes(&mut self, dir: &Path, changes: &[Change]) {
        for change in changes {
            if let Change::Modified(path) | Change::Removed(path) | Change::Renamed(path, _) = change {
                self.shared.cache.remove(path);
                self.shared.failed.remove(path);
            }
        }
        // Stars follow renames; a favourites entry deleted unstars its file.
        let library_changes: Vec<Change> = changes.iter().filter(|c| matches!(c, Change::Renamed(..) | Change::Removed(_))).cloned().collect();
        if !library_changes.is_empty() {
            std::thread::spawn(move || {
                for change in library_changes {
                    match change {
                        Change::Renamed(old, new) => library::renamed(&old, &new),
                        Change::Removed(path) => library::removed(&path),
                        _ => {}
                    }
                }
            });
        }
        if changes.contains(&Change::Rescan) {
            for viewer in self.viewers.values() {
                for (tab, file) in viewer.files_shown_in(dir) {
                    let (id, sort, proxy) = (viewer.id, viewer.sort(), self.shared.proxy.clone());
                    std::thread::spawn(move || {
                        let _ = proxy.send_event(Event::Listed { viewer: id, tab, folder: folder::list(&file, sort) });
                    });
                }
            }
        } else {
            for viewer in self.viewers.values_mut() {
                viewer.on_folder_changed(dir, changes);
            }
        }
        self.plan();
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
                            viewer.tab.last_decode_ms = Some(decode_ms);
                            viewer.on_preloaded(&path, &screen, &self.shared);
                        }
                        // A background tab that just got its first picture: list its folder.
                        self.request_listings();
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
        // A quiet moment to bring the stars in line with the disk (daily).
        std::thread::spawn(library::sweep_if_due);
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
        let settled: Vec<PathBuf> = self.folder_changes.iter().filter(|(_, (_, due))| *due <= now).map(|(dir, _)| dir.clone()).collect();
        for dir in settled {
            if let Some((changes, _)) = self.folder_changes.remove(&dir) {
                self.apply_folder_changes(&dir, &changes);
            }
        }
        let wake = self
            .viewers
            .values()
            .filter_map(Viewer::deadline)
            .chain(self.idle_since.map(|t| t + TRIM_AFTER))
            .chain(self.folder_changes.values().map(|(_, due)| *due))
            .min();
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
            Event::Decoded { viewer, tab, picture, at } => {
                let viewer = self.window_of(tab).unwrap_or(viewer);
                let Some(v) = self.viewers.get_mut(&viewer) else { return };
                if let Some(t) = v.tab_mut(tab) {
                    t.timeline.push("decoded", at);
                }
                match picture {
                    Ok(picture) => self.on_decoded(viewer, tab, picture),
                    // A new tab in a shown window gets a notice, not a closed window.
                    Err(e) if v.is_shown() => v.on_decode_failed(tab, &e),
                    Err(e) => self.fail_viewer(viewer, e),
                }
            }
            Event::GpuReady(gpu, stages) => {
                // The device is started with the first window; its timeline gets the stages.
                if let Some(first) = self.viewers.values_mut().min_by_key(|v| v.id) {
                    for (stage, at) in stages {
                        first.tab.timeline.push(stage, at);
                    }
                }
                match gpu {
                    Ok(gpu) => {
                        self.watch_gpu(&gpu);
                        self.shared.gpu = Some(gpu);
                        let ids: Vec<ViewerId> = self.viewers.keys().copied().collect();
                        for id in ids {
                            self.try_show(id);
                        }
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            Event::GpuLost(generation) => {
                if generation == self.gpu_generation && self.shared.gpu.as_ref().is_some_and(Gpu::lost) {
                    self.recover_gpu();
                }
            }
            Event::OnScreen { viewer, tab, at } => self.on_screen(self.window_of(tab).unwrap_or(viewer), tab, at),
            Event::Listed { viewer, tab, folder } => self.on_listed(self.window_of(tab).unwrap_or(viewer), tab, folder),
            Event::Preloaded { path, picture, decode_ms } => self.on_preloaded(path, picture, decode_ms),
            Event::DetailDecoded { viewer, path, level, at } => {
                if let Some(v) = self.viewers.get_mut(&viewer) {
                    v.tab.timeline.push("detail_decoded", at);
                    v.on_detail_decoded(path, level, &self.shared);
                }
            }
            Event::Anim { viewer, serial, update } => {
                if let Some(v) = self.viewers.get_mut(&viewer) {
                    v.on_anim(serial, update, &self.shared);
                }
            }
            Event::Info { viewer, path, info } => {
                if let Some(v) = self.viewers.get_mut(&viewer) {
                    v.on_info(path, info);
                }
            }
            Event::Copy { viewer, image } => {
                if let Some(v) = self.viewers.get_mut(&viewer) {
                    v.on_copy(image);
                }
            }
            Event::Entered { viewer, tab, result, at } => {
                if let Some(v) = self.viewers.get_mut(&viewer) {
                    let action = v.on_entered(tab, result, at, &self.shared);
                    self.act(event_loop, viewer, Ok(action));
                }
            }
            Event::Starred { viewer, path, result } => {
                for v in self.viewers.values_mut() {
                    let here = v.id == viewer;
                    v.on_starred(&path, &result, here);
                }
                self.plan(); // sorted by stars, the ring may have moved
            }
            Event::FolderChanged { dir, changes } => {
                let (pending, due) = self.folder_changes.entry(dir).or_insert_with(|| (Vec::new(), Instant::now()));
                pending.extend(changes);
                *due = Instant::now() + FOLDER_SETTLE;
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
        let result = viewer.window_event(event, &self.shared);
        self.act(event_loop, id, result);
        self.exit_if_failed(event_loop);
    }
}

impl App {
    /// Carries out what a window's event handling asked for.
    fn act(&mut self, event_loop: &ActiveEventLoop, id: ViewerId, result: Result<Action, String>) {
        match result {
            Ok(Action::None) => {}
            Ok(Action::Navigated) => self.plan(),
            Ok(Action::PrefsChanged(prefs)) => {
                self.prefs = prefs;
                settings::save(prefs, self.global);
                self.plan(); // a new sort order moves the preload ring
            }
            Ok(Action::Menu) => {
                // Move to Window lists the other windows, oldest first.
                let mut others: Vec<_> = self.viewers.values().filter(|v| v.id != id && v.is_shown()).map(Viewer::menu_entry).collect();
                others.sort_by_key(|(other, _, _)| *other);
                let Some(viewer) = self.viewers.get_mut(&id) else { return };
                let chosen = viewer.show_menu(&others, self.global, &self.shared);
                self.act(event_loop, id, Ok(chosen));
            }
            Ok(Action::ReopenMenu) => {
                // The window the right click landed on, if it is one of ours.
                let under = unsafe {
                    use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GetCursorPos, GA_ROOT, WindowFromPoint};
                    let mut at = windows::Win32::Foundation::POINT::default();
                    let _ = GetCursorPos(&mut at);
                    GetAncestor(WindowFromPoint(at), GA_ROOT)
                };
                let Some(viewer) = self.viewers.values().find(|v| v.hwnd() == Some(under)) else { return };
                let target = viewer.id;
                if let Some(window) = &viewer.window {
                    window.focus_window();
                }
                self.act(event_loop, target, Ok(Action::Menu));
            }
            Ok(Action::GlobalChanged(global)) => {
                self.global = global;
                settings::save(self.prefs, global);
                for viewer in self.viewers.values_mut() {
                    viewer.set_global(global);
                }
            }
            Ok(Action::MoveTab(to)) => self.move_tab(event_loop, id, to),
            Ok(Action::Duplicate { file, new_window }) => {
                let mut timeline = Timeline::default();
                timeline.mark("request");
                let request = Request { file, ..Default::default() };
                let tab_id = self.new_id();
                if new_window {
                    // Decoded at once, beside this window (as Move to New Window).
                    let new_id = self.next_id; // the id new_window gives it
                    self.spawn_decode(new_id, tab_id, &request);
                    self.new_window(event_loop, Tab::new(tab_id, request, timeline), Some(id));
                } else if let Some(viewer) = self.viewers.get_mut(&id) {
                    viewer.add_duplicate(Tab::waiting(tab_id, request, timeline));
                }
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
    let (first, first_tab): (ViewerId, TabId) = (1, 2);
    let (path, decode_proxy) = (request.file.clone(), proxy.clone());
    std::thread::spawn(move || {
        let picture = decode::decode_screen(&path, target);
        let _ = decode_proxy.send_event(Event::Decoded { viewer: first, tab: first_tab, picture, at: ms_since_start() });
    });
    let gpu_proxy = proxy.clone();
    std::thread::spawn(move || {
        let mut stages = Vec::new();
        let gpu = Gpu::create(&mut |stage| stages.push((stage, ms_since_start())));
        let _ = gpu_proxy.send_event(Event::GpuReady(gpu, stages));
    });

    let placement = if request.size.is_none() { placement::load() } else { None };
    // `--size` benchmark runs keep the defaults, as they keep the default placement.
    let (prefs, global) = if request.size.is_none() { settings::load() } else { Default::default() };
    let last_file = Some(request.file.clone());
    let mut app = App {
        shared: Shared { proxy, gpu: None, pool: None, cache: HashMap::new(), failed: HashMap::new(), target },
        viewers: HashMap::from([(first, Viewer::new(first, Tab::new(first_tab, request, timeline), placement, prefs, global, 0))]),
        windows: HashMap::new(),
        next_id: first_tab + 1,
        focus_order: vec![first],
        placement,
        prefs,
        global,
        resident,
        serving: false,
        tray: None,
        last_file,
        // A cold start can be the first of a burst too: the other launches
        // wait for the pipe, then arrive together.
        last_open: Some(Instant::now()),
        burst: Some((first, 0)),
        idle_since: None,
        error: None,
        watchers: HashMap::new(),
        folder_changes: HashMap::new(),
        gpu_generation: 0,
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
