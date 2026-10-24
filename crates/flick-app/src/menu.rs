//! The right-click menu: Honeyview's View items (without the double-page
//! modes), its render filters and Sort by. A native popup menu: Windows
//! draws it in the system style, and it costs nothing until opened.

use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, HMENU, MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenu,
};
use windows::core::{PCWSTR, w};

use crate::folder::SortKey;
use crate::settings::Prefs;
use crate::view::{Filter, Fit};

/// One menu item: its label and what choosing it does to the settings.
struct Item {
    text: PCWSTR,
    checked: fn(&Prefs) -> bool,
    choose: fn(&mut Prefs),
}

macro_rules! item {
    ($text:expr, |$p:ident| $checked:expr, |$q:ident| $choose:expr) => {
        Item { text: $text, checked: |$p: &Prefs| $checked, choose: |$q: &mut Prefs| $choose }
    };
}

/// Sections of the main menu, separated by lines.
fn main_items() -> [Vec<Item>; 3] {
    [
        vec![item!(w!("Stretch Small Images\tZ"), |p| p.mode.stretch_small, |p| p.mode.stretch_small = !p.mode.stretch_small)],
        vec![
            item!(w!("Original Size (100%)\t0"), |p| p.mode.fit == Fit::Original, |p| p.mode.fit = Fit::Original),
            item!(w!("Fit in Window\t9"), |p| p.mode.fit == Fit::Window, |p| p.mode.fit = Fit::Window),
            item!(w!("Fit to Width\t8"), |p| p.mode.fit == Fit::Width, |p| p.mode.fit = Fit::Width),
        ],
        vec![
            item!(w!("No Filter\tU"), |p| p.filter == Filter::Nearest, |p| p.filter = Filter::Nearest),
            item!(w!("Soften\tI"), |p| p.filter == Filter::Soften, |p| p.filter = Filter::Soften),
            item!(w!("Soften+Sharpen"), |p| p.filter == Filter::Sharpen, |p| p.filter = Filter::Sharpen),
        ],
    ]
}

/// The Sort by submenu's sections.
fn sort_items() -> [Vec<Item>; 2] {
    [
        vec![
            item!(w!("Name"), |p| p.sort.key == SortKey::Name, |p| p.sort.key = SortKey::Name),
            item!(w!("Date modified"), |p| p.sort.key == SortKey::Modified, |p| p.sort.key = SortKey::Modified),
            item!(w!("Size"), |p| p.sort.key == SortKey::Size, |p| p.sort.key = SortKey::Size),
            item!(w!("Type"), |p| p.sort.key == SortKey::Type, |p| p.sort.key = SortKey::Type),
        ],
        vec![item!(w!("Descending"), |p| p.sort.descending, |p| p.sort.descending = !p.sort.descending)],
    ]
}

/// Appends `sections` with ids from `first`; returns the next free id.
unsafe fn append(menu: HMENU, sections: &[Vec<Item>], prefs: &Prefs, first: usize) -> usize {
    let mut id = first;
    for (i, section) in sections.iter().enumerate() {
        if i > 0 {
            let _ = unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, None) };
        }
        for item in section {
            let flags = if (item.checked)(prefs) { MF_STRING | MF_CHECKED } else { MF_STRING };
            let _ = unsafe { AppendMenuW(menu, flags, id, item.text) };
            id += 1;
        }
    }
    id
}

/// Shows the menu at the cursor and returns the settings with the choice
/// applied, if one was made.
pub fn show(hwnd: HWND, prefs: Prefs) -> Option<Prefs> {
    let (main, sort) = (main_items(), sort_items());
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        let submenu = CreatePopupMenu().ok()?;
        let after_main = append(menu, &main, &prefs, 1);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_POPUP, submenu.0 as usize, w!("Sort by"));
        append(submenu, &sort, &prefs, after_main);
        let mut at = POINT::default();
        let _ = GetCursorPos(&mut at);
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, at.x, at.y, None, hwnd, None).0 as usize;
        let _ = DestroyMenu(menu); // and its submenu
        let item = main.iter().chain(sort.iter()).flatten().nth(chosen.checked_sub(1)?)?;
        let mut prefs = prefs;
        (item.choose)(&mut prefs);
        Some(prefs)
    }
}
