//! Modèle d'objectifs (deux fisheye) et poids de fusion de la couture.
//!
//! Chaque objectif est un fisheye équidistant : un rayon d'angle `θ` à l'axe optique arrive à la
//! distance `θ / (fov/2)` du centre, le bord de l'image carrée correspondant à `fov/2` (modèle
//! `fisheye` de `v360`). Les corrections `a_*` / `b_*` sont de petites rotations (degrés) ;
//! l'objectif B regarde vers l'arrière (rotation de 180° en lacet ajoutée).
//! Miroir de `x3studio/calibration.py`.

use crate::geometry::{rot_yaw, ypr_to_matrix, Mat3};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Calibration {
    /// Champ de vision de chaque objectif (degrés).
    pub fov: f64,
    /// Objectif A (flux 0, avant).
    pub a_yaw: f64,
    pub a_pitch: f64,
    pub a_roll: f64,
    /// Objectif B (flux 1, arrière).
    pub b_yaw: f64,
    pub b_pitch: f64,
    pub b_roll: f64,
    /// Largeur de fondu de la couture (degrés).
    pub blend: f64,
    /// Inverse le rôle des deux flux vidéo.
    pub swap_lenses: bool,
}

impl Default for Calibration {
    fn default() -> Self {
        Calibration {
            fov: 200.0,
            a_yaw: 0.0,
            a_pitch: 0.0,
            a_roll: 0.0,
            b_yaw: 0.0,
            b_pitch: 0.0,
            b_roll: 0.0,
            blend: 10.0,
            swap_lenses: false,
        }
    }
}

impl Calibration {
    pub fn load(path: &Path) -> crate::error::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Le fondu ne peut pas dépasser la zone de recouvrement des objectifs.
    pub fn effective_blend(&self) -> f64 {
        self.blend.min(self.fov - 180.0).max(0.0)
    }

    /// Matrices (sphère -> objectif) pour A et B : `d_objectif = M · d_sphère`, `d_sphère` étant la
    /// direction dans l'image équirectangulaire produite (axe de l'objectif A = z, au centre).
    pub fn lens_matrices(&self) -> (Mat3, Mat3) {
        let ra = ypr_to_matrix(self.a_yaw, self.a_pitch, self.a_roll);
        let rb = ypr_to_matrix(self.b_yaw, self.b_pitch, self.b_roll) * rot_yaw(std::f64::consts::PI);
        (ra, rb)
    }

    /// Poids de l'objectif A dans une image équirectangulaire `width x height` (0..1), ligne par ligne.
    pub fn front_weight(&self, width: usize, height: usize) -> Vec<f32> {
        let (ra, _) = self.lens_matrices();
        let half = self.effective_blend() / 2.0;
        let mut out = Vec::with_capacity(width * height);
        for j in 0..height {
            let lat = std::f64::consts::FRAC_PI_2 - (j as f64 + 0.5) / height as f64 * std::f64::consts::PI;
            for i in 0..width {
                let lon = (i as f64 + 0.5) / width as f64 * 2.0 * std::f64::consts::PI - std::f64::consts::PI;
                let (x, y, z) = (lat.cos() * lon.sin(), lat.sin(), lat.cos() * lon.cos());
                let dz = ra[(2, 0)] * x + ra[(2, 1)] * y + ra[(2, 2)] * z;
                let theta = dz.clamp(-1.0, 1.0).acos().to_degrees();
                let w = if half <= 0.0 {
                    if theta < 90.0 {
                        1.0
                    } else {
                        0.0
                    }
                } else {
                    let x = ((theta - (90.0 - half)) / (2.0 * half)).clamp(0.0, 1.0);
                    1.0 - x * x * (3.0 - 2.0 * x)
                };
                out.push(w as f32);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_json() {
        let c: Calibration = serde_json::from_str("{}").unwrap();
        assert_eq!(c, Calibration::default());
        let c: Calibration = serde_json::from_str(r#"{"fov": 205.5, "swap_lenses": true, "inconnu": 1}"#).unwrap();
        assert_eq!((c.fov, c.swap_lenses, c.blend), (205.5, true, 10.0));
    }

    #[test]
    fn blend_is_limited_by_the_overlap() {
        let c = Calibration { fov: 185.0, blend: 20.0, ..Default::default() };
        assert_eq!(c.effective_blend(), 5.0);
        assert_eq!(Calibration { fov: 170.0, ..Default::default() }.effective_blend(), 0.0);
    }

    #[test]
    fn lens_b_looks_backwards() {
        let (ra, rb) = Calibration::default().lens_matrices();
        assert!((ra - Mat3::identity()).abs().max() < 1e-12);
        let back = rb * crate::geometry::Vec3::new(0.0, 0.0, -1.0); // l'arrière de la sphère est l'axe de B
        assert!((back.z - 1.0).abs() < 1e-12);
    }

    #[test]
    fn front_weight_is_one_in_front_and_zero_behind() {
        let c = Calibration::default();
        let (w, h) = (64, 32);
        let f = c.front_weight(w, h);
        assert!(f[(h / 2) * w + w / 2] > 0.999, "centre = avant");
        assert!(f[(h / 2) * w] < 0.001, "bord gauche = arrière");
        assert!(f.iter().all(|v| (0.0..=1.0).contains(v)));
        // symétrie gauche/droite
        for j in 0..h {
            for i in 0..w {
                assert!((f[j * w + i] - f[j * w + (w - 1 - i)]).abs() < 1e-5);
            }
        }
    }
}
