//! Zoom/pan state and the bitmap-to-window transform.
//!
//! Coordinates: "image" space is the picture as displayed (after EXIF
//! orientation), in full-resolution pixels. The view maps it to window
//! pixels with a uniform `scale` around `center` (an image point drawn at the
//! window centre).

use windows_numerics::Matrix3x2;

pub const MIN_SCALE: f32 = 0.02;
pub const MAX_SCALE: f32 = 32.0;

/// Row-vector affine transform, as Direct2D uses: p' = p * m.
#[derive(Clone, Copy, Debug)]
pub struct Affine([f32; 6]); // m11 m12 m21 m22 m31 m32

impl Affine {
    pub const IDENTITY: Affine = Affine([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    pub fn scale(sx: f32, sy: f32) -> Affine {
        Affine([sx, 0.0, 0.0, sy, 0.0, 0.0])
    }

    /// `self` first, then `next`.
    pub fn then(self, next: Affine) -> Affine {
        let [a11, a12, a21, a22, a31, a32] = self.0;
        let [b11, b12, b21, b22, b31, b32] = next.0;
        Affine([
            a11 * b11 + a12 * b21,
            a11 * b12 + a12 * b22,
            a21 * b11 + a22 * b21,
            a21 * b12 + a22 * b22,
            a31 * b11 + a32 * b21 + b31,
            a31 * b12 + a32 * b22 + b32,
        ])
    }

    pub fn to_d2d(self) -> Matrix3x2 {
        let [m11, m12, m21, m22, m31, m32] = self.0;
        Matrix3x2 { M11: m11, M12: m12, M21: m21, M22: m22, M31: m31, M32: m32 }
    }
}

/// Maps stored pixels (w × h) to displayed pixels for EXIF orientation 1–8.
pub fn orientation(o: u16, w: f32, h: f32) -> Affine {
    match o {
        2 => Affine([-1.0, 0.0, 0.0, 1.0, w, 0.0]),
        3 => Affine([-1.0, 0.0, 0.0, -1.0, w, h]),
        4 => Affine([1.0, 0.0, 0.0, -1.0, 0.0, h]),
        5 => Affine([0.0, 1.0, 1.0, 0.0, 0.0, 0.0]),
        6 => Affine([0.0, 1.0, -1.0, 0.0, h, 0.0]),
        7 => Affine([0.0, -1.0, -1.0, 0.0, h, w]),
        8 => Affine([0.0, -1.0, 1.0, 0.0, 0.0, w]),
        _ => Affine::IDENTITY,
    }
}

/// Displayed size of a stored w × h image.
pub fn oriented_size(o: u16, (w, h): (u32, u32)) -> (u32, u32) {
    if o >= 5 { (h, w) } else { (w, h) }
}

/// How a picture is sized when it is shown (the View menu, as in Honeyview).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    /// The whole picture in the window.
    Window,
    /// The window's width, from the top; taller pictures pan vertically.
    Width,
    /// 100 %: one picture pixel per screen pixel.
    Original,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub fit: Fit,
    /// Fits enlarge pictures smaller than the window (else they stay at 100 %).
    pub stretch_small: bool,
}

impl Default for Mode {
    fn default() -> Mode {
        Mode { fit: Fit::Window, stretch_small: false }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct View {
    pub image: (f32, f32),
    pub window: (f32, f32),
    pub scale: f32,
    pub center: (f32, f32),
    pub mode: Mode,
    /// Follows the mode (re-applied on resize) until the user zooms or pans.
    pub follows_mode: bool,
}

impl View {
    pub fn new(image: (u32, u32), window: (u32, u32), mode: Mode) -> View {
        let mut view = View {
            image: (image.0 as f32, image.1 as f32),
            window: (window.0 as f32, window.1 as f32),
            scale: 1.0,
            center: (0.0, 0.0),
            mode,
            follows_mode: true,
        };
        view.apply_mode();
        view
    }

    /// Whole picture in the window; small pictures stay at 100 %.
    pub fn fit_scale(&self) -> f32 {
        f32::min(1.0, f32::min(self.window.0 / self.image.0, self.window.1 / self.image.1))
    }

    fn mode_scale(&self) -> f32 {
        let fit = match self.mode.fit {
            Fit::Window => f32::min(self.window.0 / self.image.0, self.window.1 / self.image.1),
            Fit::Width => self.window.0 / self.image.0,
            Fit::Original => return 1.0,
        };
        if self.mode.stretch_small { fit } else { fit.min(1.0) }
    }

    /// Sizes and places the picture as its mode says.
    pub fn apply_mode(&mut self) {
        self.scale = self.mode_scale().clamp(MIN_SCALE, MAX_SCALE);
        let top = self.window.1 / 2.0 / self.scale; // picture's top at the window's
        let y = if self.mode.fit == Fit::Width { top } else { self.image.1 / 2.0 };
        self.center = (self.image.0 / 2.0, y);
        self.follows_mode = true;
        self.clamp();
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.apply_mode();
    }

    pub fn actual_size(&mut self, cursor: (f32, f32)) {
        self.zoom_at(cursor, 1.0 / self.scale);
    }

    /// Double-click: from the mode's sizing to 100 % at the cursor (or, when
    /// that already is 100 %, to the whole picture), and back to the mode.
    pub fn toggle_zoom(&mut self, cursor: (f32, f32)) {
        if !self.follows_mode {
            self.apply_mode();
        } else if (self.scale - 1.0).abs() > 0.001 {
            self.actual_size(cursor);
        } else {
            let center = (self.window.0 / 2.0, self.window.1 / 2.0);
            self.zoom_at(center, self.fit_scale() / self.scale);
        }
    }

    pub fn resize(&mut self, window: (u32, u32)) {
        self.window = (window.0 as f32, window.1 as f32);
        if self.follows_mode {
            self.apply_mode();
        } else {
            self.clamp();
        }
    }

    /// Zooms by `factor`, keeping the image point under `cursor` in place.
    pub fn zoom_at(&mut self, cursor: (f32, f32), factor: f32) {
        let new_scale = (self.scale * factor).clamp(MIN_SCALE.min(self.fit_scale()), MAX_SCALE);
        let point = self.to_image(cursor);
        self.scale = new_scale;
        let offset = (cursor.0 - self.window.0 / 2.0, cursor.1 - self.window.1 / 2.0);
        self.center = (point.0 - offset.0 / self.scale, point.1 - offset.1 / self.scale);
        self.follows_mode = false;
        self.clamp();
    }

    /// Moves the picture by `delta` window pixels.
    pub fn pan(&mut self, delta: (f32, f32)) {
        self.center = (self.center.0 - delta.0 / self.scale, self.center.1 - delta.1 / self.scale);
        self.follows_mode = false;
        self.clamp();
    }

    /// Whether the whole picture is visible (nothing to pan).
    pub fn fits(&self) -> bool {
        self.image.0 * self.scale <= self.window.0 + 0.5 && self.image.1 * self.scale <= self.window.1 + 0.5
    }

    fn to_image(&self, p: (f32, f32)) -> (f32, f32) {
        (self.center.0 + (p.0 - self.window.0 / 2.0) / self.scale, self.center.1 + (p.1 - self.window.1 / 2.0) / self.scale)
    }

    /// Keeps the picture centred on axes where it is smaller than the window,
    /// and stops it leaving the window on the others.
    fn clamp(&mut self) {
        let axis = |center: f32, image: f32, window: f32, scale: f32| {
            let half = window / 2.0 / scale;
            if image * scale <= window { image / 2.0 } else { center.clamp(half, image - half) }
        };
        self.center = (
            axis(self.center.0, self.image.0, self.window.0, self.scale),
            axis(self.center.1, self.image.1, self.window.1, self.scale),
        );
    }

    #[cfg(test)]
    fn top(&self) -> f32 {
        self.window.1 / 2.0 - self.center.1 * self.scale
    }

    /// Image space → window pixels.
    pub fn transform(&self) -> Affine {
        Affine([
            self.scale,
            0.0,
            0.0,
            self.scale,
            self.window.0 / 2.0 - self.center.0 * self.scale,
            self.window.1 / 2.0 - self.center.1 * self.scale,
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: (u32, u32) = (1000, 500);
    fn mode(fit: Fit, stretch_small: bool) -> Mode {
        Mode { fit, stretch_small }
    }

    #[test]
    fn fit_window_shrinks_large_and_keeps_small() {
        assert_eq!(View::new((4000, 1000), WINDOW, mode(Fit::Window, false)).scale, 0.25);
        assert_eq!(View::new((100, 50), WINDOW, mode(Fit::Window, false)).scale, 1.0);
        assert_eq!(View::new((100, 50), WINDOW, mode(Fit::Window, true)).scale, 10.0);
    }

    #[test]
    fn fit_width_starts_at_the_top() {
        let view = View::new((2000, 4000), WINDOW, mode(Fit::Width, false));
        assert_eq!(view.scale, 0.5);
        assert_eq!(view.top(), 0.0);
        // A small picture stays at 100 % unless stretched.
        assert_eq!(View::new((200, 100), WINDOW, mode(Fit::Width, false)).scale, 1.0);
        assert_eq!(View::new((200, 100), WINDOW, mode(Fit::Width, true)).scale, 5.0);
    }

    #[test]
    fn original_ignores_stretch() {
        assert_eq!(View::new((4000, 3000), WINDOW, mode(Fit::Original, true)).scale, 1.0);
    }

    #[test]
    fn resize_reapplies_the_mode_until_zoomed() {
        let mut view = View::new((4000, 1000), WINDOW, mode(Fit::Window, false));
        view.resize((2000, 1000));
        assert_eq!(view.scale, 0.5);
        view.zoom_at((0.0, 0.0), 2.0);
        view.resize((1000, 500));
        assert_eq!(view.scale, 1.0, "zoomed: the scale stays");
    }
}
