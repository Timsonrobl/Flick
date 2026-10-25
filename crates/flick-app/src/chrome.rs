//! Hover chrome (ARCHITECTURE.md §10): a top bar that fades in when the
//! cursor comes within twice its height of the window's top edge, as in
//! Honeyview. Title on the left, window buttons on the right; dragging the
//! title moves the window. It is drawn over the picture, never beside it, and
//! its text resources are created on first appearance, off the open path.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ROUNDED_RECT, ID2D1DeviceContext, ID2D1SolidColorBrush};
use windows::Win32::Graphics::DirectWrite::*;
use windows::core::w;
use windows_numerics::Vector2;

/// Bar height and window-button width at 100 % display scaling.
const BAR: f32 = 36.0;
const BUTTON: f32 = 46.0;
/// The bar appears when the cursor is within this many bar heights of the top.
const APPEAR: f32 = 2.0;
const FADE_IN_S: f32 = 0.10;
const FADE_OUT_S: f32 = 0.20;

/// Window colours (ARCHITECTURE.md §10a): each window's hover bar has its
/// own deep jewel tone, so windows are easy to tell apart (and to pick in
/// Move to Window), quiet next to the dark picture area. Light text (`INK`).
pub const PALETTE: [(&str, [f32; 3]); 8] = [
    ("Sapphire", [0.180, 0.290, 0.420]),
    ("Emerald", [0.184, 0.365, 0.310]),
    ("Garnet", [0.420, 0.227, 0.180]),
    ("Amethyst", [0.290, 0.235, 0.420]),
    ("Topaz", [0.420, 0.353, 0.137]),
    ("Ruby", [0.420, 0.180, 0.290]),
    ("Teal", [0.141, 0.345, 0.369]),
    ("Smoke", [0.310, 0.290, 0.247]),
];

/// The bar's text and glyphs.
const INK: [f32; 3] = [0.93, 0.93, 0.93];

/// `a` moved toward `b` by `t` (0: `a`, 1: `b`).
fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

/// The background behind each colour's pictures: a slightly different dark
/// grey per window (same order as PALETTE), so overlapping windows are told
/// apart even where the bar is hidden.
pub const BACKGROUNDS: [f32; 8] = [0.080, 0.115, 0.055, 0.100, 0.135, 0.065, 0.125, 0.090];

/// Tab width in the strip at 100 % scaling: at most, and the least before
/// tabs wrap onto another row.
const TAB_MAX: f32 = 220.0;
const TAB_MIN: f32 = 150.0;
const MARGIN: f32 = 8.0;

/// The tab strip's grid, every tab `tab` wide and each row a bar high. The
/// first row shares the bar with the padlock and the window buttons: `first`
/// tabs from `left`. The rows below it use the window's full width: `per_row`
/// tabs each from `lower_left`.
#[derive(Clone, Copy, Debug)]
pub struct Strip {
    left: f32,
    lower_left: f32,
    tab: f32,
    first: usize,
    per_row: usize,
    rows: usize,
    row: f32,
}

impl Strip {
    /// Row and column of strip position `i`.
    fn cell(&self, i: usize) -> (usize, usize) {
        if i < self.first { (0, i) } else { (1 + (i - self.first) / self.per_row, (i - self.first) % self.per_row) }
    }

    /// Where row `row` starts and how many tabs it holds.
    fn row_span(&self, row: usize) -> (f32, usize) {
        if row == 0 { (self.left, self.first) } else { (self.lower_left, self.per_row) }
    }

    fn index(&self, row: usize, col: usize) -> usize {
        if row == 0 { col } else { self.first + (row - 1) * self.per_row + col }
    }

    /// Top-left corner of the tab at strip position `i`.
    pub fn origin(&self, i: usize) -> (f32, f32) {
        let (row, col) = self.cell(i);
        (self.row_span(row).0 + self.tab * col as f32, self.row * row as f32)
    }

    /// Whether tab `i` starts a row (no rule before it).
    fn starts_row(&self, i: usize) -> bool {
        self.cell(i).1 == 0
    }

    /// The position of the cell nearest a tab drawn with its top-left at
    /// (x, y): where a dragged tab goes.
    pub fn nearest(&self, (x, y): (f32, f32), count: usize) -> usize {
        let row = (y / self.row).round().clamp(0.0, (self.rows - 1) as f32) as usize;
        let (left, cols) = self.row_span(row);
        let col = ((x - left) / self.tab).round().clamp(0.0, (cols - 1) as f32) as usize;
        self.index(row, col).min(count.saturating_sub(1))
    }

    /// The tab under (x, y), if any.
    fn at(&self, (x, y): (f32, f32), count: usize) -> Option<usize> {
        if y < 0.0 {
            return None;
        }
        let row = (y / self.row).floor() as usize;
        let (left, cols) = self.row_span(row);
        let col = ((x - left) / self.tab).floor();
        let i = self.index(row, col as usize);
        (x >= left && (col as usize) < cols && i < count).then_some(i)
    }

    /// Where a dragged tab may be drawn: within the grid.
    fn clamp(&self, (x, y): (f32, f32)) -> (f32, f32) {
        let y = y.clamp(0.0, self.row * (self.rows - 1) as f32);
        let (left, cols) = self.row_span((y / self.row).round() as usize);
        (x.clamp(left, left + self.tab * (cols - 1) as f32), y)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Minimize,
    Maximize,
    Close,
    Title,
    /// A tab in the strip, by position (only with two or more tabs).
    Tab(usize),
    /// The Tab Lock padlock (a click unlocks).
    Lock,
}

pub struct Chrome {
    scale: f32,
    opacity: f32,
    target: f32,
    /// Set while a fade is running.
    last_tick: Option<Instant>,
    hover: Option<Hit>,
    text: Option<(IDWriteTextFormat, f32)>,
    /// Tabs in the strip; with one, there is no strip (just the title).
    tabs: usize,
    /// The window's colour, an index into PALETTE.
    color: usize,
    /// Where each tab (by id) is drawn: tabs glide to their slots when the
    /// order changes, like Chrome's.
    positions: HashMap<u64, (f32, f32)>,
    /// The tab being dragged along the strip and its top-left corner.
    drag: Option<(u64, (f32, f32))>,
    /// When tabs last moved, while some are still gliding.
    glide_tick: Option<Instant>,
    locked: bool,
    /// The cursor is in the bar's zone.
    near: bool,
    /// Shown without the cursor until then (a tab switch from the keys).
    reveal_until: Option<Instant>,
    /// Preferences ▸ Multi-row Tabs: off keeps one row, however narrow.
    multirow: bool,
    /// The cursor rests on a tab (or the title): its full path shows from
    /// then (`TIP_AFTER`), if its name is cut short.
    tip_at: Option<Instant>,
    tip_shown: bool,
    /// The tip's text: wraps, no ellipsis.
    tip_text: Option<(IDWriteTextFormat, f32)>,
}

/// How long the cursor rests on a tab before its full path shows.
const TIP_AFTER: Duration = Duration::from_secs(1);

/// Room for the Tab Lock padlock at 100 % scaling.
const LOCK_WIDTH: f32 = 30.0;

/// Gliding tabs close this fraction of their distance per GLIDE_TAU seconds
/// (exponential easing, frame-rate independent).
const GLIDE_TAU: f32 = 0.05;

impl Chrome {
    pub fn new(scale: f32, color: usize, multirow: bool) -> Chrome {
        Chrome {
            scale,
            opacity: 0.0,
            target: 0.0,
            last_tick: None,
            hover: None,
            text: None,
            tabs: 1,
            color,
            positions: HashMap::new(),
            drag: None,
            glide_tick: None,
            locked: false,
            near: false,
            reveal_until: None,
            multirow,
            tip_at: None,
            tip_shown: false,
            tip_text: None,
        }
    }

    /// When a resting cursor's tip is due (the viewer wakes then, `show_tip`).
    pub fn tip_deadline(&self) -> Option<Instant> {
        self.tip_at
    }

    /// The tip came due. Returns whether the bar must redraw.
    pub fn show_tip(&mut self) -> bool {
        if self.tip_at.is_some_and(|t| t <= Instant::now()) {
            self.tip_at = None;
            self.tip_shown = true;
            return true;
        }
        false
    }

    pub fn set_multirow(&mut self, multirow: bool) {
        self.multirow = multirow;
    }

    /// A tab dragged along the strip: drawn with its top-left corner at the
    /// given point, on top.
    pub fn set_drag(&mut self, drag: Option<(u64, (f32, f32))>) {
        self.drag = drag;
    }

    /// Tab Lock: a padlock at the bar's left, before the tabs or title.
    pub fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
    }

    /// Where the title or the strip starts: after the padlock when locked.
    fn content_left(&self) -> f32 {
        if self.locked { LOCK_WIDTH * self.scale } else { 0.0 }
    }

    /// Tabs are still gliding into place: the caller requests another frame.
    pub fn moving(&self) -> bool {
        self.glide_tick.is_some()
    }

    /// Advances every tab toward its cell (the dragged one sits at the
    /// cursor) and returns their top-left corners, in strip order.
    fn glide(&mut self, tabs: &[(u64, String)], strip: &Strip) -> Vec<(f32, f32)> {
        let now = Instant::now();
        let dt = self.glide_tick.map_or(0.0, |t| (now - t).as_secs_f32().min(0.05));
        let ease = 1.0 - (-dt / GLIDE_TAU).exp();
        let mut moving = false;
        let corners = tabs
            .iter()
            .enumerate()
            .map(|(i, (id, _))| {
                let cell = strip.origin(i);
                let at = match self.drag {
                    Some((dragged, at)) if dragged == *id => strip.clamp(at),
                    _ => {
                        let (x, y) = *self.positions.get(id).unwrap_or(&cell);
                        let (x, y) = (x + (cell.0 - x) * ease, y + (cell.1 - y) * ease);
                        if (cell.0 - x).abs() < 0.5 && (cell.1 - y).abs() < 0.5 { cell } else { (x, y) }
                    }
                };
                moving |= at != cell && self.drag.is_none_or(|(dragged, _)| dragged != *id);
                self.positions.insert(*id, at);
                at
            })
            .collect();
        self.positions.retain(|id, _| tabs.iter().any(|(t, _)| t == id));
        self.glide_tick = moving.then_some(now);
        corners
    }

    pub fn set_tabs(&mut self, tabs: usize) {
        self.tabs = tabs;
    }

    /// The tab strip's grid, if there is a strip (two or more tabs): one row
    /// between the padlock and the buttons while tabs can be at least
    /// TAB_MIN wide (with Multi-row Tabs off, whatever their width). Past
    /// that, tabs of one width fill rows that run the window's full width
    /// below the first; the first holds as many as fit before the buttons.
    pub fn strip(&self, width: f32) -> Option<Strip> {
        if self.tabs < 2 {
            return None;
        }
        let s = self.scale;
        let left = MARGIN * s + self.content_left();
        let room = (width - 3.0 * BUTTON * s - left - MARGIN * s).max(1.0);
        let fit = if self.multirow { (room / (TAB_MIN * s)).floor() as usize } else { self.tabs };
        let first = fit.clamp(1, self.tabs);
        let row = self.bar();
        if first == self.tabs {
            let tab = (room / self.tabs as f32).min(TAB_MAX * s);
            return Some(Strip { left, lower_left: left, tab, first, per_row: first, rows: 1, row });
        }
        // The lower rows set the width, filling the window exactly; the first
        // row takes as many as fit before the buttons, leaving the rest of it
        // free (to drag the window by).
        let full = (width - 2.0 * MARGIN * s).max(1.0);
        let per_row = ((full / (TAB_MIN * s)).floor() as usize).max(1);
        let tab = full / per_row as f32;
        let first = ((room / tab).floor() as usize).clamp(1, first);
        let lower_left = MARGIN * s;
        let rows = 1 + (self.tabs - first).div_ceil(per_row);
        Some(Strip { left, lower_left, tab, first, per_row, rows, row })
    }

    /// The bar's height: a row per row of tabs.
    fn height(&self, width: f32) -> f32 {
        self.strip(width).map_or(1, |strip| strip.rows) as f32 * self.bar()
    }

    pub fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
    }

    fn bar(&self) -> f32 {
        BAR * self.scale
    }

    /// Follows the cursor (`None` when it left the window). Returns whether
    /// the bar needs redrawing.
    pub fn pointer(&mut self, pos: Option<(f32, f32)>, width: f32) -> bool {
        // Within the bar (all its rows), plus the same margin below it as ever.
        let zone = self.height(width) + (APPEAR - 1.0) * self.bar();
        self.near = pos.is_some_and(|(_, y)| y >= 0.0 && y < zone);
        let hover = pos.and_then(|p| self.hit_raw(p, width));
        let mut changed = self.retarget() || (hover != self.hover && self.opacity > 0.0);
        if hover != self.hover {
            // A new place to rest on: its tip waits again; one shown goes.
            changed |= std::mem::take(&mut self.tip_shown);
            self.tip_at = matches!(hover, Some(Hit::Tab(_) | Hit::Title)).then(|| Instant::now() + TIP_AFTER);
        }
        self.hover = hover;
        changed
    }

    /// Fades toward shown while the cursor is near or a reveal lasts.
    /// Returns whether that changed.
    fn retarget(&mut self) -> bool {
        let shown = self.near || self.reveal_until.is_some_and(|t| t > Instant::now());
        let target = if shown { 1.0 } else { 0.0 };
        if target == self.target {
            return false;
        }
        self.target = target;
        self.last_tick = Some(Instant::now());
        true
    }

    /// Shows the bar for `time` without the cursor (e.g. the tab strip
    /// after a tab switch from the keys), then lets it fade.
    pub fn reveal(&mut self, time: Duration) {
        self.reveal_until = Some(Instant::now() + time);
        self.retarget();
    }

    /// When a reveal ends (the viewer wakes then, `end_reveal`).
    pub fn reveal_deadline(&self) -> Option<Instant> {
        self.reveal_until
    }

    /// Ends a reveal that ran out. Returns whether the bar must redraw.
    pub fn end_reveal(&mut self) -> bool {
        if self.reveal_until.is_some_and(|t| t <= Instant::now()) {
            self.reveal_until = None;
            return self.retarget();
        }
        false
    }

    /// What a click at `pos` lands on, if the bar is visible enough to use.
    pub fn hit(&self, pos: (f32, f32), width: f32) -> Option<Hit> {
        if self.opacity < 0.5 { None } else { self.hit_raw(pos, width) }
    }

    fn hit_raw(&self, (x, y): (f32, f32), width: f32) -> Option<Hit> {
        if !(0.0..self.height(width)).contains(&y) {
            return None;
        }
        let from_right = width - x;
        let button = BUTTON * self.scale;
        let first_row = y < self.bar();
        let tab = self.strip(width).and_then(|strip| strip.at((x, y), self.tabs));
        Some(if first_row && from_right < button {
            Hit::Close
        } else if first_row && from_right < 2.0 * button {
            Hit::Maximize
        } else if first_row && from_right < 3.0 * button {
            Hit::Minimize
        } else if first_row && self.locked && x < MARGIN * self.scale + self.content_left() {
            Hit::Lock
        } else if let Some(i) = tab {
            Hit::Tab(i)
        } else {
            Hit::Title
        })
    }

    /// Advances the fade. Returns true while it is still running, so the
    /// caller requests another frame; once it ends, nothing redraws.
    pub fn animate(&mut self) -> bool {
        let Some(last) = self.last_tick else { return false };
        let now = Instant::now();
        let dt = (now - last).as_secs_f32();
        self.last_tick = Some(now);
        self.opacity = if self.target > self.opacity {
            (self.opacity + dt / FADE_IN_S).min(self.target)
        } else {
            (self.opacity - dt / FADE_OUT_S).max(self.target)
        };
        if self.opacity == self.target {
            self.last_tick = None;
        }
        self.last_tick.is_some()
    }

    /// Draws the bar; with two or more `tabs` (labels in strip order) the
    /// strip takes the title's place. `paths`: each tab's file, in strip
    /// order, for the tip of a tab whose name is cut short.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(&mut self, context: &ID2D1DeviceContext, width: f32, title: &str, tabs: &[(u64, String)], paths: &[PathBuf], active: usize, maximized: bool) {
        if self.opacity <= 0.0 {
            // Hidden: tabs settle at once, nothing keeps redrawing.
            self.positions.clear();
            self.glide_tick = None;
            return;
        }
        let strip = self.strip(width).map(|strip| (strip, self.glide(tabs, &strip)));
        let height = self.height(width);
        if self.text.as_ref().is_none_or(|(_, scale)| *scale != self.scale) {
            self.text = text_format(14.0 * self.scale).ok().map(|f| (f, self.scale));
        }
        if self.tip_shown && self.tip_text.as_ref().is_none_or(|(_, scale)| *scale != self.scale) {
            self.tip_text = tip_format(14.0 * self.scale).ok().map(|f| (f, self.scale));
        }
        // The tab (or title) under a resting cursor: what it shows, the
        // room for it, and where the tip goes (left, top).
        let mut rested: Option<(String, f32, (f32, f32), usize)> = None;
        let (s, bar, a) = (self.scale, self.bar(), self.opacity);
        let brush = |r: f32, g: f32, b: f32, alpha: f32| -> Option<ID2D1SolidColorBrush> {
            unsafe { context.CreateSolidColorBrush(&D2D1_COLOR_F { r, g, b, a: alpha * a }, None).ok() }
        };
        // The window's jewel tone; light text and glyphs.
        let tone = PALETTE[self.color % PALETTE.len()].1;
        let shade = |c: [f32; 3]| brush(c[0], c[1], c[2], 1.0);
        let (Some(panel), Some(ink), Some(on_red)) = (brush(tone[0], tone[1], tone[2], 0.95), shade(INK), brush(1.0, 1.0, 1.0, 1.0)) else {
            return;
        };
        let button = BUTTON * s;
        let button_rect = |i: f32| D2D_RECT_F { left: width - button * (i + 1.0), top: 0.0, right: width - button * i, bottom: bar };
        unsafe {
            context.FillRectangle(&D2D_RECT_F { left: 0.0, top: 0.0, right: width, bottom: height }, &panel);

            let hovered = match self.hover {
                Some(Hit::Close) => Some((0.0, brush(0.77, 0.17, 0.11, 1.0))),
                Some(Hit::Maximize) => Some((1.0, brush(1.0, 1.0, 1.0, 0.10))),
                Some(Hit::Minimize) => Some((2.0, brush(1.0, 1.0, 1.0, 0.10))),
                _ => None,
            };
            if let Some((i, Some(fill))) = hovered {
                context.FillRectangle(&button_rect(i), &fill);
            }

            let line = |x0: f32, y0: f32, x1: f32, y1: f32| context.DrawLine(Vector2 { X: x0, Y: y0 }, Vector2 { X: x1, Y: y1 }, &ink, s, None);
            let center = |i: f32| (width - button * (i + 0.5), bar / 2.0);
            let r = 5.0 * s;

            let (cx, cy) = center(0.0); // close: white on its red hover
            let close_ink = if self.hover == Some(Hit::Close) { &on_red } else { &ink };
            context.DrawLine(Vector2 { X: cx - r, Y: cy - r }, Vector2 { X: cx + r, Y: cy + r }, close_ink, s, None);
            context.DrawLine(Vector2 { X: cx - r, Y: cy + r }, Vector2 { X: cx + r, Y: cy - r }, close_ink, s, None);

            let (cx, cy) = center(1.0); // maximize / restore
            if maximized {
                let o = 2.0 * s;
                context.DrawRectangle(&D2D_RECT_F { left: cx - r, top: cy - r + o, right: cx + r - o, bottom: cy + r }, &ink, s, None);
                line(cx - r + o, cy - r + o, cx - r + o, cy - r);
                line(cx - r + o, cy - r, cx + r, cy - r);
                line(cx + r, cy - r, cx + r, cy + r - o);
                line(cx + r, cy + r - o, cx + r - o, cy + r - o);
            } else {
                context.DrawRectangle(&D2D_RECT_F { left: cx - r, top: cy - r, right: cx + r, bottom: cy + r }, &ink, s, None);
            }

            let (cx, cy) = center(2.0); // minimize
            line(cx - r, cy, cx + r, cy);

            let Some((format, _)) = &self.text else { return };
            let text = |label: &str, rect: D2D_RECT_F, brush: &ID2D1SolidColorBrush| {
                let wide: Vec<u16> = label.encode_utf16().collect();
                context.DrawText(&wide, format, &rect, brush, D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL);
            };
            if self.locked {
                // A padlock: shackle over a body, centred in its room.
                let (cx, cy) = (MARGIN * s + LOCK_WIDTH * s / 2.0, bar / 2.0);
                let body = D2D_RECT_F { left: cx - 6.0 * s, top: cy - 1.0 * s, right: cx + 6.0 * s, bottom: cy + 8.0 * s };
                context.FillRectangle(&body, &ink);
                let shackle = D2D_RECT_F { left: cx - 4.0 * s, top: cy - 8.0 * s, right: cx + 4.0 * s, bottom: cy + 2.0 * s };
                context.DrawRoundedRectangle(&D2D1_ROUNDED_RECT { rect: shackle, radiusX: 4.0 * s, radiusY: 4.0 * s }, &ink, 1.6 * s, None);
            }
            match strip {
                None => {
                    let left = 14.0 * s + self.content_left();
                    let rect = D2D_RECT_F { left, top: 0.0, right: (width - 3.0 * button - 14.0 * s).max(left), bottom: bar };
                    text(title, rect, &ink);
                    if self.hover == Some(Hit::Title) {
                        rested = Some((title.to_owned(), rect.right - rect.left, (left, bar), 0));
                    }
                }
                Some((strip, corners)) => {
                    let tab = strip.tab;
                    // The active tab is a brighter shade of the bar's tone;
                    // the others' text quieter (the ink part-way into the
                    // tone); a dragged one is lifted too, and drawn last.
                    let (Some(lifted), Some(quiet), Some(hover_fill), Some(rule)) =
                        (shade(mix(tone, [1.0; 3], 0.2)), shade(mix(INK, tone, 0.4)), brush(1.0, 1.0, 1.0, 0.07), shade(mix(INK, tone, 0.72)))
                    else {
                        return;
                    };
                    let round = |rect: D2D_RECT_F| D2D1_ROUNDED_RECT { rect, radiusX: 4.0 * s, radiusY: 4.0 * s };
                    let pad = 4.0 * s;
                    let dragged = |i: usize| self.drag.is_some_and(|(id, _)| id == tabs[i].0);
                    let mut order: Vec<usize> = (0..tabs.len()).filter(|&i| !dragged(i)).collect();
                    order.extend((0..tabs.len()).filter(|&i| dragged(i)));
                    for i in order {
                        let ((x, y), label) = (corners[i], &tabs[i].1);
                        let cell = D2D_RECT_F { left: x + 1.0 * s, top: y + pad, right: x + tab - 1.0 * s, bottom: y + bar - pad };
                        if i == active || dragged(i) {
                            context.FillRoundedRectangle(&round(cell), &lifted);
                        } else if self.hover == Some(Hit::Tab(i)) && self.drag.is_none() {
                            context.FillRoundedRectangle(&round(cell), &hover_fill);
                        }
                        // A rule between neighbours in a row, not beside the lifted tab.
                        if !strip.starts_row(i) && i != active && i != active + 1 && self.drag.is_none() {
                            context.FillRectangle(&D2D_RECT_F { left: x, top: y + 10.0 * s, right: x + 1.0 * s, bottom: y + bar - 10.0 * s }, &rule);
                        }
                        let rect = D2D_RECT_F { left: x + 10.0 * s, top: y, right: x + tab - 10.0 * s, bottom: y + bar };
                        let shown = if i == active { title } else { label };
                        text(shown, rect, if i == active { &ink } else { &quiet });
                        if self.hover == Some(Hit::Tab(i)) && self.drag.is_none() {
                            rested = Some((shown.to_owned(), rect.right - rect.left, (x, y + bar), i));
                        }
                    }
                }
            }

            // The full path of a tab whose name is cut short, under it.
            let Some((shown, room, (x, top), i)) = rested.filter(|_| self.tip_shown) else { return };
            let (Some(path), Some((tip_format, _))) = (paths.get(i), &self.tip_text) else { return };
            if text_size(format, &shown, f32::MAX).is_none_or(|(w, _)| w <= room + 0.5) {
                return; // it fits
            }
            let (pad_x, pad_y, margin) = (8.0 * s, 5.0 * s, MARGIN * s);
            let full = path.display().to_string();
            let Some((w, h)) = text_size(tip_format, &full, (width - 2.0 * (margin + pad_x)).max(1.0)) else { return };
            let (w, h) = (w + 2.0 * pad_x, h + 2.0 * pad_y);
            let left = x.min(width - margin - w).max(margin);
            let top = top + 2.0 * s;
            let (Some(back), Some(edge)) = (brush(tone[0] * 0.8, tone[1] * 0.8, tone[2] * 0.8, 0.97), shade(mix(INK, tone, 0.6))) else { return };
            let rect = D2D_RECT_F { left, top, right: left + w, bottom: top + h };
            let round = D2D1_ROUNDED_RECT { rect, radiusX: 4.0 * s, radiusY: 4.0 * s };
            context.FillRoundedRectangle(&round, &back);
            context.DrawRoundedRectangle(&round, &edge, s, None);
            let wide: Vec<u16> = full.encode_utf16().collect();
            let inner = D2D_RECT_F { left: left + pad_x, top: top + pad_y, right: left + w - pad_x, bottom: top + h - pad_y };
            context.DrawText(&wide, tip_format, &inner, &ink, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
        }
    }
}

/// How large `text` lays out in `format`, at most `max_width` wide.
fn text_size(format: &IDWriteTextFormat, text: &str, max_width: f32) -> Option<(f32, f32)> {
    unsafe {
        let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).ok()?;
        let wide: Vec<u16> = text.encode_utf16().collect();
        let layout = factory.CreateTextLayout(&wide, format, max_width, f32::MAX).ok()?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        layout.GetMetrics(&mut metrics).ok()?;
        Some((metrics.widthIncludingTrailingWhitespace, metrics.height))
    }
}

/// A tip's text: wraps anywhere (a path has few spaces), never cut short.
fn tip_format(size: f32) -> windows::core::Result<IDWriteTextFormat> {
    unsafe {
        let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let format = factory.CreateTextFormat(w!("Segoe UI"), None, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size, w!(""))?;
        format.SetWordWrapping(DWRITE_WORD_WRAPPING_CHARACTER)?;
        Ok(format)
    }
}

fn text_format(size: f32) -> windows::core::Result<IDWriteTextFormat> {
    unsafe {
        let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let format = factory.CreateTextFormat(w!("Segoe UI"), None, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size, w!(""))?;
        format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        let trimming = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
        let ellipsis = factory.CreateEllipsisTrimmingSign(&format)?;
        format.SetTrimming(&trimming, &ellipsis)?;
        Ok(format)
    }
}
