//! The right-click menu's choices (view mode, render filter, sort order,
//! the Tab info panel, a persisted turn, the slideshow interval), last
//! chosen in any window, kept in `%LOCALAPPDATA%\Flick\settings` so new
//! windows and the next run start with them. Each open window keeps its own
//! (viewer.rs): the rule for every setting unless one says otherwise.

use std::path::PathBuf;

use crate::folder::{Sort, SortKey};
use crate::view::{Filter, Fit, Mode, Turn};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prefs {
    pub mode: Mode,
    pub filter: Filter,
    pub sort: Sort,
    /// The info panel (Tab): file details and EXIF.
    pub info: bool,
    /// Rotate ▸ Persist: new tabs start with `turn`, and it's saved (else
    /// every tab starts unturned and nothing is kept).
    pub persist_turn: bool,
    pub turn: Turn,
    /// How long a slideshow shows each picture, in milliseconds.
    pub slide_ms: u32,
}

impl Default for Prefs {
    fn default() -> Prefs {
        Prefs {
            mode: Mode::default(),
            filter: Filter::default(),
            sort: Sort::default(),
            info: false,
            persist_turn: false,
            turn: Turn::default(),
            slide_ms: 3000,
        }
    }
}

fn path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("Flick").join("settings"))
}

const FITS: [(Fit, &str); 5] = [(Fit::Window, "window"), (Fit::Fill, "fill"), (Fit::Width, "width"), (Fit::Height, "height"), (Fit::Original, "original")];
const FILTERS: [(Filter, &str); 3] = [(Filter::Nearest, "none"), (Filter::Soften, "soften"), (Filter::Sharpen, "soften+sharpen")];
const SORTS: [(SortKey, &str); 6] = [
    (SortKey::Name, "name"),
    (SortKey::Modified, "modified"),
    (SortKey::Size, "size"),
    (SortKey::Type, "type"),
    (SortKey::Stars, "stars"),
    (SortKey::Starred, "starred"),
];

fn find<T: Copy>(table: &[(T, &str)], text: &str) -> Option<T> {
    table.iter().find(|(_, name)| *name == text).map(|(value, _)| *value)
}

fn name<T: PartialEq>(table: &[(T, &'static str)], value: T) -> &'static str {
    table.iter().find(|(v, _)| *v == value).map_or("", |(_, name)| name)
}

/// Preferences ▸ in the menu: app-wide, unlike `Prefs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Global {
    /// Opened files become tabs of the window used last (else new windows).
    pub tabs: bool,
    /// Tabs wrap onto more rows (else one row, squeezed).
    pub multirow: bool,
    /// A picture looked at for a moment gets its full resolution decoded,
    /// so a first zoom is sharp at once (GPU memory and CPU for every
    /// picture paused on, zoomed or not).
    pub prefetch: bool,
    /// An installed Flick looks for updates daily and gets them ready
    /// (update.rs).
    pub updates: bool,
}

impl Default for Global {
    fn default() -> Global {
        Global { tabs: true, multirow: true, prefetch: false, updates: true }
    }
}

pub fn load() -> (Prefs, Global) {
    let text = path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    parse(&text, Prefs::default())
}

/// One window's menu settings in the settings file's form (`parse` reads
/// them back): for the close history (`session.rs`).
pub fn prefs_text(prefs: Prefs) -> String {
    text(prefs, Global::default())
}

/// Settings text over `prefs` (what it doesn't name keeps `prefs`' value).
pub fn parse(text: &str, prefs: Prefs) -> (Prefs, Global) {
    let (mut prefs, mut global) = (prefs, Global::default());
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=').map(|(k, v)| (k.trim(), v.trim())) else { continue };
        match key {
            "fit" => prefs.mode.fit = find(&FITS, value).unwrap_or(prefs.mode.fit),
            "stretch_small" => prefs.mode.stretch_small = value == "1",
            "filter" => prefs.filter = find(&FILTERS, value).unwrap_or(prefs.filter),
            "sort" => prefs.sort.key = find(&SORTS, value).unwrap_or(prefs.sort.key),
            "descending" => prefs.sort.descending = value == "1",
            "info" => prefs.info = value == "1",
            "persist_turn" => prefs.persist_turn = value == "1",
            "slideshow_ms" => prefs.slide_ms = value.parse().unwrap_or(prefs.slide_ms),
            "turn" => {
                let parts: Vec<&str> = value.split(',').collect();
                if let [quarter, flip_h, flip_v] = parts[..] {
                    prefs.turn = Turn { quarter: quarter.parse().unwrap_or(0) % 4, flip_h: flip_h == "1", flip_v: flip_v == "1" };
                }
            }
            "tabs" => global.tabs = value != "0",
            "multirow" => global.multirow = value != "0",
            "prefetch_full" => global.prefetch = value == "1",
            "updates" => global.updates = value != "0",
            _ => {}
        }
    }
    (prefs, global)
}

pub fn save(prefs: Prefs, global: Global) {
    let Some(path) = path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, text(prefs, global));
}

fn text(prefs: Prefs, global: Global) -> String {
    format!(
        "fit={}\nstretch_small={}\nfilter={}\nsort={}\ndescending={}\ninfo={}\npersist_turn={}\nturn={},{},{}\nslideshow_ms={}\ntabs={}\nmultirow={}\nprefetch_full={}\nupdates={}\n",
        name(&FITS, prefs.mode.fit),
        u8::from(prefs.mode.stretch_small),
        name(&FILTERS, prefs.filter),
        name(&SORTS, prefs.sort.key),
        u8::from(prefs.sort.descending),
        u8::from(prefs.info),
        u8::from(prefs.persist_turn),
        // Kept only while persisted.
        if prefs.persist_turn { prefs.turn.quarter } else { 0 },
        u8::from(prefs.persist_turn && prefs.turn.flip_h),
        u8::from(prefs.persist_turn && prefs.turn.flip_v),
        prefs.slide_ms,
        u8::from(global.tabs),
        u8::from(global.multirow),
        u8::from(global.prefetch),
        u8::from(global.updates),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_windows_settings_read_back_as_they_were() {
        let prefs = Prefs {
            mode: Mode { fit: Fit::Height, stretch_small: true },
            filter: Filter::Nearest,
            sort: Sort { key: SortKey::Modified, descending: true },
            info: true,
            persist_turn: true,
            turn: Turn { quarter: 3, flip_h: true, flip_v: false },
            slide_ms: 1500,
        };
        assert_eq!(parse(&prefs_text(prefs), Prefs::default()).0, prefs);
    }
}
