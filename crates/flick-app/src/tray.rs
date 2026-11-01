//! Tray icon for the resident process (ARCHITECTURE.md §3): shows that Flick
//! keeps running with no windows open, and is how it is quit. Callbacks go to
//! a hidden window on the UI thread, whose messages winit's loop dispatches
//! like any other. When Explorer restarts, its new taskbar has no icons: it
//! broadcasts "TaskbarCreated" to top-level windows (not message-only
//! ones, hence a hidden top-level window), and the icon is added again.
//!
//! An update ready (update.rs) badges the icon, adds Relaunch to Update to
//! its menu, and says so once in a notification that relaunches when clicked.

use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_USER, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIN_BALLOONUSERCLICK, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, w};

#[derive(Clone, Copy, Debug)]
pub enum Command {
    /// Left click: bring Flick forward (or reopen the last image).
    Activate,
    Quit,
    /// Relaunch to Update (the menu item, or the notification clicked).
    Relaunch,
}

const CALLBACK: u32 = WM_APP + 1;
const QUIT: usize = 1;
const RELAUNCH: usize = 2;

type Handler = Box<dyn Fn(Command) + Send + Sync>;
static HANDLER: OnceLock<Handler> = OnceLock::new();
/// Explorer's "TaskbarCreated" message.
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
/// The version of an update ready, for the menu.
static UPDATE: Mutex<Option<String>> = Mutex::new(None);

pub struct Tray {
    hwnd: HWND,
    /// Flick's own icon, if it loaded (else the shared system one).
    icon: Option<HICON>,
    /// The icon with its "update ready" badge, once one is.
    badged: Option<HICON>,
}

impl Tray {
    pub fn create(on_command: impl Fn(Command) + Send + Sync + 'static) -> Result<Tray, String> {
        let _ = HANDLER.set(Box::new(on_command));
        unsafe {
            let _ = TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));
            let instance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
            let class = WNDCLASSW { lpfnWndProc: Some(window_proc), hInstance: instance.into(), lpszClassName: w!("FlickTray"), ..Default::default() };
            RegisterClassW(&class);
            // Never shown; a tool window, so it would stay off the taskbar anyway.
            let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW, w!("FlickTray"), w!("Flick"), WS_POPUP, 0, 0, 0, 0, None, None, Some(instance.into()), None)
                .map_err(|e| e.to_string())?;
            let icon = crate::icon::tray();
            let shown = match icon {
                Some(icon) => icon,
                None => LoadIconW(None, IDI_APPLICATION).map_err(|e| e.to_string())?,
            };
            // Kept with the window, to add it again.
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, shown.0 as isize);
            if !add(hwnd, shown) {
                let _ = DestroyWindow(hwnd);
                if let Some(icon) = icon {
                    let _ = DestroyIcon(icon);
                }
                return Err("could not add the tray icon".into());
            }
            Ok(Tray { hwnd, icon, badged: None })
        }
    }

    /// Update `version` is ready: the badge, the menu item, and (`notify`:
    /// the first time) a notification.
    pub fn update_ready(&mut self, version: &str, notify: bool) {
        *UPDATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(version.to_string());
        if self.badged.is_none() {
            self.badged = self.icon.and_then(crate::icon::badged);
        }
        let mut data = icon_data(self.hwnd);
        data.uFlags = NIF_TIP;
        fill(&mut data.szTip, &format!("Flick: update {version} ready"));
        if let Some(badged) = self.badged {
            data.uFlags |= NIF_ICON;
            data.hIcon = badged;
            // Added again with the badge after an Explorer restart.
            unsafe { SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, badged.0 as isize) };
        }
        if notify {
            data.uFlags |= NIF_INFO;
            data.dwInfoFlags = NIIF_USER;
            fill(&mut data.szInfoTitle, &format!("Flick {version} is ready"));
            fill(&mut data.szInfo, "Click to relaunch Flick and update now; otherwise it updates when Flick quits.");
        }
        let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &icon_data(self.hwnd));
            let _ = DestroyWindow(self.hwnd);
            for icon in [self.icon, self.badged].into_iter().flatten() {
                let _ = DestroyIcon(icon);
            }
        }
    }
}

/// `text` into a fixed, zero-terminated UTF-16 field (cut to fit).
fn fill(field: &mut [u16], text: &str) {
    field.fill(0);
    let room = field.len() - 1;
    for (slot, c) in field[..room].iter_mut().zip(text.encode_utf16()) {
        *slot = c;
    }
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: 1, ..Default::default() }
}

/// Puts the icon in the taskbar's notification area.
fn add(hwnd: HWND, icon: HICON) -> bool {
    let mut data = icon_data(hwnd);
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.uCallbackMessage = CALLBACK;
    data.hIcon = icon;
    fill(&mut data.szTip, "Flick");
    unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool()
}

fn send(command: Command) {
    if let Some(handler) = HANDLER.get() {
        handler(command);
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == CALLBACK {
        match (lparam.0 & 0xFFFF) as u32 {
            WM_LBUTTONUP => send(Command::Activate),
            WM_RBUTTONUP | WM_CONTEXTMENU => unsafe { show_menu(hwnd) },
            NIN_BALLOONUSERCLICK => send(Command::Relaunch),
            _ => {}
        }
        return LRESULT(0);
    }
    if TASKBAR_CREATED.get() == Some(&msg) {
        // Explorer restarted: the new taskbar starts without our icon.
        let icon = HICON(unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut _);
        add(hwnd, icon);
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

unsafe fn show_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        if let Some(version) = UPDATE.lock().unwrap_or_else(|e| e.into_inner()).as_deref() {
            let _ = AppendMenuW(menu, MF_STRING, RELAUNCH, &HSTRING::from(format!("Relaunch to Update Flick ({version})")));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        }
        let _ = AppendMenuW(menu, MF_STRING, QUIT, w!("Quit Flick"));
        let mut at = POINT::default();
        let _ = GetCursorPos(&mut at);
        // Required so the menu closes when clicking elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, at.x, at.y, None, hwnd, None);
        let _ = DestroyMenu(menu);
        match chosen.0 as usize {
            QUIT => send(Command::Quit),
            RELAUNCH => send(Command::Relaunch),
            _ => {}
        }
    }
}
