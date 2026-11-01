//! The formats WIC doesn't cover here (ARCHITECTURE.md §7):
//! JPEG XL (jxl-oxide), SVG (resvg), OpenEXR and Radiance HDR (the image
//! crate, tone-mapped to 8 bits), and the animations WIC shows as stills,
//! APNG and animated WebP (the image crate). Everything else goes to WIC.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use image::AnimationDecoder;
use resvg::{tiny_skia, usvg};

use crate::decode::{Frame, Image};

/// Which decoder a file goes to, by its first bytes (SVG by extension).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Wic,
    /// An animated PNG with this many frames.
    Apng(u32),
    /// An animated WebP with this many frames.
    AnimatedWebp(u32),
    Jxl,
    Svg,
    /// OpenEXR or Radiance HDR: floating-point, tone-mapped for display.
    Float,
}

pub fn kind(path: &Path) -> Kind {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if ext == "svg" || ext == "svgz" {
        return Kind::Svg;
    }
    let mut head = [0u8; 32];
    let Ok(n) = File::open(path).and_then(|mut f| f.read(&mut head)) else { return Kind::Wic };
    let head = &head[..n];
    if head.starts_with(&[0xFF, 0x0A]) || head.starts_with(b"\0\0\0\x0cJXL \r\n\x87\n") {
        Kind::Jxl
    } else if head.starts_with(&[0x76, 0x2F, 0x31, 0x01]) || head.starts_with(b"#?RADIANCE") || head.starts_with(b"#?RGBE") {
        Kind::Float
    } else if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        apng_frames(path).filter(|&n| n > 1).map_or(Kind::Wic, Kind::Apng)
    } else if head.len() >= 21 && &head[..4] == b"RIFF" && &head[8..16] == b"WEBPVP8X" && head[20] & 0x02 != 0 {
        webp_frames(path).filter(|&n| n > 1).map_or(Kind::Wic, Kind::AnimatedWebp)
    } else {
        Kind::Wic
    }
}

/// An APNG's frame count, from its `acTL` chunk (which comes before the
/// first `IDAT`; a PNG without one is a still).
fn apng_frames(path: &Path) -> Option<u32> {
    let mut file = BufReader::new(File::open(path).ok()?);
    file.seek(SeekFrom::Start(8)).ok()?;
    for _ in 0..64 {
        let mut header = [0u8; 8];
        file.read_exact(&mut header).ok()?;
        let length = u32::from_be_bytes(header[..4].try_into().ok()?);
        match &header[4..] {
            b"acTL" => {
                let mut frames = [0u8; 4];
                file.read_exact(&mut frames).ok()?;
                return Some(u32::from_be_bytes(frames));
            }
            b"IDAT" => return None,
            _ => file.seek_relative(i64::from(length) + 4).ok()?,
        }
    }
    None
}

/// An animated WebP's frame count: its `ANMF` chunks.
fn webp_frames(path: &Path) -> Option<u32> {
    let mut file = BufReader::new(File::open(path).ok()?);
    file.seek(SeekFrom::Start(12)).ok()?;
    let mut frames = 0;
    let mut header = [0u8; 8];
    while file.read_exact(&mut header).is_ok() {
        let length = u32::from_le_bytes(header[4..].try_into().ok()?);
        frames += u32::from(&header[..4] == b"ANMF");
        file.seek_relative(i64::from(length + (length & 1))).ok()?;
    }
    Some(frames)
}

/// Straight RGBA (or gray, gray+alpha, RGB: `channels`) to premultiplied BGRA.
fn premultiplied_bgra(samples: &[u8], channels: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() / channels * 4);
    for p in samples.chunks_exact(channels) {
        let (r, g, b, a) = match channels {
            1 => (p[0], p[0], p[0], 255),
            2 => (p[0], p[0], p[0], p[1]),
            3 => (p[0], p[1], p[2], 255),
            _ => (p[0], p[1], p[2], p[3]),
        };
        let m = |c: u8| ((u16::from(c) * u16::from(a) + 127) / 255) as u8;
        out.extend_from_slice(&[m(b), m(g), m(r), a]);
    }
    out
}

/// A JPEG XL's first frame at full size, orientation applied.
pub fn jxl(path: &Path) -> Result<Image, String> {
    let image = jxl_oxide::JxlImage::builder().open(path).map_err(|e| e.to_string())?;
    let render = image.render_frame(0).map_err(|e| e.to_string())?;
    let mut stream = render.stream();
    let (width, height, channels) = (stream.width(), stream.height(), stream.channels() as usize);
    let mut samples = vec![0u8; width as usize * height as usize * channels];
    stream.write_to_buffer(&mut samples);
    Ok(Image { width, height, pixels: premultiplied_bgra(&samples, channels.min(4)) })
}

/// System fonts for SVG text, loaded on the first SVG (not at startup).
fn fonts() -> Arc<usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut db = usvg::fontdb::Database::new();
            db.load_system_fonts();
            Arc::new(db)
        })
        .clone()
}

pub struct Svg(usvg::Tree);

/// Where an SVG's "pixels" end: it has none, so Flick treats it as at least
/// 4096 px on its longer side (its own size if larger, up to 16384), and
/// renders the Screen and Full levels from that.
const SVG_MIN: f32 = 4096.0;
const SVG_MAX: f32 = 16384.0;

impl Svg {
    pub fn open(path: &Path) -> Result<Svg, String> {
        let data = std::fs::read(path).map_err(|e| e.to_string())?;
        let options = usvg::Options { resources_dir: path.parent().map(Path::to_path_buf), fontdb: fonts(), ..Default::default() };
        usvg::Tree::from_data(&data, &options).map(Svg).map_err(|e| e.to_string())
    }

    pub fn full_size(&self) -> (u32, u32) {
        let size = self.0.size();
        let longer = size.width().max(size.height());
        let scale = longer.clamp(SVG_MIN, SVG_MAX) / longer;
        ((size.width() * scale).round().max(1.0) as u32, (size.height() * scale).round().max(1.0) as u32)
    }

    /// Renders the whole drawing into `size` pixels.
    pub fn render(&self, (width, height): (u32, u32)) -> Result<Image, String> {
        let mut pixmap = tiny_skia::Pixmap::new(width, height).ok_or("SVG size out of range")?;
        let size = self.0.size();
        let transform = tiny_skia::Transform::from_scale(width as f32 / size.width(), height as f32 / size.height());
        resvg::render(&self.0, transform, &mut pixmap.as_mut());
        // tiny-skia is premultiplied RGBA already.
        let mut pixels = pixmap.take();
        for p in pixels.chunks_exact_mut(4) {
            p.swap(0, 2);
        }
        Ok(Image { width, height, pixels })
    }
}

/// OpenEXR or Radiance HDR, tone-mapped to sRGB: extended Reinhard on
/// luminance with the white point at the brightest pixels (the 99.9th
/// percentile, so a few hot pixels don't darken the rest), or plain
/// clipping if nothing is brighter than 1.0.
pub fn float(path: &Path) -> Result<Image, String> {
    let image = image::ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| e.to_string())?
        .decode()
        .map_err(|e| e.to_string())?
        .into_rgba32f();
    let (width, height) = image.dimensions();
    let luminance = |p: &[f32]| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2];
    let mut sample: Vec<f32> = image.as_raw().chunks_exact(4).step_by(16).map(luminance).filter(|l| l.is_finite()).collect();
    sample.sort_by(f32::total_cmp);
    let white = sample.get(sample.len() * 999 / 1000).copied().unwrap_or(1.0).max(1.0);
    let srgb = |c: f32| {
        let c = c.clamp(0.0, 1.0);
        if c <= 0.003_130_8 { 12.92 * c } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
    };
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    for p in image.as_raw().chunks_exact(4) {
        let l = luminance(p).max(0.0);
        let scale = if white > 1.0 { (1.0 + l / (white * white)) / (1.0 + l) } else { 1.0 };
        let a = if p[3].is_finite() { p[3].clamp(0.0, 1.0) } else { 1.0 };
        let byte = |v: f32| (v * 255.0 + 0.5) as u8;
        pixels.extend_from_slice(&[byte(srgb(p[2] * scale) * a), byte(srgb(p[1] * scale) * a), byte(srgb(p[0] * scale) * a), byte(a)]);
    }
    Ok(Image { width, height, pixels })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_of(name: &str, bytes: &[u8]) -> Kind {
        let path = std::env::temp_dir().join(format!("flick-kind-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        let kind = kind(&path);
        let _ = std::fs::remove_file(&path);
        kind
    }

    fn chunk(kind: &[u8], data: &[u8]) -> Vec<u8> {
        [&(data.len() as u32).to_be_bytes()[..], kind, data, &[0; 4]].concat()
    }

    #[test]
    fn sniffs_formats_by_content() {
        let png = |extra: &[u8]| [&b"\x89PNG\r\n\x1a\n"[..], &chunk(b"IHDR", &[0; 13]), extra, &chunk(b"IDAT", &[])].concat();
        assert_eq!(kind_of("a.png", &png(&[])), Kind::Wic, "a still PNG");
        assert_eq!(kind_of("b.png", &png(&chunk(b"acTL", &[0, 0, 0, 3, 0, 0, 0, 0]))), Kind::Apng(3));
        assert_eq!(kind_of("c.png", &png(&chunk(b"acTL", &[0, 0, 0, 1, 0, 0, 0, 0]))), Kind::Wic, "one frame is a still");

        let riff = |chunk: &[u8], data: &[u8]| [chunk, &(data.len() as u32).to_le_bytes()[..], data].concat();
        let webp = |flags: u8| {
            let body = [riff(b"VP8X", &[flags, 0, 0, 0, 0, 0, 0, 0, 0, 0]), riff(b"ANIM", &[0; 6]), riff(b"ANMF", &[0; 2]), riff(b"ANMF", &[0; 3]), vec![0]].concat();
            [&b"RIFF"[..], &(body.len() as u32 + 4).to_le_bytes(), b"WEBP", &body].concat()
        };
        assert_eq!(kind_of("d.webp", &webp(0x02)), Kind::AnimatedWebp(2));
        assert_eq!(kind_of("e.webp", &webp(0x10)), Kind::Wic, "no animation flag");

        assert_eq!(kind_of("f.jxl", &[0xFF, 0x0A, 1, 2]), Kind::Jxl);
        assert_eq!(kind_of("g", b"\0\0\0\x0cJXL \r\n\x87\n\0\0"), Kind::Jxl, "JPEG XL container, any name");
        assert_eq!(kind_of("h.exr", &[0x76, 0x2F, 0x31, 0x01, 2, 0]), Kind::Float);
        assert_eq!(kind_of("i.hdr", b"#?RADIANCE\n"), Kind::Float);
        assert_eq!(kind_of("j.svg", b"<svg/>"), Kind::Svg);
        assert_eq!(kind_of("k.jpg", &[0xFF, 0xD8, 0xFF]), Kind::Wic);
    }

    #[test]
    fn premultiplies_into_bgra() {
        assert_eq!(premultiplied_bgra(&[255, 0, 0, 128], 4), [0, 0, 128, 128]);
        assert_eq!(premultiplied_bgra(&[10, 20, 30], 3), [30, 20, 10, 255]);
        assert_eq!(premultiplied_bgra(&[90, 51], 2), [18, 18, 18, 51]);
    }

    #[test]
    fn tone_maps_hdr_brightness_in_order() {
        let path = std::env::temp_dir().join(format!("flick-tone-{}.hdr", std::process::id()));
        let pixels = [image::Rgb([0.05f32; 3]), image::Rgb([0.5; 3]), image::Rgb([4.0; 3])];
        image::codecs::hdr::HdrEncoder::new(std::fs::File::create(&path).unwrap()).encode(&pixels, 3, 1).unwrap();
        let image = float(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let grey: Vec<u8> = image.pixels.chunks_exact(4).map(|p| p[1]).collect();
        assert!(grey[0] < grey[1] && grey[1] < grey[2], "{grey:?}");
        assert_eq!(grey[2], 255, "the brightest pixel is the white point");
        assert!(image.pixels.chunks_exact(4).all(|p| p[3] == 255));
    }
}

/// An APNG's or animated WebP's frames, composed into whole pictures by the
/// image crate, handed to `each` until it returns false (one pass). Delays
/// under 20 ms play at 100 ms, as for GIFs.
pub fn frames(path: &Path, kind: Kind, mut each: impl FnMut(Frame) -> bool) -> Result<(), String> {
    let reader = BufReader::new(File::open(path).map_err(|e| e.to_string())?);
    let frames = match kind {
        Kind::Apng(_) => image::codecs::png::PngDecoder::new(reader).and_then(|d| d.apng()).map(AnimationDecoder::into_frames),
        _ => image::codecs::webp::WebPDecoder::new(reader).map(AnimationDecoder::into_frames),
    }
    .map_err(|e| e.to_string())?;
    for frame in frames {
        let frame = frame.map_err(|e| e.to_string())?;
        let (numer, denom) = frame.delay().numer_denom_ms();
        let ms = numer / denom.max(1);
        let delay = Duration::from_millis(u64::from(if ms < 20 { 100 } else { ms }));
        let buffer = frame.into_buffer();
        let (width, height) = buffer.dimensions();
        if !each(Frame { image: Image { width, height, pixels: premultiplied_bgra(buffer.as_raw(), 4) }, delay }) {
            break;
        }
    }
    Ok(())
}
