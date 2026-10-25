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

fn meta_of(m: &std::fs::Metadata) -> Meta {
    Meta { modified: m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as u64), size: m.len() }
}

/// Where `a` goes relative to `b` in `sort` order (names lowercased).
fn compare(sort: Sort, (a, a_meta): (&str, Meta), (b, b_meta): (&str, Meta)) -> Ordering {
    let ext = |n: &str| n.rsplit_once('.').map_or("", |(_, e)| e).to_string();
    let key = match sort.key {
        SortKey::Name => Ordering::Equal,
        SortKey::Modified => a_meta.modified.cmp(&b_meta.modified),
        SortKey::Size => a_meta.size.cmp(&b_meta.size),
        SortKey::Type => ext(a).cmp(&ext(b)),
    };
    let order = key.then_with(|| natural_cmp_lower(a, b));
    if sort.descending { order.reverse() } else { order }
}

/// A change to a watched folder (`watch.rs`), by full path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Added(PathBuf),
    Removed(PathBuf),
    Renamed(PathBuf, PathBuf),
    /// Its contents (size, date) changed.
    Modified(PathBuf),
    /// Too much changed to say what (the change buffer overflowed): list again.
    Rescan,
}

pub struct Folder {
    /// Where the files are (the folder watched for changes).
    pub dir: PathBuf,
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
        .map(|entry| (entry.path(), entry.metadata().map_or_else(|_| Meta::default(), |m| meta_of(&m))))
        .unzip();
    let lower = name(&file).to_lowercase();
    if !files.iter().any(|p| name(p).to_lowercase() == lower) {
        // Opened a file the filter doesn't know: keep it.
        files.push(file.clone());
        meta.push(Meta::default());
    }
    let mut folder = Folder { dir: dir.to_path_buf(), files, meta, index: 0, sort, shuffle: None };
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
            // As `compare`, without its per-call allocations.
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

    /// Where `path` is in the list.
    pub fn position(&self, path: &Path) -> Option<usize> {
        self.files.iter().position(|p| p == path)
    }

    /// Changes from the folder's watcher, applied in place: new images go
    /// where the sort order puts them, removed ones leave, renamed and
    /// modified ones move if the order says so. The current file stays
    /// current (a removed one is followed by its successor). Returns whether
    /// the list changed. (`Rescan` is the caller's: it lists again.)
    pub fn apply(&mut self, changes: &[Change]) -> bool {
        let mut current = self.files.get(self.index).cloned();
        let mut fallback = self.index;
        let mut changed = false;
        for change in changes {
            match change {
                Change::Added(path) => changed |= self.insert(path),
                Change::Removed(path) => {
                    if let Some(i) = self.position(path).filter(|_| self.files.len() > 1) {
                        self.remove_at(i);
                        changed = true;
                        if current.as_ref() == Some(path) {
                            current = None;
                            fallback = i;
                        }
                    }
                }
                Change::Renamed(old, new) => {
                    // The new name first, so the list never runs empty.
                    let added = self.insert(new);
                    if let Some(i) = self.position(old).filter(|_| self.files.len() > 1) {
                        self.remove_at(i);
                        changed = true;
                    }
                    if added && current.as_ref() == Some(old) {
                        current = Some(new.clone());
                    }
                    changed |= added;
                }
                Change::Modified(path) => {
                    // Its size and date changed: where it sorts may have too.
                    let Some(i) = self.position(path) else { continue };
                    let Ok(meta) = std::fs::metadata(path) else { continue };
                    self.remove_at(i);
                    self.insert_with(path, meta_of(&meta));
                    changed = true;
                }
                Change::Rescan => {}
            }
        }
        if changed {
            self.index = current.and_then(|c| self.position(&c)).unwrap_or(fallback.min(self.files.len().saturating_sub(1)));
            // The shuffle no longer matches the list: random paging starts a new one.
            self.shuffle = None;
        }
        changed
    }

    fn remove_at(&mut self, i: usize) {
        self.files.remove(i);
        self.meta.remove(i);
    }

    /// Puts a new image where the sort order wants it (binary search).
    /// Returns false for files that aren't images or are listed already.
    fn insert(&mut self, path: &Path) -> bool {
        if !is_image(path) || self.position(path).is_some() {
            return false;
        }
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_file() => {
                self.insert_with(path, meta_of(&meta));
                true
            }
            _ => false,
        }
    }

    fn insert_with(&mut self, path: &Path, meta: Meta) {
        let lower = name(path).to_lowercase();
        let (mut low, mut high) = (0, self.files.len());
        while low < high {
            let mid = (low + high) / 2;
            let other = name(&self.files[mid]).to_lowercase();
            if compare(self.sort, (&other, self.meta[mid]), (&lower, meta)) == Ordering::Less {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        self.files.insert(low, path.to_path_buf());
        self.meta.insert(low, meta);
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
            dir: PathBuf::new(),
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
    fn applies_folder_changes_in_place() {
        let dir = std::env::temp_dir().join(format!("flick-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = |n: &str| dir.join(n);
        let touch = |n: &str| std::fs::write(file(n), n).unwrap();
        for n in ["a1.jpg", "a3.jpg", "a5.jpg"] {
            touch(n);
        }
        let mut f = list(&file("a3.jpg"), Sort::default()).unwrap();
        assert_eq!(f.index, 1);

        for n in ["a2.jpg", "a6.jpg", "notes.txt"] {
            touch(n);
        }
        let added = [Change::Added(file("a2.jpg")), Change::Added(file("a6.jpg")), Change::Added(file("notes.txt"))];
        assert!(f.apply(&added));
        assert_eq!(names(&f), ["a1.jpg", "a2.jpg", "a3.jpg", "a5.jpg", "a6.jpg"], "in name order, images only");
        assert_eq!(f.files[f.index], file("a3.jpg"), "the current file stays current");

        std::fs::remove_file(file("a3.jpg")).unwrap();
        f.apply(&[Change::Removed(file("a3.jpg"))]);
        assert_eq!(f.files[f.index], file("a5.jpg"), "a removed current file: its successor");

        std::fs::rename(file("a5.jpg"), file("a0.jpg")).unwrap();
        f.apply(&[Change::Renamed(file("a5.jpg"), file("a0.jpg"))]);
        assert_eq!(names(&f), ["a0.jpg", "a1.jpg", "a2.jpg", "a6.jpg"]);
        assert_eq!(f.files[f.index], file("a0.jpg"), "a renamed current file stays current");

        // Newest last: a file written to now moves to the end.
        f.sort(Sort { key: SortKey::Modified, descending: false });
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        std::fs::File::options().write(true).open(file("a1.jpg")).unwrap().set_modified(later).unwrap();
        f.apply(&[Change::Modified(file("a1.jpg"))]);
        assert_eq!(f.files.last(), Some(&file("a1.jpg")));
        assert_eq!(f.files[f.index], file("a0.jpg"));

        // The last file can't be removed from under the viewer.
        let mut one = list(&file("a0.jpg"), Sort::default()).unwrap();
        let all: Vec<Change> = one.files.iter().map(|p| Change::Removed(p.clone())).collect();
        one.apply(&all);
        assert_eq!(one.files.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
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
