//! Zoom et déplacement de l'image. Tout est exprimé en pixels physiques
//! (1,0 = un pixel d'image pour un pixel d'écran); l'appelant convertit.

use eframe::egui::Vec2;

pub const MIN_ZOOM: f32 = 0.01;
pub const MAX_ZOOM: f32 = 64.0;
/// Facteur appliqué par les touches `+` et `-`.
pub const KEY_STEP: f32 = 1.25;

/// Zoom d'ajustement : l'image entre dans la zone, sans agrandissement au-delà de 100 %.
pub fn fit_zoom(img: Vec2, avail: Vec2) -> f32 {
    if img.x <= 0.0 || img.y <= 0.0 {
        return 1.0;
    }
    (avail.x / img.x).min(avail.y / img.y).clamp(MIN_ZOOM, 1.0)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    /// Mode « ajusté » : zoom et décalage sont recalculés à chaque taille de zone.
    fit: bool,
    zoom: f32,
    /// Décalage du centre de l'image par rapport au centre de la zone.
    offset: Vec2,
}

impl Default for View {
    fn default() -> Self {
        Self {
            fit: true,
            zoom: 1.0,
            offset: Vec2::ZERO,
        }
    }
}

impl View {
    pub fn is_fit(&self) -> bool {
        self.fit
    }

    pub fn zoom(&self, img: Vec2, avail: Vec2) -> f32 {
        if self.fit { fit_zoom(img, avail) } else { self.zoom }
    }

    pub fn offset(&self) -> Vec2 {
        if self.fit { Vec2::ZERO } else { self.offset }
    }

    pub fn set_fit(&mut self) {
        *self = Self::default();
    }

    /// Zoom absolu en gardant fixe le point sous `cursor` (relatif au centre de la zone).
    pub fn zoom_to(&mut self, img: Vec2, avail: Vec2, cursor: Vec2, new_zoom: f32) {
        let old = self.zoom(img, avail);
        let new = new_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.offset = cursor - (cursor - self.offset()) * (new / old);
        self.zoom = new;
        self.fit = false;
        self.clamp(img, avail);
    }

    pub fn zoom_by(&mut self, img: Vec2, avail: Vec2, cursor: Vec2, factor: f32) {
        let target = self.zoom(img, avail) * factor;
        self.zoom_to(img, avail, cursor, target);
    }

    /// Bascule entre ajusté et taille réelle (100 %).
    pub fn toggle_actual(&mut self, img: Vec2, avail: Vec2, cursor: Vec2) {
        if !self.fit && (self.zoom - 1.0).abs() < 1e-3 {
            self.set_fit();
        } else {
            self.zoom_to(img, avail, cursor, 1.0);
        }
    }

    pub fn pan(&mut self, delta: Vec2, img: Vec2, avail: Vec2) {
        if self.fit {
            return;
        }
        self.offset += delta;
        self.clamp(img, avail);
    }

    /// Garde l'image dans la zone : centrée si plus petite, bords atteignables sinon.
    pub fn clamp(&mut self, img: Vec2, avail: Vec2) {
        if self.fit {
            return;
        }
        let size = img * self.zoom;
        let max = ((size - avail) / 2.0).max(Vec2::ZERO);
        self.offset = self.offset.clamp(-max, max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::vec2;

    #[test]
    fn fit_never_upscales() {
        assert_eq!(fit_zoom(vec2(100.0, 50.0), vec2(1000.0, 1000.0)), 1.0);
    }

    #[test]
    fn fit_downscales_to_limiting_axis() {
        let z = fit_zoom(vec2(4000.0, 1000.0), vec2(1000.0, 1000.0));
        assert!((z - 0.25).abs() < 1e-6);
    }

    #[test]
    fn zoom_keeps_point_under_cursor() {
        let (img, avail) = (vec2(4000.0, 3000.0), vec2(1000.0, 800.0));
        let mut v = View::default();
        let cursor = vec2(120.0, -80.0);
        // Point de l'image sous le curseur, en coordonnées image.
        let before = (cursor - v.offset()) / v.zoom(img, avail);
        v.zoom_to(img, avail, cursor, 1.0);
        let after = (cursor - v.offset()) / v.zoom(img, avail);
        assert!((before - after).length() < 1e-3, "{before:?} {after:?}");
    }

    #[test]
    fn zoom_is_clamped() {
        let (img, avail) = (vec2(100.0, 100.0), vec2(500.0, 500.0));
        let mut v = View::default();
        v.zoom_to(img, avail, Vec2::ZERO, 1e9);
        assert_eq!(v.zoom(img, avail), MAX_ZOOM);
        v.zoom_to(img, avail, Vec2::ZERO, 0.0);
        assert_eq!(v.zoom(img, avail), MIN_ZOOM);
    }

    #[test]
    fn pan_is_clamped_and_disabled_when_fit() {
        let (img, avail) = (vec2(2000.0, 1000.0), vec2(500.0, 500.0));
        let mut v = View::default();
        v.pan(vec2(50.0, 50.0), img, avail);
        assert_eq!(v.offset(), Vec2::ZERO);
        v.zoom_to(img, avail, Vec2::ZERO, 1.0);
        v.pan(vec2(1e6, 1e6), img, avail);
        // (2000-500)/2 en x, (1000-500)/2 en y.
        assert_eq!(v.offset(), vec2(750.0, 250.0));
    }

    #[test]
    fn small_image_stays_centered() {
        let (img, avail) = (vec2(100.0, 100.0), vec2(500.0, 500.0));
        let mut v = View::default();
        v.zoom_to(img, avail, vec2(200.0, 200.0), 2.0);
        assert_eq!(v.offset(), Vec2::ZERO);
    }

    #[test]
    fn toggle_goes_actual_then_back_to_fit() {
        let (img, avail) = (vec2(4000.0, 3000.0), vec2(1000.0, 800.0));
        let mut v = View::default();
        v.toggle_actual(img, avail, Vec2::ZERO);
        assert_eq!(v.zoom(img, avail), 1.0);
        assert!(!v.is_fit());
        v.toggle_actual(img, avail, Vec2::ZERO);
        assert!(v.is_fit());
    }
}
