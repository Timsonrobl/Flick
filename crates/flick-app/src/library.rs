//! Stars, and later tags: what Flick knows about files beyond the files
//! themselves, in `%LOCALAPPDATA%\Flick\library.db` (SQLite). Files are
//! never modified.
//!
//! A file is known by its NTFS ID (volume serial + 128-bit file ID), which
//! survives renames and moves on its drive and is never reused for another
//! file; its last known path is kept too, for looking up a whole folder at
//! once (sorting by stars). A file with stars is in the favourites folder:
//! a hard link to it (another name for the same file: no copy), or a copy
//! for a file on another drive. The favourites folder decides what is
//! starred: a link deleted in Explorer unstars its file (`sweep`).
//!
//! Tags later: `tags (id, name)` and `file_tags (volume, file_id, tag)`,
//! keyed like `files`; a tag can get its own folder of links the same way.
//!
//! The database opens on first use (a star, the info panel, sorting by
//! stars), never at startup. Calls may come from any thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};
use windows::Win32::Foundation::{CloseHandle, ERROR_NOT_SAME_DEVICE, HANDLE};
use windows::Win32::Storage::FileSystem::*;
use windows::core::HSTRING;

static DB: Mutex<Option<Connection>> = Mutex::new(None);

fn db_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("Flick").join("library.db"))
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS files (
        volume INTEGER NOT NULL,
        file_id BLOB NOT NULL,
        path TEXT NOT NULL,
        dir TEXT NOT NULL,          -- the path's folder, lowercased (folder lookups)
        stars INTEGER NOT NULL DEFAULT 0,
        link TEXT,                  -- its entry in the favourites folder
        copy INTEGER NOT NULL DEFAULT 0, -- that entry is a copy (another drive)
        starred INTEGER,            -- when it got its first star (Unix seconds)
        PRIMARY KEY (volume, file_id)
    );
    CREATE INDEX IF NOT EXISTS files_dir ON files (dir);
    CREATE INDEX IF NOT EXISTS files_link ON files (link);
    CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

/// The schema's version (`PRAGMA user_version`): 2 added `starred`.
const VERSION: i32 = 2;

/// Creates the tables, or brings an older library up to date.
fn prepare(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(SCHEMA)?;
    let version: i32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 2 {
        // Absent in version 1 libraries (made by the CREATE above it's there already).
        let _ = db.execute("ALTER TABLE files ADD COLUMN starred INTEGER", []);
    }
    db.execute_batch(&format!("PRAGMA user_version = {VERSION}"))
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// A file's stars, and when it got its first one (Unix seconds; 0 if not
/// known, for files starred before this was kept).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stars {
    pub count: u32,
    pub since: i64,
}

/// Runs `f` on the database, opening it first if needed. With `create`
/// false, a library that doesn't exist yet is left alone (None).
fn with_db<T>(create: bool, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Option<T> {
    let mut db = DB.lock().unwrap_or_else(|e| e.into_inner());
    if db.is_none() {
        let path = db_path()?;
        if !create && !path.exists() {
            return None;
        }
        std::fs::create_dir_all(path.parent()?).ok()?;
        let connection = Connection::open(&path).ok()?;
        prepare(&connection).ok()?;
        *db = Some(connection);
    }
    f(db.as_ref()?).ok()
}

/// A file's identity on its drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Id {
    volume: i64,
    file: [u8; 16],
    links: u32,
}

fn identity(path: &Path) -> Option<Id> {
    unsafe {
        let handle = CreateFileW(
            &HSTRING::from(path.as_os_str()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
        .ok()?;
        let id = id_of(handle);
        let _ = CloseHandle(handle);
        id
    }
}

unsafe fn id_of(handle: HANDLE) -> Option<Id> {
    let mut info = FILE_ID_INFO::default();
    let mut basic = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandleEx(handle, FileIdInfo, (&raw mut info).cast(), size_of::<FILE_ID_INFO>() as u32).ok()?;
        GetFileInformationByHandle(handle, &mut basic).ok()?;
    }
    Some(Id { volume: info.VolumeSerialNumber as i64, file: info.FileId.Identifier, links: basic.nNumberOfLinks })
}

fn dir_key(path: &Path) -> String {
    path.parent().map_or_else(String::new, |d| d.to_string_lossy().to_lowercase())
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The favourites folder, if one was chosen.
pub fn favorites() -> Option<PathBuf> {
    with_db(false, |db| db.query_row("SELECT value FROM settings WHERE key = 'favorites'", [], |r| r.get::<_, String>(0)).optional())
        .flatten()
        .map(PathBuf::from)
}

pub fn set_favorites(folder: &Path) -> Result<(), String> {
    with_db(true, |db| db.execute("INSERT OR REPLACE INTO settings (key, value) VALUES ('favorites', ?1)", params![text(folder)]))
        .map(|_| ())
        .ok_or_else(|| "can't open the library".into())
}

/// A file's row in `files`.
struct Record {
    volume: i64,
    file: Vec<u8>,
    stars: Stars,
    link: Option<String>,
    copy: bool,
}

/// The record for `path`: by its ID, else as a copy in the favourites
/// folder (a copy has an ID of its own).
fn find(db: &Connection, path: &Path, id: Option<Id>) -> rusqlite::Result<Option<Record>> {
    let row = |r: &rusqlite::Row| {
        let stars = Stars { count: r.get(2)?, since: r.get::<_, Option<i64>>(5)?.unwrap_or(0) };
        Ok(Record { volume: r.get(0)?, file: r.get(1)?, stars, link: r.get(3)?, copy: r.get::<_, i64>(4)? != 0 })
    };
    let columns = "SELECT volume, file_id, stars, link, copy, starred FROM files";
    if let Some(id) = id {
        if let Some(found) = db.query_row(&format!("{columns} WHERE volume = ?1 AND file_id = ?2"), params![id.volume, &id.file[..]], row).optional()? {
            return Ok(Some(found));
        }
    }
    db.query_row(&format!("{columns} WHERE link = ?1 AND copy = 1"), params![text(path)], row).optional()
}

/// A file's stars (none if unstarred, or no library yet). A file found by
/// its ID under another path (moved since) gets its record's path updated,
/// so sorting its new folder by stars finds it.
pub fn stars(path: &Path) -> Stars {
    let id = identity(path);
    let Some(record) = with_db(false, |db| find(db, path, id)).flatten() else { return Stars::default() };
    // Viewed through its favourites entry: the record keeps the original's path.
    let is_entry = record.link.as_deref() == Some(&*text(path));
    if !is_entry {
        with_db(false, |db| {
            db.execute(
                "UPDATE files SET path = ?3, dir = ?4 WHERE volume = ?1 AND file_id = ?2 AND path <> ?3",
                params![record.volume, record.file, text(path), dir_key(path)],
            )
        });
    }
    record.stars
}

/// The starred files in `dir` by lowercased name (one query, for sorting a
/// folder; files moved since starring are found once viewed, by ID). Their
/// favourites entries count as in the favourites folder.
pub fn stars_in(dir: &Path) -> HashMap<String, Stars> {
    let key = dir.to_string_lossy().to_lowercase();
    with_db(false, |db| {
        let mut query = db.prepare("SELECT path, stars, starred, link FROM files WHERE stars > 0 AND (dir = ?1 OR link IS NOT NULL)")?;
        let rows = query.query_map(params![key], |r| {
            let stars = Stars { count: r.get(1)?, since: r.get::<_, Option<i64>>(2)?.unwrap_or(0) };
            Ok((r.get::<_, String>(0)?, stars, r.get::<_, Option<String>>(3)?))
        })?;
        let mut found = HashMap::new();
        for (path, stars, link) in rows.filter_map(Result::ok) {
            for name in [Some(path), link].into_iter().flatten() {
                let name = Path::new(&name);
                if dir_key(name) == key {
                    found.insert(name.file_name().unwrap_or_default().to_string_lossy().to_lowercase(), stars);
                }
            }
        }
        Ok(found)
    })
    .unwrap_or_default()
}

/// Adds `delta` stars to `path` (or takes them away; not below 0) and
/// returns the new count. The first star puts the file in the favourites
/// folder and notes when; the last one taken away removes it from there.
pub fn add_stars(path: &Path, delta: i32, favorites: &Path) -> Result<Stars, String> {
    let id = identity(path).ok_or("the file can't be opened")?;
    let found = with_db(true, |db| find(db, path, Some(id))).ok_or("can't open the library")?;
    let Record { volume, file, stars: old, link, copy } =
        found.unwrap_or(Record { volume: id.volume, file: id.file.to_vec(), stars: Stars::default(), link: None, copy: false });
    let count = (old.count as i64 + i64::from(delta)).max(0) as u32;
    if count == old.count {
        return Ok(old);
    }
    let (mut link, mut copy, mut since) = (link, copy, old.since);
    if old.count == 0 {
        let (made, is_copy) = favourite(path, id, favorites)?;
        (link, copy, since) = (Some(text(&made)), is_copy, now());
    }
    if count == 0 {
        if let Some(entry) = link.take() {
            unfavourite(Path::new(&entry), id, copy);
        }
        with_db(true, |db| db.execute("DELETE FROM files WHERE volume = ?1 AND file_id = ?2", params![volume, file])).ok_or("can't update the library")?;
        return Ok(Stars::default());
    }
    // A copy viewed in the favourites folder keeps its original's path.
    let original = if copy && link.as_deref() == Some(&*text(path)) { None } else { Some(path) };
    with_db(true, |db| {
        db.execute(
            "INSERT INTO files (volume, file_id, path, dir, stars, link, copy, starred) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?10)
             ON CONFLICT (volume, file_id) DO UPDATE SET path = COALESCE(?8, path), dir = COALESCE(?9, dir), stars = ?5, link = ?6, copy = ?7,
                 starred = COALESCE(starred, ?10)",
            params![volume, file, text(path), dir_key(path), count, link, i64::from(copy), original.map(text), original.map(dir_key), since],
        )
    })
    .ok_or("can't update the library")?;
    Ok(Stars { count, since })
}

/// Puts `path` in the favourites folder: a hard link, or (another drive) a
/// copy, under its own name or "name (2)", … Returns the entry and whether
/// it is a copy. A file already there (viewed in the favourites folder)
/// is its own entry.
fn favourite(path: &Path, id: Id, favorites: &Path) -> Result<(PathBuf, bool), String> {
    if path.parent().is_some_and(|dir| same_dir(dir, favorites)) {
        return Ok((path.to_path_buf(), false));
    }
    std::fs::create_dir_all(favorites).map_err(|e| format!("{}: {e}", favorites.display()))?;
    let name = Path::new(path.file_name().ok_or("no file name")?);
    let (stem, ext) = (name.file_stem().unwrap_or_default().to_string_lossy(), name.extension().map(|e| format!(".{}", e.to_string_lossy())));
    for n in 1..1000 {
        let candidate = if n == 1 { favorites.join(name) } else { favorites.join(format!("{stem} ({n}){}", ext.as_deref().unwrap_or(""))) };
        if candidate.exists() {
            if identity(&candidate).is_some_and(|c| (c.volume, c.file) == (id.volume, id.file)) {
                return Ok((candidate, false)); // linked already
            }
            continue;
        }
        let (target, existing) = (HSTRING::from(candidate.as_os_str()), HSTRING::from(path.as_os_str()));
        return match unsafe { CreateHardLinkW(&target, &existing, None) } {
            Ok(()) => Ok((candidate, false)),
            Err(e) if e.code() == ERROR_NOT_SAME_DEVICE.to_hresult() => unsafe { CopyFileW(&existing, &target, true) }
                .map(|()| (candidate, true))
                .map_err(|e| format!("copying to favourites: {e}")),
            Err(e) => Err(format!("linking into favourites: {e}")),
        };
    }
    Err("too many files of that name in favourites".into())
}

/// Removes a favourites entry: a copy Flick made, or a hard link while the
/// file has another name (never a file's only name).
fn unfavourite(entry: &Path, id: Id, copy: bool) {
    let still_ours = identity(entry).is_some_and(|e| if copy { true } else { (e.volume, e.file) == (id.volume, id.file) && e.links > 1 });
    if still_ours {
        let _ = std::fs::remove_file(entry);
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().trim_end_matches('\\').eq_ignore_ascii_case(b.to_string_lossy().trim_end_matches('\\'))
}

/// A file was renamed (seen by a folder watcher): its record follows.
pub fn renamed(old: &Path, new: &Path) {
    with_db(false, |db| db.execute("UPDATE files SET path = ?2, dir = ?3 WHERE path = ?1", params![text(old), text(new), dir_key(new)]));
    with_db(false, |db| db.execute("UPDATE files SET link = ?2 WHERE link = ?1", params![text(old), text(new)]));
}

/// A file's name went away (seen by a folder watcher). Its favourites
/// entry deleted: it's unstarred. The original deleted while its hard link
/// lives on: the record now points at the link.
pub fn removed(path: &Path) {
    with_db(false, |db| db.execute("DELETE FROM files WHERE link = ?1", params![text(path)]));
    let links: Vec<String> = with_db(false, |db| {
        let mut query = db.prepare("SELECT link FROM files WHERE path = ?1 AND link IS NOT NULL")?;
        let rows = query.query_map(params![text(path)], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(Result::ok).collect())
    })
    .unwrap_or_default();
    for link in links {
        with_db(false, |db| db.execute("UPDATE files SET path = ?1, dir = ?2 WHERE link = ?1", params![link, dir_key(Path::new(&link))]));
    }
}

/// Brings the library in line with the disk, at most once a day (run when
/// Flick is idle): records whose favourites entry is gone are dropped
/// (unstarred in Explorer, or deleted), records whose file moved away
/// point at their entry.
pub fn sweep_if_due() {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let last = with_db(false, |db| db.query_row("SELECT value FROM settings WHERE key = 'swept'", [], |r| r.get::<_, String>(0)).optional()).flatten();
    if last.and_then(|t| t.parse::<i64>().ok()).is_some_and(|t| now - t < 24 * 3600) {
        return;
    }
    let Some(rows) = with_db(false, |db| {
        let mut query = db.prepare("SELECT path, link FROM files")?;
        let rows = query.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?;
        Ok(rows.filter_map(Result::ok).collect::<Vec<_>>())
    }) else {
        return;
    };
    for (path, link) in rows {
        match link {
            Some(link) if !Path::new(&link).exists() => removed(Path::new(&link)),
            Some(_) if !Path::new(&path).exists() => removed(Path::new(&path)),
            _ => {}
        }
    }
    with_db(false, |db| db.execute("INSERT OR REPLACE INTO settings (key, value) VALUES ('swept', ?1)", params![now.to_string()]));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stars, the favourites folder and the records, on real files: the
    /// temp folder (C:) for the photos and favourites, and the build folder
    /// (H: here) for a photo from another drive.
    #[test]
    fn stars_link_copy_follow_and_unstar() {
        let root = std::env::temp_dir().join(format!("flick-library-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (photos, other, favorites) = (root.join("photos"), root.join("other"), root.join("favourites"));
        std::fs::create_dir_all(&photos).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        *DB.lock().unwrap() = Some({
            let connection = Connection::open(root.join("library.db")).unwrap();
            prepare(&connection).unwrap();
            connection
        });
        let count = |result: Result<Stars, String>| result.map(|s| s.count);
        let a = photos.join("a.jpg");
        std::fs::write(&a, b"a").unwrap();

        // The first star links it into favourites: the same file, two names.
        let first = add_stars(&a, 1, &favorites).unwrap();
        assert_eq!(first.count, 1);
        assert!(first.since > 0, "the first star notes when");
        let entry = favorites.join("a.jpg");
        assert_eq!(identity(&entry).map(|i| (i.volume, i.file)), identity(&a).map(|i| (i.volume, i.file)));
        assert_eq!(identity(&a).unwrap().links, 2);
        assert_eq!(add_stars(&a, 2, &favorites), Ok(Stars { count: 3, since: first.since }), "more stars keep the date");
        assert_eq!(stars(&entry).count, 3, "seen through the favourites entry too");
        assert_eq!(stars_in(&photos).get("a.jpg").map(|s| s.count), Some(3));
        assert_eq!(stars_in(&favorites).get("a.jpg"), Some(&Stars { count: 3, since: first.since }), "the favourites folder sorts by it too");

        // Another a.jpg: a name of its own in favourites.
        let other_a = other.join("a.jpg");
        std::fs::write(&other_a, b"other").unwrap();
        assert_eq!(count(add_stars(&other_a, 1, &favorites)), Ok(1));
        assert!(favorites.join("a (2).jpg").exists());

        // Renamed: the record follows (and by ID, even unannounced).
        let b = photos.join("b.jpg");
        std::fs::rename(&a, &b).unwrap();
        renamed(&a, &b);
        assert_eq!(stars_in(&photos).get("b.jpg").map(|s| s.count), Some(3));

        // The last star taken away: the link goes, the photo stays.
        assert_eq!(count(add_stars(&b, -5, &favorites)), Ok(0));
        assert!(!entry.exists() && b.exists());
        assert_eq!(stars(&b), Stars::default());

        // A favourites entry deleted in Explorer: unstarred.
        removed(&favorites.join("a (2).jpg"));
        assert_eq!(stars(&other_a).count, 0);

        // Another drive: a copy, which goes with the last star.
        let far_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../target/flick-library-{}", std::process::id()));
        std::fs::create_dir_all(&far_dir).unwrap();
        let far = far_dir.join("far.jpg");
        std::fs::write(&far, b"far").unwrap();
        if identity(&far).map(|i| i.volume) != identity(&b).map(|i| i.volume) {
            assert_eq!(count(add_stars(&far, 1, &favorites)), Ok(1));
            let copy = favorites.join("far.jpg");
            assert!(copy.exists() && identity(&copy).map(|i| i.file) != identity(&far).map(|i| i.file));
            assert_eq!(stars(&copy).count, 1, "the copy is found by its path");
            assert_eq!(count(add_stars(&copy, -1, &favorites)), Ok(0));
            assert!(!copy.exists() && far.exists());
        }
        *DB.lock().unwrap() = None;
        let _ = std::fs::remove_dir_all(&far_dir);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A version 1 library (before `starred`) gains the column, its rows
    /// an unknown date.
    #[test]
    fn upgrades_a_version_1_library() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE files (volume INTEGER NOT NULL, file_id BLOB NOT NULL, path TEXT NOT NULL, dir TEXT NOT NULL,
                 stars INTEGER NOT NULL DEFAULT 0, link TEXT, copy INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (volume, file_id));
             INSERT INTO files VALUES (1, x'00', 'C:\\a.jpg', 'c:\\', 2, 'C:\\fav\\a.jpg', 0);
             PRAGMA user_version = 1;",
        )
        .unwrap();
        prepare(&db).unwrap();
        let (stars, starred): (u32, Option<i64>) = db.query_row("SELECT stars, starred FROM files", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((stars, starred), (2, None));
        assert_eq!(db.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0)).unwrap(), VERSION);
        prepare(&db).unwrap(); // and again: nothing to do
    }
}
