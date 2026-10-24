//! Hover chrome (ARCHITECTURE.md §10): a top bar that fades in when the
//! cursor comes within twice its height of the window's top edge, as in
//! Honeyview. Title on the left, window buttons on the right; dragging the
//! title moves the window. It is drawn over the picture, never beside it, and
//! its text resources are created on first appearance, off the open path.

use std::time::Instant;

use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{D2D1_DRAW_TEXT_OPTIONS_CLIP, ID2D1DeviceContext, ID2D1SolidColorBrush};
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

/// Tab width in the strip at 100 % scaling: at most, and before squeezing.
const TAB_MAX: f32 = 220.0;
const MARGIN: f32 = 8.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Minimize,
    Maximize,
    Close,
    Title,
    /// A tab in the strip, by position (only with two or more tabs).
    Tab(usize),
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
}

impl Chrome {
    pub fn new(scale: f32) -> Chrome {
        Chrome { scale, opacity: 0.0, target: 0.0, last_tick: None, hover: None, text: None, tabs: 1 }
    }

    pub fn set_tabs(&mut self, tabs: usize) {
        self.tabs = tabs;
    }

    /// The strip's tab width and where it starts, if there is a strip.
    fn tab_layout(&self, width: f32) -> Option<(f32, f32)> {
        if self.tabs < 2 {
            return None;
        }
        let left = MARGIN * self.scale;
        let room = (width - 3.0 * BUTTON * self.scale - 2.0 * left).max(0.0);
        Some((left, (room / self.tabs as f32).min(TAB_MAX * self.scale)))
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
        let near = pos.is_some_and(|(_, y)| y >= 0.0 && y < APPEAR * self.bar());
        let target = if near { 1.0 } else { 0.0 };
        let hover = pos.and_then(|p| self.hit_raw(p, width));
        let changed = target != self.target || (hover != self.hover && self.opacity > 0.0);
        if target != self.target {
            self.target = target;
            self.last_tick = Some(Instant::now());
        }
        self.hover = hover;
        changed
    }

    /// What a click at `pos` lands on, if the bar is visible enough to use.
    pub fn hit(&self, pos: (f32, f32), width: f32) -> Option<Hit> {
        if self.opacity < 0.5 { None } else { self.hit_raw(pos, width) }
    }

    fn hit_raw(&self, (x, y): (f32, f32), width: f32) -> Option<Hit> {
        if !(0.0..self.bar()).contains(&y) {
            return None;
        }
        let from_right = width - x;
        let button = BUTTON * self.scale;
        let tab = self.tab_layout(width).and_then(|(left, tab)| {
            let i = ((x - left) / tab).floor();
            (x >= left && i < self.tabs as f32).then_some(i as usize)
        });
        Some(if from_right < button {
            Hit::Close
        } else if from_right < 2.0 * button {
            Hit::Maximize
        } else if from_right < 3.0 * button {
            Hit::Minimize
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
    /// strip takes the title's place.
    pub fn draw(&mut self, context: &ID2D1DeviceContext, width: f32, title: &str, tabs: &[String], active: usize, maximized: bool) {
        if self.opacity <= 0.0 {
            return;
        }
        if self.text.as_ref().is_none_or(|(_, scale)| *scale != self.scale) {
            self.text = text_format(14.0 * self.scale).ok().map(|f| (f, self.scale));
        }
        let (s, bar, a) = (self.scale, self.bar(), self.opacity);
        let brush = |r: f32, g: f32, b: f32, alpha: f32| -> Option<ID2D1SolidColorBrush> {
            unsafe { context.CreateSolidColorBrush(&D2D1_COLOR_F { r, g, b, a: alpha * a }, None).ok() }
        };
        // Light, as in Honeyview: pale grey bar, dark text and glyphs.
        let (Some(panel), Some(ink), Some(on_red)) = (brush(0.94, 0.94, 0.94, 0.94), brush(0.10, 0.10, 0.10, 1.0), brush(1.0, 1.0, 1.0, 1.0)) else {
            return;
        };
        let button = BUTTON * s;
        let button_rect = |i: f32| D2D_RECT_F { left: width - button * (i + 1.0), top: 0.0, right: width - button * i, bottom: bar };
        unsafe {
            context.FillRectangle(&D2D_RECT_F { left: 0.0, top: 0.0, right: width, bottom: bar }, &panel);

            let hovered = match self.hover {
                Some(Hit::Close) => Some((0.0, brush(0.77, 0.17, 0.11, 1.0))),
                Some(Hit::Maximize) => Some((1.0, brush(0.0, 0.0, 0.0, 0.09))),
                Some(Hit::Minimize) => Some((2.0, brush(0.0, 0.0, 0.0, 0.09))),
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
            match self.tab_layout(width) {
                None => {
                    let rect = D2D_RECT_F { left: 14.0 * s, top: 0.0, right: (width - 3.0 * button - 14.0 * s).max(14.0 * s), bottom: bar };
                    text(title, rect, &ink);
                }
                Some((left, tab)) => {
                    // The active tab lifts out of the bar; the others are quieter.
                    let (Some(lifted), Some(quiet), Some(hover_fill), Some(rule)) =
                        (brush(1.0, 1.0, 1.0, 1.0), brush(0.10, 0.10, 0.10, 0.62), brush(0.0, 0.0, 0.0, 0.06), brush(0.0, 0.0, 0.0, 0.12))
                    else {
                        return;
                    };
                    let pad = 4.0 * s;
                    for (i, label) in tabs.iter().enumerate() {
                        let x = left + tab * i as f32;
                        let cell = D2D_RECT_F { left: x + 1.0 * s, top: pad, right: x + tab - 1.0 * s, bottom: bar - pad };
                        if i == active {
                            context.FillRectangle(&cell, &lifted);
                        } else if self.hover == Some(Hit::Tab(i)) {
                            context.FillRectangle(&cell, &hover_fill);
                        }
                        if i > 0 && i != active && i != active + 1 {
                            context.FillRectangle(&D2D_RECT_F { left: x, top: 10.0 * s, right: x + 1.0 * s, bottom: bar - 10.0 * s }, &rule);
                        }
                        let rect = D2D_RECT_F { left: x + 10.0 * s, top: 0.0, right: x + tab - 10.0 * s, bottom: bar };
                        text(if i == active { title } else { label }, rect, if i == active { &ink } else { &quiet });
                    }
                }
            }
        }
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
