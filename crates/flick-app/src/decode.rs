//! WIC decoding. The `Screen` level is decoded at the smallest DCT scale that
//! still covers the target (the fastest final-quality picture, measured in
//! milestone 0); `Full` is the native resolution, for zooming in.

use std::path::Path;

use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::System::Variant::VT_UI2;
use windows::core::{GUID, HSTRING, Interface, w};

/// Opaque BGRA pixels, rows tightly packed, in the file's stored orientation.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// The first picture of a file.
pub struct Picture {
    pub screen: Image,
    /// Native size, as stored (before orientation).
    pub full_size: (u32, u32),
    /// EXIF orientation, 1–8 (1 = as stored).
    pub orientation: u16,
}

struct Source {
    factory: IWICImagingFactory,
    frame: IWICBitmapFrameDecode,
    size: (u32, u32),
}

fn open(path: &Path) -> windows::core::Result<Source> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let decoder = factory.CreateDecoderFromFilename(&HSTRING::from(path.as_os_str()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)?;
        let frame = decoder.GetFrame(0)?;
        let (mut w, mut h) = (0, 0);
        frame.GetSize(&mut w, &mut h)?;
        Ok(Source { factory, frame, size: (w, h) })
    }
}

fn orientation(frame: &IWICBitmapFrameDecode) -> u16 {
    unsafe {
        let Ok(reader) = frame.GetMetadataQueryReader() else { return 1 };
        let mut value = PROPVARIANT::default();
        // JPEG/EXIF first, then the TIFF-style path used by other containers.
        for query in [w!("/app1/ifd/{ushort=274}"), w!("/ifd/{ushort=274}")] {
            if reader.GetMetadataByName(query, &mut value).is_ok() && value.Anonymous.Anonymous.vt == VT_UI2 {
                let o = value.Anonymous.Anonymous.Anonymous.uiVal;
                return if (1..=8).contains(&o) { o } else { 1 };
            }
        }
        1
    }
}

/// Decodes at the smallest DCT scale (1/2, 1/4, 1/8) whose output still
/// covers `target` (in displayed orientation).
pub fn decode_screen(path: &Path, target: (u32, u32)) -> Result<Picture, String> {
    let inner = || -> windows::core::Result<Picture> {
        let source = open(path)?;
        let orientation = orientation(&source.frame);
        let (w, h) = source.size;
        let (dw, dh) = if orientation >= 5 { (h, w) } else { (w, h) };
        let fit = f64::min(f64::from(target.0) / f64::from(dw), f64::from(target.1) / f64::from(dh));
        let denom = [8, 4, 2].into_iter().find(|&d| 1.0 / f64::from(d) >= fit).unwrap_or(1);
        let screen = if denom > 1 { scaled(&source, denom)? } else { None };
        let screen = match screen {
            Some(image) => image,
            None => full(&source)?,
        };
        Ok(Picture { screen, full_size: source.size, orientation })
    };
    inner().map_err(|e| format!("{}: {e}", path.display()))
}

pub fn decode_full(path: &Path) -> Result<Image, String> {
    open(path).and_then(|source| full(&source)).map_err(|e| format!("{}: {e}", path.display()))
}

/// DCT-scaled decode through the codec's own transform, if it has one.
fn scaled(source: &Source, denom: u32) -> windows::core::Result<Option<Image>> {
    unsafe {
        let Ok(transform) = source.frame.cast::<IWICBitmapSourceTransform>() else { return Ok(None) };
        let (mut w, mut h) = (source.size.0.div_ceil(denom), source.size.1.div_ceil(denom));
        transform.GetClosestSize(&mut w, &mut h)?;
        let mut format: GUID = GUID_WICPixelFormat32bppBGRA;
        transform.GetClosestPixelFormat(&mut format)?;
        let bpp = match format {
            f if f == GUID_WICPixelFormat32bppBGRA => 4,
            f if f == GUID_WICPixelFormat24bppBGR => 3,
            _ => return Ok(None),
        };
        let mut raw = vec![0u8; w as usize * h as usize * bpp];
        transform.CopyPixels(std::ptr::null(), w, h, &format, WICBitmapTransformRotate0, w * bpp as u32, &mut raw)?;
        let pixels = if bpp == 3 { raw.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect() } else { raw };
        Ok(Some(Image { width: w, height: h, pixels }))
    }
}

fn full(source: &Source) -> windows::core::Result<Image> {
    unsafe {
        let converter = source.factory.CreateFormatConverter()?;
        converter.Initialize(&source.frame, &GUID_WICPixelFormat32bppBGRA, WICBitmapDitherTypeNone, None::<&IWICPalette>, 0.0, WICBitmapPaletteTypeCustom)?;
        let (w, h) = source.size;
        let mut pixels = vec![0u8; w as usize * h as usize * 4];
        converter.CopyPixels(std::ptr::null(), w * 4, &mut pixels)?;
        Ok(Image { width: w, height: h, pixels })
    }
}
