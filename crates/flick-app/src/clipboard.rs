//! Ctrl+C: the picture at full resolution onto the clipboard, as a 32-bit
//! DIBv5 with straight alpha (Windows makes CF_DIB and CF_BITMAP from it for
//! programs that want those).

use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows::Win32::Graphics::Gdi::{BI_BITFIELDS, BITMAPV5HEADER};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_DIBV5;

use crate::decode::Image;

/// LCS_sRGB: the pixels are sRGB.
const LCS_SRGB: u32 = 0x7352_4742;

pub fn put(hwnd: HWND, image: &Image) -> Result<(), String> {
    let (w, h) = (image.width as usize, image.height as usize);
    let header = BITMAPV5HEADER {
        bV5Size: size_of::<BITMAPV5HEADER>() as u32,
        bV5Width: w as i32,
        bV5Height: h as i32, // bottom-up: what most programs expect
        bV5Planes: 1,
        bV5BitCount: 32,
        bV5Compression: BI_BITFIELDS,
        bV5SizeImage: (w * h * 4) as u32,
        bV5RedMask: 0x00FF_0000,
        bV5GreenMask: 0x0000_FF00,
        bV5BlueMask: 0x0000_00FF,
        bV5AlphaMask: 0xFF00_0000,
        bV5CSType: LCS_SRGB,
        ..Default::default()
    };
    let header_bytes = size_of::<BITMAPV5HEADER>();
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, header_bytes + w * h * 4).map_err(|e| e.to_string())?;
        let base = GlobalLock(memory).cast::<u8>();
        if base.is_null() {
            let _ = GlobalFree(Some(memory));
            return Err("GlobalLock failed".into());
        }
        std::ptr::copy_nonoverlapping((&raw const header).cast::<u8>(), base, header_bytes);
        let pixels = std::slice::from_raw_parts_mut(base.add(header_bytes), w * h * 4);
        // Rows bottom-up, premultiplied alpha undone.
        for (y, row) in image.pixels.chunks_exact(w * 4).enumerate() {
            let out = &mut pixels[(h - 1 - y) * w * 4..][..w * 4];
            for (o, p) in out.chunks_exact_mut(4).zip(row.chunks_exact(4)) {
                let a = p[3];
                let straight = |c: u8| if a == 0 { 0 } else { ((u16::from(c) * 255 + u16::from(a) / 2) / u16::from(a)).min(255) as u8 };
                o.copy_from_slice(&[straight(p[0]), straight(p[1]), straight(p[2]), a]);
            }
        }
        let _ = GlobalUnlock(memory);

        if OpenClipboard(Some(hwnd)).is_err() {
            let _ = GlobalFree(Some(memory));
            return Err("the clipboard is busy".into());
        }
        let _ = EmptyClipboard();
        // On success the clipboard owns the memory.
        let result = SetClipboardData(u32::from(CF_DIBV5.0), Some(HANDLE(memory.0)));
        let _ = CloseClipboard();
        if let Err(e) = result {
            let _ = GlobalFree(Some(memory));
            return Err(e.to_string());
        }
    }
    Ok(())
}
