//! The right-click View menu (Honeyview's, without the double-page modes).
//! A native popup menu: Windows draws it in the system style, and it costs
//! nothing until opened.

use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, MF_CHECKED, MF_SEPARATOR, MF_STRING, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
};
use windows::core::{PCWSTR, w};

use crate::view::{Fit, Mode};

const STRETCH: usize = 1;
const ORIGINAL: usize = 2;
const WINDOW: usize = 3;
const WIDTH: usize = 4;

/// Shows the menu at the cursor and returns the mode chosen, if any.
pub fn show(hwnd: HWND, mode: Mode) -> Option<Mode> {
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        let check = |on: bool| if on { MF_STRING | MF_CHECKED } else { MF_STRING };
        let items: [(usize, PCWSTR, bool); 4] = [
            (STRETCH, w!("Stretch Small Images\tZ"), mode.stretch_small),
            (ORIGINAL, w!("Original Size (100%)\t0"), mode.fit == Fit::Original),
            (WINDOW, w!("Fit in Window\t9"), mode.fit == Fit::Window),
            (WIDTH, w!("Fit to Width\t8"), mode.fit == Fit::Width),
        ];
        for (id, text, on) in items {
            let _ = AppendMenuW(menu, check(on), id, text);
            if id == STRETCH {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            }
        }
        let mut at = POINT::default();
        let _ = GetCursorPos(&mut at);
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, at.x, at.y, None, hwnd, None).0 as usize;
        let _ = DestroyMenu(menu);
        match chosen {
            STRETCH => Some(Mode { stretch_small: !mode.stretch_small, ..mode }),
            ORIGINAL => Some(Mode { fit: Fit::Original, ..mode }),
            WINDOW => Some(Mode { fit: Fit::Window, ..mode }),
            WIDTH => Some(Mode { fit: Fit::Width, ..mode }),
            _ => None,
        }
    }
}
