//! F12 debug overlay: the open timeline, memory and view state as text.
//! Created on first use, so DirectWrite stays off the open path.

use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{D2D1_DRAW_TEXT_OPTIONS_NONE, ID2D1DeviceContext};
use windows::Win32::Graphics::DirectWrite::*;
use windows::core::w;

pub struct DebugText {
    format: IDWriteTextFormat,
}

impl DebugText {
    pub fn new() -> Result<DebugText, String> {
        unsafe {
            let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).map_err(|e| e.to_string())?;
            let format = factory
                .CreateTextFormat(w!("Consolas"), None, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, 14.0, w!("en-us"))
                .map_err(|e| e.to_string())?;
            Ok(DebugText { format })
        }
    }

    /// Draws the panel with its top edge at `top` (below the hover bar).
    pub fn draw(&self, context: &ID2D1DeviceContext, text: &str, top: f32) {
        let lines = text.lines().count() as f32;
        let width = text.lines().map(str::len).max().unwrap_or(0) as f32 * 7.8 + 24.0;
        let rect = D2D_RECT_F { left: 12.0, top, right: 12.0 + width, bottom: top + lines * 17.0 + 16.0 };
        let text_rect = D2D_RECT_F { left: rect.left + 8.0, top: rect.top + 8.0, right: rect.right - 8.0, bottom: rect.bottom - 8.0 };
        let wide: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let Ok(panel) = context.CreateSolidColorBrush(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.72 }, None) else { return };
            let Ok(ink) = context.CreateSolidColorBrush(&D2D1_COLOR_F { r: 0.92, g: 0.92, b: 0.92, a: 1.0 }, None) else { return };
            context.FillRectangle(&rect, &panel);
            context.DrawText(&wide, &self.format, &text_rect, &ink, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
        }
    }
}
