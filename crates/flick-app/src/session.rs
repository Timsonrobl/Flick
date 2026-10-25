//! The close history (Ctrl+Shift+T, or the tray icon with no window open):
//! the last HISTORY closes, each a tab or whole windows, undone newest
//! first, in `%LOCALAPPDATA%\Flick\closed.json`. Quitting closes all the
//! windows at once, as one close; so does a crash or a shutdown, found at
//! the next start from the open windows each Flick process keeps in its own
//! `open-<process id>.json` (one file per process: a newer build taking
//! over from a running one isn't a crash).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// Closes kept to undo.
const HISTORY: usize = 50;

/// A tab as it was: enough to show it again as it looked.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedTab {
    pub file: PathBuf,
    /// Rotate ▸ chosen in the tab: quarter turns, flipped across, flipped down.
    pub turn: Option<(u8, bool, bool)>,
    /// Zoom and pan: scale, and the centre as a fraction of the picture;
    /// None while it followed the window's view mode.
    pub zoom: Option<(f32, (f32, f32))>,
}

/// A window as it was.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedWindow {
    /// Its normal rect (left, top, right, bottom) and whether it was maximized.
    pub placement: Option<((i32, i32, i32, i32), bool)>,
    pub color: usize,
    /// Its menu settings, in the settings file's form (`settings::parse`).
    pub prefs: String,
    pub locked: bool,
    /// The active tab's place in `tabs`.
    pub active: usize,
    pub tabs: Vec<SavedTab>,
}

/// One close.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Closed {
    /// A tab of a window that stayed open: back there (`window`, in the run
    /// `run`), at `at`, if that window still is.
    Tab { run: u64, window: u64, at: usize, tab: SavedTab },
    /// Windows closed together: one, or all at a quit or a crash.
    Windows(Vec<SavedWindow>),
}

fn dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Flick"))
}

/// This process's run, telling its windows from same-numbered ones of others.
pub fn run() -> u64 {
    static RUN: OnceLock<u64> = OnceLock::new();
    *RUN.get_or_init(|| {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
        nanos ^ u64::from(std::process::id()).rotate_left(32)
    })
}

/// Writes `value` as JSON to `path` through a temporary file, so a crash
/// never leaves half a file.
fn write(path: &Path, value: &impl Serialize) {
    let Ok(text) = serde_json::to_string(value) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let temp = path.with_extension("tmp");
    if std::fs::write(&temp, text).is_ok() {
        let _ = std::fs::rename(&temp, path);
    }
}

fn read<T: for<'a> Deserialize<'a>>(path: &Path) -> Option<T> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The history in `dir` (read afresh each time: another Flick process may
/// have added to it).
fn history(dir: &Path) -> Vec<Closed> {
    read(&dir.join("closed.json")).unwrap_or_default()
}

fn push_in(dir: &Path, closed: Closed) {
    let mut all = history(dir);
    all.push(closed);
    let over = all.len().saturating_sub(HISTORY);
    all.drain(..over);
    write(&dir.join("closed.json"), &all);
}

fn pop_in(dir: &Path) -> Option<Closed> {
    let mut all = history(dir);
    let last = all.pop()?;
    write(&dir.join("closed.json"), &all);
    Some(last)
}

/// One change to the history at a time (`recover` runs on its own thread).
static LOCK: Mutex<()> = Mutex::new(());

/// A close, the newest; past HISTORY the oldest goes.
pub fn push(closed: Closed) {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(dir) = dir() {
        push_in(&dir, closed);
    }
}

/// The newest close, taken out of the history to undo it.
pub fn pop() -> Option<Closed> {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    pop_in(&dir()?)
}

/// This process's open windows, for a crash or a shutdown to leave behind
/// (none: the file goes).
pub fn save_open(windows: &[SavedWindow]) {
    let Some(dir) = dir() else { return };
    let path = dir.join(format!("open-{}.json", std::process::id()));
    if windows.is_empty() {
        let _ = std::fs::remove_file(path);
    } else {
        write(&path, &windows);
    }
}

/// At start, before this process saves its own: the windows Flick
/// processes left open when they ended without closing them (a crash, a
/// kill, a shutdown), each process's as one close. Files of processes still
/// running stay. (One with this process's id is an old one's.)
pub fn recover() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(dir) = dir() else { return };
    let own = std::process::id();
    recover_in(&dir, &|pid| pid != own && running(pid));
}

fn recover_in(dir: &Path, alive: &dyn Fn(u32) -> bool) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for path in entries.flatten().map(|e| e.path()) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let Some(pid) = name.strip_prefix("open-").and_then(|n| n.strip_suffix(".json")).and_then(|n| n.parse().ok()) else { continue };
        if alive(pid) {
            continue;
        }
        if let Some(windows) = read::<Vec<SavedWindow>>(&path).filter(|w| !w.is_empty()) {
            push_in(dir, Closed::Windows(windows));
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// Whether process `pid` still runs.
fn running(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
        let mut code = 0;
        let alive = GetExitCodeProcess(process, &mut code).is_ok() && code == STILL_ACTIVE.0 as u32;
        let _ = CloseHandle(process);
        alive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(name: &str) -> SavedTab {
        SavedTab { file: PathBuf::from(name), turn: Some((1, false, true)), zoom: Some((2.0, (0.25, 0.75))) }
    }

    fn window(names: &[&str]) -> SavedWindow {
        SavedWindow { placement: Some(((10, 20, 810, 620), false)), color: 3, prefs: "fit=width\n".into(), locked: true, active: 1, tabs: names.iter().map(|n| tab(n)).collect() }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("flick-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn undoes_newest_first_and_keeps_fifty() {
        let dir = temp("history");
        for i in 0..60 {
            push_in(&dir, Closed::Tab { run: 7, window: 1, at: i, tab: tab(&format!("{i}.jpg")) });
        }
        push_in(&dir, Closed::Windows(vec![window(&["a.jpg", "b.jpg"])]));
        assert_eq!(history(&dir).len(), HISTORY);
        assert_eq!(pop_in(&dir), Some(Closed::Windows(vec![window(&["a.jpg", "b.jpg"])])), "the last close first, intact");
        assert!(matches!(pop_in(&dir), Some(Closed::Tab { at: 59, .. })));
        // The 11 oldest went (at 0 to 10).
        let left = history(&dir);
        assert_eq!(left.len(), 48);
        assert!(matches!(left.first(), Some(Closed::Tab { at: 11, .. })));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dead_processs_open_windows_become_one_close() {
        let dir = temp("recover");
        write(&dir.join("open-111.json"), &vec![window(&["a.jpg"]), window(&["b.jpg", "c.jpg"])]);
        write(&dir.join("open-222.json"), &vec![window(&["d.jpg"])]);
        recover_in(&dir, &|pid| pid == 222);
        assert_eq!(history(&dir), vec![Closed::Windows(vec![window(&["a.jpg"]), window(&["b.jpg", "c.jpg"])])]);
        assert!(!dir.join("open-111.json").exists(), "recovered once");
        assert!(dir.join("open-222.json").exists(), "a running process's windows aren't a crash");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
