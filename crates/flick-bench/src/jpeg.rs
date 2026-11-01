//! Minimal JPEG marker walking: dimensions and the embedded EXIF thumbnail,
//! without decoding anything, so it must stay cheap: what a header probe
//! before decoding would cost (ARCHITECTURE.md §6; not built into Flick).

/// Marker segments before start-of-scan, as `(marker, payload)`.
fn segments(data: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut pos = if data.starts_with(&[0xFF, 0xD8]) { 2 } else { data.len() };
    std::iter::from_fn(move || {
        loop {
            if pos + 4 > data.len() || data[pos] != 0xFF {
                return None;
            }
            let marker = data[pos + 1];
            match marker {
                0xFF => pos += 1, // fill byte
                0x01 | 0xD0..=0xD8 => pos += 2, // markers without a length
                0xDA | 0xD9 => return None, // start of scan / end of image
                _ => {
                    let len = usize::from(u16::from_be_bytes([data[pos + 2], data[pos + 3]]));
                    if len < 2 || pos + 2 + len > data.len() {
                        return None;
                    }
                    let payload = &data[pos + 4..pos + 2 + len];
                    pos += 2 + len;
                    return Some((marker, payload));
                }
            }
        }
    })
}

/// Width and height from the first start-of-frame segment.
fn start_of_frame(data: &[u8]) -> Option<(u8, &[u8])> {
    segments(data).find(|(m, _)| matches!(m, 0xC0..=0xCF) && !matches!(m, 0xC4 | 0xC8 | 0xCC))
}

pub fn dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let (_, sof) = start_of_frame(data)?;
    let height = u16::from_be_bytes([*sof.get(1)?, *sof.get(2)?]);
    let width = u16::from_be_bytes([*sof.get(3)?, *sof.get(4)?]);
    Some((u32::from(width), u32::from(height)))
}

/// How the file is encoded, e.g. "baseline 4:2:0 thumb": coding process,
/// chroma subsampling, restart markers ("rst", which allow parallel decoding)
/// and an embedded EXIF thumbnail. Explains decode-time differences.
pub fn format(data: &[u8]) -> Option<String> {
    let (marker, sof) = start_of_frame(data)?;
    let coding = match marker {
        0xC2 | 0xC6 | 0xCA | 0xCE => "progressive",
        0xC3 | 0xC7 | 0xCB | 0xCF => "lossless",
        _ => "baseline",
    };
    let sampling = |i: usize| sof.get(6 + i * 3 + 1).map(|s| (s >> 4, s & 0x0F));
    let chroma = match sof.get(5)? {
        1 => "gray".to_string(),
        3 => match (sampling(0)?, sampling(1)?) {
            ((1, 1), (1, 1)) => "4:4:4".into(),
            ((2, 1), (1, 1)) => "4:2:2".into(),
            ((2, 2), (1, 1)) => "4:2:0".into(),
            ((1, 2), (1, 1)) => "4:4:0".into(),
            ((4, 1), (1, 1)) => "4:1:1".into(),
            ((h, v), (ch, cv)) => format!("Y{h}x{v}/C{ch}x{cv}"),
        },
        n => format!("{n}ch"),
    };
    let restart = segments(data).any(|(m, p)| m == 0xDD && p.len() >= 2 && u16::from_be_bytes([p[0], p[1]]) > 0);
    let mut out = format!("{coding} {chroma}");
    if restart {
        out.push_str(" rst");
    }
    if exif_thumbnail(data).is_some() {
        out.push_str(" thumb");
    }
    Some(out)
}

/// The JPEG thumbnail stored in IFD1 of the EXIF segment, borrowed from `data`.
pub fn exif_thumbnail(data: &[u8]) -> Option<&[u8]> {
    let (_, app1) = segments(data).find(|(m, p)| *m == 0xE1 && p.starts_with(b"Exif\0\0"))?;
    let tiff = &app1[6..];
    let exif = exif::Reader::new().read_raw(tiff.to_vec()).ok()?;
    let field = |tag| exif.get_field(tag, exif::In::THUMBNAIL)?.value.get_uint(0);
    let offset = field(exif::Tag::JPEGInterchangeFormat)? as usize;
    let len = field(exif::Tag::JPEGInterchangeFormatLength)? as usize;
    tiff.get(offset..offset.checked_add(len)?)
}

/// An APP1 segment holding a minimal little-endian EXIF block whose IFD1
/// carries `thumbnail` (a JPEG). Used by the sample generator.
pub fn exif_app1_with_thumbnail(thumbnail: &[u8]) -> Vec<u8> {
    const IFD0: u32 = 8;
    const IFD1: u32 = IFD0 + 2 + 12 + 4;
    const THUMB: u32 = IFD1 + 2 + 3 * 12 + 4;

    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II\x2A\x00");
    tiff.extend_from_slice(&IFD0.to_le_bytes());
    let entry = |tiff: &mut Vec<u8>, tag: u16, kind: u16, value: u32| {
        tiff.extend_from_slice(&tag.to_le_bytes());
        tiff.extend_from_slice(&kind.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&value.to_le_bytes());
    };
    const SHORT: u16 = 3;
    const LONG: u16 = 4;

    tiff.extend_from_slice(&1u16.to_le_bytes());
    entry(&mut tiff, 0x0112, SHORT, 1); // Orientation: normal
    tiff.extend_from_slice(&IFD1.to_le_bytes());

    tiff.extend_from_slice(&3u16.to_le_bytes());
    entry(&mut tiff, 0x0103, SHORT, 6); // Compression: JPEG
    entry(&mut tiff, 0x0201, LONG, THUMB); // JPEGInterchangeFormat
    entry(&mut tiff, 0x0202, LONG, thumbnail.len() as u32); // JPEGInterchangeFormatLength
    tiff.extend_from_slice(&0u32.to_le_bytes());
    debug_assert_eq!(tiff.len(), THUMB as usize);
    tiff.extend_from_slice(thumbnail);

    let len = u16::try_from(2 + 6 + tiff.len()).expect("EXIF thumbnail too large for one APP1 segment");
    let mut app1 = vec![0xFF, 0xE1];
    app1.extend_from_slice(&len.to_be_bytes());
    app1.extend_from_slice(b"Exif\0\0");
    app1.extend_from_slice(&tiff);
    app1
}
