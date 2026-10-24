//! WIC decoding. The `Screen` level is decoded at the smallest DCT scale that
//! still covers the target (the fastest final-quality picture, measured in
//! milestone 0); `Full` is the native resolution, for zooming in. Animated
//! GIFs are composed frame by frame into whole pictures (`animate`).

use std::path::Path;
use std::time::Duration;

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
}

fn open(path: &Path) -> windows::core::Result<Source> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let decoder = factory.CreateDecoderFromFilename(&HSTRING::from(path.as_os_str()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)?;
        let frame = decoder.GetFrame(0)?;
        let (mut w, mut h) = (0, 0);
        frame.GetSize(&mut w, &mut h)?;
        let frames = decoder.GetFrameCount().unwrap_or(1);
        let gif = decoder.GetContainerFormat().is_ok_and(|f| f == GUID_ContainerFormatGif);
        Ok(Source { factory, decoder, frame, size: (w, h), frames, gif })
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

/// Decodes at the smallest DCT scale (1/2, 1/4, 1/8) whose output still
/// covers `target` (in displayed orientation).
pub fn decode_screen(path: &Path, target: (u32, u32)) -> Result<Picture, String> {
    let inner = || -> windows::core::Result<Picture> {
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
            return Ok(Picture { screen, full_size, orientation: 1, frames: source.frames });
        }
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
        Ok(Picture { screen, full_size: source.size, orientation, frames: 1 })
    };
    inner().map_err(|e| format!("{}: {e}", path.display()))
}

/// Decodes an animated GIF frame by frame, each composed into a whole
/// picture, handing them to `each` in order until it returns false or the
/// frames run out (one pass; the caller loops the playback).
pub fn animate(path: &Path, each: impl FnMut(Frame) -> bool) -> Result<(), String> {
    open(path).and_then(|source| compose(&source, each)).map_err(|e| format!("{}: {e}", path.display()))
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
        converter.Initialize(&source.frame, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None::<&IWICPalette>, 0.0, WICBitmapPaletteTypeCustom)?;
        let (w, h) = source.size;
        let mut pixels = vec![0u8; w as usize * h as usize * 4];
        converter.CopyPixels(std::ptr::null(), w * 4, &mut pixels)?;
        Ok(Image { width: w, height: h, pixels })
    }
}
