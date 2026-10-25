//! The right-click menu: Honeyview's View items (without the double-page
//! modes), its render filters, Sort by, and Move to Window. A native popup
//! menu: Windows draws it in the system style, and it costs nothing until
//! opened.

use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject, HBITMAP};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, GetSystemMetrics, HMENU, MENUITEMINFOW, MF_CHECKED, MF_GRAYED, MF_POPUP,
    MF_SEPARATOR, MF_STRING, MIIM_BITMAP, SM_CXMENUCHECK, SM_SWAPBUTTON, SetMenuItemInfoW, TPM_RETURNCMD, TrackPopupMenu,
};
use windows::core::{HSTRING, PCWSTR, w};

use std::path::{Path, PathBuf};

use crate::chrome::PALETTE;
use crate::folder::SortKey;
use crate::settings::{Global, Prefs};
use crate::view::{Filter, Fit, Turn};

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
        vec![
            item!(w!("File Info\tTab"), |p| p.info, |p| p.info = !p.info),
            item!(w!("Stretch Small Images\tZ"), |p| p.mode.stretch_small, |p| p.mode.stretch_small = !p.mode.stretch_small),
        ],
        vec![
            // In middle click's order (view.rs).
            item!(w!("Fit in Window\t9"), |p| p.mode.fit == Fit::Window, |p| p.mode.fit = Fit::Window),
            item!(w!("Fill Window\t6"), |p| p.mode.fit == Fit::Fill, |p| p.mode.fit = Fit::Fill),
            item!(w!("Fit to Width\t8"), |p| p.mode.fit == Fit::Width, |p| p.mode.fit = Fit::Width),
            item!(w!("Fit to Height\t7"), |p| p.mode.fit == Fit::Height, |p| p.mode.fit = Fit::Height),
            item!(w!("Original Size (100%)\t0"), |p| p.mode.fit == Fit::Original, |p| p.mode.fit = Fit::Original),
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
            item!(w!("Stars (most first)"), |p| p.sort.key == SortKey::Stars, |p| p.sort.key = SortKey::Stars),
            item!(w!("Date starred"), |p| p.sort.key == SortKey::Starred, |p| p.sort.key = SortKey::Starred),
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

/// What a menu choice asks for.
pub enum Choice {
    Prefs(Prefs),
    /// Move to Window: the active tab goes to `targets[i]`.
    MoveTo(usize),
    /// Move to Window ▸ New Window.
    NewWindow,
    /// Tab Lock on or off (this window).
    Lock,
    /// Preferences ▸ changed.
    Global(Global),
    /// Closed by a right click elsewhere: the menu opens again where that
    /// click was (in this window or another), as in Explorer.
    Reopen,
    /// Preferences ▸ Favorites Folder…
    ChooseFavorites,
    /// Rotate ▸ a turn for this tab.
    Turn(Turn),
    /// Rotate ▸ Persist.
    PersistTurn,
    /// Slideshow ▸ Start / Stop.
    Slideshow,
    /// Slideshow ▸ an interval (ms): set, and the slideshow runs.
    SlideInterval(u32),
    /// Delete (to the Recycle Bin, after asking).
    Delete,
}

/// Slideshow ▸ intervals offered (`[` and `]` adjust in between).
const SLIDE_PRESETS: [u32; 8] = [250, 500, 1000, 2000, 3000, 5000, 10_000, 30_000];

/// Another window, as Move to Window lists it: its colour
/// (chrome::PALETTE index) and the file it shows.
pub struct Target {
    pub color: usize,
    pub label: String,
}

const NEW_WINDOW: usize = 900;
const LOCK: usize = 901;
const OPEN_IN_TABS: usize = 902;
const MULTIROW: usize = 903;
const FAVORITES: usize = 904;
/// Rotate ▸ 910 + quarter turns, flips, persist.
const TURN: usize = 910;
const FLIP_H: usize = 914;
const FLIP_V: usize = 915;
const PERSIST_TURN: usize = 916;
const SLIDESHOW: usize = 920;
/// Slideshow ▸ 921 + preset index.
const SLIDE_INTERVAL: usize = 921;
const DELETE: usize = 940;
const MOVE_TO: usize = 1000;

/// Whether the (logical) right button is down right now: the menu closed on
/// a right press. GetAsyncKeyState reports physical buttons, hence the swap.
pub fn right_button_down() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
    let swapped = unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0;
    let button = if swapped { VK_LBUTTON } else { VK_RBUTTON };
    unsafe { GetAsyncKeyState(i32::from(button.0)) < 0 }
}

/// Windows' folder picker, owned by `hwnd`, starting in `start`. None if
/// cancelled.
pub fn pick_folder(hwnd: HWND, title: &str, start: Option<&Path>) -> Option<PathBuf> {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree};
    use windows::Win32::UI::Shell::{FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED); // the UI thread's apartment, if not yet set
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog.SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        let _ = dialog.SetTitle(&HSTRING::from(title));
        if let Some(start) = start {
            if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(start.as_os_str()), None) {
                let _ = dialog.SetFolder(&item);
            }
        }
        dialog.Show(Some(hwnd)).ok()?;
        let name = dialog.GetResult().ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(name.0.cast()));
        path
    }
}

/// A colour swatch for a menu item: the window's colour with a darker rim.
unsafe fn swatch(color: usize) -> HBITMAP {
    let size = unsafe { GetSystemMetrics(SM_CXMENUCHECK) }.max(12) as usize;
    let [r, g, b] = PALETTE[color % PALETTE.len()].1;
    let bgra = |k: f32| [(b * k * 255.0) as u8, (g * k * 255.0) as u8, (r * k * 255.0) as u8, 255];
    let (fill, rim) = (bgra(1.0), bgra(0.72));
    let mut pixels = Vec::with_capacity(size * size * 4);
    for y in 0..size {
        for x in 0..size {
            let edge = x == 0 || y == 0 || x == size - 1 || y == size - 1;
            pixels.extend_from_slice(if edge { &rim } else { &fill });
        }
    }
    unsafe { CreateBitmap(size as i32, size as i32, 1, 32, Some(pixels.as_ptr().cast())) }
}

/// Shows the menu at the cursor and returns what was chosen. `targets` are
/// the other windows; `can_split` whether the active tab can leave for a
/// new window (not when it is the window's only tab, nor with tabs off);
/// `lock` this window's Tab Lock (None: no tabs, no item); `favorites` the
/// favourites folder, if chosen; `turn` this tab's Rotate ▸ choice;
/// `slideshow` whether one is running.
#[allow(clippy::too_many_arguments)]
pub fn show(
    hwnd: HWND,
    prefs: Prefs,
    targets: &[Target],
    can_split: bool,
    lock: Option<bool>,
    global: Global,
    favorites: Option<&Path>,
    turn: Turn,
    slideshow: bool,
) -> Option<Choice> {
    let (main, sort) = (main_items(), sort_items());
    let mut swatches = Vec::new();
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        let sort_menu = CreatePopupMenu().ok()?;
        let move_menu = CreatePopupMenu().ok()?;
        let prefs_menu = CreatePopupMenu().ok()?;
        let after_main = append(menu, &main, &prefs, 1);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_POPUP, sort_menu.0 as usize, w!("Sort by"));
        append(sort_menu, &sort, &prefs, after_main);
        let checked = |on: bool| if on { MF_STRING | MF_CHECKED } else { MF_STRING };

        // Rotate ▸ this tab's turn (radio-like), flips, Persist.
        let rotate_menu = CreatePopupMenu().ok()?;
        let turns = [(0, w!("Do not rotate\tAlt+Up")), (3, w!("Rotate 90° counter-clockwise\tAlt+Left")), (1, w!("Rotate 90° clockwise\tAlt+Right")), (2, w!("180°\tAlt+Down"))];
        for (quarter, text) in turns {
            let _ = AppendMenuW(rotate_menu, checked(turn.quarter == quarter), TURN + usize::from(quarter), text);
        }
        let _ = AppendMenuW(rotate_menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(rotate_menu, MF_STRING | MF_GRAYED, 0, w!("Next rotation\tCtrl+R"));
        let _ = AppendMenuW(rotate_menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(rotate_menu, checked(turn.flip_h), FLIP_H, w!("Flip horizontally\tAlt+H"));
        let _ = AppendMenuW(rotate_menu, checked(turn.flip_v), FLIP_V, w!("Flip vertically\tAlt+V"));
        let _ = AppendMenuW(rotate_menu, MF_STRING | MF_GRAYED, 0, w!("Next flip\tAlt+F"));
        let _ = AppendMenuW(rotate_menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(rotate_menu, checked(prefs.persist_turn), PERSIST_TURN, w!("Persist"));
        let _ = AppendMenuW(menu, MF_POPUP, rotate_menu.0 as usize, w!("Rotate"));

        // Slideshow ▸ start/stop, intervals, fine steps.
        let slide_menu = CreatePopupMenu().ok()?;
        let start = if slideshow { w!("Stop\tCtrl+G") } else { w!("Start\tCtrl+G") };
        let _ = AppendMenuW(slide_menu, MF_STRING, SLIDESHOW, start);
        let _ = AppendMenuW(slide_menu, MF_SEPARATOR, 0, None);
        for (i, ms) in SLIDE_PRESETS.iter().enumerate() {
            let text = HSTRING::from(if *ms < 1000 { format!("{ms} ms") } else { format!("{} s", ms / 1000) });
            let _ = AppendMenuW(slide_menu, checked(prefs.slide_ms == *ms), SLIDE_INTERVAL + i, &text);
        }
        if !SLIDE_PRESETS.contains(&prefs.slide_ms) {
            let text = HSTRING::from(format!("{} ms (set with Ctrl+[ ])", prefs.slide_ms));
            let _ = AppendMenuW(slide_menu, MF_STRING | MF_CHECKED | MF_GRAYED, 0, &text);
        }
        let _ = AppendMenuW(slide_menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(slide_menu, MF_STRING | MF_GRAYED, 0, w!("Shorter / longer\tCtrl+[  ]"));
        let _ = AppendMenuW(menu, MF_POPUP, slide_menu.0 as usize, w!("Slideshow"));

        // Move to Window: up to three destinations stand in the main menu as
        // a block of "Move to …" items, more go into a submenu; none, nothing.
        // Each section below has its own separator, only when present.
        let destinations = targets.len() + usize::from(can_split);
        if destinations > 0 {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        }
        let inline = destinations < 4;
        let (into, prefix) = if inline { (menu, "Move to ") } else { (move_menu, "") };
        for (i, target) in targets.iter().enumerate() {
            let text = HSTRING::from(format!("{prefix}{}   {}", PALETTE[target.color % PALETTE.len()].0, target.label));
            let _ = AppendMenuW(into, MF_STRING, MOVE_TO + i, &text);
            let bitmap = swatch(target.color);
            let info = MENUITEMINFOW { cbSize: size_of::<MENUITEMINFOW>() as u32, fMask: MIIM_BITMAP, hbmpItem: bitmap, ..Default::default() };
            let _ = SetMenuItemInfoW(into, (MOVE_TO + i) as u32, false, &info);
            swatches.push(bitmap);
        }
        if can_split {
            if !targets.is_empty() && !inline {
                let _ = AppendMenuW(into, MF_SEPARATOR, 0, None);
            }
            let _ = AppendMenuW(into, MF_STRING, NEW_WINDOW, if inline { w!("Move to New Window") } else { w!("New Window") });
        }
        let attached = !inline;
        if attached {
            let _ = AppendMenuW(menu, MF_POPUP, move_menu.0 as usize, w!("Move to Window"));
        } else {
            let _ = DestroyMenu(move_menu);
        }
        if let Some(locked) = lock {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(menu, checked(locked), LOCK, w!("Tab Lock\tL"));
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, DELETE, w!("Delete\tDel"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(prefs_menu, checked(global.tabs), OPEN_IN_TABS, w!("Open Files in Tabs"));
        let _ = AppendMenuW(prefs_menu, checked(global.multirow), MULTIROW, w!("Multi-row Tabs"));
        let _ = AppendMenuW(prefs_menu, MF_SEPARATOR, 0, None);
        let folder = match favorites {
            Some(folder) => HSTRING::from(format!("Favorites Folder: {}…", folder.display())),
            None => HSTRING::from("Choose Favorites Folder…"),
        };
        let _ = AppendMenuW(prefs_menu, MF_STRING, FAVORITES, &folder);
        let _ = AppendMenuW(menu, MF_POPUP, prefs_menu.0 as usize, w!("Preferences"));

        let mut at = POINT::default();
        let _ = GetCursorPos(&mut at);
        // Items are chosen with the left button only (no TPM_RIGHTBUTTON):
        // a second right click elsewhere just closes the menu.
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD, at.x, at.y, None, hwnd, None).0 as usize;
        let _ = DestroyMenu(menu); // and its submenus
        for bitmap in swatches {
            let _ = DeleteObject(bitmap.into());
        }
        match chosen {
            0 if right_button_down() => Some(Choice::Reopen),
            0 => None,
            NEW_WINDOW => Some(Choice::NewWindow),
            LOCK => Some(Choice::Lock),
            OPEN_IN_TABS => Some(Choice::Global(Global { tabs: !global.tabs, ..global })),
            MULTIROW => Some(Choice::Global(Global { multirow: !global.multirow, ..global })),
            FAVORITES => Some(Choice::ChooseFavorites),
            id if (TURN..TURN + 4).contains(&id) => Some(Choice::Turn(Turn { quarter: (id - TURN) as u8, ..turn })),
            FLIP_H => Some(Choice::Turn(Turn { flip_h: !turn.flip_h, ..turn })),
            FLIP_V => Some(Choice::Turn(Turn { flip_v: !turn.flip_v, ..turn })),
            PERSIST_TURN => Some(Choice::PersistTurn),
            SLIDESHOW => Some(Choice::Slideshow),
            id if (SLIDE_INTERVAL..SLIDE_INTERVAL + SLIDE_PRESETS.len()).contains(&id) => Some(Choice::SlideInterval(SLIDE_PRESETS[id - SLIDE_INTERVAL])),
            DELETE => Some(Choice::Delete),
            id if id >= MOVE_TO => Some(Choice::MoveTo(id - MOVE_TO)),
            id => {
                let item = main.iter().chain(sort.iter()).flatten().nth(id - 1)?;
                let mut prefs = prefs;
                (item.choose)(&mut prefs);
                Some(Choice::Prefs(prefs))
            }
        }
    }
}
