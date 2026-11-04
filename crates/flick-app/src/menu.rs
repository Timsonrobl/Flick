//! The right-click menu: Honeyview's View items (without the double-page
//! modes), its render filters, Sort by, Move to Window, and every hotkey
//! as an item beside what it does (the mouse's gestures listed). A native popup
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
use crate::keys::{self, Command};
use crate::settings::{Global, Prefs};
use crate::view::{Filter, Fit, Turn};

/// One menu item: its label, the command whose keys it shows, and what
/// choosing it does to the settings.
struct Item {
    text: &'static str,
    keys: Option<Command>,
    checked: fn(&Prefs) -> bool,
    choose: fn(&mut Prefs),
}

macro_rules! item {
    ($text:expr, $keys:expr, |$p:ident| $checked:expr, |$q:ident| $choose:expr) => {
        Item { text: $text, keys: $keys, checked: |$p: &Prefs| $checked, choose: |$q: &mut Prefs| $choose }
    };
}

/// The main menu's first sections, separated by lines.
fn main_items() -> [Vec<Item>; 2] {
    [
        vec![item!("Stretch Small Images", Some(Command::StretchSmall), |p| p.mode.stretch_small, |p| p.mode.stretch_small = !p.mode.stretch_small)],
        vec![
            // In middle click's order (view.rs).
            item!("Fit in Window", Some(Command::Fit(Fit::Window)), |p| p.mode.fit == Fit::Window, |p| p.mode.fit = Fit::Window),
            item!("Fill Window", Some(Command::Fit(Fit::Fill)), |p| p.mode.fit == Fit::Fill, |p| p.mode.fit = Fit::Fill),
            item!("Fit to Width", Some(Command::Fit(Fit::Width)), |p| p.mode.fit == Fit::Width, |p| p.mode.fit = Fit::Width),
            item!("Fit to Height", Some(Command::Fit(Fit::Height)), |p| p.mode.fit == Fit::Height, |p| p.mode.fit = Fit::Height),
            item!("Original Size (100%)", Some(Command::Fit(Fit::Original)), |p| p.mode.fit == Fit::Original, |p| p.mode.fit = Fit::Original),
        ],
    ]
}

/// The Filter submenu.
fn filter_items() -> [Vec<Item>; 1] {
    [vec![
        item!("No Filter", Some(Command::Filter(Filter::Nearest)), |p| p.filter == Filter::Nearest, |p| p.filter = Filter::Nearest),
        item!("Soften", Some(Command::Filter(Filter::Soften)), |p| p.filter == Filter::Soften, |p| p.filter = Filter::Soften),
        item!("Soften+Sharpen", Some(Command::Filter(Filter::Sharpen)), |p| p.filter == Filter::Sharpen, |p| p.filter = Filter::Sharpen),
    ]]
}

/// The last section's settings item, beside Preferences.
fn info_items() -> [Vec<Item>; 1] {
    [vec![item!("File Info", Some(Command::Info), |p| p.info, |p| p.info = !p.info)]]
}

/// The Sort by submenu's sections.
fn sort_items() -> [Vec<Item>; 2] {
    [
        vec![
            item!("Name", None, |p| p.sort.key == SortKey::Name, |p| p.sort.key = SortKey::Name),
            item!("Date modified", None, |p| p.sort.key == SortKey::Modified, |p| p.sort.key = SortKey::Modified),
            item!("Size", None, |p| p.sort.key == SortKey::Size, |p| p.sort.key = SortKey::Size),
            item!("Type", None, |p| p.sort.key == SortKey::Type, |p| p.sort.key = SortKey::Type),
            item!("Star count", None, |p| p.sort.key == SortKey::Stars, |p| p.sort.key = SortKey::Stars),
            item!("Date starred", None, |p| p.sort.key == SortKey::Starred, |p| p.sort.key = SortKey::Starred),
        ],
        vec![item!("Descending", None, |p| p.sort.descending, |p| p.sort.descending = !p.sort.descending)],
    ]
}

/// A menu item's text: `name`, then `command`'s keys (keys.rs) if any.
fn label(name: &str, command: Option<Command>) -> HSTRING {
    let keys = command.map(keys::keys).unwrap_or_default();
    HSTRING::from(if keys.is_empty() { name.to_string() } else { format!("{name}\t{keys}") })
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
            let _ = unsafe { AppendMenuW(menu, flags, id, &label(item.text, item.keys)) };
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
    /// Preferences ▸ Starred Folder…
    ChooseStarredFolder,
    /// Rotate ▸ a turn for this tab.
    Turn(Turn),
    /// Rotate ▸ Persist.
    PersistTurn,
    /// Slideshow ▸ Start / Stop.
    Slideshow,
    /// Slideshow ▸ an interval (ms): set, and the slideshow runs.
    SlideInterval(u32),
    /// What a hotkey does, chosen from the menu instead.
    Command(Command),
    /// Relaunch to Update: an update is ready (update.rs).
    Relaunch,
}

/// The menu's items that do what a hotkey does (keys.rs): every key is in
/// the menu, beside what it does, to be learnt there. Each command's name;
/// its menu id is COMMAND + its place here.
const COMMANDS: [(Command, &str); 24] = [
    (Command::Next, "Next"),
    (Command::Previous, "Previous"),
    (Command::RandomNext, "Random Next"),
    (Command::RandomBack, "Random Back"),
    (Command::First, "First"),
    (Command::Last, "Last"),
    (Command::NextFolder, "Next Folder"),
    (Command::PreviousFolder, "Previous Folder"),
    (Command::Back, "History Back"),
    (Command::Forward, "History Forward"),
    (Command::NextTab, "Next Tab"),
    (Command::PreviousTab, "Previous Tab"),
    (Command::DuplicateTab, "Duplicate Tab"),
    (Command::DuplicateInNewWindow, "Duplicate in New Window"),
    (Command::CloseTab, "Close Tab"),
    (Command::ReopenClosed, "Reopen Closed"),
    (Command::AddStar, "Add Star"),
    (Command::RemoveStar, "Remove Star"),
    (Command::Copy, "Copy"),
    (Command::ShowInExplorer, "Show in Explorer"),
    (Command::Delete, "Delete"),
    (Command::DebugInfo, "Debug Info"),
    (Command::NextRotation, "Next rotation"),
    (Command::NextFlip, "Next flip"),
];
const COMMAND: usize = 800;

/// Appends `command`'s item to `menu`.
unsafe fn append_command(menu: HMENU, command: Command) {
    if let Some(at) = COMMANDS.iter().position(|(c, _)| *c == command) {
        let _ = unsafe { AppendMenuW(menu, MF_STRING, COMMAND + at, &label(COMMANDS[at].1, Some(command))) };
    }
}

/// Mouse ▸: what the mouse does, for reference (no keys to show).
const MOUSE: [PCWSTR; 9] = [
    w!("Turn Pages\tWheel"),
    w!("Zoom\tCtrl+Wheel"),
    w!("Zoom\tRight-drag"),
    w!("Pan\tDrag"),
    w!("Next View Mode\tMiddle click"),
    w!("Duplicate Tab\tDouble-click"),
    w!("History Back / Forward\tSide buttons"),
    w!("Close Tab\tMiddle click on it"),
    w!("Reorder Tabs\tRight-drag, Ctrl+drag"),
];

/// Slideshow ▸ intervals offered (`[` and `]` adjust in between).
const SLIDE_PRESETS: [u32; 8] = [250, 500, 1000, 2000, 3000, 5000, 10_000, 30_000];

const NEW_WINDOW: usize = 900;
const LOCK: usize = 901;
const OPEN_IN_TABS: usize = 902;
const MULTIROW: usize = 903;
const STARRED_FOLDER: usize = 904;
const PREFETCH: usize = 905;
const UPDATES: usize = 906;
const RELAUNCH: usize = 907;
const FULL_SCREEN: usize = 908;
/// Rotate ▸ 910 + quarter turns, flips, persist.
const TURN: usize = 910;
const FLIP_H: usize = 914;
const FLIP_V: usize = 915;
const PERSIST_TURN: usize = 916;
const SLIDESHOW: usize = 920;
/// Slideshow ▸ 921 + preset index.
const SLIDE_INTERVAL: usize = 921;
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
/// the other windows' colours (chrome::PALETTE indices); `can_split` whether the active tab can leave for a
/// new window (not when it is the window's only tab, nor with tabs off);
/// `lock` this window's Tab Lock (None: one tab, greyed); `starred` the
/// starred folder, if chosen; `turn` this tab's Rotate ▸ choice;
/// `slideshow` whether one is running; `fullscreen` whether the window is;
/// `update` the version of an update ready to relaunch into, first in the
/// menu.
#[allow(clippy::too_many_arguments)]
pub fn show(
    hwnd: HWND,
    prefs: Prefs,
    targets: &[usize],
    can_split: bool,
    lock: Option<bool>,
    global: Global,
    starred: Option<&Path>,
    turn: Turn,
    slideshow: bool,
    fullscreen: bool,
    update: Option<&str>,
) -> Option<Choice> {
    let (main, sort, filter, info) = (main_items(), sort_items(), filter_items(), info_items());
    let mut swatches = Vec::new();
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        let sort_menu = CreatePopupMenu().ok()?;
        let filter_menu = CreatePopupMenu().ok()?;
        let move_menu = CreatePopupMenu().ok()?;
        let prefs_menu = CreatePopupMenu().ok()?;
        if let Some(version) = update {
            let _ = AppendMenuW(menu, MF_STRING, RELAUNCH, &HSTRING::from(format!("Relaunch to Update Flick ({version})")));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        }
        let full_screen = label("Full Screen", Some(Command::FullScreen));
        let _ = AppendMenuW(menu, if fullscreen { MF_STRING | MF_CHECKED } else { MF_STRING }, FULL_SCREEN, &full_screen);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        // Settings items take ids from 1 in this order: main, sort, filter, info.
        let after_main = append(menu, &main, &prefs, 1);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_POPUP, sort_menu.0 as usize, w!("Sort by"));
        let after_sort = append(sort_menu, &sort, &prefs, after_main);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);

        // Navigate ▸ paging, folders, the tab's history.
        let navigate_menu = CreatePopupMenu().ok()?;
        let go = [
            &[Command::Next, Command::Previous, Command::RandomNext, Command::RandomBack, Command::First, Command::Last][..],
            &[Command::NextFolder, Command::PreviousFolder],
            &[Command::Back, Command::Forward],
        ];
        for (i, section) in go.iter().enumerate() {
            if i > 0 {
                let _ = AppendMenuW(navigate_menu, MF_SEPARATOR, 0, None);
            }
            for command in *section {
                append_command(navigate_menu, *command);
            }
        }
        let _ = AppendMenuW(menu, MF_POPUP, navigate_menu.0 as usize, w!("Navigate"));
        let _ = AppendMenuW(menu, MF_POPUP, filter_menu.0 as usize, w!("Filter"));
        let after_filter = append(filter_menu, &filter, &prefs, after_sort);
        let checked = |on: bool| if on { MF_STRING | MF_CHECKED } else { MF_STRING };

        // Rotate ▸ this tab's turn (radio-like), flips, Persist.
        let rotate_menu = CreatePopupMenu().ok()?;
        let turns = [(0, "Do not rotate"), (3, "Rotate 90° counter-clockwise"), (1, "Rotate 90° clockwise"), (2, "180°")];
        for (quarter, name) in turns {
            let _ = AppendMenuW(rotate_menu, checked(turn.quarter == quarter), TURN + usize::from(quarter), &label(name, Some(Command::Turn(quarter))));
        }
        let _ = AppendMenuW(rotate_menu, MF_SEPARATOR, 0, None);
        append_command(rotate_menu, Command::NextRotation);
        let _ = AppendMenuW(rotate_menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(rotate_menu, checked(turn.flip_h), FLIP_H, &label("Flip horizontally", Some(Command::FlipH)));
        let _ = AppendMenuW(rotate_menu, checked(turn.flip_v), FLIP_V, &label("Flip vertically", Some(Command::FlipV)));
        append_command(rotate_menu, Command::NextFlip);
        let _ = AppendMenuW(rotate_menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(rotate_menu, checked(prefs.persist_turn), PERSIST_TURN, w!("Persist"));
        let _ = AppendMenuW(menu, MF_POPUP, rotate_menu.0 as usize, w!("Rotate"));

        // Slideshow ▸ start/stop, intervals, fine steps.
        let slide_menu = CreatePopupMenu().ok()?;
        let start = label(if slideshow { "Stop" } else { "Start" }, Some(Command::Slideshow));
        let _ = AppendMenuW(slide_menu, MF_STRING, SLIDESHOW, &start);
        let (shorter, longer) = (keys::keys(Command::SlideShorter), keys::keys(Command::SlideLonger));
        let _ = AppendMenuW(slide_menu, MF_SEPARATOR, 0, None);
        for (i, ms) in SLIDE_PRESETS.iter().enumerate() {
            let text = HSTRING::from(if *ms < 1000 { format!("{ms} ms") } else { format!("{} s", ms / 1000) });
            let _ = AppendMenuW(slide_menu, checked(prefs.slide_ms == *ms), SLIDE_INTERVAL + i, &text);
        }
        if !SLIDE_PRESETS.contains(&prefs.slide_ms) {
            let text = HSTRING::from(format!("{} ms (set with {shorter}, {longer})", prefs.slide_ms));
            let _ = AppendMenuW(slide_menu, MF_STRING | MF_CHECKED | MF_GRAYED, 0, &text);
        }
        let _ = AppendMenuW(slide_menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(slide_menu, MF_STRING | MF_GRAYED, 0, &HSTRING::from(format!("Shorter / longer\t{shorter}, {longer}")));
        let _ = AppendMenuW(menu, MF_POPUP, slide_menu.0 as usize, w!("Slideshow"));

        // Tabs ▸ switching, duplicating, closing and reopening, Tab Lock
        // (greyed with one tab: nothing to lock to).
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let tabs_menu = CreatePopupMenu().ok()?;
        append_command(tabs_menu, Command::NextTab);
        append_command(tabs_menu, Command::PreviousTab);
        let tab_keys = format!("Tab 1 to 9\t{} … {}", keys::keys(Command::Tab(0)), keys::keys(Command::Tab(8)));
        let _ = AppendMenuW(tabs_menu, MF_STRING | MF_GRAYED, 0, &HSTRING::from(tab_keys));
        let _ = AppendMenuW(tabs_menu, MF_SEPARATOR, 0, None);
        for command in [Command::DuplicateTab, Command::DuplicateInNewWindow, Command::CloseTab, Command::ReopenClosed] {
            append_command(tabs_menu, command);
        }
        let _ = AppendMenuW(tabs_menu, MF_SEPARATOR, 0, None);
        let lock_flags = match lock {
            Some(locked) => checked(locked),
            None => MF_STRING | MF_GRAYED,
        };
        let _ = AppendMenuW(tabs_menu, lock_flags, LOCK, &label("Tab Lock", Some(Command::TabLock)));
        let _ = AppendMenuW(menu, MF_POPUP, tabs_menu.0 as usize, w!("Tabs"));

        // Move to Window, beside Tabs: up to three destinations stand in the
        // main menu as a block of "Move to …" items, more go into a submenu.
        let destinations = targets.len() + usize::from(can_split);
        let inline = destinations < 4;
        let (into, prefix) = if inline { (menu, "Move to ") } else { (move_menu, "") };
        for (i, &color) in targets.iter().enumerate() {
            let text = HSTRING::from(format!("{prefix}{} Window", PALETTE[color % PALETTE.len()].0));
            let _ = AppendMenuW(into, MF_STRING, MOVE_TO + i, &text);
            let bitmap = swatch(color);
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
        // The picture's file: stars, copying, Explorer, deleting.
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        for command in [Command::AddStar, Command::RemoveStar, Command::Copy, Command::ShowInExplorer, Command::Delete] {
            append_command(menu, command);
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        append(menu, &info, &prefs, after_filter);
        let _ = AppendMenuW(prefs_menu, checked(global.tabs), OPEN_IN_TABS, w!("Open Files in Tabs"));
        let _ = AppendMenuW(prefs_menu, checked(global.multirow), MULTIROW, w!("Multi-row Tabs"));
        let _ = AppendMenuW(prefs_menu, checked(global.prefetch), PREFETCH, w!("Load Full Resolution Ahead"));
        let _ = AppendMenuW(prefs_menu, checked(global.updates), UPDATES, w!("Update Automatically"));
        let _ = AppendMenuW(prefs_menu, MF_SEPARATOR, 0, None);
        let folder = match starred {
            Some(folder) => HSTRING::from(format!("Starred Folder: {}…", folder.display())),
            None => HSTRING::from("Choose Starred Folder…"),
        };
        let _ = AppendMenuW(prefs_menu, MF_STRING, STARRED_FOLDER, &folder);
        let _ = AppendMenuW(prefs_menu, MF_SEPARATOR, 0, None);
        let mouse_menu = CreatePopupMenu().ok()?;
        for text in MOUSE {
            let _ = AppendMenuW(mouse_menu, MF_STRING | MF_GRAYED, 0, text);
        }
        let _ = AppendMenuW(prefs_menu, MF_POPUP, mouse_menu.0 as usize, w!("Mouse"));
        let _ = AppendMenuW(prefs_menu, MF_SEPARATOR, 0, None);
        append_command(prefs_menu, Command::DebugInfo);
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
            PREFETCH => Some(Choice::Global(Global { prefetch: !global.prefetch, ..global })),
            UPDATES => Some(Choice::Global(Global { updates: !global.updates, ..global })),
            RELAUNCH => Some(Choice::Relaunch),
            FULL_SCREEN => Some(Choice::Command(Command::FullScreen)),
            STARRED_FOLDER => Some(Choice::ChooseStarredFolder),
            id if (TURN..TURN + 4).contains(&id) => Some(Choice::Turn(Turn { quarter: (id - TURN) as u8, ..turn })),
            FLIP_H => Some(Choice::Turn(Turn { flip_h: !turn.flip_h, ..turn })),
            FLIP_V => Some(Choice::Turn(Turn { flip_v: !turn.flip_v, ..turn })),
            PERSIST_TURN => Some(Choice::PersistTurn),
            SLIDESHOW => Some(Choice::Slideshow),
            id if (SLIDE_INTERVAL..SLIDE_INTERVAL + SLIDE_PRESETS.len()).contains(&id) => Some(Choice::SlideInterval(SLIDE_PRESETS[id - SLIDE_INTERVAL])),
            id if (COMMAND..COMMAND + COMMANDS.len()).contains(&id) => Some(Choice::Command(COMMANDS[id - COMMAND].0)),
            id if id >= MOVE_TO => Some(Choice::MoveTo(id - MOVE_TO)),
            id => {
                let item = main.iter().chain(&sort).chain(&filter).chain(&info).flatten().nth(id - 1)?;
                let mut prefs = prefs;
                (item.choose)(&mut prefs);
                Some(Choice::Prefs(prefs))
            }
        }
    }
}
