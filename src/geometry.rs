//! Géométrie sphérique : rotations, quaternions, interpolation de keyframes.
//!
//! Conventions (celles du filtre `v360` de FFmpeg, vérifiées empiriquement avec une image dont la
//! couleur code la direction ; reprises à l'identique de la version Python `x3studio/geometry.py`) :
//!
//! * repère main droite : x vers la droite, y vers le haut, z vers l'avant ;
//! * `M(yaw, pitch, roll)` (ordre `ypr`) transforme la direction d'un pixel de la *sortie* en la
//!   direction échantillonnée dans l'*entrée* : `d_entrée = M · d_sortie` ;
//! * `yaw > 0` regarde à droite, `pitch > 0` regarde vers le haut.
//!
//! `M = Ry(yaw) · Rp(pitch) · Rr(roll)`.

use nalgebra::{Matrix3, Rotation3, UnitQuaternion, Vector3};
use serde::{Deserialize, Serialize};

pub type Mat3 = Matrix3<f64>;
pub type Vec3 = Vector3<f64>;
/// Quaternion unitaire `[w, x, y, z]`.
pub type Quat = [f64; 4];

// --------------------------------------------------------------------------
// Matrices élémentaires (angles en radians)
// --------------------------------------------------------------------------
pub fn rot_yaw(a: f64) -> Mat3 {
    let (s, c) = a.sin_cos();
    Mat3::new(c, 0.0, s, 0.0, 1.0, 0.0, -s, 0.0, c)
}

pub fn rot_pitch(a: f64) -> Mat3 {
    let (s, c) = a.sin_cos();
    Mat3::new(1.0, 0.0, 0.0, 0.0, c, s, 0.0, -s, c)
}

pub fn rot_roll(a: f64) -> Mat3 {
    let (s, c) = a.sin_cos();
    Mat3::new(c, s, 0.0, -s, c, 0.0, 0.0, 0.0, 1.0)
}

/// Angles en degrés -> matrice (convention v360, ordre `ypr`).
pub fn ypr_to_matrix(yaw: f64, pitch: f64, roll: f64) -> Mat3 {
    rot_yaw(yaw.to_radians()) * rot_pitch(pitch.to_radians()) * rot_roll(roll.to_radians())
}

/// Inverse de [`ypr_to_matrix`] (angles en degrés). En blocage de cardan (|pitch| = 90°), roll = 0.
pub fn matrix_to_ypr(m: &Mat3) -> (f64, f64, f64) {
    let sp = m[(1, 2)].clamp(-1.0, 1.0);
    let pitch = sp.asin();
    let (yaw, roll) = if sp.abs() > 0.999_999 {
        ((-m[(2, 0)]).atan2(m[(0, 0)]), 0.0)
    } else {
        (m[(0, 2)].atan2(m[(2, 2)]), (-m[(1, 0)]).atan2(m[(1, 1)]))
    };
    (yaw.to_degrees(), pitch.to_degrees(), roll.to_degrees())
}

/// Ramène un angle en degrés dans [-180, 180).
pub fn wrap180(a: f64) -> f64 {
    (a + 180.0).rem_euclid(360.0) - 180.0
}

// --------------------------------------------------------------------------
// Quaternions
// --------------------------------------------------------------------------
pub fn quat_from_matrix(m: &Mat3) -> Quat {
    let q = UnitQuaternion::from_rotation_matrix(&Rotation3::from_matrix_unchecked(*m));
    [q.w, q.i, q.j, q.k]
}

pub fn matrix_from_quat(q: &Quat) -> Mat3 {
    let n = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    let (w, x, y, z) = (q[0] / n, q[1] / n, q[2] / n, q[3] / n);
    Mat3::new(
        1.0 - 2.0 * (y * y + z * z),
        2.0 * (x * y - z * w),
        2.0 * (x * z + y * w),
        2.0 * (x * y + z * w),
        1.0 - 2.0 * (x * x + z * z),
        2.0 * (y * z - x * w),
        2.0 * (x * z - y * w),
        2.0 * (y * z + x * w),
        1.0 - 2.0 * (x * x + y * y),
    )
}

// --------------------------------------------------------------------------
// Rotation « utilisateur » et vue
// --------------------------------------------------------------------------
/// Cap / inclinaison / roulis en degrés (orientation de l'utilisateur, calibration...).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Ypr {
    pub yaw: f64,
    pub pitch: f64,
    pub roll: f64,
}

impl Ypr {
    pub fn matrix(&self) -> Mat3 {
        ypr_to_matrix(self.yaw, self.pitch, self.roll)
    }
    pub fn is_zero(&self) -> bool {
        self.yaw.abs() < 1e-9 && self.pitch.abs() < 1e-9 && self.roll.abs() < 1e-9
    }
}

/// Vue de la caméra virtuelle : `fov` = champ de vision horizontal en degrés, `distortion` = forme de la
/// perspective (0 = rectiligne, 1 = stéréographique « petite planète » ; projection plate seulement).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct View {
    pub yaw: f64,
    pub pitch: f64,
    pub roll: f64,
    pub fov: f64,
    pub distortion: f64,
}

impl Default for View {
    fn default() -> Self {
        View { yaw: 0.0, pitch: 0.0, roll: 0.0, fov: 90.0, distortion: 0.0 }
    }
}

impl View {
    pub fn matrix(&self) -> Mat3 {
        ypr_to_matrix(self.yaw, self.pitch, self.roll)
    }
}

/// Keyframe : instant `t` (secondes, temps source) ; un champ absent reprend la valeur de la vue de base.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Keyframe {
    pub t: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yaw: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roll: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fov: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distortion: Option<f64>,
}

impl Keyframe {
    pub fn full(t: f64, v: &View) -> Self {
        Keyframe {
            t,
            yaw: Some(v.yaw),
            pitch: Some(v.pitch),
            roll: Some(v.roll),
            fov: Some(v.fov),
            distortion: Some(v.distortion),
        }
    }
    fn view(&self, base: &View) -> View {
        View {
            yaw: self.yaw.unwrap_or(base.yaw),
            pitch: self.pitch.unwrap_or(base.pitch),
            roll: self.roll.unwrap_or(base.roll),
            fov: self.fov.unwrap_or(base.fov),
            distortion: self.distortion.unwrap_or(base.distortion),
        }
    }
}

fn smoothstep(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// Vue à l'instant `t` : interpolation douce (smoothstep) entre keyframes ; le yaw suit le chemin
/// angulaire le plus court ; hors de l'intervalle la vue des extrémités est maintenue ; sans
/// keyframe, `base`. Miroir de `web/geometry.js::interpolateView`.
pub fn interpolate_view(keyframes: &[Keyframe], t: f64, base: &View) -> View {
    if keyframes.is_empty() {
        return *base;
    }
    let mut k: Vec<&Keyframe> = keyframes.iter().collect();
    k.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
    if t <= k[0].t || k.len() == 1 {
        return k[0].view(base);
    }
    if t >= k[k.len() - 1].t {
        return k[k.len() - 1].view(base);
    }
    for w in k.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a.t <= t && t <= b.t {
            let span = b.t - a.t;
            let u = if span > 0.0 { smoothstep((t - a.t) / span) } else { 1.0 };
            let (va, vb) = (a.view(base), b.view(base));
            return View {
                yaw: va.yaw + wrap180(vb.yaw - va.yaw) * u,
                pitch: va.pitch + (vb.pitch - va.pitch) * u,
                roll: va.roll + (vb.roll - va.roll) * u,
                fov: va.fov + (vb.fov - va.fov) * u,
                distortion: va.distortion + (vb.distortion - va.distortion) * u,
            };
        }
    }
    *base
}

/// Champ vertical (degrés) d'une projection rectiligne de champ horizontal `h_fov`.
pub fn flat_vfov(h_fov: f64, width: u32, height: u32) -> f64 {
    2.0 * ((h_fov.to_radians() / 2.0).tan() * height as f64 / width as f64).atan().to_degrees()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: &Mat3, b: &Mat3, eps: f64) -> bool {
        (a - b).abs().max() < eps
    }

    #[test]
    fn ypr_roundtrip() {
        let mut seed = 12345u64;
        let mut rnd = |lo: f64, hi: f64| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            lo + (hi - lo) * ((seed >> 11) as f64 / (1u64 << 53) as f64)
        };
        for _ in 0..200 {
            let (y, p, r) = (rnd(-179.0, 179.0), rnd(-88.0, 88.0), rnd(-179.0, 179.0));
            let m = ypr_to_matrix(y, p, r);
            assert!(close(&(m * m.transpose()), &Mat3::identity(), 1e-12));
            let (y2, p2, r2) = matrix_to_ypr(&m);
            assert!((y - y2).abs() < 1e-6 && (p - p2).abs() < 1e-6 && (r - r2).abs() < 1e-6);
        }
    }

    #[test]
    fn gimbal_lock_still_represents_the_rotation() {
        let m = ypr_to_matrix(30.0, 90.0, 20.0);
        let (y, p, r) = matrix_to_ypr(&m);
        assert!(close(&ypr_to_matrix(y, p, r), &m, 1e-6));
    }

    #[test]
    fn v360_convention_basics() {
        // yaw > 0 : le centre de la sortie échantillonne la direction (sin, 0, cos) = à droite
        let d = ypr_to_matrix(30.0, 0.0, 0.0) * Vec3::new(0.0, 0.0, 1.0);
        assert!((d.x - 0.5).abs() < 1e-12 && d.y.abs() < 1e-12 && (d.z - 0.866_025_403_784_438_6).abs() < 1e-12);
        // pitch > 0 : on regarde vers le haut
        let d = ypr_to_matrix(0.0, 30.0, 0.0) * Vec3::new(0.0, 0.0, 1.0);
        assert!((d.y - 0.5).abs() < 1e-12);
    }

    #[test]
    fn quaternion_roundtrip() {
        for (y, p, r) in [(10.0, 5.0, 3.0), (-170.0, 80.0, 120.0), (0.0, 0.0, 0.0), (179.0, -45.0, -90.0)] {
            let m = ypr_to_matrix(y, p, r);
            assert!(close(&matrix_from_quat(&quat_from_matrix(&m)), &m, 1e-9));
        }
    }

    #[test]
    fn interpolation() {
        let kfs = [
            Keyframe::full(1.0, &View { yaw: 170.0, fov: 90.0, ..View::default() }),
            Keyframe::full(3.0, &View { yaw: -170.0, pitch: 20.0, fov: 50.0, distortion: 1.0, ..View::default() }),
        ];
        let base = View::default();
        assert_eq!(interpolate_view(&kfs, 0.0, &base).yaw, 170.0);
        assert_eq!(interpolate_view(&kfs, 9.0, &base).fov, 50.0);
        let mid = interpolate_view(&kfs, 2.0, &base);
        assert!((mid.yaw.abs() - 180.0).abs() < 1e-9, "chemin le plus court via 180° : {}", mid.yaw);
        assert!((mid.pitch - 10.0).abs() < 1e-9 && (mid.fov - 70.0).abs() < 1e-9);
        assert!((mid.distortion - 0.5).abs() < 1e-9, "la distorsion s'interpole comme le champ : {}", mid.distortion);
        let only_fov = View { fov: 77.0, ..View::default() };
        assert_eq!(interpolate_view(&[], 5.0, &only_fov).fov, 77.0);
        // champ absent d'un keyframe : valeur de la vue de base
        let partial = [Keyframe { t: 0.0, yaw: Some(40.0), ..Default::default() }];
        let v = interpolate_view(&partial, 1.0, &View { pitch: 7.0, ..View::default() });
        assert_eq!((v.yaw, v.pitch), (40.0, 7.0));
    }

    #[test]
    fn flat_vfov_values() {
        assert!((flat_vfov(90.0, 1920, 1080) - 58.716).abs() < 1e-2);
        assert!((flat_vfov(90.0, 1000, 1000) - 90.0).abs() < 1e-9);
    }

    #[test]
    fn keyframe_json_shape() {
        let k: Keyframe = serde_json::from_str(r#"{"t": 1.5, "yaw": 10, "fov": 70}"#).unwrap();
        assert_eq!((k.t, k.yaw, k.pitch, k.fov), (1.5, Some(10.0), None, Some(70.0)));
        assert_eq!(serde_json::to_string(&k).unwrap(), r#"{"t":1.5,"yaw":10.0,"fov":70.0}"#);
    }
}
