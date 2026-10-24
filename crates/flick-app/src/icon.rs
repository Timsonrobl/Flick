//! The app icon: resource 1 in flick.exe (flick.rc, built from
//! assets/icon by `flick-icon`). Windows picks the frame closest to each
//! size asked for: 16-24 px are the tray-tuned artwork.

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, HICON, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW, SM_CXSMICON, SM_CYSMICON};
use windows::core::PCWSTR;
use winit::dpi::PhysicalSize;
use winit::platform::windows::IconExtWindows;
use winit::window::Icon;

const RESOURCE: u16 = 1;

fn small_size() -> (i32, i32) {
    unsafe { (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) }
}

/// (window icon, taskbar icon): small (Alt+Tab thumbnails, title) and large
/// (taskbar button), at the system's icon sizes.
pub fn window_icons() -> (Option<Icon>, Option<Icon>) {
    let (w, h) = small_size();
    let small = Icon::from_resource(RESOURCE, Some(PhysicalSize::new(w as u32, h as u32))).ok();
    let large = Icon::from_resource(RESOURCE, None).ok();
    (small, large)
}

/// The tray's small icon; the caller destroys it.
pub fn tray() -> Option<HICON> {
    let (w, h) = small_size();
    unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        let handle: HANDLE = LoadImageW(Some(instance.into()), PCWSTR(RESOURCE as usize as *const u16), IMAGE_ICON, w, h, LR_DEFAULTCOLOR).ok()?;
        Some(HICON(handle.0))
    }
}
