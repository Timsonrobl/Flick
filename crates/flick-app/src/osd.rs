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
    /// The display scale the formats were made for.
    scale: f32,
}

/// Distance from the window's edges, at 100 % scaling.
const MARGIN: f32 = 12.0;
const PADDING: f32 = 10.0;
/// Below the hover bar's first row (chrome.rs BAR, 36 px) and a margin.
const TOP: f32 = 36.0 + 8.0;

impl Osd {
    pub fn new(scale: f32) -> Result<Osd, String> {
        unsafe {
            let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).map_err(|e| e.to_string())?;
            let format = |size: f32, weight| {
                factory.CreateTextFormat(w!("Segoe UI"), None, weight, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size * scale, w!("")).map_err(|e| e.to_string())
            };
            let info = format(17.0, DWRITE_FONT_WEIGHT_NORMAL)?;
            let note = format(13.0, DWRITE_FONT_WEIGHT_NORMAL)?;
            Ok(Osd { factory, info, note, scale })
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

    /// The info panel: "Label: value" lines on a dark panel at the top left.
    pub fn draw_info(&self, context: &ID2D1DeviceContext, lines: &[(&str, String)], window: (f32, f32)) {
        let text = lines.iter().map(|(label, value)| format!("{label}: {value}")).collect::<Vec<_>>().join("\n");
        let (s, margin, padding) = (self.scale, MARGIN * self.scale, PADDING * self.scale);
        let max_width = (window.0 - 2.0 * (margin + padding)).min(1000.0 * s);
        let Some((layout, width, height)) = self.layout(&text, &self.info, max_width) else { return };
        let (left, top) = (margin, TOP * s);
        let panel = D2D_RECT_F { left, top, right: left + width + 2.0 * padding, bottom: top + height + 2.0 * padding };
        unsafe {
            let (Ok(fill), Ok(ink)) = (
                context.CreateSolidColorBrush(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.45 }, None),
                context.CreateSolidColorBrush(&D2D1_COLOR_F { r: 0.95, g: 0.95, b: 0.95, a: 1.0 }, None),
            ) else {
                return;
            };
            context.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: panel, radiusX: 6.0 * s, radiusY: 6.0 * s }, &fill);
            context.DrawTextLayout(Vector2 { X: left + padding, Y: top + padding }, &layout, &ink, D2D1_DRAW_TEXT_OPTIONS_NONE);
        }
    }

    /// A brief note, small, at the top right.
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
                context.CreateSolidColorBrush(&D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a: 0.9 }, None),
            ) else {
                return;
            };
            let radius = height / 2.0 + pad_y;
            context.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: pill, radiusX: radius, radiusY: radius }, &fill);
            context.DrawTextLayout(Vector2 { X: left + pad_x, Y: top + pad_y }, &layout, &ink, D2D1_DRAW_TEXT_OPTIONS_NONE);
        }
    }
}
