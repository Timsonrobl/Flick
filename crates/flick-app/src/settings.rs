//! The view mode last chosen in any window (View menu or its keys), kept in
//! `%LOCALAPPDATA%\Flick\settings` so new windows and the next run start
//! with it. Each open window keeps its own mode (viewer.rs).

use std::path::PathBuf;

use crate::view::{Fit, Mode};

fn path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("Flick").join("settings"))
}

pub fn load() -> Mode {
    let mut mode = Mode::default();
    let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else { return mode };
    for line in text.lines() {
        match line.split_once('=').map(|(k, v)| (k.trim(), v.trim())) {
            Some(("fit", "window")) => mode.fit = Fit::Window,
            Some(("fit", "width")) => mode.fit = Fit::Width,
            Some(("fit", "original")) => mode.fit = Fit::Original,
            Some(("stretch_small", v)) => mode.stretch_small = v == "1",
            _ => {}
        }
    }
    mode
}

pub fn save(mode: Mode) {
    let Some(path) = path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let fit = match mode.fit {
        Fit::Window => "window",
        Fit::Width => "width",
        Fit::Original => "original",
    };
    let _ = std::fs::write(path, format!("fit={fit}\nstretch_small={}\n", u8::from(mode.stretch_small)));
}
