//! Tray icon for the resident process (ARCHITECTURE.md §3): shows that Flick
//! keeps running with no windows open, and is how it is quit. Callbacks go to
//! a message-only window on the UI thread, whose messages winit's loop
//! dispatches like any other.

use std::sync::OnceLock;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

#[derive(Clone, Copy, Debug)]
pub enum Command {
    /// Left click: bring Flick forward (or reopen the last image).
    Activate,
    Quit,
}

const CALLBACK: u32 = WM_APP + 1;
const QUIT: usize = 1;

type Handler = Box<dyn Fn(Command) + Send + Sync>;
static HANDLER: OnceLock<Handler> = OnceLock::new();

pub struct Tray {
    hwnd: HWND,
    /// Flick's own icon, if it loaded (else the shared system one).
    icon: Option<HICON>,
}

impl Tray {
    pub fn create(on_command: impl Fn(Command) + Send + Sync + 'static) -> Result<Tray, String> {
        let _ = HANDLER.set(Box::new(on_command));
        unsafe {
            let instance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
            let class = WNDCLASSW { lpfnWndProc: Some(window_proc), hInstance: instance.into(), lpszClassName: w!("FlickTray"), ..Default::default() };
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(Default::default(), w!("FlickTray"), w!("Flick"), WINDOW_STYLE(0), 0, 0, 0, 0, Some(HWND_MESSAGE), None, Some(instance.into()), None)
                .map_err(|e| e.to_string())?;
            let mut data = icon_data(hwnd);
            data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
            data.uCallbackMessage = CALLBACK;
            let icon = crate::icon::tray();
            data.hIcon = match icon {
                Some(icon) => icon,
                None => LoadIconW(None, IDI_APPLICATION).map_err(|e| e.to_string())?,
            };
            for (slot, c) in data.szTip.iter_mut().zip("Flick".encode_utf16()) {
                *slot = c;
            }
            if !Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
                let _ = DestroyWindow(hwnd);
                if let Some(icon) = icon {
                    let _ = DestroyIcon(icon);
                }
                return Err("could not add the tray icon".into());
            }
            Ok(Tray { hwnd, icon })
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &icon_data(self.hwnd));
            let _ = DestroyWindow(self.hwnd);
            if let Some(icon) = self.icon {
                let _ = DestroyIcon(icon);
            }
        }
    }
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: 1, ..Default::default() }
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
            _ => {}
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

unsafe fn show_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING, QUIT, w!("Quit Flick"));
        let mut at = POINT::default();
        let _ = GetCursorPos(&mut at);
        // Required so the menu closes when clicking elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, at.x, at.y, None, hwnd, None);
        let _ = DestroyMenu(menu);
        if chosen.0 as usize == QUIT {
            send(Command::Quit);
        }
    }
}
