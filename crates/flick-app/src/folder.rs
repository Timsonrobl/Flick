//! The folder being paged through: its image files in Explorer-like
//! "natural" order (img2 before img10). Listed on a background thread after
//! the first picture is on screen (ARCHITECTURE.md §12).

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

/// Extensions WIC can usually decode (some need Store codec extensions).
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "jpe", "jfif", "png", "bmp", "gif", "tif", "tiff", "webp", "heic", "heif", "avif", "jxl"];

pub struct Folder {
    pub files: Vec<PathBuf>,
    pub index: usize,
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.iter().any(|known| known.eq_ignore_ascii_case(e)))
}

/// Lists the images next to `file` and finds `file` among them.
pub fn list(file: &Path) -> Result<Folder, String> {
    let file = std::path::absolute(file).map_err(|e| e.to_string())?;
    let dir = file.parent().ok_or("file has no folder")?;
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .map(|entry| entry.path())
        .filter(|path| is_image(path))
        .collect();
    files.sort_by(|a, b| natural_cmp(&a.file_name().unwrap_or_default().to_string_lossy(), &b.file_name().unwrap_or_default().to_string_lossy()));
    let name = file.file_name().ok_or("no file name")?.to_string_lossy().to_lowercase();
    let index = match files.iter().position(|p| p.file_name().is_some_and(|n| n.to_string_lossy().to_lowercase() == name)) {
        Some(i) => i,
        None => {
            // Opened a file the filter doesn't know: keep it, in order.
            let at = files.partition_point(|p| natural_cmp(&p.file_name().unwrap_or_default().to_string_lossy(), &name) == Ordering::Less);
            files.insert(at, file.clone());
            at
        }
    };
    Ok(Folder { files, index })
}

/// Case-insensitive, with digit runs compared as numbers.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut digits = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        digits.push(c);
                        it.next();
                    }
                    digits
                };
                let (da, db) = (take(&mut a), take(&mut b));
                let (ta, tb) = (da.trim_start_matches('0'), db.trim_start_matches('0'));
                let order = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb)).then_with(|| da.len().cmp(&db.len()));
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                let order = x.to_lowercase().cmp(y.to_lowercase());
                if order != Ordering::Equal {
                    return order;
                }
                a.next();
                b.next();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::natural_cmp;
    use std::cmp::Ordering::*;

    #[test]
    fn numbers_compare_by_value() {
        assert_eq!(natural_cmp("img2.jpg", "img10.jpg"), Less);
        assert_eq!(natural_cmp("img10.jpg", "img2.jpg"), Greater);
        assert_eq!(natural_cmp("a(1).jpg", "a(10).jpg"), Less);
    }

    #[test]
    fn case_is_ignored() {
        assert_eq!(natural_cmp("Apple.jpg", "banana.jpg"), Less);
        assert_eq!(natural_cmp("ABC.jpg", "abc.jpg"), Equal);
    }
}
