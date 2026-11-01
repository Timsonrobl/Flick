//! The app icon: resource 1 in flick.exe (build.rs, from
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

/// `icon` with a yellow dot cut into its lower right corner: the tray's
/// "update ready". The caller destroys it.
pub fn badged(icon: HICON) -> Option<HICON> {
    use windows::Win32::Graphics::Gdi::{BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC};
    use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, GetIconInfo, ICONINFO};
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(icon, &mut info).ok()?;
        let mut bitmap = BITMAP::default();
        let sized = GetObjectW(info.hbmColor.into(), size_of::<BITMAP>() as i32, Some((&raw mut bitmap).cast())) != 0;
        let (w, h) = (bitmap.bmWidth, bitmap.bmHeight);
        let mut header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER { biSize: size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -h, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..Default::default() },
            ..Default::default()
        };
        let mut pixels = vec![0u8; (w.max(0) * h.max(0) * 4) as usize];
        let dc = GetDC(None);
        let read = sized && GetDIBits(dc, info.hbmColor, 0, h as u32, Some(pixels.as_mut_ptr().cast()), &mut header, DIB_RGB_COLORS) != 0;
        ReleaseDC(None, dc);
        let _ = DeleteObject(info.hbmColor.into());
        let _ = DeleteObject(info.hbmMask.into());
        if !read {
            return None;
        }
        // The dot, and a transparent ring around it so it reads on any icon.
        let (r, gap) = (w as f32 * 0.24, (w as f32 / 16.0).max(1.0));
        let (cx, cy) = (w as f32 - r, h as f32 - r);
        let dot = [10.0, 204.0, 255.0]; // BGR: #FFCC0A
        for y in 0..h {
            for x in 0..w {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                let (cover, cut) = ((r - d + 0.5).clamp(0.0, 1.0), (r + gap - d + 0.5).clamp(0.0, 1.0));
                if cut == 0.0 {
                    continue;
                }
                let p = &mut pixels[((y * w + x) * 4) as usize..][..4];
                let under = f32::from(p[3]) / 255.0 * (1.0 - cut);
                let alpha = cover + under * (1.0 - cover);
                for c in 0..3 {
                    let mixed = dot[c] * cover + f32::from(p[c]) * under * (1.0 - cover);
                    p[c] = if alpha > 0.0 { (mixed / alpha).round() as u8 } else { 0 };
                }
                p[3] = (alpha * 255.0).round() as u8;
            }
        }
        let color = CreateBitmap(w, h, 1, 32, Some(pixels.as_ptr().cast()));
        let zeros = vec![0u8; (((w + 15) / 16 * 2) * h) as usize];
        let mask = CreateBitmap(w, h, 1, 1, Some(zeros.as_ptr().cast()));
        let badged = CreateIconIndirect(&ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color });
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        badged.ok()
    }
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
