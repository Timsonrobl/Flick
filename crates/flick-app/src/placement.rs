//! Remembers the window's placement between runs: the normal (restored) rect
//! and whether it was maximized, in `%LOCALAPPDATA%\Flick\window-placement`.
//! Windows' own `WINDOWPLACEMENT` is used for saving and restoring, so a
//! maximized window also keeps the rect it restores to.

use std::path::PathBuf;

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL, MONITORINFO, MonitorFromRect, MonitorFromWindow};
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Copy, Debug)]
pub struct Placement {
    normal: RECT,
    pub maximized: bool,
}

fn path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("Flick").join("window-placement"))
}

/// The saved placement, if any and if it is still on a connected monitor.
pub fn load() -> Option<Placement> {
    let text = std::fs::read_to_string(path()?).ok()?;
    let mut normal = None;
    let mut maximized = false;
    for line in text.lines() {
        match line.split_once('=') {
            Some(("maximized", v)) => maximized = v.trim() == "1",
            Some(("normal", v)) => {
                let n: Vec<i32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                if let [left, top, right, bottom] = n[..] {
                    normal = Some(RECT { left, top, right, bottom });
                }
            }
            _ => {}
        }
    }
    let normal = normal?;
    Placement::from_parts(((normal.left, normal.top, normal.right, normal.bottom), maximized))
}

impl Placement {
    /// The normal rect (left, top, right, bottom) and whether maximized:
    /// for the close history (`session.rs`).
    pub fn parts(&self) -> ((i32, i32, i32, i32), bool) {
        let r = self.normal;
        ((r.left, r.top, r.right, r.bottom), self.maximized)
    }

    /// `parts` back, if still usable and on a connected monitor.
    pub fn from_parts(((left, top, right, bottom), maximized): ((i32, i32, i32, i32), bool)) -> Option<Placement> {
        let normal = RECT { left, top, right, bottom };
        let usable = right - left >= 200 && bottom - top >= 150;
        let on_screen = !unsafe { MonitorFromRect(&normal, MONITOR_DEFAULTTONULL) }.is_invalid();
        (usable && on_screen).then_some(Placement { normal, maximized })
    }

    /// The same placement moved by (dx, dy), for cascading new windows.
    pub fn offset(&self, dx: i32, dy: i32) -> Placement {
        let r = self.normal;
        Placement { normal: RECT { left: r.left + dx, top: r.top + dy, right: r.right + dx, bottom: r.bottom + dy }, maximized: self.maximized }
    }
}

/// The window's placement now.
pub fn current(hwnd: HWND) -> Option<Placement> {
    let mut wp = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
    unsafe { GetWindowPlacement(hwnd, &mut wp) }.ok()?;
    let maximized = wp.showCmd == SW_SHOWMAXIMIZED.0 as u32 || (wp.showCmd == SW_SHOWMINIMIZED.0 as u32 && wp.flags.contains(WPF_RESTORETOMAXIMIZED));
    // A snapped window's normal rect is where it was before snapping: keep
    // where it is instead, so it reopens where it was left.
    let snapped = !maximized && wp.showCmd == SW_SHOWNORMAL.0 as u32 && unsafe { IsWindowArranged(hwnd) }.as_bool();
    let r = if snapped { arranged_rect(hwnd).unwrap_or(wp.rcNormalPosition) } else { wp.rcNormalPosition };
    Some(Placement { normal: r, maximized })
}

/// Saves the window's placement and returns it.
pub fn save(hwnd: HWND) -> Option<Placement> {
    let placement = current(hwnd)?;
    let (r, path) = (placement.normal, path()?);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("maximized={}\nnormal={},{},{},{}\n", u8::from(placement.maximized), r.left, r.top, r.right, r.bottom));
    Some(placement)
}

/// The window's rect on screen, in the workspace coordinates
/// WINDOWPLACEMENT uses (screen coordinates less a taskbar on the left or top).
fn arranged_rect(hwnd: HWND) -> Option<RECT> {
    unsafe {
        let mut rect = RECT::default();
        GetWindowRect(hwnd, &mut rect).ok()?;
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut info).as_bool() {
            return None;
        }
        let (dx, dy) = (info.rcWork.left - info.rcMonitor.left, info.rcWork.top - info.rcMonitor.top);
        Some(RECT { left: rect.left - dx, top: rect.top - dy, right: rect.right - dx, bottom: rect.bottom - dy })
    }
}

/// Puts the still-hidden window where it will be shown and returns its
/// client size, so the first frame is drawn at its final size.
pub fn prepare(hwnd: HWND, placement: &Placement) -> (u32, u32) {
    unsafe {
        if placement.maximized {
            // Where maximizing will put it: the monitor's work area. The
            // normal rect is restored at show time, together with maximizing.
            let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            let monitor = MonitorFromRect(&placement.normal, MONITOR_DEFAULTTONEAREST);
            if GetMonitorInfoW(monitor, &mut info).as_bool() {
                let work = info.rcWork;
                let (w, h) = (work.right - work.left, work.bottom - work.top);
                let _ = SetWindowPos(hwnd, None, work.left, work.top, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
                return (w as u32, h as u32);
            }
        }
        let r = at_least_min(placement.normal);
        let wp = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, showCmd: SW_HIDE.0 as u32, rcNormalPosition: r, ..Default::default() };
        let _ = SetWindowPlacement(hwnd, &wp);
        ((r.right - r.left) as u32, (r.bottom - r.top) as u32)
    }
}

/// `r` grown to the least window size (chrome::MIN_WIDTH × MIN_HEIGHT, at
/// the DPI of the monitor it is on), keeping its top-left corner: a
/// placement saved smaller (before there was a least size) opens usable.
fn at_least_min(r: RECT) -> RECT {
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    let (mut dpi, mut unused) = (96, 96);
    let _ = unsafe { GetDpiForMonitor(MonitorFromRect(&r, MONITOR_DEFAULTTONEAREST), MDT_EFFECTIVE_DPI, &mut dpi, &mut unused) };
    let scale = dpi as f32 / 96.0;
    let (w, h) = ((crate::chrome::MIN_WIDTH * scale).ceil() as i32, (crate::chrome::MIN_HEIGHT * scale).ceil() as i32);
    RECT { right: r.right.max(r.left + w), bottom: r.bottom.max(r.top + h), ..r }
}

/// Where a new window opens next to the window `hwnd` it came from (Move to
/// New Window, or a file opened with tabs off): maximized if that one is;
/// the other side of the screen if that one is snapped to a side; else the
/// same size, beside it without overlapping (or as little as fits).
pub fn beside(hwnd: HWND) -> Option<Placement> {
    unsafe {
        let mut wp = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        GetWindowPlacement(hwnd, &mut wp).ok()?;
        if IsZoomed(hwnd).as_bool() {
            return Some(Placement { normal: wp.rcNormalPosition, maximized: true });
        }
        let mut rect = RECT::default();
        GetWindowRect(hwnd, &mut rect).ok()?;
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut info).as_bool() {
            return None;
        }
        let (work, screen) = (info.rcWork, info.rcMonitor);
        // SetWindowPlacement takes workspace coordinates: screen coordinates
        // less a taskbar on the left or top.
        let (dx, dy) = (work.left - screen.left, work.top - screen.top);
        let placed = |left: i32, top: i32, right: i32, bottom: i32| {
            Some(Placement { normal: RECT { left: left - dx, top: top - dy, right: right - dx, bottom: bottom - dy }, maximized: false })
        };
        const NEAR: i32 = 8;
        if IsWindowArranged(hwnd).as_bool() {
            if rect.left <= work.left + NEAR && rect.right < work.right - NEAR {
                return placed(rect.right, rect.top, work.right, rect.bottom);
            }
            if rect.right >= work.right - NEAR && rect.left > work.left + NEAR {
                return placed(work.left, rect.top, rect.left, rect.bottom);
            }
        }
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        let fits = |x: i32, y: i32| x >= work.left && y >= work.top && x + w <= work.right && y + h <= work.bottom;
        let (x, y) = [(rect.right, rect.top), (rect.left - w, rect.top), (rect.left, rect.bottom), (rect.left, rect.top - h)]
            .into_iter()
            .find(|&(x, y)| fits(x, y))
            // No room anywhere: against the edge with more space, overlapping least.
            .unwrap_or_else(|| {
                let x = if rect.left - work.left > work.right - rect.right { work.left } else { (work.right - w).max(work.left) };
                (x, rect.top.clamp(work.top, (work.bottom - h).max(work.top)))
            });
        placed(x, y, x + w, y + h)
    }
}

/// Shows the window maximized, keeping the saved rect it restores to.
pub fn show_maximized(hwnd: HWND, placement: &Placement) {
    let wp = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        showCmd: SW_SHOWMAXIMIZED.0 as u32,
        rcNormalPosition: placement.normal,
        ..Default::default()
    };
    let _ = unsafe { SetWindowPlacement(hwnd, &wp) };
}
