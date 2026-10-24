//! WIC decode at the smallest DCT scale that still covers the window: the
//! "screen" level the bench found fastest for a final-quality first frame.

use std::path::Path;

use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::core::{GUID, HSTRING, Interface};

/// Opaque BGRA pixels, rows tightly packed.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

pub fn decode_to_fit(path: &Path, target: (u32, u32)) -> Result<Image, String> {
    decode(path, target).map_err(|e| format!("decode {}: {e}", path.display()))
}

fn decode(path: &Path, target: (u32, u32)) -> windows::core::Result<Image> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let decoder = factory.CreateDecoderFromFilename(&HSTRING::from(path.as_os_str()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)?;
        let frame = decoder.GetFrame(0)?;
        let (mut width, mut height) = (0, 0);
        frame.GetSize(&mut width, &mut height)?;

        let fit = f64::min(f64::from(target.0) / f64::from(width), f64::from(target.1) / f64::from(height));
        let denom = [8, 4, 2].into_iter().find(|&d| 1.0 / f64::from(d) >= fit).unwrap_or(1);

        if let Ok(transform) = frame.cast::<IWICBitmapSourceTransform>() {
            let (mut w, mut h) = (width.div_ceil(denom), height.div_ceil(denom));
            transform.GetClosestSize(&mut w, &mut h)?;
            let mut format: GUID = GUID_WICPixelFormat32bppBGRA;
            transform.GetClosestPixelFormat(&mut format)?;
            if format == GUID_WICPixelFormat32bppBGRA || format == GUID_WICPixelFormat24bppBGR {
                let bpp = if format == GUID_WICPixelFormat24bppBGR { 3 } else { 4 };
                let mut raw = vec![0u8; w as usize * h as usize * bpp];
                transform.CopyPixels(std::ptr::null(), w, h, &format, WICBitmapTransformRotate0, w * bpp as u32, &mut raw)?;
                let pixels = if bpp == 3 { raw.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect() } else { raw };
                return Ok(Image { width: w, height: h, pixels });
            }
        }

        // No scaled path for this codec: full decode, converted to BGRA.
        let converter = factory.CreateFormatConverter()?;
        converter.Initialize(&frame, &GUID_WICPixelFormat32bppBGRA, WICBitmapDitherTypeNone, None::<&IWICPalette>, 0.0, WICBitmapPaletteTypeCustom)?;
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        converter.CopyPixels(std::ptr::null(), width * 4, &mut pixels)?;
        Ok(Image { width, height, pixels })
    }
}
