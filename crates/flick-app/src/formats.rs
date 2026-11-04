//! The formats WIC doesn't cover here (ARCHITECTURE.md §7):
//! JPEG XL (jxl-oxide), SVG (resvg), OpenEXR and Radiance HDR (the image
//! crate, tone-mapped to 8 bits), and the animations WIC shows as stills,
//! APNG and animated WebP (the image crate). Everything else goes to WIC.
//! Sizes are checked from each file's header before decoding (`failure`),
//! and an SVG is read as an image, not a document: no files or network
//! outside it, nesting and `<use>` within bounds (§7a).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use image::{AnimationDecoder, ImageDecoder};
use resvg::{tiny_skia, usvg};

use crate::decode::{Frame, Image};
use crate::failure::{self, Error, MAX_PIXELS, check, damaged, pixels};

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

/// Chunks `webp_frames` reads at most: an animated WebP of 100 000 frames
/// is ~an hour of video; a file of empty chunks shouldn't hold up an open.
const WEBP_CHUNKS: u32 = 100_000;

/// An animated WebP's frame count: its `ANMF` chunks.
fn webp_frames(path: &Path) -> Option<u32> {
    let mut file = BufReader::new(File::open(path).ok()?);
    file.seek(SeekFrom::Start(12)).ok()?;
    let mut frames = 0;
    let mut header = [0u8; 8];
    for _ in 0..WEBP_CHUNKS {
        if file.read_exact(&mut header).is_err() {
            break;
        }
        let length = u64::from(u32::from_le_bytes(header[4..].try_into().ok()?));
        frames += u32::from(&header[..4] == b"ANMF");
        file.seek_relative((length + (length & 1)) as i64).ok()?;
    }
    Some(frames)
}

/// An animation's canvas, from its header: an APNG's IHDR, an animated
/// WebP's VP8X (each up to 2³¹ or 2²⁴ px a side, from a few bytes).
fn canvas(path: &Path, kind: Kind) -> Option<(u64, u64)> {
    let mut head = [0u8; 30];
    File::open(path).ok()?.read_exact(&mut head).ok()?;
    let be = |at: usize| u64::from(u32::from_be_bytes(head[at..at + 4].try_into().unwrap()));
    let le24 = |at: usize| u64::from(head[at]) | u64::from(head[at + 1]) << 8 | u64::from(head[at + 2]) << 16;
    match kind {
        Kind::Apng(_) if &head[12..16] == b"IHDR" => Some((be(16), be(20))),
        Kind::AnimatedWebp(_) => Some((le24(24) + 1, le24(27) + 1)),
        _ => None,
    }
}

/// An animation's frames are each a whole canvas (and some are kept to play
/// back), so animations stop at a quarter of a still's limit: 67 MP.
fn check_animation(w: u64, h: u64) -> failure::Result<()> {
    check(w, h)?;
    if w * h > MAX_PIXELS / 4 { Err(Error::TooLarge(w, h)) } else { Ok(()) }
}

/// The image crate's limits for what Flick has checked already: no
/// allocation over a still's limit in BGRA, twice over.
fn limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_PIXELS * 8);
    limits
}

fn image_error(e: image::ImageError) -> Error {
    match e {
        image::ImageError::Limits(_) => Error::TooLarge(0, 0),
        e => damaged(e),
    }
}

/// Straight RGBA (or gray, gray+alpha, RGB: `channels`) to premultiplied BGRA.
fn premultiplied_bgra(samples: &[u8], channels: usize) -> failure::Result<Vec<u8>> {
    let mut out = Vec::new();
    out.try_reserve_exact(samples.len() / channels * 4).map_err(|_| Error::Memory)?;
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
    Ok(out)
}

/// What jxl-oxide may allocate for one picture (its own tracker refuses
/// past it): 2 GiB, a ~150 MP picture.
const JXL_MAX_BYTES: usize = 2 << 30;

/// A JPEG XL's first frame at full size, orientation applied.
pub fn jxl(path: &Path) -> failure::Result<Image> {
    let image = jxl_oxide::JxlImage::builder().alloc_tracker(jxl_oxide::AllocTracker::with_limit(JXL_MAX_BYTES)).open(path).map_err(damaged)?;
    check(image.width().into(), image.height().into())?;
    let render = image.render_frame(0).map_err(damaged)?;
    let mut stream = render.stream();
    let (width, height, channels) = (stream.width(), stream.height(), stream.channels() as usize);
    check(width.into(), height.into())?;
    // Grey, grey + alpha, RGB, RGBA; CMYK comes out as five channels or more.
    if !(1..=4).contains(&channels) {
        return Err(damaged(format!("{channels} channels (a CMYK JPEG XL?)")));
    }
    let mut samples = pixels(width, height, channels)?;
    stream.write_to_buffer(&mut samples);
    Ok(Image { width, height, pixels: premultiplied_bgra(&samples, channels)? })
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

/// The most XML an SVG may have, inflated (SVGZ deflates ~1000 to 1).
const SVG_MAX_BYTES: u64 = 64 << 20;
/// Elements nested deeper than this are refused: usvg and resvg recurse
/// per level, and a stack overflow ends the process (browsers stop at a
/// few hundred too).
const SVG_MAX_DEPTH: usize = 256;
/// Elements once every `<use>` is expanded: references to references can
/// double at each step (a "billion laughs" of the drawing).
const SVG_MAX_ELEMENTS: u64 = 2_000_000;
/// usvg and resvg run on a thread with this much stack (reserved, not
/// committed, so it costs nothing unless a deep drawing needs it).
const SVG_STACK: usize = 64 << 20;

impl Svg {
    pub fn open(path: &Path) -> failure::Result<Svg> {
        let text = svg_text(path)?;
        on_big_stack(|| {
            let doc = roxmltree::Document::parse_with_options(&text, roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() }).map_err(damaged)?;
            svg_weight(&doc)?;
            // An image, as a browser's <img> loads one: pictures embedded in
            // the file (data: URLs) only. A path in `href` would read any
            // file, and a \\server\share path would make Windows connect
            // to that server and offer it the user's credentials.
            let images = usvg::ImageHrefResolver { resolve_data: usvg::ImageHrefResolver::default_data_resolver(), resolve_string: Box::new(|_, _| None) };
            let options = usvg::Options { resources_dir: None, fontdb: fonts(), image_href_resolver: images, ..Default::default() };
            usvg::Tree::from_xmltree(&doc, &options).map(Svg).map_err(damaged)
        })
    }

    pub fn full_size(&self) -> (u32, u32) {
        let size = self.0.size();
        let longer = size.width().max(size.height());
        let scale = longer.clamp(SVG_MIN, SVG_MAX) / longer;
        ((size.width() * scale).round().max(1.0) as u32, (size.height() * scale).round().max(1.0) as u32)
    }

    /// Renders the whole drawing into `size` pixels.
    pub fn render(&self, (width, height): (u32, u32)) -> failure::Result<Image> {
        check(width.into(), height.into())?;
        let size = tiny_skia::IntSize::from_wh(width, height).ok_or(Error::TooLarge(width.into(), height.into()))?;
        let mut pixmap = tiny_skia::Pixmap::from_vec(pixels(width, height, 4)?, size).ok_or(Error::TooLarge(width.into(), height.into()))?;
        let svg_size = self.0.size();
        let transform = tiny_skia::Transform::from_scale(width as f32 / svg_size.width(), height as f32 / svg_size.height());
        on_big_stack(|| {
            resvg::render(&self.0, transform, &mut pixmap.as_mut());
            Ok(())
        })?;
        // tiny-skia is premultiplied RGBA already.
        let mut pixels = pixmap.take();
        for p in pixels.chunks_exact_mut(4) {
            p.swap(0, 2);
        }
        Ok(Image { width, height, pixels })
    }
}

/// An SVG's text: SVGZ inflated, at most SVG_MAX_BYTES either way.
fn svg_text(path: &Path) -> failure::Result<String> {
    let mut file = File::open(path).map_err(damaged)?;
    let mut magic = [0u8; 2];
    let gzip = file.read_exact(&mut magic).is_ok() && magic == [0x1f, 0x8b];
    file.seek(SeekFrom::Start(0)).map_err(damaged)?;
    let mut data = Vec::new();
    let read = if gzip {
        flate2::read::GzDecoder::new(BufReader::new(file)).take(SVG_MAX_BYTES + 1).read_to_end(&mut data)
    } else {
        file.take(SVG_MAX_BYTES + 1).read_to_end(&mut data)
    };
    read.map_err(damaged)?;
    if data.len() as u64 > SVG_MAX_BYTES {
        return Err(Error::Complex(format!("over {} MB of XML", SVG_MAX_BYTES >> 20)));
    }
    String::from_utf8(data).map_err(|_| damaged("not UTF-8 text"))
}

/// Refuses a drawing nested deeper than SVG_MAX_DEPTH, or larger than
/// SVG_MAX_ELEMENTS once its `<use>` references are expanded (each counted
/// as a copy of what it refers to; a reference back into itself counts
/// nothing, as usvg skips it).
fn svg_weight(doc: &roxmltree::Document) -> failure::Result<()> {
    let count = doc.descendants().map(|n| n.id().get_usize()).max().unwrap_or(0) + 1;
    // Depth in document order (parents come first): no recursion here.
    let mut depth = vec![0usize; count];
    for node in doc.descendants() {
        if let Some(parent) = node.parent() {
            let d = depth[parent.id().get_usize()] + 1;
            if d > SVG_MAX_DEPTH {
                return Err(Error::Complex(format!("elements nested over {SVG_MAX_DEPTH} deep")));
            }
            depth[node.id().get_usize()] = d;
        }
    }
    let ids: HashMap<&str, roxmltree::Node> = doc.descendants().filter_map(|n| Some((n.attribute("id")?, n))).collect();
    // Weight of each element, worked out once: None unknown, Some(None) under way.
    let mut memo: Vec<Option<Option<u64>>> = vec![None; count];
    fn weight(node: roxmltree::Node, ids: &HashMap<&str, roxmltree::Node>, memo: &mut [Option<Option<u64>>], hops: usize) -> failure::Result<u64> {
        // Nesting is bounded above; each `<use>` hop nests once more.
        if hops > 4 * SVG_MAX_DEPTH {
            return Err(Error::Complex("references nested too deep".into()));
        }
        match memo[node.id().get_usize()] {
            Some(Some(w)) => return Ok(w),
            Some(None) => return Ok(0), // a reference back into itself
            None => {}
        }
        memo[node.id().get_usize()] = Some(None);
        let mut total: u64 = 1;
        for child in node.children().filter(roxmltree::Node::is_element) {
            total = total.saturating_add(weight(child, ids, memo, hops + 1)?);
        }
        if node.tag_name().name() == "use" {
            let href = node.attributes().find(|a| a.name() == "href").map(|a| a.value());
            if let Some(target) = href.and_then(|h| h.strip_prefix('#')).and_then(|id| ids.get(id)) {
                total = total.saturating_add(weight(*target, ids, memo, hops + 1)?);
            }
        }
        if total > SVG_MAX_ELEMENTS {
            return Err(Error::Complex(format!("over {SVG_MAX_ELEMENTS} elements with <use> expanded")));
        }
        memo[node.id().get_usize()] = Some(Some(total));
        Ok(total)
    }
    weight(doc.root_element(), &ids, &mut memo, 0).map(|_| ())
}

/// Runs `work` on a thread with SVG_STACK of stack, passing its panic on.
fn on_big_stack<T: Send>(work: impl FnOnce() -> failure::Result<T> + Send) -> failure::Result<T> {
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new().name("svg".into()).stack_size(SVG_STACK).spawn_scoped(scope, work).map_err(|_| Error::Memory)?;
        handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

/// OpenEXR or Radiance HDR, tone-mapped to sRGB: extended Reinhard on
/// luminance with the white point at the brightest pixels (the 99.9th
/// percentile, so a few hot pixels don't darken the rest), or plain
/// clipping if nothing is brighter than 1.0. The image crate's default
/// limit (512 MB while decoding) applies.
pub fn float(path: &Path) -> failure::Result<Image> {
    let image = image::ImageReader::open(path).and_then(|r| r.with_guessed_format()).map_err(damaged)?.decode().map_err(image_error)?.into_rgba32f();
    let (width, height) = image.dimensions();
    check(width.into(), height.into())?;
    let luminance = |p: &[f32]| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2];
    let mut sample: Vec<f32> = image.as_raw().chunks_exact(4).step_by(16).map(luminance).filter(|l| l.is_finite()).collect();
    sample.sort_by(f32::total_cmp);
    let white = sample.get(sample.len() * 999 / 1000).copied().unwrap_or(1.0).max(1.0);
    let srgb = |c: f32| {
        let c = c.clamp(0.0, 1.0);
        if c <= 0.003_130_8 { 12.92 * c } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
    };
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(width as usize * height as usize * 4).map_err(|_| Error::Memory)?;
    for p in image.as_raw().chunks_exact(4) {
        let l = luminance(p).max(0.0);
        let scale = if white > 1.0 { (1.0 + l / (white * white)) / (1.0 + l) } else { 1.0 };
        let a = if p[3].is_finite() { p[3].clamp(0.0, 1.0) } else { 1.0 };
        let byte = |v: f32| (v * 255.0 + 0.5) as u8;
        pixels.extend_from_slice(&[byte(srgb(p[2] * scale) * a), byte(srgb(p[1] * scale) * a), byte(srgb(p[0] * scale) * a), byte(a)]);
    }
    Ok(Image { width, height, pixels })
}

/// A PNG WIC refuses, as far as its rows decode (the rest transparent): a
/// download or copy cut short keeps its top. None if no row decodes, or it
/// is interlaced (its early passes are a whole picture of gaps).
pub fn salvage_png(path: &Path) -> Option<Image> {
    let file = BufReader::new(File::open(path).ok()?);
    let mut decoder = png::Decoder::new_with_limits(file, png::Limits { bytes: (MAX_PIXELS * 4) as usize });
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let (width, height, interlaced) = (reader.info().width, reader.info().height, reader.info().interlaced);
    check(width.into(), height.into()).ok()?;
    if interlaced {
        return None;
    }
    let channels = reader.output_color_type().0.samples();
    let mut out = pixels(width, height, 4).ok()?;
    let row_bytes = width as usize * 4;
    let mut rows = 0;
    while rows < height as usize {
        let Ok(Some(row)) = reader.next_row() else { break };
        let data = row.data().get(..width as usize * channels)?;
        out[rows * row_bytes..][..row_bytes].copy_from_slice(&premultiplied_bgra(data, channels).ok()?);
        rows += 1;
    }
    (rows > 0).then_some(Image { width, height, pixels: out })
}

/// An APNG's or animated WebP's frames, composed into whole pictures by the
/// image crate, handed to `each` until it returns false (one pass). Delays
/// under 20 ms play at 100 ms, as for GIFs. A frame that fails after the
/// first ends the animation there.
pub fn frames(path: &Path, kind: Kind, mut each: impl FnMut(Frame) -> bool) -> failure::Result<()> {
    let (w, h) = canvas(path, kind).ok_or_else(|| damaged("no canvas size"))?;
    check_animation(w, h)?;
    let reader = BufReader::new(File::open(path).map_err(damaged)?);
    let frames = match kind {
        Kind::Apng(_) => image::codecs::png::PngDecoder::with_limits(reader, limits()).and_then(|d| d.apng()).map(AnimationDecoder::into_frames),
        _ => image::codecs::webp::WebPDecoder::new(reader)
            .and_then(|mut d| {
                d.set_limits(limits())?;
                Ok(d)
            })
            .map(AnimationDecoder::into_frames),
    }
    .map_err(image_error)?;
    for (index, frame) in frames.enumerate() {
        let frame = match frame {
            Ok(frame) => frame,
            Err(e) if index == 0 => return Err(image_error(e)),
            Err(_) => break,
        };
        let (numer, denom) = frame.delay().numer_denom_ms();
        let ms = numer / denom.max(1);
        let delay = Duration::from_millis(u64::from(if ms < 20 { 100 } else { ms }));
        let buffer = frame.into_buffer();
        let (width, height) = buffer.dimensions();
        check_animation(width.into(), height.into())?;
        if !each(Frame { image: Image { width, height, pixels: premultiplied_bgra(buffer.as_raw(), 4)? }, delay }) {
            break;
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;

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
        assert_eq!(premultiplied_bgra(&[255, 0, 0, 128], 4).unwrap(), [0, 0, 128, 128]);
        assert_eq!(premultiplied_bgra(&[10, 20, 30], 3).unwrap(), [30, 20, 10, 255]);
        assert_eq!(premultiplied_bgra(&[90, 51], 2).unwrap(), [18, 18, 18, 51]);
    }

    fn temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("flick-crafted-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// Crafted animations claiming huge canvases are refused from their
    /// headers, before the image crate allocates a frame.
    #[test]
    fn refuses_huge_animation_canvases() {
        let mut ihdr = vec![0u8; 13];
        ihdr[..4].copy_from_slice(&100_000u32.to_be_bytes());
        ihdr[4..8].copy_from_slice(&100_000u32.to_be_bytes());
        ihdr[8] = 8;
        ihdr[9] = 6;
        let png = [&b"\x89PNG\r\n\x1a\n"[..], &chunk(b"IHDR", &ihdr), &chunk(b"acTL", &[0, 0, 0, 3, 0, 0, 0, 0]), &chunk(b"IDAT", &[])].concat();
        let path = temp("huge.png", &png);
        assert_eq!(kind(&path), Kind::Apng(3));
        let result = frames(&path, Kind::Apng(3), |_| true);
        let _ = std::fs::remove_file(&path);
        assert!(matches!(result, Err(Error::TooLarge(100_000, 100_000))), "{result:?}");

        let body = [&b"VP8X"[..], &10u32.to_le_bytes(), &[0x02, 0, 0, 0], &[0xFF, 0xFF, 0xFF], &[0xFF, 0xFF, 0xFF], b"ANMF", &[0; 4], b"ANMF", &[0; 4]].concat();
        let webp = [&b"RIFF"[..], &(body.len() as u32 + 4).to_le_bytes(), b"WEBP", &body].concat();
        let path = temp("huge.webp", &webp);
        let result = frames(&path, kind(&path), |_| true);
        let _ = std::fs::remove_file(&path);
        assert!(matches!(result, Err(Error::TooLarge(16_777_216, 16_777_216))), "{result:?}");
    }

    #[test]
    fn webp_chunk_lengths_do_not_overflow() {
        let body = [&b"VP8X"[..], &10u32.to_le_bytes(), &[0x02; 10], b"ANMF", &u32::MAX.to_le_bytes()].concat();
        let webp = [&b"RIFF"[..], &(body.len() as u32 + 4).to_le_bytes(), b"WEBP", &body].concat();
        let path = temp("overflow.webp", &webp);
        let _ = kind(&path);
        let _ = std::fs::remove_file(&path);
    }

    fn svg(name: &str, text: &str) -> failure::Result<Image> {
        let path = temp(name, text.as_bytes());
        let result = Svg::open(&path).and_then(|svg| svg.render((64, 64)));
        let _ = std::fs::remove_file(&path);
        result
    }

    /// SVGs are images: nothing outside the file loads, and drawings that
    /// would overflow the stack or expand without end are refused.
    #[test]
    fn svg_is_drawn_as_an_image_only() {
        // A file beside it is not read (nor would a \\server\share path be).
        let red = temp("red.png", &{
            let mut png = Vec::new();
            image::codecs::png::PngEncoder::new(&mut png).write_image(&[255, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8).unwrap();
            png
        });
        let name = red.file_name().unwrap().to_string_lossy().into_owned();
        let drawn = svg("img.svg", &format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><image href="{name}" width="64" height="64"/></svg>"#)).unwrap();
        let _ = std::fs::remove_file(&red);
        assert!(drawn.pixels.iter().all(|&b| b == 0), "the neighbouring file stayed unread");
        // An embedded picture draws.
        let data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==";
        let drawn = svg("data.svg", &format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><image href="data:image/png;base64,{data}" width="64" height="64"/></svg>"#)).unwrap();
        assert!(drawn.pixels.iter().any(|&b| b != 0), "a data: picture is drawn");

        let deep = format!(r#"<svg xmlns="http://www.w3.org/2000/svg">{}{}</svg>"#, "<g>".repeat(100_000), "</g>".repeat(100_000));
        assert!(matches!(svg("deep.svg", &deep), Err(Error::Complex(_))));

        let mut bomb = String::from(r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><defs><rect id="l0" width="1" height="1"/>"#);
        for i in 1..40 {
            bomb += &format!(r##"<g id="l{i}"><use xlink:href="#l{0}"/><use xlink:href="#l{0}"/></g>"##, i - 1);
        }
        bomb += r##"</defs><use xlink:href="#l39"/></svg>"##;
        assert!(matches!(svg("bomb.svg", &bomb), Err(Error::Complex(_))));

        let cycle = r##"<svg xmlns="http://www.w3.org/2000/svg"><g id="a"><use href="#a"/></g></svg>"##;
        assert!(svg("cycle.svg", cycle).is_ok());
    }

    #[test]
    fn svgz_bombs_stop_at_the_limit() {
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(b"<svg xmlns=\"http://www.w3.org/2000/svg\">").unwrap();
        let zeros = vec![b' '; 1 << 20];
        for _ in 0..80 {
            gz.write_all(&zeros).unwrap();
        }
        let path = temp("bomb.svgz", &gz.finish().unwrap());
        let result = Svg::open(&path);
        let _ = std::fs::remove_file(&path);
        assert!(matches!(result, Err(Error::Complex(_))));
    }

    /// A PNG cut short keeps the rows before the cut.
    #[test]
    fn salvages_the_top_of_a_truncated_png() {
        let (w, h) = (64u32, 64u32);
        let rgba: Vec<u8> = (0..w * h).flat_map(|i| [(i % 251) as u8, (i / 7) as u8, 200, 255]).collect();
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new_with_quality(&mut png, image::codecs::png::CompressionType::Fast, image::codecs::png::FilterType::NoFilter)
            .write_image(&rgba, w, h, image::ExtendedColorType::Rgba8)
            .unwrap();
        let path = temp("cut.png", &png[..png.len() * 2 / 3]);
        let image = salvage_png(&path);
        // Shown either way (WIC shows the top of a cut PNG itself), and marked damaged.
        let picture = crate::decode::decode_screen(&path, (1920, 1080));
        let _ = std::fs::remove_file(&path);
        assert!(picture.expect("shown").damaged, "a PNG without IEND is marked damaged");
        let whole = temp("whole.png", &png);
        let picture = crate::decode::decode_screen(&whole, (1920, 1080));
        let _ = std::fs::remove_file(&whole);
        assert!(!picture.unwrap().damaged);
        let image = image.expect("the rows before the cut");
        assert_eq!((image.width, image.height), (w, h));
        assert_eq!(image.pixels[..4], [200, 0, 0, 255], "first pixel, BGRA");
        assert_eq!(image.pixels[image.pixels.len() - 4..], [0, 0, 0, 0], "the missing rows are transparent");
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
