//! On-screen text over the picture: the info panel (Tab: file details and
//! EXIF) at the top left, and brief notes (the zoom level, "Copied to
//! clipboard") at the top right, both just below the hover bar's first row
//! (the bar draws over them). Made on first use, so DirectWrite stays off
//! the open path.

use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ROUNDED_RECT, ID2D1DeviceContext};
use windows::Win32::Graphics::DirectWrite::*;
use windows::core::w;
use windows_numerics::Vector2;

pub struct Osd {
    factory: IDWriteFactory,
    info: IDWriteTextFormat,
    note: IDWriteTextFormat,
    title: IDWriteTextFormat,
    /// The display scale the formats were made for.
    scale: f32,
}

/// Distance from the window's edges, at 100 % scaling.
const MARGIN: f32 = 12.0;
const PADDING: f32 = 10.0;
/// Below the hover bar's first row (chrome.rs BAR, 36 px) and a margin.
const TOP: f32 = 36.0 + 8.0;
/// The info panel's and the notes' text size (100 % scaling), about
/// Honeyview's (17 px reads as too big).
const TEXT: f32 = 14.0;
/// The info panel wraps at this width where the picture leaves no border
/// wide enough to hold it (100 % scaling).
const PANEL_WIDTH: f32 = 400.0;
/// A border beside the picture at least this wide holds the panel.
const BORDER_MIN: f32 = 240.0;
/// A notice (a file that can't be shown) wraps at this width.
const NOTICE_WIDTH: f32 = 520.0;

impl Osd {
    pub fn new(scale: f32) -> Result<Osd, String> {
        unsafe {
            let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).map_err(|e| e.to_string())?;
            let format = |size: f32, weight| {
                factory.CreateTextFormat(w!("Segoe UI"), None, weight, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size * scale, w!("")).map_err(|e| e.to_string())
            };
            let info = format(TEXT, DWRITE_FONT_WEIGHT_NORMAL)?;
            // Airier than the font's own spacing, as Honeyview's panel.
            info.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM, TEXT * scale * 1.65, TEXT * scale * 1.25).map_err(|e| e.to_string())?;
            // A run with nowhere to break (a long file name) still breaks.
            info.SetWordWrapping(DWRITE_WORD_WRAPPING_EMERGENCY_BREAK).map_err(|e| e.to_string())?;
            // The info panel's font (its own line spacing: one line).
            let note = format(TEXT, DWRITE_FONT_WEIGHT_NORMAL)?;
            // A notice's first line ("Can't show …").
            let title = format(TEXT * 1.3, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            for f in [&title, &info] {
                let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_EMERGENCY_BREAK);
            }
            Ok(Osd { factory, info, note, title, scale })
        }
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Lays `text` out no wider than `max_width`; returns it with its size.
    fn layout(&self, text: &str, format: &IDWriteTextFormat, max_width: f32) -> Option<(IDWriteTextLayout, f32, f32)> {
        let wide: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let layout = self.factory.CreateTextLayout(&wide, format, max_width.max(1.0), f32::MAX).ok()?;
            let mut metrics = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut metrics).ok()?;
            Some((layout, metrics.widthIncludingTrailingWhitespace, metrics.height))
        }
    }

    /// The info panel: "Label: value" lines on a dark panel at the top left,
    /// in small type as Honeyview's. Where the picture leaves a border on the
    /// left at least BORDER_MIN wide (`picture_left`: where it starts), the
    /// panel fits in it, off the picture; else it wraps at PANEL_WIDTH.
    /// Labels make a column and values wrap within their own (a long file
    /// name continues under itself, not under "File:"). Returns where it was
    /// drawn (a click there closes it).
    pub fn draw_info(&self, context: &ID2D1DeviceContext, lines: &[(&str, String)], window: (f32, f32), picture_left: f32) -> Option<D2D_RECT_F> {
        let (s, margin, padding) = (self.scale, MARGIN * self.scale, PADDING * self.scale);
        let border = picture_left - 2.0 * (margin + padding);
        let max_width = if picture_left >= BORDER_MIN * s { border } else { (window.0 - 2.0 * (margin + padding)).min(PANEL_WIDTH * s) };
        let labels: Vec<_> = lines.iter().map(|(label, _)| self.layout(&if label.is_empty() { String::new() } else { format!("{label}: ") }, &self.info, f32::MAX)).collect::<Option<_>>()?;
        let label_width = labels.iter().map(|(_, w, _)| *w).fold(0.0, f32::max);
        // An empty label is a gap between groups: one empty line.
        let values: Vec<_> = lines.iter().map(|(label, value)| self.layout(if label.is_empty() { "" } else { value }, &self.info, (max_width - label_width).max(1.0))).collect::<Option<_>>()?;
        let width = label_width + values.iter().map(|(_, w, _)| *w).fold(0.0, f32::max);
        let height: f32 = values.iter().map(|(_, _, h)| *h).sum();
        let (left, top) = (margin, TOP * s);
        let panel = D2D_RECT_F { left, top, right: left + width + 2.0 * padding, bottom: top + height + 2.0 * padding };
        unsafe {
            let (Ok(fill), Ok(ink)) = (
                context.CreateSolidColorBrush(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.45 }, None),
                context.CreateSolidColorBrush(&D2D1_COLOR_F { r: 0.95, g: 0.95, b: 0.95, a: 1.0 }, None),
            ) else {
                return None;
            };
            context.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: panel, radiusX: 6.0 * s, radiusY: 6.0 * s }, &fill);
            let mut y = top + padding;
            for ((label, _, _), (value, _, height)) in labels.iter().zip(&values) {
                context.DrawTextLayout(Vector2 { X: left + padding, Y: y }, label, &ink, D2D1_DRAW_TEXT_OPTIONS_NONE);
                context.DrawTextLayout(Vector2 { X: left + padding + label_width, Y: y }, value, &ink, D2D1_DRAW_TEXT_OPTIONS_NONE);
                y += height;
            }
        }
        Some(panel)
    }

    /// Why a picture can't be shown, in the middle of the window: `text`'s
    /// first line ("Can't show name") larger, then the reason, then the
    /// decoder's detail dimmer (`failure::message`), wrapped to at most
    /// NOTICE_WIDTH on a dark panel.
    pub fn draw_notice(&self, context: &ID2D1DeviceContext, text: &str, window: (f32, f32)) {
        let (s, padding) = (self.scale, 2.0 * PADDING * self.scale);
        let mut lines = text.lines();
        let (title, reason) = (lines.next().unwrap_or(""), lines.next().unwrap_or(""));
        let detail: Vec<&str> = lines.collect();
        let width = (window.0 - 2.0 * (MARGIN * s + padding)).min(NOTICE_WIDTH * s).max(1.0);
        let parts = [(title, &self.title, 1.0), (reason, &self.info, 1.0), (&detail.join("\n")[..], &self.info, 0.6)];
        let laid: Vec<_> = parts.iter().filter(|(t, _, _)| !t.is_empty()).filter_map(|(t, format, alpha)| Some((self.layout(t, format, width)?, *alpha))).collect();
        let gap = 6.0 * s;
        let height = laid.iter().map(|((_, _, h), _)| h).sum::<f32>() + gap * laid.len().saturating_sub(1) as f32;
        let inner = laid.iter().map(|((_, w, _), _)| *w).fold(0.0, f32::max);
        let (left, top) = ((window.0 - inner) / 2.0, ((window.1 - height) / 2.0).max(TOP * s));
        let panel = D2D_RECT_F { left: left - padding, top: top - padding, right: left + inner + padding, bottom: top + height + padding };
        unsafe {
            let Ok(fill) = context.CreateSolidColorBrush(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.45 }, None) else { return };
            context.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: panel, radiusX: 8.0 * s, radiusY: 8.0 * s }, &fill);
            let mut y = top;
            for ((layout, w, h), alpha) in &laid {
                let Ok(ink) = context.CreateSolidColorBrush(&D2D1_COLOR_F { r: 0.95, g: 0.95, b: 0.95, a: *alpha }, None) else { return };
                // Each part centred on its own.
                context.DrawTextLayout(Vector2 { X: left + (inner - w) / 2.0, Y: y }, layout, &ink, D2D1_DRAW_TEXT_OPTIONS_NONE);
                y += h + gap;
            }
        }
    }

    /// A brief note at the top right, in the info panel's lettering.
    pub fn draw_note(&self, context: &ID2D1DeviceContext, text: &str, window: (f32, f32)) {
        let (s, margin) = (self.scale, MARGIN * self.scale);
        let (pad_x, pad_y) = (8.0 * s, 3.0 * s);
        let Some((layout, width, height)) = self.layout(text, &self.note, window.0 - 2.0 * (margin + pad_x)) else { return };
        let right = window.0 - margin;
        let (left, top) = (right - width - 2.0 * pad_x, TOP * s);
        let pill = D2D_RECT_F { left, top, right, bottom: top + height + 2.0 * pad_y };
        unsafe {
            let (Ok(fill), Ok(ink)) = (
                context.CreateSolidColorBrush(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.35 }, None),
                context.CreateSolidColorBrush(&D2D1_COLOR_F { r: 0.95, g: 0.95, b: 0.95, a: 1.0 }, None),
            ) else {
                return;
            };
            let radius = height / 2.0 + pad_y;
            context.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: pill, radiusX: radius, radiusY: radius }, &fill);
            context.DrawTextLayout(Vector2 { X: left + pad_x, Y: top + pad_y }, &layout, &ink, D2D1_DRAW_TEXT_OPTIONS_NONE);
        }
    }
}
