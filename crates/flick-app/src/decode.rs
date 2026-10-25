//! Decoding. WIC does most formats; `formats` the ones it doesn't cover
//! here (JPEG XL, SVG, EXR/HDR, APNG and animated WebP). The `Screen` level
//! is decoded at the smallest DCT scale that still covers the target (the
//! fastest final-quality picture, measured in milestone 0), or scaled down
//! to the target with WIC's Fant scaler; `Full` is the native resolution,
//! for zooming in. Animations are composed frame by frame into whole
//! pictures (`animate`). A picture with a colour profile other than sRGB
//! (Display P3, Adobe RGB...) is converted to sRGB as it decodes
//! (`to_srgb`); the rest, nearly all, are taken as sRGB at no cost.

use std::path::Path;
use std::time::Duration;

use crate::formats::{self, Kind};

use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::System::Variant::{VT_UI1, VT_UI2};
use windows::core::{GUID, HSTRING, Interface, PCWSTR, w};

/// Premultiplied BGRA pixels, rows tightly packed, in the file's stored
/// orientation.
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
    /// More than one: an animated GIF (`animate` plays it).
    pub frames: u32,
    /// A quick soft stand-in (`decode_preview`): the real screen level follows.
    pub preview: bool,
}

/// One composed frame of an animation and how long it shows.
pub struct Frame {
    pub image: Image,
    pub delay: Duration,
}

struct Source {
    factory: IWICImagingFactory,
    decoder: IWICBitmapDecoder,
    frame: IWICBitmapFrameDecode,
    size: (u32, u32),
    frames: u32,
    gif: bool,
    /// The frame's colour, when it needs converting to sRGB.
    profile: Option<IWICColorContext>,
}

/// `image` (as stored) turned to how EXIF orientation `o` (1–8) displays it.
pub fn oriented(image: Image, o: u16) -> Image {
    if !(2..=8).contains(&o) {
        return image;
    }
    let (w, h) = (image.width as usize, image.height as usize);
    let (ow, oh) = if o >= 5 { (h, w) } else { (w, h) };
    let mut pixels = vec![0u8; image.pixels.len()];
    for y in 0..h {
        for x in 0..w {
            // Where stored pixel (x, y) lands, as view::orientation maps it.
            let (dx, dy) = match o {
                2 => (w - 1 - x, y),
                3 => (w - 1 - x, h - 1 - y),
                4 => (x, h - 1 - y),
                5 => (y, x),
                6 => (h - 1 - y, x),
                7 => (h - 1 - y, w - 1 - x),
                _ => (y, w - 1 - x),
            };
            let (s, d) = ((y * w + x) * 4, (dy * ow + dx) * 4);
            pixels[d..d + 4].copy_from_slice(&image.pixels[s..s + 4]);
        }
    }
    Image { width: ow as u32, height: oh as u32, pixels }
}

/// A WIC factory for this (worker) thread.
pub fn factory() -> windows::core::Result<IWICImagingFactory> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
    }
}

fn open(path: &Path) -> windows::core::Result<Source> {
    unsafe {
        let factory = factory()?;
        let decoder = factory.CreateDecoderFromFilename(&HSTRING::from(path.as_os_str()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)?;
        let frame = decoder.GetFrame(0)?;
        let (mut w, mut h) = (0, 0);
        frame.GetSize(&mut w, &mut h)?;
        let frames = decoder.GetFrameCount().unwrap_or(1);
        let gif = decoder.GetContainerFormat().is_ok_and(|f| f == GUID_ContainerFormatGif);
        let profile = profile(&factory, &frame);
        Ok(Source { factory, decoder, frame, size: (w, h), frames, gif, profile })
    }
}

/// The frame's colour profile if it isn't sRGB: an embedded ICC profile,
/// or Adobe RGB by EXIF. None (no profile, or sRGB, or gray): shown as is.
fn profile(factory: &IWICImagingFactory, frame: &IWICBitmapFrameDecode) -> Option<IWICColorContext> {
    unsafe {
        let mut count = 0;
        frame.GetColorContexts(&mut [], &mut count).ok()?;
        if count == 0 {
            return None;
        }
        let mut contexts: Vec<Option<IWICColorContext>> = (0..count).map(|_| factory.CreateColorContext().ok()).collect();
        frame.GetColorContexts(&mut contexts, &mut count).ok()?;
        let contexts: Vec<IWICColorContext> = contexts.into_iter().flatten().collect();
        for context in &contexts {
            if context.GetType().ok()? == WICColorContextProfile {
                return (!needs_no_conversion(&profile_bytes(context)?)).then(|| context.clone());
            }
        }
        // No profile: EXIF's colour space (2: Adobe RGB, by the DCF rule).
        contexts.into_iter().find(|c| c.GetType().ok() == Some(WICColorContextExifColorSpace) && c.GetExifColorSpace().ok() == Some(2))
    }
}

/// The frame's colour profile by name, for the info panel: an embedded ICC
/// profile's description ("sRGB IEC61966-2.1", "Display P3"), or Adobe RGB
/// by EXIF; None without either.
pub fn profile_name(factory: &IWICImagingFactory, frame: &IWICBitmapFrameDecode) -> Option<String> {
    unsafe {
        let mut count = 0;
        frame.GetColorContexts(&mut [], &mut count).ok()?;
        let mut contexts: Vec<Option<IWICColorContext>> = (0..count).map(|_| factory.CreateColorContext().ok()).collect();
        frame.GetColorContexts(&mut contexts, &mut count).ok()?;
        let contexts: Vec<IWICColorContext> = contexts.into_iter().flatten().collect();
        if let Some(icc) = contexts.iter().find(|c| c.GetType().ok() == Some(WICColorContextProfile)) {
            return Some(icc_description(&profile_bytes(icc)?).unwrap_or_else(|| "ICC profile".into()));
        }
        contexts.iter().any(|c| c.GetType().ok() == Some(WICColorContextExifColorSpace) && c.GetExifColorSpace().ok() == Some(2)).then(|| "Adobe RGB (EXIF)".into())
    }
}

fn profile_bytes(context: &IWICColorContext) -> Option<Vec<u8>> {
    unsafe {
        // Asked with no buffer, it gives the size (and a "too small" error).
        let mut size = 0;
        let _ = context.GetProfileBytes(&mut [], &mut size);
        let mut bytes = vec![0u8; size as usize];
        context.GetProfileBytes(&mut bytes, &mut size).ok()?;
        Some(bytes)
    }
}

/// An ICC profile's description: its 'desc' tag, v2 ('desc', ASCII) or v4
/// ('mluc', UTF-16).
fn icc_description(icc: &[u8]) -> Option<String> {
    let be32 = |b: &[u8], o: usize| b.get(o..o + 4).map_or(0, |s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]) as usize);
    let count = be32(icc, 128).min(100);
    let entry = (0..count).map(|i| 132 + i * 12).find(|&e| icc.get(e..e + 4) == Some(b"desc"))?;
    let at = be32(icc, entry + 4);
    let tag = icc.get(at..at.saturating_add(be32(icc, entry + 8)).min(icc.len()))?;
    let text = match tag.get(0..4)? {
        b"desc" => String::from_utf8_lossy(tag.get(12..12 + be32(tag, 8)).unwrap_or(&[])).into_owned(),
        b"mluc" => {
            let (n, at) = (be32(tag, 20), be32(tag, 24));
            let wide: Vec<u16> = tag.get(at..at + n).unwrap_or(&[]).chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            String::from_utf16_lossy(&wide)
        }
        _ => return None,
    };
    Some(text.trim_end_matches('\0').trim().to_string()).filter(|t| !t.is_empty())
}

/// An ICC profile that is sRGB (by its description: "sRGB IEC61966-2.1"
/// and kin, and Facebook's small "c2"), or gray: converting it would
/// change nothing worth the time.
fn needs_no_conversion(icc: &[u8]) -> bool {
    if icc.get(16..20) == Some(b"GRAY") {
        return true;
    }
    let Some(text) = icc_description(icc).map(|t| t.to_lowercase()) else { return false };
    text.contains("srgb") || text.contains("61966") || text == "c2" || text == "c2ci"
}

/// `bitmap` (pixels of `source`'s frame) in sRGB, as 32bpp BGRA, converted
/// from the frame's profile; unchanged when it has none to convert from.
fn to_srgb(source: &Source, bitmap: IWICBitmapSource) -> windows::core::Result<IWICBitmapSource> {
    let Some(profile) = &source.profile else { return Ok(bitmap) };
    unsafe {
        let srgb = source.factory.CreateColorContext()?;
        srgb.InitializeFromExifColorSpace(1)?;
        let transform = source.factory.CreateColorTransformer()?;
        transform.Initialize(&bitmap, profile, &srgb, &GUID_WICPixelFormat32bppBGRA)?;
        transform.cast()
    }
}

/// A small unsigned metadata value (GIF descriptors are UI1/UI2).
fn meta(reader: &IWICMetadataQueryReader, name: PCWSTR) -> Option<u32> {
    unsafe {
        let mut value = PROPVARIANT::default();
        reader.GetMetadataByName(name, &mut value).ok()?;
        let inner = &value.Anonymous.Anonymous;
        match inner.vt {
            VT_UI2 => Some(u32::from(inner.Anonymous.uiVal)),
            VT_UI1 => Some(u32::from(inner.Anonymous.bVal)),
            _ => None,
        }
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

/// The size `size` shrinks to so it fits in `target`, if it is larger.
fn fit_size((w, h): (u32, u32), target: (u32, u32)) -> Option<(u32, u32)> {
    let scale = f64::min(f64::from(target.0) / f64::from(w), f64::from(target.1) / f64::from(h));
    (scale < 1.0).then(|| (((f64::from(w) * scale).ceil() as u32).max(1), ((f64::from(h) * scale).ceil() as u32).max(1)))
}

/// A progressive JPEG's first scans only, at 1/8 size: a soft stand-in in
/// ~90 ms where the screen level takes ~1 s (24 MP at top quality), for
/// paging fast. None for anything else (or should it fail).
pub fn decode_preview(path: &Path) -> Option<Picture> {
    if formats::kind(path) != Kind::Wic {
        return None;
    }
    let source = open(path).ok()?;
    let jpeg = source.frame.cast::<IWICJpegFrameDecode>().ok()?;
    let mut header = WICJpegFrameHeader::default();
    unsafe { jpeg.GetFrameHeader(&mut header) }.ok()?;
    if header.ScanType != WICJpegScanTypeProgressive {
        return None;
    }
    // Level 0: the DC scan, an 1/8 picture in its own right.
    unsafe { source.frame.cast::<IWICProgressiveLevelControl>().ok()?.SetCurrentLevel(0) }.ok()?;
    let screen = scaled(&source, 8).ok()??;
    Some(Picture { screen, full_size: source.size, orientation: orientation(&source.frame), frames: 1, preview: true })
}

/// The Screen level: the picture as a window first shows it, no larger than
/// `target` (in displayed orientation) where the format allows.
pub fn decode_screen(path: &Path, target: (u32, u32)) -> Result<Picture, String> {
    let named = |e: String| format!("{}: {e}", path.display());
    let still = |screen: Image, full_size| Picture { screen, full_size, orientation: 1, frames: 1, preview: false };
    match formats::kind(path) {
        Kind::Wic => wic_screen(path, target).map_err(|e| named(e.to_string())),
        kind @ (Kind::Apng(frames) | Kind::AnimatedWebp(frames)) => {
            // Animations are shown at full size, as GIFs are.
            let screen = first_frame(path, kind).map_err(named)?;
            let full_size = (screen.width, screen.height);
            Ok(Picture { screen, full_size, orientation: 1, frames, preview: false })
        }
        Kind::Jxl => {
            let full = formats::jxl(path).map_err(named)?;
            let full_size = (full.width, full.height);
            Ok(still(shrink(full, target).map_err(|e| named(e.to_string()))?, full_size))
        }
        Kind::Svg => {
            let svg = formats::Svg::open(path).map_err(named)?;
            let full_size = svg.full_size();
            Ok(still(svg.render(fit_size(full_size, target).unwrap_or(full_size)).map_err(named)?, full_size))
        }
        Kind::Float => {
            let full = formats::float(path).map_err(named)?;
            let full_size = (full.width, full.height);
            Ok(still(shrink(full, target).map_err(|e| named(e.to_string()))?, full_size))
        }
    }
}

fn first_frame(path: &Path, kind: Kind) -> Result<Image, String> {
    let mut first = None;
    formats::frames(path, kind, |frame| {
        first = Some(frame.image);
        false
    })?;
    first.ok_or_else(|| "no frames".into())
}

/// A decoded picture scaled down to fit `target` (Fant, through WIC), if
/// it is larger.
fn shrink(image: Image, target: (u32, u32)) -> windows::core::Result<Image> {
    let Some((w, h)) = fit_size((image.width, image.height), target) else { return Ok(image) };
    unsafe {
        let factory = factory()?;
        let bitmap = factory.CreateBitmapFromMemory(image.width, image.height, &GUID_WICPixelFormat32bppPBGRA, image.width * 4, &image.pixels)?;
        let scaler = factory.CreateBitmapScaler()?;
        scaler.Initialize(&bitmap, w, h, WICBitmapInterpolationModeFant)?;
        let mut pixels = vec![0u8; w as usize * h as usize * 4];
        scaler.CopyPixels(std::ptr::null(), w * 4, &mut pixels)?;
        Ok(Image { width: w, height: h, pixels })
    }
}

/// A progressive JPEG decoded by zune-jpeg, colour-managed as the rest: WIC's
/// decoder takes about twice as long on these (24 MP at top quality: 1.9 s
/// against 0.93), and its DCT scaling saves nothing (every scan is read
/// whatever the size wanted). Only YCbCr and greyscale ones (WIC's frame
/// format says which): a CMYK JPEG's colours need its profile on the CMYK
/// values themselves. None: not one of these, or zune-jpeg failed (WIC then
/// decodes it).
fn zune_full(path: &Path, source: &Source) -> Option<Image> {
    managed(source, zune_raw(path, source)?)
}

/// `image` (opaque BGRA of `source`'s frame) converted from the frame's
/// colour profile to sRGB, if it has one to convert from.
fn managed(source: &Source, image: Image) -> Option<Image> {
    if source.profile.is_none() {
        return Some(image); // opaque: premultiplied as it is
    }
    let (width, height) = (image.width, image.height);
    unsafe {
        let bitmap = source.factory.CreateBitmapFromMemory(width, height, &GUID_WICPixelFormat32bppBGRA, width * 4, &image.pixels).ok()?;
        let mut pixels = vec![0u8; image.pixels.len()];
        to_srgb(source, bitmap.cast().ok()?).ok()?.CopyPixels(std::ptr::null(), width * 4, &mut pixels).ok()?;
        Some(Image { width, height, pixels })
    }
}

/// zune-jpeg's decode, before colour management (`zune_full`).
fn zune_raw(path: &Path, source: &Source) -> Option<Image> {
    use zune_jpeg::zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions};
    let jpeg = source.frame.cast::<IWICJpegFrameDecode>().ok()?;
    let mut header = WICJpegFrameHeader::default();
    unsafe { jpeg.GetFrameHeader(&mut header) }.ok()?;
    let format = unsafe { source.frame.GetPixelFormat() }.ok()?;
    if header.ScanType != WICJpegScanTypeProgressive || !(format == GUID_WICPixelFormat24bppBGR || format == GUID_WICPixelFormat8bppGray) {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::BGRA).set_max_width(1 << 16).set_max_height(1 << 16);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(&data), options);
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let (width, height) = (u32::from(info.width), u32::from(info.height));
    ((width, height) == source.size && pixels.len() == width as usize * height as usize * 4).then_some(Image { width, height, pixels })
}

/// WIC formats: the smallest DCT scale (1/2, 1/4, 1/8) whose output still
/// covers `target`; else, for a picture larger than it, the Fant scaler. A
/// progressive JPEG: zune-jpeg's full size, then the Fant scaler.
fn wic_screen(path: &Path, target: (u32, u32)) -> windows::core::Result<Picture> {
    let source = open(path)?;
    if source.gif {
        // Frame 0 composed on the GIF's logical screen, as `animate` shows it.
        let mut first = None;
        compose(&source, |frame| {
            first = Some(frame.image);
            false
        })?;
        let screen = first.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
        let full_size = (screen.width, screen.height);
        return Ok(Picture { screen, full_size, orientation: 1, frames: source.frames, preview: false });
    }
    let orientation = orientation(&source.frame);
    let stored_target = if orientation >= 5 { (target.1, target.0) } else { target };
    // Shrunk first, then colour-managed: converting the whole picture took ~3× as long.
    if let Some(screen) = zune_raw(path, &source).and_then(|full| managed(&source, shrink(full, stored_target).ok()?)) {
        return Ok(Picture { screen, full_size: source.size, orientation, frames: 1, preview: false });
    }
    let (w, h) = source.size;
    let (dw, dh) = if orientation >= 5 { (h, w) } else { (w, h) };
    let fit = f64::min(f64::from(target.0) / f64::from(dw), f64::from(target.1) / f64::from(dh));
    let denom = [8, 4, 2].into_iter().find(|&d| 1.0 / f64::from(d) >= fit).unwrap_or(1);
    let screen = if denom > 1 { scaled(&source, denom)? } else { None };
    let stored_target = if orientation >= 5 { (target.1, target.0) } else { target };
    let screen = match (screen, fit_size(source.size, stored_target)) {
        (Some(image), _) => image,
        (None, Some(size)) => fitted(&source, size)?,
        (None, None) => full(&source)?,
    };
    Ok(Picture { screen, full_size: source.size, orientation, frames: 1, preview: false })
}

/// Decodes an animation (GIF, APNG, animated WebP) frame by frame, each
/// composed into a whole picture, handing them to `each` in order until it
/// returns false or the frames run out (one pass; the caller loops the
/// playback).
pub fn animate(path: &Path, each: impl FnMut(Frame) -> bool) -> Result<(), String> {
    let result = match formats::kind(path) {
        kind @ (Kind::Apng(_) | Kind::AnimatedWebp(_)) => formats::frames(path, kind, each),
        _ => open(path).and_then(|source| compose(&source, each)).map_err(|e| e.to_string()),
    };
    result.map_err(|e| format!("{}: {e}", path.display()))
}

/// GIF composition: every frame is a rectangle drawn onto the logical
/// screen, with transparent pixels left as they were; afterwards its
/// disposal leaves it (0, 1), clears its rectangle to transparent (2) or puts
/// back what was there before (3). Browsers treat delays under 20 ms as 100.
fn compose(source: &Source, mut each: impl FnMut(Frame) -> bool) -> windows::core::Result<()> {
    unsafe {
        let screen = source.decoder.GetMetadataQueryReader().ok();
        let logical = |name| screen.as_ref().and_then(|r| meta(r, name)).filter(|&v| v > 0);
        let (width, height) = match (logical(w!("/logscrdesc/Width")), logical(w!("/logscrdesc/Height"))) {
            (Some(w), Some(h)) => (w, h),
            _ => source.size,
        };
        let stride = width as usize * 4;
        let mut canvas = vec![0u8; stride * height as usize];
        // What the previous frame's disposal undoes before the next is drawn.
        let mut undo: Option<(u32, (u32, u32, u32, u32), Option<Vec<u8>>)> = None;
        for index in 0..source.frames {
            let frame = source.decoder.GetFrame(index)?;
            let reader = frame.GetMetadataQueryReader().ok();
            let get = |name| reader.as_ref().and_then(|r| meta(r, name)).unwrap_or(0);
            let (mut fw, mut fh) = (0, 0);
            frame.GetSize(&mut fw, &mut fh)?;
            let rect = (get(w!("/imgdesc/Left")), get(w!("/imgdesc/Top")), fw, fh);
            let disposal = get(w!("/grctlext/Disposal"));
            let delay_cs = get(w!("/grctlext/Delay"));

            match undo.take() {
                Some((2, (x, y, w, h), _)) => fill(&mut canvas, stride, (width, height), (x, y, w, h)),
                Some((3, _, Some(saved))) => canvas = saved,
                _ => {}
            }
            undo = Some((disposal, rect, (disposal == 3).then(|| canvas.clone())));

            let converter = source.factory.CreateFormatConverter()?;
            converter.Initialize(&frame, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None::<&IWICPalette>, 0.0, WICBitmapPaletteTypeCustom)?;
            let mut pixels = vec![0u8; fw as usize * fh as usize * 4];
            converter.CopyPixels(std::ptr::null(), fw * 4, &mut pixels)?;
            let (x, y) = (rect.0, rect.1);
            let rows = if x < width { fh.min(height.saturating_sub(y)) } else { 0 };
            for row in 0..rows {
                let src = &pixels[row as usize * fw as usize * 4..][..fw as usize * 4];
                let dst = &mut canvas[(y + row) as usize * stride + x as usize * 4..][..(fw.min(width.saturating_sub(x)) as usize * 4)];
                for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    if s[3] != 0 {
                        d.copy_from_slice(s);
                    }
                }
            }

            let delay = if delay_cs < 2 { Duration::from_millis(100) } else { Duration::from_millis(u64::from(delay_cs) * 10) };
            if !each(Frame { image: Image { width, height, pixels: canvas.clone() }, delay }) {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Milestone 4 part B's test picture: a 30000 × 8000 JPEG (240 MP) of
    /// colour gradients, a 500 px grid and fine diagonals (a tile seam would
    /// break them), written in strips. `cargo test --release -p flick-app
    /// huge_test_picture -- --ignored --nocapture`; then `bands` times it.
    #[test]
    #[ignore]
    fn huge_test_picture() {
        let (w, h) = (30_000u32, 8_000u32);
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/samples/huge-30000x8000.jpg");
        let started = std::time::Instant::now();
        unsafe {
            let factory = factory().unwrap();
            let stream = factory.CreateStream().unwrap();
            stream.InitializeFromFilename(&HSTRING::from(path.as_os_str()), windows::Win32::Foundation::GENERIC_WRITE.0).unwrap();
            let encoder = factory.CreateEncoder(&GUID_ContainerFormatJpeg, std::ptr::null()).unwrap();
            encoder.Initialize(&stream, WICBitmapEncoderNoCache).unwrap();
            let (mut frame, mut options) = (None, None);
            encoder.CreateNewFrame(&mut frame, &mut options).unwrap();
            let frame = frame.unwrap();
            frame.Initialize(options.as_ref()).unwrap();
            frame.SetSize(w, h).unwrap();
            let mut format = GUID_WICPixelFormat24bppBGR;
            frame.SetPixelFormat(&mut format).unwrap();
            let rows = 256;
            let mut strip = vec![0u8; w as usize * rows * 3];
            for top in (0..h).step_by(rows) {
                let n = (rows as u32).min(h - top);
                for dy in 0..n {
                    let y = top + dy;
                    for x in 0..w {
                        let i = (dy * w + x) as usize * 3;
                        let line = x % 500 < 3 || y % 500 < 3;
                        let diagonal = (x + y) % 97 < 2 || (x + 3 * y) % 211 < 2;
                        let px = if line { [0, 0, 0] } else if diagonal { [255, 255, 255] } else { [(255 * y / h) as u8, 128, (255 * x / w) as u8] };
                        strip[i..i + 3].copy_from_slice(&px);
                    }
                }
                frame.WritePixels(n, w * 3, &strip[..(n * w * 3) as usize]).unwrap();
            }
            frame.Commit().unwrap();
            encoder.Commit().unwrap();
        }
        println!("wrote {} in {:.1} s", path.display(), started.elapsed().as_secs_f64());
        let started = std::time::Instant::now();
        let mut count = 0;
        bands(&path, 256, |size, _, rows, pixels| {
            assert_eq!(pixels.len(), (size.0 * rows * 4) as usize);
            count += 1;
            Ok(())
        })
        .unwrap();
        println!("decoded in {count} bands in {:.2} s", started.elapsed().as_secs_f64());
    }

    const RED: [u8; 4] = [0, 0, 255, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [255, 0, 0, 255];
    const CLEAR: [u8; 4] = [0, 0, 0, 0];

    /// Pictures are uploaded as premultiplied BGRA, so an opaque photo's
    /// screen level (DCT-scaled path) must be fully opaque. Uses the local
    /// burst set; skipped where it isn't present.
    #[test]
    fn jpeg_screen_level_is_opaque() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/photos/burst");
        let Some(file) = std::fs::read_dir(dir).ok().and_then(|mut d| d.find_map(|e| e.ok().map(|e| e.path()))) else { return };
        let picture = decode_screen(&file, (1920, 1080)).unwrap();
        assert!(picture.screen.pixels.chunks_exact(4).all(|p| p[3] == 255));
    }

    /// How fast WIC gives a progressive JPEG's early levels (scans), each from
    /// a fresh open at 1/8 size: for a preview while paging fast. `$env:FLICK_SAMPLE`.
    #[test]
    #[ignore]
    fn progressive_level_timings() {
        let file = std::path::PathBuf::from(std::env::var_os("FLICK_SAMPLE").expect("FLICK_SAMPLE"));
        let levels = unsafe { open(&file).unwrap().frame.cast::<IWICProgressiveLevelControl>().unwrap().GetLevelCount().unwrap() };
        println!("{levels} levels");
        for level in 0..levels.min(6) {
            let started = std::time::Instant::now();
            let source = open(&file).unwrap();
            unsafe { source.frame.cast::<IWICProgressiveLevelControl>().unwrap().SetCurrentLevel(level).unwrap() };
            let image = scaled(&source, 8).unwrap().unwrap();
            let mean = image.pixels.iter().map(|&v| f64::from(v)).sum::<f64>() / image.pixels.len() as f64;
            println!("level {level}: {}x{} in {:.0} ms (mean {mean:.0})", image.width, image.height, started.elapsed().as_secs_f64() * 1000.0);
        }
    }

    /// Progressive JPEGs (local samples; skipped where not present) decode
    /// through zune-jpeg to what WIC's decoder makes, within rounding, a
    /// Display P3 one colour-managed alike; and faster. With `--nocapture`,
    /// prints both times and the differences.
    #[test]
    fn zune_decodes_progressive_jpegs_as_wic_does() {
        let samples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/samples");
        for name in ["heavy/progressive-1.jpg", "progressive-2.jpg"] {
            let file = samples.join(name);
            let Ok(source) = open(&file) else { continue };
            let started = std::time::Instant::now();
            let zune = zune_full(&file, &source).expect("a progressive YCbCr JPEG: zune-jpeg's");
            let zune_ms = started.elapsed().as_secs_f64() * 1000.0;
            let started = std::time::Instant::now();
            let wic = full(&source).unwrap();
            let wic_ms = started.elapsed().as_secs_f64() * 1000.0;
            assert_eq!((zune.width, zune.height), (wic.width, wic.height));
            let diffs: Vec<u32> = zune.pixels.iter().zip(&wic.pixels).map(|(a, b)| u32::from(a.abs_diff(*b))).collect();
            let mean = diffs.iter().sum::<u32>() as f64 / diffs.len() as f64;
            let large = diffs.iter().filter(|&&d| d > 8).count() as f64 / diffs.len() as f64;
            println!("{name}: zune {zune_ms:.0} ms, WIC {wic_ms:.0} ms; mean difference {mean:.2}, over 8: {:.3} %, profile {}", large * 100.0, source.profile.is_some());
            assert!(mean < 1.5 && large < 0.001, "{name}: mean {mean}, {large} of values off by more than 8");
        }
    }

    /// A Display P3 photo (local sample; skipped where it isn't present)
    /// comes out converted to sRGB, so more saturated than its pixels taken
    /// as they are, on every WIC path: DCT-scaled, Fant-scaled and full.
    /// With `--nocapture`, prints what the conversion costs.
    #[test]
    fn converts_a_display_p3_photo_to_srgb() {
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/samples/display-p3.jpg");
        let Ok(mut source) = open(&file) else { return };
        assert!(source.profile.is_some(), "a Display P3 profile, to convert from");
        let spread = |image: &Image| image.pixels.chunks_exact(4).map(|p| f64::from(p[0].max(p[1]).max(p[2]) - p[0].min(p[1]).min(p[2]))).sum::<f64>() / f64::from(image.width * image.height);
        let paths: [(&str, fn(&Source) -> windows::core::Result<Image>); 3] = [
            ("DCT-scaled", |s| scaled(s, 4).map(|i| i.unwrap())),
            ("Fant-scaled", |s| fitted(s, (1000, 667))),
            ("full", full),
        ];
        let profile = source.profile.take();
        for (name, decode) in paths {
            let started = std::time::Instant::now();
            let raw = decode(&source).unwrap();
            let raw_ms = started.elapsed().as_secs_f64() * 1000.0;
            source.profile = profile.clone();
            let started = std::time::Instant::now();
            let managed = decode(&source).unwrap();
            let managed_ms = started.elapsed().as_secs_f64() * 1000.0;
            source.profile = None;
            println!("{name}: {raw_ms:.1} ms as is, {managed_ms:.1} ms converted; colour spread {:.2} → {:.2}", spread(&raw), spread(&managed));
            assert!(spread(&managed) > spread(&raw) * 1.03, "{name}: converted to sRGB, P3 colours spread wider");
        }
    }

    /// Milestone 7: every sample from `bench/gen-formats.ps1`, decoded as a
    /// window would (screen level for a 3377×1440 target, then full), with
    /// times. `cargo test --release -p flick-app formats_report -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn formats_report() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/samples/formats");
        let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok().map(|e| e.path())).collect();
        files.sort();
        for file in files {
            let name = file.file_name().unwrap().to_string_lossy().into_owned();
            let started = std::time::Instant::now();
            let screen = decode_screen(&file, (3377, 1440));
            let screen_ms = started.elapsed().as_secs_f64() * 1000.0;
            let started = std::time::Instant::now();
            let full = decode_full(&file);
            let full_ms = started.elapsed().as_secs_f64() * 1000.0;
            if let (Ok(p), Some(out)) = (&screen, std::env::var_os("FLICK_FORMATS_OUT")) {
                // The screen level over a checkerboard, as a PNG to look at.
                let s = &p.screen;
                let rgb = (0..s.width * s.height).flat_map(|i| {
                    let px = &s.pixels[i as usize * 4..][..4];
                    let (x, y) = (i % s.width, i / s.width);
                    let check = if (x / 16 + y / 16) % 2 == 0 { 200u16 } else { 120 };
                    let under = |c: u8| (u16::from(c) + check * (255 - u16::from(px[3])) / 255).min(255) as u8;
                    [under(px[2]), under(px[1]), under(px[0])]
                });
                let image = image::RgbImage::from_raw(s.width, s.height, rgb.collect()).unwrap();
                std::fs::create_dir_all(&out).unwrap();
                image.save(Path::new(&out).join(format!("{name}.png"))).unwrap();
            }
            let screen = match screen {
                Ok(p) => format!("{}x{} of {}x{}, {} frame(s)", p.screen.width, p.screen.height, p.full_size.0, p.full_size.1, p.frames),
                Err(e) => format!("ERROR {}", e.rsplit(": ").next().unwrap_or(&e)),
            };
            let full = match full {
                Ok(i) => format!("{}x{}", i.width, i.height),
                Err(e) => format!("ERROR {}", e.rsplit(": ").next().unwrap_or(&e)),
            };
            println!("{name:<26} screen {screen_ms:>6.0} ms  {screen:<40} full {full_ms:>6.0} ms  {full}");
        }
    }

    /// How fast the large animations decode, frame by frame (they stream
    /// during playback, so decoding must keep up with their frame rate).
    /// Needs `bench/gen-formats.ps1 -LargeAnimations`.
    #[test]
    #[ignore]
    fn large_animation_throughput() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/samples/formats");
        for name in ["gif-video-1080p.gif", "webp-video-1080p.webp"] {
            let started = std::time::Instant::now();
            let (mut frames, mut playing) = (0u32, Duration::ZERO);
            animate(&dir.join(name), |frame| {
                frames += 1;
                playing += frame.delay;
                true
            })
            .unwrap();
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            println!("{name}: {frames} frames in {ms:.0} ms, {:.1} ms per frame; plays {:.1} ms per frame", ms / f64::from(frames), playing.as_secs_f64() * 1000.0 / f64::from(frames));
        }
    }

    /// Pixels land where `view::orientation` draws them.
    #[test]
    fn orients_pixels_as_drawn() {
        // 2 × 1: red then green.
        let image = || Image { width: 2, height: 1, pixels: [RED, GREEN].concat() };
        let turned = oriented(image(), 6); // 90° clockwise: 1 × 2, red on top
        assert_eq!((turned.width, turned.height), (1, 2));
        assert_eq!((pixel(&turned, 0, 0), pixel(&turned, 0, 1)), (RED, GREEN));
        let turned = oriented(image(), 8); // 90° anticlockwise: green on top
        assert_eq!((pixel(&turned, 0, 0), pixel(&turned, 0, 1)), (GREEN, RED));
        let turned = oriented(image(), 2); // mirrored
        assert_eq!((pixel(&turned, 0, 0), pixel(&turned, 1, 0)), (GREEN, RED));
        assert_eq!(oriented(image(), 1).pixels, image().pixels);
    }

    fn pixel(image: &Image, x: u32, y: u32) -> [u8; 4] {
        let i = (y * image.width + x) as usize * 4;
        image.pixels[i..i + 4].try_into().unwrap()
    }

    /// A 4×4 GIF: a red background frame; a 2×2 green frame at (1,1) whose
    /// first pixel is transparent and which is cleared afterwards (disposal
    /// 2); a blue pixel at (0,0) restored afterwards (disposal 3); a last
    /// frame that draws nothing.
    #[test]
    fn composes_offsets_transparency_and_disposal() {
        let palette = [255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0];
        let mut file = Vec::new();
        {
            let mut encoder = gif::Encoder::new(&mut file, 4, 4, &palette).unwrap();
            let frame = |left, top, width, height, indices: &[u8], delay, dispose| gif::Frame {
                left,
                top,
                width,
                height,
                buffer: std::borrow::Cow::Owned(indices.to_vec()),
                transparent: Some(3),
                delay,
                dispose,
                ..Default::default()
            };
            encoder.write_frame(&frame(0, 0, 4, 4, &[0; 16], 0, gif::DisposalMethod::Keep)).unwrap();
            encoder.write_frame(&frame(1, 1, 2, 2, &[3, 1, 1, 1], 5, gif::DisposalMethod::Background)).unwrap();
            encoder.write_frame(&frame(0, 0, 1, 1, &[2], 5, gif::DisposalMethod::Previous)).unwrap();
            encoder.write_frame(&frame(3, 3, 1, 1, &[3], 5, gif::DisposalMethod::Keep)).unwrap();
        }
        let path = std::env::temp_dir().join(format!("flick-test-{}.gif", std::process::id()));
        std::fs::write(&path, &file).unwrap();

        let picture = decode_screen(&path, (1920, 1080)).unwrap();
        assert_eq!((picture.screen.width, picture.screen.height, picture.frames), (4, 4, 4));
        let mut frames = Vec::new();
        animate(&path, |frame| {
            frames.push(frame);
            true
        })
        .unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(frames.len(), 4);

        assert_eq!(frames[0].delay, Duration::from_millis(100), "delay 0 plays at 100 ms");
        assert_eq!(frames[1].delay, Duration::from_millis(50));
        assert_eq!(pixel(&frames[0].image, 2, 2), RED);
        // Frame 1: transparent pixel keeps the red below; the rest is green.
        assert_eq!(pixel(&frames[1].image, 1, 1), RED);
        assert_eq!(pixel(&frames[1].image, 2, 2), GREEN);
        // Frame 2: frame 1's rectangle cleared, blue at the origin.
        assert_eq!(pixel(&frames[2].image, 2, 2), CLEAR);
        assert_eq!(pixel(&frames[2].image, 1, 1), CLEAR);
        assert_eq!(pixel(&frames[2].image, 0, 0), BLUE);
        assert_eq!(pixel(&frames[2].image, 3, 0), RED);
        // Frame 3: the blue pixel restored to what was before it (red).
        assert_eq!(pixel(&frames[3].image, 0, 0), RED);
        assert_eq!(pixel(&frames[3].image, 2, 2), CLEAR);
        assert_eq!(pixel(&picture.screen, 2, 2), RED);
    }
}

/// Clears `rect` (clipped to the canvas) to transparent.
fn fill(canvas: &mut [u8], stride: usize, (width, height): (u32, u32), (x, y, w, h): (u32, u32, u32, u32)) {
    for row in y..(y + h).min(height) {
        let start = row as usize * stride + x.min(width) as usize * 4;
        let end = row as usize * stride + (x + w).min(width) as usize * 4;
        canvas[start..end].fill(0);
    }
}

/// The Full level: native resolution (for an SVG, its `full_size`).
pub fn decode_full(path: &Path) -> Result<Image, String> {
    let result = match formats::kind(path) {
        Kind::Wic => open(path).and_then(|source| zune_full(path, &source).map_or_else(|| full(&source), Ok)).map_err(|e| e.to_string()),
        kind @ (Kind::Apng(_) | Kind::AnimatedWebp(_)) => first_frame(path, kind),
        Kind::Jxl => formats::jxl(path),
        Kind::Svg => formats::Svg::open(path).and_then(|svg| svg.render(svg.full_size())),
        Kind::Float => formats::float(path),
    };
    result.map_err(|e| format!("{}: {e}", path.display()))
}

/// A zoom's detail level: 1/`denom` of native size where the codec scales
/// while decoding (JPEG's DCT; for a picture too large for one bitmap at
/// full size: scaling saves little time, Huffman decoding dominates), else
/// full size. Returns it with the fraction it got.
pub fn decode_level(path: &Path, denom: u32) -> Result<(Image, u32), String> {
    if denom > 1 && formats::kind(path) == Kind::Wic {
        let level = open(path).and_then(|source| match scaled(&source, denom)? {
            Some(image) => Ok((image, denom)),
            None => full(&source).map(|image| (image, 1)),
        });
        return level.map_err(|e| format!("{}: {e}", path.display()));
    }
    decode_full(path).map(|image| (image, 1))
}

/// A WIC picture at full resolution, top to bottom in bands of `rows` rows
/// (premultiplied BGRA, colour-managed, stored orientation), so a picture
/// too large to hold whole never is: `each(size, y, rows, pixels)`.
pub fn bands(path: &Path, rows: u32, mut each: impl FnMut((u32, u32), u32, u32, &[u8]) -> Result<(), String>) -> Result<(), String> {
    let named = |e: windows::core::Error| format!("{}: {e}", path.display());
    let source = open(path).map_err(named)?;
    let (w, h) = source.size;
    let converter = unsafe {
        let converter = source.factory.CreateFormatConverter().map_err(named)?;
        converter
            .Initialize(&to_srgb(&source, source.frame.cast().map_err(named)?).map_err(named)?, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None::<&IWICPalette>, 0.0, WICBitmapPaletteTypeCustom)
            .map_err(named)?;
        converter
    };
    let mut band = vec![0u8; w as usize * rows as usize * 4];
    for y in (0..h).step_by(rows as usize) {
        let n = rows.min(h - y);
        let rect = WICRect { X: 0, Y: y as i32, Width: w as i32, Height: n as i32 };
        let pixels = &mut band[..w as usize * n as usize * 4];
        unsafe { converter.CopyPixels(&rect, w * 4, pixels) }.map_err(named)?;
        each((w, h), y, n, pixels)?;
    }
    Ok(())
}

/// DCT-scaled decode through the codec's own transform, if it has one.
fn scaled(source: &Source, denom: u32) -> windows::core::Result<Option<Image>> {
    unsafe {
        let Ok(transform) = source.frame.cast::<IWICBitmapSourceTransform>() else { return Ok(None) };
        let (want_w, want_h) = (source.size.0.div_ceil(denom), source.size.1.div_ceil(denom));
        let (mut w, mut h) = (want_w, want_h);
        transform.GetClosestSize(&mut w, &mut h)?;
        // Some codecs (PNG) offer the transform but only at full size.
        if w > want_w || h > want_h {
            return Ok(None);
        }
        let mut format: GUID = GUID_WICPixelFormat32bppBGRA;
        transform.GetClosestPixelFormat(&mut format)?;
        let bpp = match format {
            f if f == GUID_WICPixelFormat32bppBGRA => 4,
            f if f == GUID_WICPixelFormat24bppBGR => 3,
            _ => return Ok(None),
        };
        let mut raw = vec![0u8; w as usize * h as usize * bpp];
        transform.CopyPixels(std::ptr::null(), w, h, &format, WICBitmapTransformRotate0, w * bpp as u32, &mut raw)?;
        if source.profile.is_some() {
            let bitmap = source.factory.CreateBitmapFromMemory(w, h, &format, w * bpp as u32, &raw)?;
            let mut pixels = vec![0u8; w as usize * h as usize * 4];
            to_srgb(source, bitmap.cast()?)?.CopyPixels(std::ptr::null(), w * 4, &mut pixels)?;
            return Ok(Some(Image { width: w, height: h, pixels }));
        }
        let pixels = if bpp == 3 { raw.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect() } else { raw };
        Ok(Some(Image { width: w, height: h, pixels }))
    }
}

/// Scaled to exactly `size` with the Fant scaler, after conversion to
/// premultiplied BGRA (so transparent edges don't fringe). A colour profile
/// is converted after scaling, on the small picture (before it, on the whole
/// one, took ~3× as long): on straight alpha, so a profiled picture with
/// transparency may fringe slightly at its edges.
fn fitted(source: &Source, (w, h): (u32, u32)) -> windows::core::Result<Image> {
    unsafe {
        let premultiplied = |bitmap: &IWICBitmapSource| -> windows::core::Result<IWICBitmapSource> {
            let converter = source.factory.CreateFormatConverter()?;
            converter.Initialize(bitmap, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None::<&IWICPalette>, 0.0, WICBitmapPaletteTypeCustom)?;
            converter.cast()
        };
        let scaler = source.factory.CreateBitmapScaler()?;
        let out = if source.profile.is_none() {
            scaler.Initialize(&premultiplied(&source.frame.cast()?)?, w, h, WICBitmapInterpolationModeFant)?;
            scaler.cast()?
        } else {
            scaler.Initialize(&source.frame, w, h, WICBitmapInterpolationModeFant)?;
            premultiplied(&to_srgb(source, scaler.cast()?)?)?
        };
        let mut pixels = vec![0u8; w as usize * h as usize * 4];
        out.CopyPixels(std::ptr::null(), w * 4, &mut pixels)?;
        Ok(Image { width: w, height: h, pixels })
    }
}

fn full(source: &Source) -> windows::core::Result<Image> {
    unsafe {
        let converter = source.factory.CreateFormatConverter()?;
        converter.Initialize(&to_srgb(source, source.frame.cast()?)?, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None::<&IWICPalette>, 0.0, WICBitmapPaletteTypeCustom)?;
        let (w, h) = source.size;
        let mut pixels = vec![0u8; w as usize * h as usize * 4];
        converter.CopyPixels(std::ptr::null(), w * 4, &mut pixels)?;
        Ok(Image { width: w, height: h, pixels })
    }
}
