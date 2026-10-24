//! Format samples ffmpeg can't write (milestone 7): JPEG XL (zune-jpegxl's
//! lossless encoder) and HEIC (WIC's HEIF encoder, from the HEIF and HEVC
//! extensions). `bench/gen-formats.ps1` makes the rest with ffmpeg.

use std::path::Path;

use anyhow::{Context, Result};
use zune_core::bit_depth::BitDepth;
use zune_core::colorspace::ColorSpace;
use zune_core::options::EncoderOptions;
use zune_jpegxl::JxlSimpleEncoder;

/// Writes `<stem>.jxl` and `<stem>.heic` next to each other in `dir`, from a
/// PNG (RGB or RGBA, 8-bit).
pub fn generate(source: &Path, dir: &Path, stem: &str) -> Result<()> {
    let image = image::open(source).with_context(|| format!("reading {}", source.display()))?;
    let (width, height) = (image.width(), image.height());
    let alpha = image.color().has_alpha();
    std::fs::create_dir_all(dir)?;

    let (pixels, space) = if alpha { (image.to_rgba8().into_raw(), ColorSpace::RGBA) } else { (image.to_rgb8().into_raw(), ColorSpace::RGB) };
    let mut jxl = Vec::new();
    JxlSimpleEncoder::new(&pixels, EncoderOptions::new(width as usize, height as usize, space, BitDepth::Eight))
        .encode(&mut jxl)
        .map_err(|e| anyhow::anyhow!("JPEG XL: {e:?}"))?;
    let path = dir.join(format!("{stem}.jxl"));
    std::fs::write(&path, jxl)?;
    eprintln!("wrote {}", path.display());

    #[cfg(windows)]
    {
        let path = dir.join(format!("{stem}.heic"));
        match heic(&image.to_rgba8(), &path) {
            Ok(()) => eprintln!("wrote {}", path.display()),
            Err(e) => eprintln!("HEIC skipped (no WIC HEIF encoder?): {e}"),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn heic(image: &image::RgbaImage, path: &Path) -> windows::core::Result<()> {
    use windows::Win32::Foundation::GENERIC_WRITE;
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::StructuredStorage::IPropertyBag2;
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
    use windows::core::HSTRING;

    // WIC wants BGRA.
    let bgra: Vec<u8> = image.pixels().flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let stream = factory.CreateStream()?;
        stream.InitializeFromFilename(&HSTRING::from(path.as_os_str()), GENERIC_WRITE.0)?;
        let encoder = factory.CreateEncoder(&GUID_ContainerFormatHeif, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut frame = None;
        let mut options: Option<IPropertyBag2> = None;
        encoder.CreateNewFrame(&mut frame, &mut options)?;
        let frame = frame.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
        frame.Initialize(options.as_ref())?;
        frame.SetSize(image.width(), image.height())?;
        let mut format = GUID_WICPixelFormat32bppBGRA;
        frame.SetPixelFormat(&mut format)?;
        frame.WritePixels(image.height(), image.width() * 4, &bgra)?;
        frame.Commit()?;
        encoder.Commit()?;
    }
    Ok(())
}
