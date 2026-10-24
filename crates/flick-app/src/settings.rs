//! The right-click menu's choices (view mode, render filter, sort order),
//! last chosen in any window, kept in `%LOCALAPPDATA%\Flick\settings` so new
//! windows and the next run start with them. Each open window keeps its own
//! (viewer.rs): the rule for every setting unless one says otherwise.

use std::path::PathBuf;

use crate::folder::{Sort, SortKey};
use crate::view::{Filter, Fit, Mode};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Prefs {
    pub mode: Mode,
    pub filter: Filter,
    pub sort: Sort,
}

fn path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("Flick").join("settings"))
}

const FITS: [(Fit, &str); 3] = [(Fit::Window, "window"), (Fit::Width, "width"), (Fit::Original, "original")];
const FILTERS: [(Filter, &str); 3] = [(Filter::Nearest, "none"), (Filter::Soften, "soften"), (Filter::Sharpen, "soften+sharpen")];
const SORTS: [(SortKey, &str); 4] = [(SortKey::Name, "name"), (SortKey::Modified, "modified"), (SortKey::Size, "size"), (SortKey::Type, "type")];

fn find<T: Copy>(table: &[(T, &str)], text: &str) -> Option<T> {
    table.iter().find(|(_, name)| *name == text).map(|(value, _)| *value)
}

fn name<T: PartialEq>(table: &[(T, &'static str)], value: T) -> &'static str {
    table.iter().find(|(v, _)| *v == value).map_or("", |(_, name)| name)
}

pub fn load() -> Prefs {
    let mut prefs = Prefs::default();
    let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else { return prefs };
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=').map(|(k, v)| (k.trim(), v.trim())) else { continue };
        match key {
            "fit" => prefs.mode.fit = find(&FITS, value).unwrap_or(prefs.mode.fit),
            "stretch_small" => prefs.mode.stretch_small = value == "1",
            "filter" => prefs.filter = find(&FILTERS, value).unwrap_or(prefs.filter),
            "sort" => prefs.sort.key = find(&SORTS, value).unwrap_or(prefs.sort.key),
            "descending" => prefs.sort.descending = value == "1",
            _ => {}
        }
    }
    prefs
}

pub fn save(prefs: Prefs) {
    let Some(path) = path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = format!(
        "fit={}\nstretch_small={}\nfilter={}\nsort={}\ndescending={}\n",
        name(&FITS, prefs.mode.fit),
        u8::from(prefs.mode.stretch_small),
        name(&FILTERS, prefs.filter),
        name(&SORTS, prefs.sort.key),
        u8::from(prefs.sort.descending),
    );
    let _ = std::fs::write(path, text);
}
