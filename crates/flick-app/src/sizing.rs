//! Resizing a window by its edge or corner stops for a moment where the
//! window takes the picture's shape, so a window with no border around the
//! picture is easy to hit (as zooming stops at 100 %). The edge stays put
//! while the cursor travels through a short band, then follows it again.
//! Each dimension has its own stop: on a corner drag the width stops where
//! the side bars go and the height where the top and bottom ones go, each
//! for the cursor moving along it.
//!
//! A window a pixel off the picture's shape would leave a hairline of
//! background along the picture; the fit fills such a window instead
//! (`View::scale_for`), so the whole pixels a stop lands on show no bars.
//!
//! Windows' own resize loop sends WM_SIZING with the rectangle it is about
//! to apply on every mouse move; a subclass of the window changes that
//! rectangle, so nothing is drawn at the wrong size first. winit handles
//! WM_SIZING only for resize increments, which Flick doesn't use.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetWindowRect, WM_ENTERSIZEMOVE, WM_NCDESTROY, WM_SIZING, WMSZ_BOTTOM, WMSZ_BOTTOMLEFT, WMSZ_LEFT, WMSZ_RIGHT, WMSZ_TOP, WMSZ_TOPLEFT,
    WMSZ_TOPRIGHT,
};

/// How far either side of the picture's shape the edge holds, in 96-dpi px.
const BAND: f32 = 12.0;

/// The shape a window's edge stops at, shared between its viewer (which
/// sets it whenever it draws) and the window's subclass (which reads it
/// during a resize). Zero: no stop.
#[derive(Clone, Default)]
pub struct Shape(Arc<AtomicU64>);

impl Shape {
    /// The picture fills a window `aspect` (width / height) wide, up to
    /// `cap` px wide (0: at any size; a small picture that isn't enlarged
    /// leaves a border past its own size whatever the window's shape).
    pub fn set(&self, aspect: f32, cap: u32) {
        let aspect = if aspect.is_finite() && aspect > 0.0 { aspect } else { 0.0 };
        self.0.store((u64::from(aspect.to_bits()) << 32) | u64::from(cap), Ordering::Relaxed);
    }

    pub fn clear(&self) {
        self.0.store(0, Ordering::Relaxed);
    }

}

/// The aspect and cap a Shape holds.
fn read(shape: &AtomicU64) -> (f32, u32) {
    let packed = shape.load(Ordering::Relaxed);
    (f32::from_bits((packed >> 32) as u32), packed as u32)
}

const ID: usize = 0x666c_6b73; // "flks"

/// Hooks the window's resize loop; the hook goes with the window.
pub fn install(hwnd: HWND, shape: &Shape) {
    let data = Arc::into_raw(shape.0.clone()) as usize;
    if !unsafe { SetWindowSubclass(hwnd, Some(proc), ID, data) }.as_bool() {
        drop(unsafe { Arc::from_raw(data as *const AtomicU64) });
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, data: usize) -> LRESULT {
    match msg {
        WM_ENTERSIZEMOVE => DRAG.set(Drag::default()),
        WM_SIZING => {
            // A panic must not cross into Windows' resize loop.
            let _ = std::panic::catch_unwind(|| {
                let shape = unsafe { &*(data as *const AtomicU64) };
                let rect = unsafe { &mut *(lparam.0 as *mut RECT) };
                on_sizing(hwnd, wparam.0 as u32, rect, read(shape));
            });
        }
        WM_NCDESTROY => unsafe {
            let _ = RemoveWindowSubclass(hwnd, Some(proc), ID);
            drop(Arc::from_raw(data as *const AtomicU64));
        },
        _ => {}
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

fn on_sizing(hwnd: HWND, edge: u32, rect: &mut RECT, (aspect, cap): (f32, u32)) {
    if aspect <= 0.0 {
        return;
    }
    // The window's frame, if it has one (Flick's are frameless: none).
    let (mut outer, mut inner) = (RECT::default(), RECT::default());
    if unsafe { GetWindowRect(hwnd, &mut outer).is_err() || GetClientRect(hwnd, &mut inner).is_err() } {
        return;
    }
    let frame = ((outer.right - outer.left) - inner.right, (outer.bottom - outer.top) - inner.bottom);
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32 / 96.0;
    let band = (BAND * dpi).round() as i32;
    let least = ((crate::chrome::MIN_WIDTH * dpi).ceil() as i32, (crate::chrome::MIN_HEIGHT * dpi).ceil() as i32);
    let size = ((rect.right - rect.left) - frame.0, (rect.bottom - rect.top) - frame.1);
    let mut drag = DRAG.get();
    let snapped = snap(edge, size, &mut drag, aspect, cap, band, least);
    DRAG.set(drag);
    let Some((width, height)) = snapped else { return };
    let (width, height) = (width + frame.0, height + frame.1);
    if matches!(edge, WMSZ_LEFT | WMSZ_TOPLEFT | WMSZ_BOTTOMLEFT) {
        rect.left = rect.right - width;
    } else {
        rect.right = rect.left + width;
    }
    if matches!(edge, WMSZ_TOP | WMSZ_TOPLEFT | WMSZ_TOPRIGHT) {
        rect.top = rect.bottom - height;
    } else {
        rect.bottom = rect.top + height;
    }
}

/// Which dimension a stop holds: the width (the side bars just gone) or the
/// height (the top and bottom ones).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Axis {
    Width,
    Height,
}

/// The resize under way (one at a time: the hook runs on the UI thread).
#[derive(Clone, Copy, Default)]
struct Drag {
    /// The size Windows proposed last, for the cursor's direction.
    last: Option<(i32, i32)>,
    /// A corner drag held by this dimension's stop.
    hold: Option<Axis>,
}

thread_local! {
    static DRAG: std::cell::Cell<Drag> = std::cell::Cell::new(Drag::default());
}

/// The client size to use instead of `size` while dragging `edge` (a WMSZ_
/// value), if a dimension is within `band` px of where the picture leaves
/// no bars. Each dimension has its own stop: a side's is the dimension it
/// moves; a corner's, the dimension the cursor is moving along as it comes
/// to the stop (across: the width holds; up or down: the height), kept until
/// that dimension leaves its band. None past the band, past `cap`, or below
/// the least size.
fn snap(edge: u32, (width, height): (i32, i32), drag: &mut Drag, aspect: f32, cap: u32, band: i32, least: (i32, i32)) -> Option<(i32, i32)> {
    let target = |axis| match axis {
        Axis::Width => ((height as f32 * aspect).round() as i32, height),
        Axis::Height => (width, (width as f32 / aspect).round() as i32),
    };
    let holds = |axis: Axis| {
        let (w, h) = target(axis);
        let off = if axis == Axis::Width { (width - w).abs() } else { (height - h).abs() };
        off <= band && w >= least.0 && h >= least.1 && (cap == 0 || w <= cap as i32)
    };
    // How far the cursor moved along each dimension, in the same units (a
    // pixel of height moves the window `aspect` px of width off the shape).
    let moved = drag.last.map(|(w, h)| ((width - w).abs() as f32, (height - h).abs() as f32 * aspect));
    drag.last = Some((width, height));
    let axis = match edge {
        WMSZ_LEFT | WMSZ_RIGHT => Some(Axis::Width),
        WMSZ_TOP | WMSZ_BOTTOM => Some(Axis::Height),
        _ => drag.hold.filter(|&axis| holds(axis)).or_else(|| moved.map(|(across, down)| if across >= down { Axis::Width } else { Axis::Height })),
    }
    .filter(|&axis| holds(axis));
    drag.hold = axis;
    axis.map(target).filter(|&size| size != (width, height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::WMSZ_BOTTOMRIGHT;

    const LEAST: (i32, i32) = (200, 150);

    /// One move of a fresh drag.
    fn once(edge: u32, size: (i32, i32), aspect: f32, cap: u32) -> Option<(i32, i32)> {
        snap(edge, size, &mut Drag::default(), aspect, cap, 12, LEAST)
    }

    /// The sizes shown along a drag through `sizes` (what Windows proposes).
    fn drag(edge: u32, sizes: impl Iterator<Item = (i32, i32)>) -> Vec<(i32, i32)> {
        let mut drag = Drag::default();
        sizes.map(|size| snap(edge, size, &mut drag, 1.5, 0, 12, LEAST).unwrap_or(size)).collect()
    }

    #[test]
    fn a_side_holds_at_the_pictures_shape() {
        // A 3:2 picture, 600 px high: the match is 900 wide.
        assert_eq!(once(WMSZ_RIGHT, (905, 600), 1.5, 0), Some((900, 600)), "just past it");
        assert_eq!(once(WMSZ_LEFT, (889, 600), 1.5, 0), Some((900, 600)), "just short of it");
        assert_eq!(once(WMSZ_RIGHT, (913, 600), 1.5, 0), None, "past the band it lets go");
        assert_eq!(once(WMSZ_RIGHT, (900, 600), 1.5, 0), None, "already there");
        assert_eq!(once(WMSZ_BOTTOM, (900, 590), 1.5, 0), Some((900, 600)), "top and bottom change the height");
        assert_eq!(once(WMSZ_TOP, (900, 640), 1.5, 0), None);
    }

    #[test]
    fn a_corner_stops_each_dimension_on_its_own() {
        // Across, 600 high: the width holds at 900 from 888 to 912, and the
        // height stays where the cursor has it (moving the height instead
        // would make the window jump the way the cursor isn't going).
        let shown = drag(WMSZ_BOTTOMRIGHT, (880..=920).step_by(4).map(|w| (w, 600)));
        assert!(shown.iter().all(|&(_, h)| h == 600), "{shown:?}");
        let widths: Vec<i32> = shown.iter().map(|&(w, _)| w).collect();
        assert_eq!(widths, [880, 884, 900, 900, 900, 900, 900, 900, 900, 916, 920]);
        // Down, 900 wide: the height holds at 600 from 588 to 612, the width stays.
        let shown = drag(WMSZ_TOPLEFT, (580..=620).step_by(4).map(|h| (900, h)));
        assert!(shown.iter().all(|&(w, _)| w == 900), "{shown:?}");
        let heights: Vec<i32> = shown.iter().map(|&(_, h)| h).collect();
        assert_eq!(heights, [580, 584, 600, 600, 600, 600, 600, 600, 600, 616, 620]);
    }

    #[test]
    fn no_stop_where_the_picture_cant_fill_the_window() {
        // A 400 x 300 picture not enlarged: past 400 wide it has a border anyway.
        assert_eq!(once(WMSZ_RIGHT, (390, 300), 4.0 / 3.0, 400), Some((400, 300)));
        assert_eq!(once(WMSZ_RIGHT, (540, 400), 4.0 / 3.0, 400), None);
        // Nor below the window's least size.
        assert_eq!(once(WMSZ_BOTTOM, (210, 150), 1.5, 0), None, "140 high is too low");
    }
}
