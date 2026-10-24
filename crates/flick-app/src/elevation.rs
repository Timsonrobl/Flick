//! Flick never becomes the resident instance while elevated (ARCHITECTURE.md
//! §3). A program running as administrator, such as Everything, launches
//! Flick elevated too. An elevated resident can't be reached by normal
//! launches (Windows blocks writes from a lower integrity level, so its pipe
//! answers "access denied"), and decoding untrusted images as administrator
//! is a needless risk. So an elevated launch that would become resident starts
//! Flick again at the desktop's level through Explorer and exits. An elevated
//! launch that finds a resident running just hands its file over: writing
//! down to a normal process is allowed, so warm opens cost nothing extra.

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Com::{CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IDispatch, IServiceProvider};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    CSIDL_DESKTOP, IShellBrowser, IShellDispatch2, IShellFolderViewDual, IShellWindows, SID_STopLevelBrowser, SVGIO_BACKGROUND, SWC_DESKTOP, SWFO_NEEDDISPATCH,
    ShellWindows,
};
use windows::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow, SW_SHOWNORMAL};
use windows::core::{BSTR, Interface};

use crate::instance::Request;

/// Marks a relaunch, so a still-elevated one (UAC off: Explorer itself runs
/// elevated) doesn't relaunch again.
pub const RELAUNCHED: &str = "--unelevated";

pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0;
        let ok = GetTokenInformation(token, TokenElevation, Some((&raw mut elevation).cast()), size_of::<TOKEN_ELEVATION>() as u32, &mut size).is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// Starts this executable for `request` as a child of Explorer (the shell's
/// own ShellExecute, reached through the desktop window), so at its level.
pub fn relaunch_unelevated(request: &Request) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut args = format!("\"{}\"", request.file.display());
    if let Some((w, h)) = request.size {
        args += &format!(" --size {w}x{h}");
    }
    if let Some(path) = &request.timeline_out {
        args += &format!(" --timeline-out \"{}\"", path.display());
    }
    args += &format!(" {RELAUNCHED}");
    let err = |e: windows::core::Error| format!("could not start Flick unelevated: {e}");
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let shell_windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER).map_err(err)?;
        let mut hwnd = 0;
        let desktop = shell_windows
            .FindWindowSW(&VARIANT::from(CSIDL_DESKTOP as i32), &VARIANT::default(), SWC_DESKTOP, &mut hwnd, SWFO_NEEDDISPATCH)
            .map_err(err)?;
        let browser: IShellBrowser = desktop.cast::<IServiceProvider>().map_err(err)?.QueryService(&SID_STopLevelBrowser).map_err(err)?;
        let view: IDispatch = browser.QueryActiveShellView().map_err(err)?.GetItemObject(SVGIO_BACKGROUND).map_err(err)?;
        let shell: IShellDispatch2 = view.cast::<IShellFolderViewDual>().map_err(err)?.Application().map_err(err)?.cast().map_err(err)?;
        // The new process may take the foreground, as this one may.
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        shell
            .ShellExecute(&BSTR::from(exe.to_string_lossy().as_ref()), &VARIANT::from(args.as_str()), &VARIANT::from(""), &VARIANT::from("open"), &VARIANT::from(SW_SHOWNORMAL.0))
            .map_err(err)
    }
}
