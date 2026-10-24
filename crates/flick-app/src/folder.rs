//! The folder being paged through: its image files in the window's sort
//! order (by default Explorer-like "natural" names: img2 before img10), and
//! the window's shuffled order for random paging. Listed on a background
//! thread after the first picture is on screen (ARCHITECTURE.md §12).

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Extensions WIC can usually decode (some need Store codec extensions).
const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "apng", "bmp", "dib", "gif", "tif", "tiff", "webp", "heic", "heif", "avif", "jxl", "jxr", "wdp", "ico",
    "svg", "svgz", "exr", "hdr",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Modified,
    Size,
    /// The extension, then the name.
    Type,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
}

impl Default for Sort {
    fn default() -> Sort {
        Sort { key: SortKey::Name, descending: false }
    }
}

/// What sorting needs besides the name; read with the listing (free on
/// Windows: the directory scan returns it).
#[derive(Clone, Copy, Default)]
struct Meta {
    modified: u64,
    size: u64,
}

pub struct Folder {
    pub files: Vec<PathBuf>,
    meta: Vec<Meta>,
    pub index: usize,
    /// The order `files` are in.
    pub sort: Sort,
    /// Built on the first random step (a folder may hold 100 000 files).
    shuffle: Option<Shuffle>,
}

/// A shuffled order of the files: random forward is the next file in it,
/// random back the previous, from wherever the window is.
struct Shuffle {
    order: Vec<u32>,
    /// Each file's position in `order`.
    position: Vec<u32>,
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.iter().any(|known| known.eq_ignore_ascii_case(e)))
}

fn name(path: &Path) -> std::borrow::Cow<'_, str> {
    path.file_name().unwrap_or_default().to_string_lossy()
}

/// Lists the images next to `file`, in `sort` order, and finds `file`.
pub fn list(file: &Path, sort: Sort) -> Result<Folder, String> {
    let file = std::path::absolute(file).map_err(|e| e.to_string())?;
    let dir = file.parent().ok_or("file has no folder")?;
    let (mut files, mut meta): (Vec<PathBuf>, Vec<Meta>) = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()) && is_image(&entry.path()))
        .map(|entry| {
            let meta = entry.metadata().map_or_else(|_| Meta::default(), |m| Meta {
                modified: m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as u64),
                size: m.len(),
            });
            (entry.path(), meta)
        })
        .unzip();
    let lower = name(&file).to_lowercase();
    if !files.iter().any(|p| name(p).to_lowercase() == lower) {
        // Opened a file the filter doesn't know: keep it.
        files.push(file.clone());
        meta.push(Meta::default());
    }
    let mut folder = Folder { files, meta, index: 0, sort, shuffle: None };
    folder.sort(sort);
    folder.index = folder.files.iter().position(|p| name(p).to_lowercase() == lower).unwrap_or(0);
    Ok(folder)
}

impl Folder {
    /// Reorders the files; the current file stays current, and the shuffled
    /// order (random paging) stays the same walk.
    pub fn sort(&mut self, sort: Sort) {
        // Lowercased once, compared without allocating: 100 000 files sort
        // in tens of milliseconds (folder.rs tests, `huge_folder_timing`).
        let names: Vec<String> = self.files.iter().map(|p| name(p).to_lowercase()).collect();
        let ext = |i: usize| names[i].rsplit_once('.').map_or("", |(_, e)| e);
        let meta = &self.meta;
        let mut order: Vec<usize> = (0..names.len()).collect();
        order.sort_by(|&a, &b| {
            let by_name = || natural_cmp_lower(&names[a], &names[b]);
            let key = match sort.key {
                SortKey::Name => Ordering::Equal,
                SortKey::Modified => meta[a].modified.cmp(&meta[b].modified),
                SortKey::Size => meta[a].size.cmp(&meta[b].size),
                SortKey::Type => ext(a).cmp(ext(b)),
            };
            let order = key.then_with(by_name);
            if sort.descending { order.reverse() } else { order }
        });
        self.sort = sort;
        let mut new_index = vec![0u32; order.len()];
        for (new, &old) in order.iter().enumerate() {
            new_index[old] = new as u32;
        }
        self.files = order.iter().map(|&i| self.files[i].clone()).collect();
        self.meta = order.iter().map(|&i| self.meta[i]).collect();
        if !self.files.is_empty() {
            self.index = new_index[self.index.min(order.len() - 1)] as usize;
        }
        if let Some(shuffle) = &mut self.shuffle {
            for file in shuffle.order.iter_mut() {
                *file = new_index[*file as usize];
            }
            for (at, &file) in shuffle.order.iter().enumerate() {
                shuffle.position[file as usize] = at as u32;
            }
        }
    }

    /// The file one random step from the current one (forward or back), in
    /// the window's shuffled order seeded by `seed`.
    pub fn random_step(&mut self, forward: bool, seed: u64) -> Option<usize> {
        let n = self.files.len();
        if n < 2 {
            return None;
        }
        let shuffle = self.shuffle.get_or_insert_with(|| Shuffle::new(n, seed));
        let at = shuffle.position[self.index] as usize;
        let to = if forward { (at + 1) % n } else { (at + n - 1) % n };
        Some(shuffle.order[to] as usize)
    }

    /// The file `steps` random steps from the current one (negative: back),
    /// once random paging is in use; for preloading along the walk.
    pub fn random_at(&self, steps: i64) -> Option<usize> {
        let shuffle = self.shuffle.as_ref()?;
        let n = self.files.len() as i64;
        let at = i64::from(shuffle.position[self.index]);
        Some(shuffle.order[(at + steps).rem_euclid(n) as usize] as usize)
    }
}

/// 0..n in a shuffled order seeded by `seed`, as random paging walks files
/// (Tab Lock walks a window's tabs this way).
pub fn shuffled(n: usize, seed: u64) -> Vec<u32> {
    Shuffle::new(n, seed).order
}

impl Shuffle {
    /// Fisher–Yates with SplitMix64: ~1 ms and 8 bytes per file for 100 000.
    fn new(n: usize, seed: u64) -> Shuffle {
        let mut state = seed;
        let mut next = || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        let mut order: Vec<u32> = (0..n as u32).collect();
        for i in (1..n).rev() {
            order.swap(i, (next() % (i as u64 + 1)) as usize);
        }
        let mut position = vec![0u32; n];
        for (at, &file) in order.iter().enumerate() {
            position[file as usize] = at as u32;
        }
        Shuffle { order, position }
    }
}

/// Case-insensitive, with digit runs compared as numbers.
#[cfg(test)]
fn natural_cmp(a: &str, b: &str) -> Ordering {
    natural_cmp_lower(&a.to_lowercase(), &b.to_lowercase())
}

/// `natural_cmp` for names already lowercased; allocation-free.
fn natural_cmp_lower(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    let run = |s: &[u8], from: usize| from + s[from..].iter().take_while(|c| c.is_ascii_digit()).count();
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let (ea, eb) = (run(a, i), run(b, j));
            let (da, db) = (&a[i..ea], &b[j..eb]);
            let zeros = |d: &[u8]| d.iter().take_while(|&&c| c == b'0').count();
            let (ta, tb) = (&da[zeros(da)..], &db[zeros(db)..]);
            let order = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb)).then_with(|| da.len().cmp(&db.len()));
            if order != Ordering::Equal {
                return order;
            }
            (i, j) = (ea, eb);
        } else {
            // Bytes of UTF-8 compare in code point order.
            if a[i] != b[j] {
                return a[i].cmp(&b[j]);
            }
            (i, j) = (i + 1, j + 1);
        }
    }
    (a.len() - i).cmp(&(b.len() - j))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    fn folder(names: &[(&str, u64, u64)]) -> Folder {
        Folder {
            files: names.iter().map(|(n, _, _)| PathBuf::from(n)).collect(),
            meta: names.iter().map(|&(_, modified, size)| Meta { modified, size }).collect(),
            index: 0,
            sort: Sort::default(),
            shuffle: None,
        }
    }

    fn names(folder: &Folder) -> Vec<String> {
        folder.files.iter().map(|p| name(p).into_owned()).collect()
    }

    #[test]
    fn sorts_by_each_key_and_keeps_the_current_file() {
        let mut f = folder(&[("b10.jpg", 3, 100), ("b2.png", 1, 300), ("a.gif", 2, 200)]);
        f.index = 1; // b2.png
        f.sort(Sort { key: SortKey::Name, descending: false });
        assert_eq!(names(&f), ["a.gif", "b2.png", "b10.jpg"]);
        assert_eq!(names(&f)[f.index], "b2.png");
        f.sort(Sort { key: SortKey::Modified, descending: false });
        assert_eq!(names(&f), ["b2.png", "a.gif", "b10.jpg"]);
        f.sort(Sort { key: SortKey::Size, descending: true });
        assert_eq!(names(&f), ["b2.png", "a.gif", "b10.jpg"]);
        f.sort(Sort { key: SortKey::Type, descending: false });
        assert_eq!(names(&f), ["a.gif", "b10.jpg", "b2.png"]);
        assert_eq!(names(&f)[f.index], "b2.png");
    }

    /// Timing for a huge folder (run with --ignored --nocapture).
    #[test]
    #[ignore]
    fn huge_folder_timing() {
        let files: Vec<(String, u64, u64)> = (0..100_000u64).map(|i| (format!("IMG_{:05}_{}.jpg", (i * 7919) % 100_000, i), i * 31 % 977, i)).collect();
        let entries: Vec<(&str, u64, u64)> = files.iter().map(|(n, m, s)| (n.as_str(), *m, *s)).collect();
        let mut f = folder(&entries);
        for sort in [SortKey::Name, SortKey::Modified, SortKey::Type] {
            let started = std::time::Instant::now();
            f.sort(Sort { key: sort, descending: false });
            println!("sort {sort:?}: {:.0} ms", started.elapsed().as_secs_f64() * 1000.0);
        }
        let started = std::time::Instant::now();
        f.random_step(true, 7);
        println!("first random step (shuffle): {:.1} ms", started.elapsed().as_secs_f64() * 1000.0);
    }

    #[test]
    fn random_walk_visits_all_and_back_retraces_across_resorts() {
        let mut f = folder(&[("1", 5, 0), ("2", 4, 0), ("3", 3, 0), ("4", 2, 0), ("5", 1, 0)]);
        let mut visited = vec![names(&f)[f.index].clone()];
        for _ in 0..4 {
            f.index = f.random_step(true, 42).unwrap();
            visited.push(names(&f)[f.index].clone());
        }
        let mut all = visited.clone();
        all.sort();
        assert_eq!(all, ["1", "2", "3", "4", "5"], "every file once");
        // A re-sort mid-walk doesn't change the walk.
        f.sort(Sort { key: SortKey::Modified, descending: false });
        for expected in visited.iter().rev().skip(1) {
            f.index = f.random_step(false, 42).unwrap();
            assert_eq!(&names(&f)[f.index], expected);
        }
    }

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
