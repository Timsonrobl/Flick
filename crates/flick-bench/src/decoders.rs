//! Decoder adapters under test.
//!
//! Every adapter produces 4-channel pixels (RGBA or BGRA), because that is what
//! a GPU upload needs; any RGB-to-RGBA pass a decoder forces on us is part of
//! its cost. These are bench-local on purpose: the winners move into
//! `flick-codecs` behind the real `Decoder` trait (ARCHITECTURE.md §7).

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::jpeg;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Native resolution.
    Full,
    /// Smallest DCT scale (1/2, 1/4, 1/8) that still covers the image fitted to the screen.
    Screen,
    /// 1/8 DCT scale: the first-pixel fallback for files without an EXIF thumbnail.
    Preview,
    /// The embedded EXIF thumbnail.
    Thumb,
}

impl Mode {
    /// The DCT scale denominator a scaling decoder should use for `src`.
    fn denom(self, src: (u32, u32), screen: (u32, u32)) -> u32 {
        match self {
            Mode::Screen => screen_denom(src, screen),
            Mode::Preview => 8,
            Mode::Full | Mode::Thumb => 1,
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(match self {
            Mode::Full => "full",
            Mode::Screen => "screen",
            Mode::Preview => "preview",
            Mode::Thumb => "thumb",
        })
    }
}

pub struct Request {
    pub mode: Mode,
    pub screen: (u32, u32),
}

pub struct Output {
    pub width: u32,
    pub height: u32,
    /// DCT scale denominator actually used (1 = native).
    pub denom: u32,
    /// Kept so freeing it happens outside the timed region.
    #[allow(dead_code)]
    pub pixels: Vec<u8>,
}

pub trait Decoder {
    fn decode(&mut self, data: &[u8], req: &Request) -> Result<Output>;
}

pub struct Info {
    pub name: &'static str,
    pub modes: &'static [Mode],
    pub note: &'static str,
}

pub const ALL: &[Info] = &[
    Info { name: "image-rs", modes: &[Mode::Full], note: "image 0.25 (zune-jpeg inside) + into_rgba8; what most Rust viewers do" },
    Info { name: "zune-jpeg", modes: &[Mode::Full], note: "pure Rust, SIMD, BGRA output, no scaled decode" },
    Info { name: "jpeg-decoder", modes: &[Mode::Full, Mode::Screen, Mode::Preview], note: "pure Rust, DCT-scaled decode, RGB output (+ expand pass)" },
    #[cfg(windows)]
    Info { name: "wic", modes: &[Mode::Full, Mode::Screen, Mode::Preview], note: "Windows Imaging Component; scaled via IWICBitmapSourceTransform" },
    #[cfg(windows)]
    Info { name: "wic-scaler", modes: &[Mode::Screen], note: "WIC IWICBitmapScaler (Fant) straight to exact fit size" },
    #[cfg(feature = "turbojpeg")]
    Info { name: "turbojpeg", modes: &[Mode::Full, Mode::Screen, Mode::Preview], note: "libjpeg-turbo, DCT-scaled decode, BGRA output" },
    Info { name: "exif-thumb", modes: &[Mode::Thumb], note: "locate IFD1 thumbnail + zune-jpeg decode" },
];

pub fn info(name: &str) -> Option<&'static Info> {
    ALL.iter().find(|d| d.name == name)
}

pub fn create(name: &str) -> Result<Box<dyn Decoder>> {
    Ok(match name {
        "image-rs" => Box::new(ImageRs),
        "zune-jpeg" => Box::new(ZuneJpeg),
        "jpeg-decoder" => Box::new(JpegDecoder),
        #[cfg(windows)]
        "wic" => Box::new(wic::Wic::new(false)?),
        #[cfg(windows)]
        "wic-scaler" => Box::new(wic::Wic::new(true)?),
        #[cfg(feature = "turbojpeg")]
        "turbojpeg" => Box::new(TurboJpeg(turbojpeg::Decompressor::new()?)),
        "exif-thumb" => Box::new(ExifThumb),
        other => bail!("unknown decoder {other}"),
    })
}

/// Largest DCT denominator whose output still covers `src` fitted into `screen`.
pub fn screen_denom(src: (u32, u32), screen: (u32, u32)) -> u32 {
    let fit = f64::min(f64::from(screen.0) / f64::from(src.0), f64::from(screen.1) / f64::from(src.1));
    [8, 4, 2].into_iter().find(|&d| 1.0 / f64::from(d) >= fit).unwrap_or(1)
}

/// `src` fitted into `screen`, never upscaled.
pub fn fit_size(src: (u32, u32), screen: (u32, u32)) -> (u32, u32) {
    let fit = f64::min(1.0, f64::min(f64::from(screen.0) / f64::from(src.0), f64::from(screen.1) / f64::from(src.1)));
    ((f64::from(src.0) * fit).round().max(1.0) as u32, (f64::from(src.1) * fit).round().max(1.0) as u32)
}

fn expand_3_to_4(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() / 3 * 4);
    for px in src.chunks_exact(3) {
        out.extend_from_slice(&[px[0], px[1], px[2], 255]);
    }
    out
}

struct ImageRs;

impl Decoder for ImageRs {
    fn decode(&mut self, data: &[u8], _: &Request) -> Result<Output> {
        let img = image::load_from_memory_with_format(data, image::ImageFormat::Jpeg)?.into_rgba8();
        Ok(Output { width: img.width(), height: img.height(), denom: 1, pixels: img.into_raw() })
    }
}

struct ZuneJpeg;

fn zune_decode(data: &[u8]) -> Result<Output> {
    use zune_jpeg::zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions};

    let options = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::BGRA)
        .set_max_width(1 << 16)
        .set_max_height(1 << 16);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(data), options);
    let pixels = decoder.decode().map_err(|e| anyhow!("zune-jpeg: {e:?}"))?;
    let info = decoder.info().context("zune-jpeg: no image info")?;
    Ok(Output { width: u32::from(info.width), height: u32::from(info.height), denom: 1, pixels })
}

impl Decoder for ZuneJpeg {
    fn decode(&mut self, data: &[u8], _: &Request) -> Result<Output> {
        zune_decode(data)
    }
}

struct JpegDecoder;

impl Decoder for JpegDecoder {
    fn decode(&mut self, data: &[u8], req: &Request) -> Result<Output> {
        let mut decoder = jpeg_decoder::Decoder::new(data);
        decoder.read_info()?;
        let info = decoder.info().context("jpeg-decoder: no image info")?;
        let src = (u32::from(info.width), u32::from(info.height));
        let (mut width, mut height, mut denom) = (src.0, src.1, 1);
        if matches!(req.mode, Mode::Screen | Mode::Preview) {
            denom = req.mode.denom(src, req.screen);
            let want = (src.0.div_ceil(denom) as u16, src.1.div_ceil(denom) as u16);
            let (w, h) = decoder.scale(want.0, want.1)?;
            (width, height) = (u32::from(w), u32::from(h));
        }
        let raw = decoder.decode()?;
        let pixels = match decoder.info().map(|i| i.pixel_format) {
            Some(jpeg_decoder::PixelFormat::RGB24) => expand_3_to_4(&raw),
            Some(jpeg_decoder::PixelFormat::L8) => raw.iter().flat_map(|&l| [l, l, l, 255]).collect(),
            Some(jpeg_decoder::PixelFormat::CMYK32) => raw,
            other => bail!("jpeg-decoder: unsupported pixel format {other:?}"),
        };
        Ok(Output { width, height, denom, pixels })
    }
}

#[cfg(feature = "turbojpeg")]
struct TurboJpeg(turbojpeg::Decompressor);

#[cfg(feature = "turbojpeg")]
impl Decoder for TurboJpeg {
    fn decode(&mut self, data: &[u8], req: &Request) -> Result<Output> {
        let header = self.0.read_header(data)?;
        let denom = req.mode.denom((header.width as u32, header.height as u32), req.screen);
        let factor = turbojpeg::ScalingFactor::new(1, denom as usize);
        self.0.set_scaling_factor(factor)?;
        let scaled = header.scaled(factor);
        let mut image = turbojpeg::Image {
            pixels: vec![0u8; scaled.width * scaled.height * 4],
            width: scaled.width,
            pitch: scaled.width * 4,
            height: scaled.height,
            format: turbojpeg::PixelFormat::BGRA,
        };
        self.0.decompress(data, image.as_deref_mut())?;
        Ok(Output { width: scaled.width as u32, height: scaled.height as u32, denom, pixels: image.pixels })
    }
}

struct ExifThumb;

impl Decoder for ExifThumb {
    fn decode(&mut self, data: &[u8], _: &Request) -> Result<Output> {
        let thumb = jpeg::exif_thumbnail(data).context("no EXIF thumbnail")?;
        zune_decode(thumb)
    }
}

#[cfg(windows)]
mod wic {
    use anyhow::{Result, bail};
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
    use windows::core::{GUID, Interface};

    use super::{Decoder, Mode, Output, Request, expand_3_to_4, fit_size};

    pub struct Wic {
        factory: IWICImagingFactory,
        use_scaler: bool,
    }

    impl Wic {
        pub fn new(use_scaler: bool) -> Result<Self> {
            unsafe {
                CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
                let factory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
                Ok(Self { factory, use_scaler })
            }
        }

        fn to_bgra(&self, source: &IWICBitmapSource, width: u32, height: u32) -> Result<Vec<u8>> {
            unsafe {
                let converter = self.factory.CreateFormatConverter()?;
                converter.Initialize(source, &GUID_WICPixelFormat32bppBGRA, WICBitmapDitherTypeNone, None::<&IWICPalette>, 0.0, WICBitmapPaletteTypeCustom)?;
                let mut pixels = vec![0u8; width as usize * height as usize * 4];
                converter.CopyPixels(std::ptr::null(), width * 4, &mut pixels)?;
                Ok(pixels)
            }
        }
    }

    impl Decoder for Wic {
        fn decode(&mut self, data: &[u8], req: &Request) -> Result<Output> {
            unsafe {
                let stream = self.factory.CreateStream()?;
                stream.InitializeFromMemory(data)?;
                let decoder = self.factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
                let frame = decoder.GetFrame(0)?;
                let (mut width, mut height) = (0, 0);
                frame.GetSize(&mut width, &mut height)?;

                match (req.mode, self.use_scaler) {
                    (Mode::Full, _) => {
                        let pixels = self.to_bgra(&frame.cast()?, width, height)?;
                        Ok(Output { width, height, denom: 1, pixels })
                    }
                    (Mode::Screen | Mode::Preview, false) => {
                        let denom = req.mode.denom((width, height), req.screen);
                        let transform: IWICBitmapSourceTransform = frame.cast()?;
                        let (mut w, mut h) = (width.div_ceil(denom), height.div_ceil(denom));
                        transform.GetClosestSize(&mut w, &mut h)?;
                        let mut format: GUID = GUID_WICPixelFormat32bppBGRA;
                        transform.GetClosestPixelFormat(&mut format)?;
                        let bpp = match format {
                            f if f == GUID_WICPixelFormat32bppBGRA => 4,
                            f if f == GUID_WICPixelFormat24bppBGR => 3,
                            f => bail!("wic: transform offers unexpected pixel format {f:?}"),
                        };
                        let mut raw = vec![0u8; w as usize * h as usize * bpp];
                        transform.CopyPixels(std::ptr::null(), w, h, &format, WICBitmapTransformRotate0, w * bpp as u32, &mut raw)?;
                        let pixels = if bpp == 3 { expand_3_to_4(&raw) } else { raw };
                        Ok(Output { width: w, height: h, denom: width / w.max(1), pixels })
                    }
                    (Mode::Screen, true) => {
                        let (w, h) = fit_size((width, height), req.screen);
                        let scaler = self.factory.CreateBitmapScaler()?;
                        scaler.Initialize(&frame, w, h, WICBitmapInterpolationModeFant)?;
                        let pixels = self.to_bgra(&scaler.cast()?, w, h)?;
                        Ok(Output { width: w, height: h, denom: 1, pixels })
                    }
                    (Mode::Preview | Mode::Thumb, true) | (Mode::Thumb, false) => bail!("wic: {} mode not supported here", req.mode),
                }
            }
        }
    }
}
