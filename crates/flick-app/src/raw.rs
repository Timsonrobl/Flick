//! Camera RAW files: the JPEGs a camera stores inside them, found without
//! decoding anything (a few small reads). Most TIFF-based RAWs (CR2, NEF,
//! ARW, DNG, PEF, RW2, ORF...) point at theirs from their IFDs; CR3 is an
//! ISO media file with the JPEG as its first track; RAF gives its JPEG's
//! place in its header. Each candidate is checked by its frame header, so
//! the lossless-JPEG sensor data some RAWs also store is never taken.

use std::fs::File;
use std::os::windows::fs::FileExt;
use std::path::Path;

/// RAW extensions Flick opens: the embedded JPEG where there is one large
/// enough, else Windows' own RAW decoder (the Raw Image Extension).
pub const EXTENSIONS: &[&str] = &["cr2", "cr3", "nef", "nrw", "arw", "srf", "sr2", "dng", "orf", "pef", "rw2", "rwl", "raf", "srw", "3fr", "iiq", "erf"];

pub fn is_raw(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| EXTENSIONS.iter().any(|known| known.eq_ignore_ascii_case(e)))
}

/// A baseline or progressive JPEG stored at `offset`, `len` bytes long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Jpeg {
    pub offset: u64,
    pub len: u64,
    pub size: (u32, u32),
}

/// What a RAW file holds that a viewer can show without demosaicing.
#[derive(Debug, Default)]
pub struct Contents {
    pub jpegs: Vec<Jpeg>,
    /// EXIF orientation of the RAW itself (the JPEGs often carry none).
    pub orientation: Option<u16>,
}

impl Contents {
    /// The largest JPEG, by pixel count.
    pub fn largest(&self) -> Option<Jpeg> {
        self.jpegs.iter().copied().max_by_key(|j| u64::from(j.size.0) * u64::from(j.size.1))
    }
}

/// The JPEGs inside a RAW file, and its orientation. None: not a RAW file
/// this understands (or unreadable).
pub fn contents(path: &Path) -> Option<Contents> {
    let file = Bytes::open(path)?;
    let head = file.read(0, 16)?;
    let mut found = Contents::default();
    let mut candidates = Vec::new();
    if head.starts_with(b"FUJIFILMCCD-RAW") {
        // The JPEG's offset and length, big-endian, at 84 and 88.
        let at = file.read(84, 8)?;
        candidates.push((u64::from(be32(&at, 0)), u64::from(be32(&at, 4))));
    } else if &head[4..8] == b"ftyp" {
        cr3(&file, 0, file.len, &mut candidates, &mut found.orientation, 0);
    } else {
        let tiff = Tiff::at(&file, 0, &head)?;
        let mut budget = 64;
        tiff.ifd(u64::from(tiff.u32(&head, 4)), Ifd::Zeroth, &mut candidates, &mut found.orientation, &mut budget);
    }
    candidates.sort_unstable();
    candidates.dedup();
    found.jpegs = candidates.into_iter().filter_map(|(offset, len)| Some(Jpeg { offset, len, size: jpeg_size(&file, offset, len)? })).collect();
    Some(found)
}

/// Positional reads of a file.
struct Bytes {
    file: File,
    len: u64,
}

impl Bytes {
    fn open(path: &Path) -> Option<Bytes> {
        let file = File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        Some(Bytes { file, len })
    }

    /// Exactly `n` bytes at `at`; None past the end.
    fn read(&self, at: u64, n: usize) -> Option<Vec<u8>> {
        if at.checked_add(n as u64)? > self.len {
            return None;
        }
        let mut buf = vec![0u8; n];
        let mut done = 0;
        while done < n {
            match self.file.seek_read(&mut buf[done..], at + done as u64) {
                Ok(0) | Err(_) => return None,
                Ok(k) => done += k,
            }
        }
        Some(buf)
    }
}

fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// The size of the JPEG at `offset` from its frame header: 8-bit baseline,
/// extended or progressive, grey or colour. None for anything else
/// (lossless JPEG sensor data, not a JPEG at all).
fn jpeg_size(file: &Bytes, offset: u64, len: u64) -> Option<(u32, u32)> {
    if len < 4 || file.read(offset, 2)? != [0xFF, 0xD8] {
        return None;
    }
    let end = offset.checked_add(len)?.min(file.len);
    let mut at = offset + 2;
    for _ in 0..64 {
        let marker = file.read(at, 4)?;
        if marker[0] != 0xFF {
            return None;
        }
        if marker[1] == 0xFF {
            at += 1; // fill byte
            continue;
        }
        match marker[1] {
            0xC0..=0xC2 => {
                let frame = file.read(at + 4, 6)?;
                let (height, width) = (u32::from(be16(&frame, 1)), u32::from(be16(&frame, 3)));
                return (frame[0] == 8 && matches!(frame[5], 1 | 3) && width > 0 && height > 0).then_some((width, height));
            }
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF | 0xDA | 0xD9 => return None,
            _ => at += 2 + u64::from(be16(&marker, 2)),
        }
        if at >= end {
            return None;
        }
    }
    None
}

/// Which IFD is being read, for the tags that mean something only there.
#[derive(Clone, Copy, PartialEq)]
enum Ifd {
    /// IFD0: the RAW's orientation.
    Zeroth,
    Other,
    /// An Olympus maker note's main IFD, and its camera settings IFD
    /// (preview start and length, tags 0x101 and 0x102).
    Olympus,
    OlympusCamera,
}

/// A TIFF structure: offsets in it are from `base`.
struct Tiff<'a> {
    file: &'a Bytes,
    base: u64,
    little: bool,
}

impl<'a> Tiff<'a> {
    /// The TIFF header at `base` (its first bytes in `head`): II or MM. The
    /// magic number isn't checked (ORF and RW2 use their own).
    fn at(file: &'a Bytes, base: u64, head: &[u8]) -> Option<Tiff<'a>> {
        match head.get(0..2)? {
            b"II" => Some(Tiff { file, base, little: true }),
            b"MM" => Some(Tiff { file, base, little: false }),
            _ => None,
        }
    }

    fn u16(&self, b: &[u8], o: usize) -> u16 {
        let v = [b[o], b[o + 1]];
        if self.little { u16::from_le_bytes(v) } else { u16::from_be_bytes(v) }
    }

    fn u32(&self, b: &[u8], o: usize) -> u32 {
        let v = [b[o], b[o + 1], b[o + 2], b[o + 3]];
        if self.little { u32::from_le_bytes(v) } else { u32::from_be_bytes(v) }
    }

    /// An entry's values (BYTE, SHORT, LONG or IFD), inline or where its
    /// offset points; at most 64.
    fn values(&self, entry: &[u8]) -> Vec<u32> {
        let (kind, count) = (self.u16(entry, 2), self.u32(entry, 4).min(64) as usize);
        let width = match kind {
            1 | 7 => 1,
            3 => 2,
            4 | 13 => 4,
            _ => return Vec::new(),
        };
        let data = if width * count <= 4 {
            entry[8..12].to_vec()
        } else {
            match self.file.read(self.base + u64::from(self.u32(entry, 8)), width * count) {
                Some(data) => data,
                None => return Vec::new(),
            }
        };
        (0..count)
            .map(|i| match width {
                1 => u32::from(data[i]),
                2 => u32::from(self.u16(&data, i * 2)),
                _ => self.u32(&data, i * 4),
            })
            .collect()
    }

    /// Reads the IFD at `at`, its sub-IFDs and the IFDs chained after it,
    /// adding (offset, length) of each possible JPEG to `out`. `budget`
    /// caps how many IFDs are read (a looping file stops).
    fn ifd(&self, at: u64, kind: Ifd, out: &mut Vec<(u64, u64)>, orientation: &mut Option<u16>, budget: &mut u32) {
        if at == 0 || *budget == 0 {
            return;
        }
        *budget -= 1;
        let Some(count) = self.file.read(self.base + at, 2).map(|c| usize::from(self.u16(&c, 0)).min(1000)) else { return };
        let Some(body) = self.file.read(self.base + at + 2, count * 12 + 4) else { return };
        let entries: Vec<&[u8]> = body[..count * 12].chunks_exact(12).collect();
        let find = |tag: u16| entries.iter().find(|e| self.u16(e, 0) == tag).copied();
        let one = |tag: u16| find(tag).map(|e| self.values(e)).filter(|v| v.len() == 1).map(|v| v[0]);
        let mut add = |offset: u32, len: u32| out.push((self.base + u64::from(offset), u64::from(len)));

        if kind == Ifd::OlympusCamera {
            if let (Some(offset), Some(len)) = (one(0x101), one(0x102)) {
                add(offset, len);
            }
            return;
        }
        if kind == Ifd::Olympus {
            if let Some(camera) = one(0x2020) {
                self.ifd(u64::from(camera), Ifd::OlympusCamera, out, orientation, budget);
            }
            return;
        }
        // JPEGInterchangeFormat and its length.
        if let (Some(offset), Some(len)) = (one(513), one(514)) {
            add(offset, len);
        }
        // A JPEG-compressed image in one strip.
        if matches!(one(259), Some(6 | 7 | 34892)) {
            if let (Some(offset), Some(len)) = (one(273), one(279)) {
                add(offset, len);
            }
        }
        // Panasonic's JpgFromRaw: the JPEG is the entry's own data.
        if let Some(e) = find(0x2E).filter(|e| self.u16(e, 2) == 7) {
            add(self.u32(e, 8), self.u32(e, 4));
        }
        if kind == Ifd::Zeroth {
            *orientation = one(274).map(|o| o as u16).filter(|o| (1..=8).contains(o));
        }
        for sub in find(330).map(|e| self.values(e)).unwrap_or_default() {
            self.ifd(u64::from(sub), Ifd::Other, out, orientation, budget);
        }
        if let Some(exif) = one(34665) {
            self.ifd(u64::from(exif), Ifd::Other, out, orientation, budget);
        }
        if let Some(note) = find(37500) {
            self.maker_note(self.base + u64::from(self.u32(note, 8)), out, orientation, budget);
        }
        let next = u64::from(self.u32(&body, count * 12));
        if next != at {
            self.ifd(next, Ifd::Other, out, orientation, budget);
        }
    }

    /// An Olympus or OM System maker note at `at` (in the file): a TIFF
    /// structure of its own, offsets from its start.
    fn maker_note(&self, at: u64, out: &mut Vec<(u64, u64)>, orientation: &mut Option<u16>, budget: &mut u32) {
        let Some(head) = self.file.read(at, 16) else { return };
        let start = if head.starts_with(b"OLYMPUS\0") {
            8
        } else if head.starts_with(b"OM SYSTEM\0\0\0") {
            12
        } else {
            return;
        };
        let Some(note) = Tiff::at(self.file, at, &head[start..]) else { return };
        note.ifd(start as u64 + 4, Ifd::Olympus, out, orientation, budget);
    }
}

/// Canon's CR3 (ISO base media boxes), from `start` to `end`: each
/// track's first sample (the first is the full-size JPEG), the PRVW
/// preview, and the orientation from CMT1 (a TIFF IFD0).
fn cr3(file: &Bytes, start: u64, end: u64, out: &mut Vec<(u64, u64)>, orientation: &mut Option<u16>, depth: u32) {
    const CANON: [u8; 16] = [0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b, 0x6a, 0x48];
    const PREVIEW: [u8; 16] = [0xea, 0xf4, 0x2b, 0x5e, 0x1c, 0x98, 0x4b, 0x88, 0xb9, 0xfb, 0xb7, 0xdc, 0x40, 0x6e, 0x4d, 0x16];
    if depth > 6 {
        return;
    }
    let (mut at, mut size, mut offset) = (start, None, None);
    while at + 8 <= end {
        let Some(head) = file.read(at, 16.min((end - at) as usize)) else { return };
        let (mut len, kind, mut data) = (u64::from(be32(&head, 0)), [head[4], head[5], head[6], head[7]], at + 8);
        if len == 1 && head.len() >= 16 {
            len = u64::from_be_bytes(head[8..16].try_into().unwrap());
            data = at + 16;
        } else if len == 0 {
            len = end - at;
        }
        if len < 8 || at + len > end {
            return;
        }
        let box_end = at + len;
        match &kind {
            b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" => cr3(file, data, box_end, out, orientation, depth + 1),
            b"uuid" => match file.read(data, 16) {
                Some(id) if id == CANON => cr3(file, data + 16, box_end, out, orientation, depth + 1),
                Some(id) if id == PREVIEW => cr3(file, data + 24, box_end, out, orientation, depth + 1),
                _ => {}
            },
            b"CMT1" => {
                let head = file.read(data, 8).unwrap_or_default();
                if let Some(tiff) = Tiff::at(file, data, &head) {
                    tiff.ifd(u64::from(tiff.u32(&head, 4)), Ifd::Zeroth, &mut Vec::new(), orientation, &mut 1);
                }
            }
            b"PRVW" => {
                // A small header, then the JPEG to the end of the box.
                if let Some(bytes) = file.read(data, 64.min((box_end - data) as usize)) {
                    if let Some(i) = bytes.windows(3).position(|w| w == [0xFF, 0xD8, 0xFF]) {
                        out.push((data + i as u64, box_end - data - i as u64));
                    }
                }
            }
            b"stsz" => size = file.read(data + 4, 12).map(|b| if be32(&b, 0) != 0 { be32(&b, 0) } else { be32(&b, 8) }),
            b"co64" => offset = file.read(data + 8, 8).map(|b| u64::from_be_bytes(b.try_into().unwrap())),
            b"stco" => offset = file.read(data + 8, 4).map(|b| u64::from(be32(&b, 0))),
            _ => {}
        }
        at = box_end;
    }
    if let (Some(size), Some(offset)) = (size, offset) {
        out.push((offset, u64::from(size)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JPEG's first bytes up to its frame header: `sof` (0xC0 baseline,
    /// 0xC3 lossless), `width` × `height`, three components.
    fn jpeg_head(sof: u8, width: u16, height: u16) -> Vec<u8> {
        let mut b = vec![0xFF, 0xD8, 0xFF, 0xE1, 0, 4, 0, 0, 0xFF, sof, 0, 17, 8];
        b.extend_from_slice(&height.to_be_bytes());
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        b
    }

    /// A little-endian TIFF: IFD0 with orientation 6 and a 640 × 480 JPEG
    /// (513/514); a sub-IFD with a 4000 × 3000 JPEG strip (compression 6),
    /// and another with lossless sensor data, which must not be taken.
    #[test]
    fn finds_jpegs_in_a_tiff_raw() {
        let small = jpeg_head(0xC0, 640, 480);
        let large = jpeg_head(0xC0, 4000, 3000);
        let sensor = jpeg_head(0xC3, 6000, 4000);
        let entry = |tag: u16, kind: u16, value: u32| [&tag.to_le_bytes()[..], &kind.to_le_bytes(), &1u32.to_le_bytes(), &value.to_le_bytes()].concat();
        let ifd = |entries: &[Vec<u8>]| [&(entries.len() as u16).to_le_bytes()[..], &entries.concat(), &0u32.to_le_bytes()].concat();
        // Layout: header, IFD0 (4 entries), two sub-IFDs (3 entries), their offsets, then the JPEGs.
        let ifd0 = 8u32;
        let sub1 = ifd0 + 2 + 4 * 12 + 4;
        let sub2 = sub1 + 2 + 3 * 12 + 4;
        let subs = sub2 + 2 + 3 * 12 + 4;
        let data = subs + 8;
        let (a, b, c) = (data, data + small.len() as u32, data + (small.len() + large.len()) as u32);
        let mut file = b"II*\0".to_vec();
        file.extend_from_slice(&ifd0.to_le_bytes());
        file.extend(ifd(&[entry(274, 3, 6), entry(330, 4, 0), entry(513, 4, a), entry(514, 4, small.len() as u32)]));
        // SubIFDs: two offsets, so stored at `subs`.
        let at = 8 + 2 + 12;
        file[at..at + 12].copy_from_slice(&[&330u16.to_le_bytes()[..], &4u16.to_le_bytes(), &2u32.to_le_bytes(), &subs.to_le_bytes()].concat());
        file.extend(ifd(&[entry(259, 3, 6), entry(273, 4, b), entry(279, 4, large.len() as u32)]));
        file.extend(ifd(&[entry(259, 3, 7), entry(273, 4, c), entry(279, 4, sensor.len() as u32)]));
        assert_eq!(file.len() as u32, subs);
        file.extend([sub1.to_le_bytes(), sub2.to_le_bytes()].concat());
        file.extend([small, large, sensor].concat());

        let path = std::env::temp_dir().join(format!("flick-test-{}.nef", std::process::id()));
        std::fs::write(&path, &file).unwrap();
        let found = contents(&path);
        let _ = std::fs::remove_file(&path);
        let found = found.expect("a TIFF RAW");
        assert_eq!(found.orientation, Some(6));
        let sizes: Vec<_> = found.jpegs.iter().map(|j| j.size).collect();
        assert_eq!(sizes, [(640, 480), (4000, 3000)], "both JPEGs, not the lossless data");
        assert_eq!(found.largest().map(|j| (j.offset, j.size)), Some((u64::from(b), (4000, 3000))));
    }

    /// The RAW samples (`bench/samples/raw`, public files from
    /// raw.pixls.us): every embedded JPEG found, the orientation, and what
    /// Windows' RAW decoder makes of the file, with times.
    /// `cargo test --release -p flick-app raw_report -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn raw_report() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/samples/raw");
        let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok().map(|e| e.path())).collect();
        files.sort();
        for file in files {
            let name = file.file_name().unwrap().to_string_lossy().into_owned();
            let started = std::time::Instant::now();
            let contents = contents(&file);
            let parse_ms = started.elapsed().as_secs_f64() * 1000.0;
            println!("\n{name}: parsed in {parse_ms:.1} ms, orientation {:?}", contents.as_ref().and_then(|c| c.orientation));
            for jpeg in contents.iter().flat_map(|c| &c.jpegs) {
                println!("  JPEG {}x{} at {} ({:.1} MB)", jpeg.size.0, jpeg.size.1, jpeg.offset, jpeg.len as f64 / 1048576.0);
            }
            let started = std::time::Instant::now();
            let size = unsafe {
                use windows::Win32::Graphics::Imaging::*;
                let factory = crate::decode::factory().unwrap();
                factory
                    .CreateDecoderFromFilename(&windows::core::HSTRING::from(file.as_os_str()), None, windows::Win32::Foundation::GENERIC_READ, WICDecodeMetadataCacheOnDemand)
                    .and_then(|d| d.GetFrame(0))
                    .and_then(|f| {
                        let (mut w, mut h) = (0, 0);
                        f.GetSize(&mut w, &mut h).map(|_| (w, h))
                    })
            };
            println!("  WIC open and size: {size:?} in {:.1} ms", started.elapsed().as_secs_f64() * 1000.0);
            let started = std::time::Instant::now();
            let full = crate::decode::decode_full(&file);
            let full_ms = started.elapsed().as_secs_f64() * 1000.0;
            match full {
                Ok(image) => println!("  WIC full: {}x{} in {full_ms:.0} ms", image.width, image.height),
                Err(e) => println!("  WIC full: ERROR {e}"),
            }
        }
    }
}
