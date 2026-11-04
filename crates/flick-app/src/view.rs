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

    pub fn translate(dx: f32, dy: f32) -> Affine {
        Affine([1.0, 0.0, 0.0, 1.0, dx, dy])
    }

    pub fn apply(self, (x, y): (f32, f32)) -> (f32, f32) {
        let [m11, m12, m21, m22, m31, m32] = self.0;
        (x * m11 + y * m21 + m31, x * m12 + y * m22 + m32)
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

/// A zoom step (Ctrl+wheel, +/−) from `scale` by `factor`, stopping at
/// 100 % when it would cross it: the next step goes on past.
pub fn snap_step(scale: f32, factor: f32) -> f32 {
    let (from, to) = (scale - 1.0, scale * factor - 1.0);
    if from.abs() > 1e-4 && from.signum() != to.signum() { 1.0 / scale } else { factor }
}

/// A right drag's zoom: `start` doubled per `per_doubling` px of `travel`
/// (negative: halved), holding at 100 % for `detent` px of travel where it
/// passes it (a drag starting at 100 % holds for `detent` either way).
pub fn drag_scale(start: f32, travel: f32, per_doubling: f32, detent: f32) -> f32 {
    // The travel at which 100 % is reached, and which way that lies.
    let to_one = -per_doubling * start.log2();
    let side = if to_one.abs() < 1e-3 { travel.signum() } else { to_one.signum() };
    let beyond = (travel - to_one) * side; // past 100 %, in its direction
    if beyond <= 0.0 && to_one.abs() >= 1e-3 {
        start * 2f32.powf(travel / per_doubling)
    } else if beyond <= detent {
        1.0
    } else {
        2f32.powf((travel - to_one - side * detent) / per_doubling)
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

/// The 2×2 matrix of each EXIF orientation (1–8), as (x', y') = (a x + b y,
/// c x + d y) on screen axes (y down), matching `orientation`.
const ORIENTATIONS: [[i8; 4]; 8] = [[1, 0, 0, 1], [-1, 0, 0, 1], [-1, 0, 0, -1], [1, 0, 0, -1], [0, 1, 1, 0], [0, -1, 1, 0], [0, -1, -1, 0], [0, 1, -1, 0]];

fn multiply(m: [i8; 4], n: [i8; 4]) -> [i8; 4] {
    [m[0] * n[0] + m[1] * n[2], m[0] * n[1] + m[1] * n[3], m[2] * n[0] + m[3] * n[2], m[2] * n[1] + m[3] * n[3]]
}

/// The Rotate menu, for a tab's pictures: quarter turns clockwise, then
/// mirrored left-right and/or upside down, on top of each picture's EXIF
/// orientation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Turn {
    /// 0–3 quarter turns clockwise (3: 90° counter-clockwise).
    pub quarter: u8,
    pub flip_h: bool,
    pub flip_v: bool,
}

impl Turn {
    /// The orientation (1–8) a picture stored with EXIF `orientation` is
    /// drawn with, turned.
    pub fn after(self, orientation: u16) -> u16 {
        let exif = ORIENTATIONS[usize::from(orientation.clamp(1, 8)) - 1];
        let mut turn = ORIENTATIONS[0];
        for _ in 0..self.quarter % 4 {
            turn = multiply(ORIENTATIONS[5], turn); // 90° clockwise
        }
        if self.flip_h {
            turn = multiply(ORIENTATIONS[1], turn);
        }
        if self.flip_v {
            turn = multiply(ORIENTATIONS[3], turn);
        }
        let total = multiply(turn, exif);
        ORIENTATIONS.iter().position(|&m| m == total).map_or(1, |i| i as u16 + 1)
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
    /// The window filled: Fit to Width or Fit to Height, whichever leaves
    /// no border (the picture overflows one way).
    Fill,
    /// The window's width, from the top; taller pictures pan vertically.
    Width,
    /// The window's height, from the left; wider pictures pan sideways.
    Height,
    /// 100 %: one picture pixel per screen pixel.
    Original,
}

impl Fit {
    /// As the menu and notes name it.
    pub fn name(self) -> &'static str {
        match self {
            Fit::Window => "Fit to window",
            Fit::Fill => "Fill window",
            Fit::Width => "Fit to width",
            Fit::Height => "Fit to height",
            Fit::Original => "Original size",
        }
    }
}

/// Middle click's stops: the modes in the menu's order (each sets the
/// window's mode, as the menu does), then the zoom set by hand on this
/// picture (temporary).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    Mode(Fit),
    Hand,
}

const STOPS: [Stop; 6] = [Stop::Mode(Fit::Window), Stop::Mode(Fit::Fill), Stop::Mode(Fit::Width), Stop::Mode(Fit::Height), Stop::Mode(Fit::Original), Stop::Hand];

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

    /// The scale `fit` gives this picture in this window (enlarging past
    /// 100 % only with Stretch Small Images).
    pub fn scale_for(&self, fit: Fit) -> f32 {
        let (across, down) = (self.window.0 / self.image.0, self.window.1 / self.image.1);
        let scale = match fit {
            Fit::Window => {
                let fit = across.min(down);
                // A border under a pixel each side is no border but a grey
                // hairline along the picture (a window just off its shape, as
                // a whole-pixel window rarely matches a picture's exactly):
                // fill instead, cutting off a pixel or so.
                let gap = (self.window.0 - self.image.0 * fit) + (self.window.1 - self.image.1 * fit);
                if gap < 2.0 { across.max(down) } else { fit }
            }
            Fit::Fill => across.max(down),
            Fit::Width => across,
            Fit::Height => down,
            Fit::Original => return 1.0,
        };
        (if self.mode.stretch_small { scale } else { scale.min(1.0) }).clamp(MIN_SCALE, MAX_SCALE)
    }

    fn mode_scale(&self) -> f32 {
        self.scale_for(self.mode.fit)
    }

    /// Where middle click goes from `from`: the next stop in the menu's
    /// order, wrapping, that looks different from the picture now and from
    /// the stops before it (Fit to Width is skipped where it is Fit to
    /// Window, say); the zoom set by hand (`hand`: its scale) only if there
    /// is one. None: nothing else to show.
    pub fn next_stop(&self, from: Stop, hand: Option<f32>) -> Option<Stop> {
        let near = |a: f32, b: f32| (a / b - 1.0).abs() < 0.002;
        let scale = |stop: Stop| match stop {
            Stop::Mode(fit) => Some(self.scale_for(fit)),
            Stop::Hand => hand,
        };
        let at = STOPS.iter().position(|&s| s == from).unwrap_or(0);
        let new = |i: usize| scale(STOPS[i]).is_some_and(|s| !near(s, self.scale) && !STOPS[..i].iter().any(|&before| scale(before).is_some_and(|b| near(b, s))));
        (1..STOPS.len()).map(|k| (at + k) % STOPS.len()).find(|&i| new(i)).map(|i| STOPS[i])
    }

    /// The mode applied with the picture `point` (where the click was, taken
    /// before the mode changed anything) at `cursor`, on the axes where the
    /// picture overflows (middle click lands on a mode); where it fits, it
    /// sits as the mode places it.
    pub fn apply_mode_at(&mut self, point: (f32, f32), cursor: (f32, f32)) {
        self.apply_mode();
        if self.image.0 * self.scale > self.window.0 {
            self.center.0 = point.0 - (cursor.0 - self.window.0 / 2.0) / self.scale;
        }
        if self.image.1 * self.scale > self.window.1 {
            self.center.1 = point.1 - (cursor.1 - self.window.1 / 2.0) / self.scale;
        }
        self.clamp();
    }

    /// Whether the picture is at a zoom set by hand: not the mode's size
    /// (panning in the mode doesn't count).
    pub fn hand_zoomed(&self) -> bool {
        !self.follows_mode && (self.scale / self.mode_scale() - 1.0).abs() >= 0.002
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

    /// Sizes and places the picture as its mode says.
    pub fn apply_mode(&mut self) {
        self.scale = self.mode_scale().clamp(MIN_SCALE, MAX_SCALE);
        // Centred, on the axis it overflows (and pans) too.
        self.center = (self.image.0 / 2.0, self.image.1 / 2.0);
        self.follows_mode = true;
        self.clamp();
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.apply_mode();
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

    /// A right-drag zoom to `scale`, purely around the image point `anchor`
    /// held at `pressed` (window pixels): nothing keeps the picture's edges
    /// to the window's meanwhile, so the zoom never changes direction. (Kept
    /// to them, an edge that meets the window's holds the zoom there, the
    /// picture slides the other way, then the zoom moves back to the pressed
    /// point once the picture outgrows the window: it jiggles.) `settle` puts
    /// the edges right when the drag ends.
    pub fn drag_zoom(&mut self, anchor: (f32, f32), pressed: (f32, f32), scale: f32) {
        self.scale = scale.clamp(MIN_SCALE.min(self.fit_scale()), MAX_SCALE);
        let center = |anchor: f32, pressed: f32, window: f32| anchor - (pressed - window / 2.0) / self.scale;
        self.center = (center(anchor.0, pressed.0, self.window.0), center(anchor.1, pressed.1, self.window.1));
        self.follows_mode = false;
    }

    /// After a drag zoom: the edge rules (`clamp`) applied. Returns where the
    /// centre was, for gliding from there.
    pub fn settle(&mut self) -> (f32, f32) {
        let before = self.center;
        self.clamp();
        before
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

    /// Where the picture's left edge is in the window (negative: past it).
    pub fn picture_left(&self) -> f32 {
        self.window.0 / 2.0 - self.center.0 * self.scale
    }

    /// Window pixels → image space.
    pub fn to_image(&self, p: (f32, f32)) -> (f32, f32) {
        (self.center.0 + (p.0 - self.window.0 / 2.0) / self.scale, self.center.1 + (p.1 - self.window.1 / 2.0) / self.scale)
    }

    /// Image space → window pixels.
    #[cfg(test)]
    fn to_window(&self, p: (f32, f32)) -> (f32, f32) {
        ((p.0 - self.center.0) * self.scale + self.window.0 / 2.0, (p.1 - self.center.1) * self.scale + self.window.1 / 2.0)
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
    fn fit_window_leaves_no_hairline() {
        // 8256 x 5504 (3:2) in 902 x 601: fitted it is 901.5 wide, a quarter
        // pixel of background each side. It fills instead: no bar shows.
        let view = View::new((8256, 5504), (902, 601), mode(Fit::Window, false));
        assert!(view.picture_left() <= 0.0 && view.top() <= 0.0, "covers the window");
        assert!(8256.0 * view.scale - 902.0 < 2.0, "cutting off under two pixels");
        // A real border stays: 905 wide leaves 3.5 px.
        let view = View::new((8256, 5504), (905, 601), mode(Fit::Window, false));
        assert!(view.picture_left() > 1.0);
    }

    #[test]
    fn fit_width_centres_a_tall_picture() {
        let view = View::new((2000, 4000), WINDOW, mode(Fit::Width, false));
        assert_eq!(view.scale, 0.5);
        assert_eq!(view.top(), -750.0, "the picture's top 750 px above the window's: centred");
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
    fn fit_height_centres_a_wide_picture() {
        // A 4000 × 500 panorama in 1000 × 500: Fit to Height is 100 %.
        let view = View::new((4000, 500), WINDOW, mode(Fit::Height, false));
        assert_eq!(view.scale, 1.0);
        assert_eq!(view.to_image((0.0, 250.0)).0, 1500.0, "the middle 1000 columns show");
        let tall = View::new((500, 2000), WINDOW, mode(Fit::Height, false));
        assert_eq!(tall.scale, 0.25);
    }

    #[test]
    fn fill_centres_the_way_the_picture_overflows() {
        // 2000 × 4000 in 1000 × 500: the width fills (50 %), centred down.
        let tall = View::new((2000, 4000), WINDOW, mode(Fit::Fill, false));
        assert_eq!(tall.scale, 0.5);
        assert_eq!(tall.to_image((500.0, 0.0)).1, 1500.0);
        // 4000 × 1000: the height fills (50 %), centred across.
        let wide = View::new((4000, 1000), WINDOW, mode(Fit::Fill, false));
        assert_eq!(wide.scale, 0.5);
        assert_eq!(wide.to_image((0.0, 250.0)).0, 1000.0);
    }

    /// Middle click as the viewer does it: a mode stop sets the mode.
    fn click(view: &mut View, hand: &mut Option<(f32, (f32, f32))>, at: (f32, f32)) -> Option<Stop> {
        let from = if view.hand_zoomed() {
            *hand = Some(view.custom_zoom());
            Stop::Hand
        } else {
            Stop::Mode(view.mode.fit)
        };
        let stop = view.next_stop(from, hand.map(|h| h.0))?;
        match stop {
            Stop::Mode(fit) => {
                let point = view.to_image(at);
                view.mode.fit = fit;
                view.apply_mode_at(point, at);
            }
            Stop::Hand => view.set_custom_zoom(hand.unwrap()),
        }
        Some(stop)
    }

    #[test]
    fn middle_click_steps_through_the_modes_that_differ() {
        // 4000 × 1000 in 1000 × 500: Window and Width 25 %, Fill and Height 50 %.
        let mut view = View::new((4000, 1000), WINDOW, mode(Fit::Window, false));
        let mut hand = None;
        let stops: Vec<_> = (0..4).map(|_| (click(&mut view, &mut hand, (500.0, 250.0)), view.scale)).collect();
        let fit = |fit| Some(Stop::Mode(fit));
        assert_eq!(stops, [(fit(Fit::Fill), 0.5), (fit(Fit::Original), 1.0), (fit(Fit::Window), 0.25), (fit(Fit::Fill), 0.5)]);
        assert!(view.follows_mode);
    }

    #[test]
    fn turns_compose_with_exif_orientation() {
        let turn = |quarter, flip_h, flip_v| Turn { quarter, flip_h, flip_v };
        assert_eq!(Turn::default().after(6), 6, "no turn: EXIF as is");
        assert_eq!(turn(1, false, false).after(1), 6, "90° clockwise");
        assert_eq!(turn(3, false, false).after(1), 8, "90° counter-clockwise");
        assert_eq!(turn(2, false, false).after(1), 3);
        assert_eq!(turn(0, true, false).after(1), 2, "mirrored");
        assert_eq!(turn(0, false, true).after(1), 4, "upside down");
        assert_eq!(turn(1, false, false).after(6), 3, "a portrait shot (6) turned once more: 180°");
        assert_eq!(turn(3, false, false).after(6), 1, "and back: upright");
        assert_eq!(turn(0, true, true).after(1), 3, "both mirrors: 180°");
    }

    #[test]
    fn middle_click_ends_with_the_zoom_set_by_hand() {
        // 4000 × 2000 in 1000 × 500: every fit is 25 %.
        let mut view = View::new((4000, 2000), WINDOW, mode(Fit::Window, false));
        let mut hand = None;
        view.zoom_at((500.0, 250.0), 2.0); // 50 %, by hand
        assert_eq!(click(&mut view, &mut hand, (500.0, 250.0)), Some(Stop::Mode(Fit::Window)));
        assert_eq!((view.scale, hand.map(|h| h.0)), (0.25, Some(0.5)), "hand → fit, remembered");
        assert_eq!(click(&mut view, &mut hand, (750.0, 250.0)), Some(Stop::Mode(Fit::Original)));
        let under = view.to_image((750.0, 250.0));
        assert!((under.0 - 3000.0).abs() < 1.0, "around the click: {under:?}");
        assert!(view.follows_mode);
        assert_eq!(click(&mut view, &mut hand, (500.0, 250.0)), Some(Stop::Hand));
        assert_eq!((view.scale, view.mode.fit), (0.5, Fit::Original), "the hand zoom keeps the mode");
        assert_eq!(click(&mut view, &mut hand, (500.0, 250.0)), Some(Stop::Mode(Fit::Window)), "and round again");

        // Without a zoom by hand, 100 % goes back to fit.
        let mut view = View::new((4000, 2000), WINDOW, mode(Fit::Window, false));
        let mut none = None;
        click(&mut view, &mut none, (500.0, 250.0));
        click(&mut view, &mut none, (500.0, 250.0));
        assert_eq!(view.scale, 0.25);

        // Panning in a mode isn't a zoom by hand.
        let mut view = View::new((4000, 2000), WINDOW, mode(Fit::Original, false));
        view.pan((100.0, 0.0));
        assert!(!view.hand_zoomed());

        // A small picture: every mode is 100 %, nothing to cycle.
        let mut view = View::new((100, 50), WINDOW, mode(Fit::Window, false));
        assert_eq!(click(&mut view, &mut None, (500.0, 250.0)), None);
    }

    #[test]
    fn drag_zoom_keeps_the_pressed_point_on_a_small_picture() {
        // 500 × 250 in a 1000 × 500 window: centred, with borders.
        let mut view = View::new((500, 250), WINDOW, mode(Fit::Window, false));
        let pressed = (700.0, 300.0);
        let anchor = view.to_image(pressed);
        for scale in [1.1, 1.2, 1.4] {
            view.drag_zoom(anchor, pressed, scale);
            let at = view.to_image(pressed);
            assert!((at.0 - anchor.0).abs() < 0.01 && (at.1 - anchor.1).abs() < 0.01, "scale {scale}: {at:?} vs {anchor:?}");
        }
        // Grown past an edge, it goes on around the pressed point; let go, it settles inside the window.
        view.drag_zoom(anchor, pressed, 1.8);
        view.settle();
        let left = view.window.0 / 2.0 - view.center.0 * view.scale;
        assert!(left >= 0.0 && left + 500.0 * 1.8 <= view.window.0 + 0.01, "left {left}");
    }

    #[test]
    fn drag_zoom_never_turns_back() {
        // The jiggle: a 750 × 500 picture fit in 1000 × 500 (borders left and
        // right), drag-zoomed to 8× around a point on its left. Held to the
        // window's edges, the right edge would meet the window's first and
        // the picture slide left, then the zoom move back once both sides
        // were out. Every point must move steadily away from the pressed one.
        let mut view = View::new((1500, 1000), WINDOW, mode(Fit::Window, false));
        let pressed = (300.0, 250.0);
        let anchor = view.to_image(pressed);
        let points = [(0.0, 0.0), (375.0, 250.0), (750.0, 500.0), (1125.0, 750.0), (1500.0, 1000.0)];
        let mut last = points.map(|p| view.to_window(p));
        let mut scale = view.scale;
        while scale < 8.0 {
            scale *= 1.02;
            view.drag_zoom(anchor, pressed, scale);
            let now = points.map(|p| view.to_window(p));
            for (before, after) in last.iter().zip(&now) {
                // Away from the pressed point (or staying), on both axes.
                let away = |b: f32, a: f32, p: f32| (a - p).abs() >= (b - p).abs() - 0.01;
                assert!(away(before.0, after.0, pressed.0) && away(before.1, after.1, pressed.1), "turned back at {scale:.2}: {before:?} → {after:?}");
            }
            last = now;
        }
        // Let go: the edge rules apply (no border where the picture overflows).
        view.settle();
        assert!(view.to_window((0.0, 0.0)).0 <= 0.01 && view.to_window((1500.0, 0.0)).0 >= 999.99);
    }

    #[test]
    fn zoom_stops_at_100_percent_on_the_way() {
        assert_eq!(snap_step(0.9, 1.25) * 0.9, 1.0, "Ctrl+wheel from 90 % lands on 100 %");
        assert_eq!(snap_step(1.0, 1.25), 1.25, "and goes on from there");
        assert!((snap_step(1.2, 0.8) * 1.2 - 1.0).abs() < 1e-6, "zooming out too");
        assert_eq!(snap_step(0.5, 1.25), 1.25, "far from it, a plain step");
        // Right drag, 200 px per doubling, held at 100 % for 24 px.
        let drag = |start, travel| drag_scale(start, travel, 200.0, 24.0);
        assert_eq!(drag(0.25, 0.0), 0.25);
        assert_eq!(drag(0.25, 400.0), 1.0);
        assert_eq!(drag(0.25, 420.0), 1.0, "held");
        assert_eq!(drag(0.25, 624.0), 2.0, "then on, with no jump");
        assert_eq!(drag(0.25, -200.0), 0.125, "away from 100 %: plain");
        assert_eq!(drag(2.0, -224.0), 1.0);
        assert_eq!(drag(2.0, -424.0), 0.5);
        assert_eq!(drag(1.0, 20.0), 1.0, "starting at 100 %: held either way");
        assert_eq!(drag(1.0, -224.0), 0.5);
    }

    #[test]
    fn an_off_centre_small_picture_stays_put() {
        // 500 × 250 in 1000 × 500, drag-zoomed to 1.2 around a point off the centre.
        let mut view = View::new((500, 250), WINDOW, mode(Fit::Window, false));
        let pressed = (700.0, 300.0);
        view.drag_zoom(view.to_image(pressed), pressed, 1.2);
        view.settle();
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
