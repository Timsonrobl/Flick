//! What the info panel (Tab) shows beyond the picture itself: file size and
//! date, format details, and the usual EXIF fields, read through WIC's
//! format-independent photo metadata names where they exist. Runs on a
//! worker thread (WIC there, the UI thread's COM apartment is the window's).

use std::path::Path;

use windows::Win32::Foundation::{FILETIME, GENERIC_READ, SYSTEMTIME};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows::Win32::System::Variant::*;
use windows::core::{HSTRING, Interface};

use crate::decode;
use crate::formats::{self, Kind};

/// The panel's lines after the file name and picture size, in order:
/// (label, value), only those the file has.
pub struct Info {
    /// "JPEG, 4:2:0, progressive", "PNG, 16-bit", "WebP (animated)", …
    pub format: String,
    pub lines: Vec<(&'static str, String)>,
}

pub fn read(path: &Path) -> Info {
    let mut lines = Vec::new();
    if let Ok(meta) = std::fs::metadata(path) {
        lines.push(("Size", file_size(meta.len())));
        if let Some(modified) = meta.modified().ok().and_then(local_time) {
            lines.push(("Modified", modified));
        }
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_uppercase();
    let format = match formats::kind(path) {
        Kind::Jxl => "JPEG XL".to_string(),
        Kind::Svg => "SVG".to_string(),
        Kind::Float if ext == "EXR" => "OpenEXR (HDR, tone-mapped)".to_string(),
        Kind::Float => "Radiance HDR (tone-mapped)".to_string(),
        Kind::Apng(_) => "APNG".to_string(),
        Kind::AnimatedWebp(_) => "WebP (animated)".to_string(),
        Kind::Wic => match wic(path, &mut lines) {
            Some(format) => format,
            None => ext,
        },
    };
    Info { format, lines }
}

fn file_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / f64::from(1 << 20)),
        b if b >= 1 << 10 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{b} bytes"),
    }
}

/// "2026-10-22 17:20:57" in local time.
fn local_time(time: std::time::SystemTime) -> Option<String> {
    let ticks = time.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos() / 100 + 116_444_736_000_000_000;
    let file_time = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    let (mut utc, mut local) = (SYSTEMTIME::default(), SYSTEMTIME::default());
    unsafe {
        FileTimeToSystemTime(&file_time, &mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
    }
    Some(format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute, local.wSecond))
}

/// A metadata value, as text or a number.
enum Value {
    Text(String),
    Number(f64),
}

fn get(reader: &IWICMetadataQueryReader, name: &str) -> Option<Value> {
    unsafe {
        let mut value = PROPVARIANT::default();
        reader.GetMetadataByName(&HSTRING::from(name), &mut value).ok()?;
        let inner = &value.Anonymous.Anonymous;
        let v = &inner.Anonymous;
        let result = match inner.vt {
            VT_LPWSTR => v.pwszVal.to_string().ok().map(Value::Text),
            VT_LPSTR => v.pszVal.to_string().ok().map(Value::Text),
            VT_UI1 => Some(Value::Number(f64::from(v.bVal))),
            VT_UI2 => Some(Value::Number(f64::from(v.uiVal))),
            VT_UI4 => Some(Value::Number(f64::from(v.ulVal))),
            VT_UI8 => Some(Value::Number(v.uhVal as f64)),
            VT_I2 => Some(Value::Number(f64::from(v.iVal))),
            VT_I4 => Some(Value::Number(f64::from(v.lVal))),
            VT_R4 => Some(Value::Number(f64::from(v.fltVal))),
            VT_R8 => Some(Value::Number(v.dblVal)),
            _ => None,
        };
        let _ = PropVariantClear(&mut value);
        result
    }
}

fn text(reader: &IWICMetadataQueryReader, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| match get(reader, name)? {
        Value::Text(t) => Some(t.trim().trim_end_matches('\0').trim().to_string()).filter(|t| !t.is_empty()),
        Value::Number(_) => None,
    })
}

fn number(reader: &IWICMetadataQueryReader, name: &str) -> Option<f64> {
    match get(reader, name)? {
        Value::Number(n) if n.is_finite() && n > 0.0 => Some(n),
        _ => None,
    }
}

/// WIC formats: the container's name and its details, and EXIF into `lines`.
fn wic(path: &Path, lines: &mut Vec<(&'static str, String)>) -> Option<String> {
    unsafe {
        let factory = decode::factory().ok()?;
        let decoder = factory.CreateDecoderFromFilename(&HSTRING::from(path.as_os_str()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand).ok()?;
        let frame = decoder.GetFrame(0).ok()?;
        let container = decoder.GetContainerFormat().ok()?;
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        let mut format = match container {
            c if c == GUID_ContainerFormatJpeg => "JPEG",
            c if c == GUID_ContainerFormatPng => "PNG",
            c if c == GUID_ContainerFormatGif => "GIF",
            c if c == GUID_ContainerFormatTiff => "TIFF",
            c if c == GUID_ContainerFormatBmp => "BMP",
            c if c == GUID_ContainerFormatIco => "ICO",
            c if c == GUID_ContainerFormatWebp => "WebP",
            c if c == GUID_ContainerFormatWmp => "JPEG XR",
            c if c == GUID_ContainerFormatDds => "DDS",
            c if c == GUID_ContainerFormatHeif && ext == "avif" => "AVIF",
            c if c == GUID_ContainerFormatHeif => "HEIF",
            _ => "",
        }
        .to_string();
        if format.is_empty() {
            format = ext.to_ascii_uppercase();
        }
        // JPEG: chroma subsampling and scan type, from the frame header.
        if let Ok(jpeg) = frame.cast::<IWICJpegFrameDecode>() {
            let mut header = WICJpegFrameHeader::default();
            if jpeg.GetFrameHeader(&mut header).is_ok() {
                let chroma = match header.SampleFactors {
                    WIC_JPEG_SAMPLE_FACTORS_ONE => "greyscale",
                    WIC_JPEG_SAMPLE_FACTORS_THREE_420 => "4:2:0",
                    WIC_JPEG_SAMPLE_FACTORS_THREE_422 => "4:2:2",
                    WIC_JPEG_SAMPLE_FACTORS_THREE_440 => "4:4:0",
                    WIC_JPEG_SAMPLE_FACTORS_THREE_444 => "4:4:4",
                    _ => "",
                };
                if !chroma.is_empty() {
                    format += &format!(", {chroma}");
                }
                if header.ScanType == WICJpegScanTypeProgressive {
                    format += ", progressive";
                }
            }
        }
        if let Ok(pixel) = frame.GetPixelFormat() {
            let deep = [
                GUID_WICPixelFormat16bppGray,
                GUID_WICPixelFormat48bppRGB,
                GUID_WICPixelFormat48bppBGR,
                GUID_WICPixelFormat64bppRGBA,
                GUID_WICPixelFormat64bppBGRA,
                GUID_WICPixelFormat64bppPRGBA,
                GUID_WICPixelFormat64bppPBGRA,
            ];
            if deep.contains(&pixel) {
                format += ", 16-bit";
            }
        }

        if let Ok(reader) = frame.GetMetadataQueryReader() {
            exif(&reader, lines);
        }
        Some(format)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_like_explorer() {
        assert_eq!(file_size(9_437_184), "9.0 MB");
        assert_eq!(file_size(2048), "2 KB");
        assert_eq!(file_size(512), "512 bytes");
    }

    /// Prints what the panel would show for the files in `FLICK_INFO_DIR`:
    /// `cargo test --release -p flick-app print_info -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn print_info() {
        let Some(dir) = std::env::var_os("FLICK_INFO_DIR") else { return };
        let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok().map(|e| e.path())).collect();
        files.sort();
        for file in files {
            let info = read(&file);
            println!("{}: {}", file.file_name().unwrap().to_string_lossy(), info.format);
            for (label, value) in info.lines {
                println!("    {label}: {value}");
            }
        }
    }

    /// An EXIF APP1 segment: IFD0 with `ifd0`, plus an EXIF IFD with `exif`
    /// (entries as (tag, type, value bytes), tags ascending).
    fn exif_app1(ifd0: &[(u16, u16, Vec<u8>)], exif: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
        let size = |kind: u16| match kind {
            3 => 2,
            4 => 4,
            5 => 8,
            _ => 1,
        };
        let ifd_len = |n: usize| (2 + 12 * n + 4) as u32;
        let ifd0_at = 8;
        let exif_at = ifd0_at + ifd_len(ifd0.len() + 1);
        let mut data_at = exif_at + ifd_len(exif.len());
        let (mut tiff, mut data) = (b"II\x2A\x00".to_vec(), Vec::new());
        tiff.extend_from_slice(&ifd0_at.to_le_bytes());
        let mut ifd0 = ifd0.to_vec();
        ifd0.push((0x8769, 4, exif_at.to_le_bytes().to_vec())); // the EXIF IFD's offset
        for entries in [&ifd0[..], exif] {
            tiff.extend_from_slice(&(entries.len() as u16).to_le_bytes());
            for (tag, kind, bytes) in entries {
                tiff.extend_from_slice(&tag.to_le_bytes());
                tiff.extend_from_slice(&kind.to_le_bytes());
                tiff.extend_from_slice(&((bytes.len() / size(*kind)) as u32).to_le_bytes());
                if bytes.len() <= 4 {
                    let mut inline = bytes.clone();
                    inline.resize(4, 0);
                    tiff.extend_from_slice(&inline);
                } else {
                    tiff.extend_from_slice(&data_at.to_le_bytes());
                    data.extend_from_slice(bytes);
                    data_at += bytes.len() as u32;
                }
            }
            tiff.extend_from_slice(&0u32.to_le_bytes());
        }
        tiff.extend_from_slice(&data);
        let mut app1 = vec![0xFF, 0xE1];
        app1.extend_from_slice(&((2 + 6 + tiff.len()) as u16).to_be_bytes());
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&tiff);
        app1
    }

    /// Camera EXIF, as a camera writes it, comes out as the panel's lines.
    /// Uses the synthetic sample JPEG; skipped where it isn't present.
    #[test]
    fn reads_camera_exif() {
        let sample = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/samples/synthetic-4032x3024-q90.jpg");
        let Ok(jpeg) = std::fs::read(&sample) else { return };
        let (ascii, short, rational) = (2u16, 3u16, 5u16);
        let r = |n: u32, d: u32| [n.to_le_bytes(), d.to_le_bytes()].concat();
        let app1 = exif_app1(
            &[(0x010F, ascii, b"Canon\0".to_vec()), (0x0110, ascii, b"Canon EOS R5\0".to_vec())],
            &[
                (0x829A, rational, r(1, 250)),
                (0x829D, rational, r(28, 10)),
                (0x8827, short, 200u16.to_le_bytes().to_vec()),
                (0x9003, ascii, b"2026:10:01 12:34:56\0".to_vec()),
                (0x920A, rational, r(50, 1)),
                (0xA434, ascii, b"RF50mm F1.8 STM\0".to_vec()),
            ],
        );
        let path = std::env::temp_dir().join(format!("flick-exif-{}.jpg", std::process::id()));
        std::fs::write(&path, [&jpeg[..2], &app1, &jpeg[2..]].concat()).unwrap();
        let info = read(&path);
        let _ = std::fs::remove_file(&path);
        let line = |label: &str| info.lines.iter().find(|(l, _)| *l == label).map(|(_, v)| v.as_str());
        assert_eq!(line("Camera"), Some("Canon EOS R5"), "maker not repeated");
        assert_eq!(line("Lens"), Some("RF50mm F1.8 STM"));
        assert_eq!(line("Exposure"), Some("1/250 s  f/2.8  ISO 200  50 mm"));
        assert_eq!(line("Taken"), Some("2026-10-01 12:34:56"));
    }

    /// The burst set (baseline 4:2:0 JPEGs); skipped where it isn't present.
    #[test]
    fn reads_jpeg_details() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/photos/burst");
        let Some(file) = std::fs::read_dir(dir).ok().and_then(|mut d| d.find_map(|e| e.ok().map(|e| e.path()))) else { return };
        let info = read(&file);
        assert_eq!(info.format, "JPEG, 4:2:0");
        assert!(info.lines.iter().any(|(label, _)| *label == "Size"));
        assert!(info.lines.iter().any(|(label, value)| *label == "Modified" && value.len() == 19));
    }
}

/// Camera, lens, exposure and date taken.
fn exif(reader: &IWICMetadataQueryReader, lines: &mut Vec<(&'static str, String)>) {
    let make = text(reader, &["System.Photo.CameraManufacturer"]);
    let model = text(reader, &["System.Photo.CameraModel"]);
    let camera = match (make, model) {
        // Models often repeat the maker ("Canon" + "Canon EOS R5").
        (Some(make), Some(model)) if model.to_lowercase().starts_with(&make.to_lowercase()) => Some(model),
        (Some(make), Some(model)) => Some(format!("{make} {model}")),
        (make, model) => make.or(model),
    };
    if let Some(camera) = camera {
        lines.push(("Camera", camera));
    }
    if let Some(lens) = text(reader, &["System.Photo.LensModel"]) {
        lines.push(("Lens", lens));
    }
    let mut exposure = Vec::new();
    if let Some(t) = number(reader, "System.Photo.ExposureTime") {
        exposure.push(if t < 1.0 { format!("1/{:.0} s", 1.0 / t) } else { format!("{t:.1} s") });
    }
    if let Some(f) = number(reader, "System.Photo.FNumber") {
        exposure.push(format!("f/{f:.1}").replace(".0", ""));
    }
    if let Some(iso) = number(reader, "System.Photo.ISOSpeed") {
        exposure.push(format!("ISO {iso:.0}"));
    }
    if let Some(focal) = number(reader, "System.Photo.FocalLength") {
        exposure.push(format!("{focal:.0} mm"));
    }
    if !exposure.is_empty() {
        lines.push(("Exposure", exposure.join("  ")));
    }
    // The camera's own "2026:10:22 17:20:57", as written (no time zone games).
    let taken = text(reader, &["/app1/ifd/exif/{ushort=36867}", "/ifd/exif/{ushort=36867}"]);
    if let Some(taken) = taken.filter(|t| t.len() >= 19) {
        let (date, time) = taken.split_at(10);
        lines.push(("Taken", format!("{}{}", date.replace(':', "-"), time)));
    }
}
