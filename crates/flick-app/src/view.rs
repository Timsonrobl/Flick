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

/// How pixels are resampled when drawn (Honeyview's render filters).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Filter {
    /// Hard pixels ("No Filter").
    Nearest,
    /// High-quality cubic when shrinking, linear when enlarging.
    #[default]
    Soften,
    /// High-quality cubic both ways, then a light sharpen.
    Sharpen,
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

    /// Fit to Window's scale, whatever the mode: the whole picture, enlarged
    /// past 100 % only with Stretch Small Images.
    pub fn window_fit_scale(&self) -> f32 {
        let fit = f32::min(self.window.0 / self.image.0, self.window.1 / self.image.1);
        (if self.mode.stretch_small { fit } else { fit.min(1.0) }).clamp(MIN_SCALE, MAX_SCALE)
    }

    /// The whole picture, centred (in Fit to Window mode: the mode again,
    /// so it keeps fitting as the window is resized).
    pub fn fit_window(&mut self) {
        if self.mode.fit == Fit::Window {
            return self.apply_mode();
        }
        self.scale = self.window_fit_scale();
        self.center = (self.image.0 / 2.0, self.image.1 / 2.0);
        self.follows_mode = false;
        self.clamp();
    }

    /// The zoom as set by hand: scale, and the centre as a fraction of the
    /// picture (so it carries over to the next picture in a tab).
    pub fn custom_zoom(&self) -> (f32, (f32, f32)) {
        (self.scale, (self.center.0 / self.image.0, self.center.1 / self.image.1))
    }

    pub fn set_custom_zoom(&mut self, (scale, (x, y)): (f32, (f32, f32))) {
        self.scale = scale.clamp(MIN_SCALE.min(self.fit_scale()), MAX_SCALE);
        self.center = (x * self.image.0, y * self.image.1);
        self.follows_mode = false;
        self.clamp();
    }

    /// Middle click: a zoom set by hand → Fit to Window → 100 % around
    /// `cursor` → that zoom again (`custom`, remembered when leaving it; if
    /// there is none, Fit to Window) → … Returns whether the view changed.
    pub fn cycle_zoom(&mut self, cursor: (f32, f32), custom: &mut Option<(f32, (f32, f32))>) -> bool {
        let near = |a: f32, b: f32| (a / b - 1.0).abs() < 0.002;
        let fit_scale = self.window_fit_scale();
        let (fit, original) = (near(self.scale, fit_scale), near(self.scale, 1.0));
        let back = custom.filter(|&(scale, _)| !near(scale, fit_scale) && !near(scale, 1.0));
        if !fit && !original {
            *custom = Some(self.custom_zoom());
            self.fit_window();
        } else if fit && !original {
            self.zoom_at(cursor, 1.0 / self.scale);
        } else if let Some(back) = back {
            self.set_custom_zoom(back);
        } else if !fit {
            self.fit_window();
        } else {
            return false; // Fit to Window is 100 %, and no zoom to go back to
        }
        true
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

    /// A right-drag zoom: `scale`, with the image point `anchor` (taken where
    /// the drag started) at `cursor`. A picture smaller than the window on an
    /// axis is not centred there, only kept inside it, so it grows from the
    /// pressed point rather than from the window's centre.
    pub fn zoom_anchored(&mut self, anchor: (f32, f32), cursor: (f32, f32), scale: f32) {
        self.scale = scale.clamp(MIN_SCALE.min(self.fit_scale()), MAX_SCALE);
        let center = |anchor: f32, cursor: f32, window: f32| anchor - (cursor - window / 2.0) / self.scale;
        self.center = (center(anchor.0, cursor.0, self.window.0), center(anchor.1, cursor.1, self.window.1));
        self.follows_mode = false;
        self.clamp();
    }

    /// Moves the picture by `delta` window pixels, on the axes where it is
    /// larger than the window (where it fits, nothing moves).
    pub fn pan(&mut self, delta: (f32, f32)) {
        let larger = |image: f32, window: f32| image * self.scale > window;
        if larger(self.image.0, self.window.0) {
            self.center.0 -= delta.0 / self.scale;
        }
        if larger(self.image.1, self.window.1) {
            self.center.1 -= delta.1 / self.scale;
        }
        self.follows_mode = false;
        self.clamp();
    }

    /// Window pixels → image space.
    pub fn to_image(&self, p: (f32, f32)) -> (f32, f32) {
        (self.center.0 + (p.0 - self.window.0 / 2.0) / self.scale, self.center.1 + (p.1 - self.window.1 / 2.0) / self.scale)
    }

    /// Where the picture is larger than the window, no border shows; where it
    /// is smaller, it stays where it is, inside the window. (It starts
    /// centred, and the view modes centre it again; a zoom around a point
    /// can leave it off-centre, and nothing then snaps it back.)
    fn clamp(&mut self) {
        let axis = |center: f32, image: f32, window: f32, scale: f32| {
            let half = window / 2.0 / scale;
            center.clamp(half.min(image - half), half.max(image - half))
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

    #[test]
    fn middle_click_cycles_custom_fit_original() {
        // 4000 × 2000 in 1000 × 500: Fit to Window is 25 %.
        let mut view = View::new((4000, 2000), WINDOW, mode(Fit::Window, false));
        let mut custom = None;
        view.zoom_at((500.0, 250.0), 2.0); // 50 %, by hand
        assert!(view.cycle_zoom((500.0, 250.0), &mut custom));
        assert_eq!((view.scale, custom.map(|c| c.0)), (0.25, Some(0.5)), "custom → fit, remembered");
        assert!(view.follows_mode, "fit in Fit to Window mode is the mode again");
        assert!(view.cycle_zoom((750.0, 250.0), &mut custom));
        assert_eq!(view.scale, 1.0, "fit → 100 %");
        let under = view.to_image((750.0, 250.0));
        assert!((under.0 - 3000.0).abs() < 1.0, "around the click: {under:?}");
        assert!(view.cycle_zoom((500.0, 250.0), &mut custom));
        assert_eq!(view.scale, 0.5, "100 % → the custom zoom");
        assert!(view.cycle_zoom((500.0, 250.0), &mut custom));
        assert_eq!(view.scale, 0.25, "and round again");

        // Without a custom zoom, 100 % goes back to fit.
        let mut view = View::new((4000, 2000), WINDOW, mode(Fit::Window, false));
        let mut none = None;
        view.cycle_zoom((500.0, 250.0), &mut none);
        view.cycle_zoom((500.0, 250.0), &mut none);
        assert_eq!(view.scale, 0.25);

        // A small picture: fit is 100 %, nothing to cycle.
        let mut view = View::new((100, 50), WINDOW, mode(Fit::Window, false));
        assert!(!view.cycle_zoom((500.0, 250.0), &mut None));
    }

    #[test]
    fn drag_zoom_keeps_the_pressed_point_on_a_small_picture() {
        // 500 × 250 in a 1000 × 500 window: centred, with borders.
        let mut view = View::new((500, 250), WINDOW, mode(Fit::Window, false));
        let pressed = (700.0, 300.0);
        let anchor = view.to_image(pressed);
        for scale in [1.1, 1.2, 1.4] {
            view.zoom_anchored(anchor, pressed, scale);
            let at = view.to_image(pressed);
            assert!((at.0 - anchor.0).abs() < 0.01 && (at.1 - anchor.1).abs() < 0.01, "scale {scale}: {at:?} vs {anchor:?}");
        }
        // Grown to an edge, it stays inside the window rather than follow.
        view.zoom_anchored(anchor, pressed, 1.8);
        let left = view.window.0 / 2.0 - view.center.0 * view.scale;
        assert!(left >= 0.0 && left + 500.0 * 1.8 <= view.window.0 + 0.01, "left {left}");
    }

    #[test]
    fn an_off_centre_small_picture_stays_put() {
        // 500 × 250 in 1000 × 500, drag-zoomed to 1.2 around a point off the centre.
        let mut view = View::new((500, 250), WINDOW, mode(Fit::Window, false));
        let pressed = (700.0, 300.0);
        view.zoom_anchored(view.to_image(pressed), pressed, 1.2);
        let placed = view.center;
        assert!((placed.0 - 250.0).abs() > 1.0, "off-centre: {placed:?}");
        view.pan((40.0, -30.0));
        assert_eq!(view.center, placed, "panning a picture that fits moves nothing (and snaps nothing)");
        view.resize((1100, 550));
        assert_eq!(view.center, placed, "nor does a resize");
        view.zoom_at((500.0, 250.0), 1.0);
        assert_eq!(view.center, placed, "nor a zoom that changes nothing");
        view.set_mode(mode(Fit::Window, false));
        assert_eq!(view.center, (250.0, 125.0), "the view mode centres it again");
    }
}
