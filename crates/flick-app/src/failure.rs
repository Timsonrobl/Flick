//! Why a file can't be shown, and the limits that keep a damaged or hostile
//! file from taking the process down (ARCHITECTURE.md §7a). Every decode
//! runs inside `guarded`: a decoder's panic becomes an error for that one
//! file, pixel buffers are allocated fallibly (`pixels`), and pictures over
//! `MAX_PIXELS` are refused from their headers before anything is
//! allocated for them. A failure reaches the window as a two-line message,
//! the reason in words and the decoder's own detail (`message`).

use std::fmt::Display;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

/// The most pixels a picture may have to be decoded whole on the CPU: 1 GiB
/// as BGRA, what the tiles hold on the GPU (tiles::MAX_BYTES), ~268 MP.
/// Larger pictures only open through scaled decodes (WIC's DCT scaling and
/// Fant scaler stream them) and tiles.
pub const MAX_PIXELS: u64 = 1 << 28;

/// What went wrong, kept until it is put into words for one file.
#[derive(Debug)]
pub enum Error {
    /// WIC's own error (the reason comes from its code).
    Wic(windows::core::Error),
    /// A decoder refused the data (Flick's own or a crate's message).
    Damaged(String),
    /// Width × height over `MAX_PIXELS` (or a buffer the size can't be had).
    TooLarge(u64, u64),
    /// An SVG nested or self-referencing beyond what renders safely.
    Complex(String),
    /// Memory for its pixels couldn't be had.
    Memory,
    /// The decoder panicked.
    Crashed(String),
}

impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Error {
        Error::Wic(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// A decoder's message as an error.
pub fn damaged(detail: impl Display) -> Error {
    Error::Damaged(detail.to_string())
}

/// `w` × `h` refused if over `MAX_PIXELS` (or empty: nothing to show).
pub fn check(w: u64, h: u64) -> Result<()> {
    if w == 0 || h == 0 {
        return Err(Error::Damaged(format!("{w} × {h} pixels")));
    }
    if w.saturating_mul(h) > MAX_PIXELS { Err(Error::TooLarge(w, h)) } else { Ok(()) }
}

/// A zeroed buffer of `w` × `h` × `bytes` per pixel, or an error rather than
/// an abort when the size overflows or the memory can't be had. Zeroed by
/// the allocator (pages of zeros, as `vec![0; n]`), so it costs no more.
pub fn pixels(w: u32, h: u32, bytes: usize) -> Result<Vec<u8>> {
    let len = (w as usize).checked_mul(h as usize).and_then(|n| n.checked_mul(bytes)).ok_or(Error::TooLarge(w.into(), h.into()))?;
    zeroed(len)
}

/// `len` zero bytes, or `Error::Memory`.
pub fn zeroed(len: usize) -> Result<Vec<u8>> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let layout = std::alloc::Layout::array::<u8>(len).map_err(|_| Error::Memory)?;
    // SAFETY: a non-zero layout; the pointer, if not null, owns `len` zeroed
    // bytes from the global allocator with that layout, which is what a
    // Vec<u8> of capacity `len` frees.
    unsafe {
        let ptr = std::alloc::alloc_zeroed(layout);
        if ptr.is_null() {
            return Err(Error::Memory);
        }
        Ok(Vec::from_raw_parts(ptr, len, len))
    }
}

/// Runs a decode of `path`, turning a panic or an error into the message a
/// window shows (`message`).
pub fn guarded<T>(path: &Path, decode: impl FnOnce() -> Result<T>) -> std::result::Result<T, String> {
    let result = catch_unwind(AssertUnwindSafe(decode)).unwrap_or_else(|panic| {
        let text = panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned());
        Err(Error::Crashed(text.unwrap_or_default()))
    });
    result.map_err(|e| message(path, &e))
}

/// "Reason\ndetail": what a window shows for a file it can't show. The
/// reason is in words, from the error and a look at the file (gone, empty,
/// a format whose Windows codec isn't installed); the detail is the
/// decoder's own message, for a bug report.
pub fn message(path: &Path, error: &Error) -> String {
    let (reason, detail) = explain(path, error);
    if detail.is_empty() { reason } else { format!("{reason}\n{detail}") }
}

/// A failure message split back into its reason and detail.
pub fn parts(message: &str) -> (&str, &str) {
    message.split_once('\n').unwrap_or((message, ""))
}

const DAMAGED: &str = "The file is damaged or incomplete.";

fn explain(path: &Path, error: &Error) -> (String, String) {
    // The file itself first: gone, unreadable or empty says more than any decoder.
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ("The file can't be found.".into(), String::new()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return ("Windows denies access to the file.".into(), String::new()),
        Err(e) => return ("The file can't be read.".into(), e.to_string()),
        Ok(m) if m.is_file() && m.len() == 0 => return ("The file is empty (0 bytes).".into(), String::new()),
        Ok(_) => {}
    }
    match error {
        Error::TooLarge(w, h) => (format!("The picture is too large to open: {w} × {h} pixels."), String::new()),
        Error::Memory => ("There isn't enough memory to open the picture.".into(), String::new()),
        Error::Complex(detail) => ("The drawing is too complex to render safely.".into(), detail.clone()),
        Error::Crashed(detail) => ("The decoder failed on this file: it is damaged or malformed.".into(), detail.clone()),
        Error::Damaged(detail) => (DAMAGED.into(), detail.clone()),
        Error::Wic(e) => wic_reason(path, e),
    }
}

/// HRESULT_FROM_WIN32 codes WIC passes on from reading the file.
const NOT_FOUND: [u32; 2] = [0x8007_0002, 0x8007_0003];
const ACCESS_DENIED: u32 = 0x8007_0005;
const IN_USE: [u32; 2] = [0x8007_0020, 0x8007_0021];
const OUT_OF_MEMORY: u32 = 0x8007_000E;
/// A file whose contents are in the cloud, not on this PC (OneDrive), that
/// couldn't be fetched: ERROR_CLOUD_FILE_* (362–400).
const CLOUD: std::ops::RangeInclusive<u32> = 0x8007_016A..=0x8007_0190;

fn wic_reason(path: &Path, e: &windows::core::Error) -> (String, String) {
    use windows::Win32::Foundation::{WINCODEC_ERR_COMPONENTNOTFOUND, WINCODEC_ERR_IMAGESIZEOUTOFRANGE, WINCODEC_ERR_UNKNOWNIMAGEFORMAT};
    let code = e.code();
    let detail = format!("{} (0x{:08X})", e.message().trim(), code.0 as u32);
    let reason = match code.0 as u32 {
        c if NOT_FOUND.contains(&c) => "The file can't be found.".into(),
        ACCESS_DENIED => "Windows denies access to the file.".into(),
        c if IN_USE.contains(&c) => "Another program has the file locked.".into(),
        OUT_OF_MEMORY => "There isn't enough memory to open the picture.".into(),
        c if CLOUD.contains(&c) => "The file is online-only and couldn't be downloaded.".into(),
        _ if code == WINCODEC_ERR_IMAGESIZEOUTOFRANGE => "The picture is too large to open.".into(),
        _ if code == WINCODEC_ERR_COMPONENTNOTFOUND || code == WINCODEC_ERR_UNKNOWNIMAGEFORMAT => no_codec(path),
        _ => DAMAGED.into(),
    };
    (reason, detail)
}

/// No WIC decoder took the file: one from the Microsoft Store that isn't
/// installed, or the contents aren't a picture at all (or not the one its
/// name says).
fn no_codec(path: &Path) -> String {
    use std::io::Read;
    let mut head = [0u8; 16];
    let n = std::fs::File::open(path).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
    let head = &head[..n];
    let brand = (head.len() >= 12 && &head[4..8] == b"ftyp").then(|| &head[8..12]);
    match brand {
        Some(b"avif" | b"avis") => "Windows needs the AV1 Video Extension (Microsoft Store) to show AVIF pictures.".into(),
        Some(b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"mif1" | b"msf1") => {
            "Windows needs the HEIF Image Extensions and HEVC Video Extensions (Microsoft Store) to show HEIC pictures.".into()
        }
        _ if crate::raw::is_raw(path) => "Windows needs the Raw Image Extension (Microsoft Store) to show this camera's RAW files.".into(),
        _ if head.starts_with(&[0xFF, 0xD8]) || head.starts_with(b"\x89PNG") || head.starts_with(b"GIF8") => DAMAGED.into(),
        _ => "This isn't a picture Flick can read: its contents don't match its name.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_is_an_error_for_its_file() {
        let path = std::env::temp_dir().join(format!("flick-guard-{}.jpg", std::process::id()));
        std::fs::write(&path, b"\xFF\xD8\xFF").unwrap();
        let result: std::result::Result<(), String> = guarded(&path, || panic!("index out of bounds"));
        let _ = std::fs::remove_file(&path);
        let message = result.unwrap_err();
        assert!(message.starts_with("The decoder failed"), "{message}");
        assert!(message.ends_with("index out of bounds"), "{message}");
    }

    #[test]
    fn limits_refuse_before_allocating() {
        assert!(matches!(check(65_535, 65_535), Err(Error::TooLarge(..))));
        assert!(matches!(check(0, 10), Err(Error::Damaged(_))));
        assert!(check(30_000, 8_000).is_ok(), "240 MP opens");
        assert!(matches!(pixels(u32::MAX, u32::MAX, 4), Err(Error::TooLarge(..) | Error::Memory)));
        assert_eq!(pixels(3, 2, 4).unwrap(), vec![0; 24]);
    }

    #[test]
    fn says_why_in_words() {
        let gone = std::env::temp_dir().join("flick-no-such-file.jpg");
        assert_eq!(parts(&message(&gone, &damaged("x"))).0, "The file can't be found.");
        let empty = std::env::temp_dir().join(format!("flick-empty-{}.png", std::process::id()));
        std::fs::write(&empty, b"").unwrap();
        let text = message(&empty, &damaged("x"));
        let _ = std::fs::remove_file(&empty);
        assert_eq!(parts(&text).0, "The file is empty (0 bytes).");
        let heic = std::env::temp_dir().join(format!("flick-heic-{}.heic", std::process::id()));
        std::fs::write(&heic, b"\0\0\0\x18ftypheic\0\0\0\0").unwrap();
        let text = message(&heic, &Error::Wic(windows::Win32::Foundation::WINCODEC_ERR_COMPONENTNOTFOUND.into()));
        let _ = std::fs::remove_file(&heic);
        assert!(parts(&text).0.contains("HEIF Image Extensions"), "{text}");
        assert!(parts(&text).1.contains("0x88982F50"), "{text}");
    }
}
