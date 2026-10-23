//! Perche à selfie et nadir : une zone conique de l'image, **fixe dans le repère de la caméra** (la perche est
//! solidaire du boîtier, elle ne suit ni la stabilisation ni la vue), est masquée.
//!
//! * `mirror` — reconstruction : le contenu de la zone est remplacé par celui de l'anneau qui l'entoure, réfléchi par
//!   rapport au bord du cône (l'angle `α` à l'axe devient `2ρ - α`, l'azimut est conservé). Continu au bord ;
//!   donne un rendu plausible sur un sol texturé (herbe, gravier, bitume).
//! * `cover` — un disque de couleur unie au bord adouci, comme le « cache de nadir » des logiciels de montage 360°.
//!
//! La même transformation est refaite dans le shader de l'aperçu (`web/viewer.js`).
//!
//! Limite connue : la zone est un cône parfait autour d'un axe choisi à la main ; elle ne détecte pas la perche
//! dans l'image et n'a pas pu être validée sur des images réelles de perche (voir le README).

use crate::error::{export_err, Result};
use serde::{Deserialize, Serialize};

pub const RADIUS_RANGE: (f64, f64) = (1.0, 60.0);
/// Adoucissement minimal du disque de couleur (fraction du rayon) : un bord parfaitement net serait étalé par la
/// grille grossière de l'interpolation.
pub const MIN_SOFTNESS: f64 = 0.02;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StickMode {
    #[default]
    Off,
    Mirror,
    Cover,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stick {
    pub mode: StickMode,
    /// Demi-angle du cône (degrés), 1..60.
    pub radius: f64,
    /// Adoucissement du bord du disque de couleur, 0..1 (fraction du rayon).
    pub softness: f64,
    /// Axe du cône dans le repère de la caméra : cap et inclinaison (degrés). Par défaut vers le bas.
    pub yaw: f64,
    pub pitch: f64,
    /// Couleur du disque (`#rrggbb`).
    pub color: String,
}

impl Default for Stick {
    fn default() -> Self {
        Stick { mode: StickMode::Off, radius: 20.0, softness: 0.3, yaw: 0.0, pitch: -90.0, color: "#202020".into() }
    }
}

/// `#rrggbb` -> octets.
pub fn parse_color(text: &str) -> Option<[u8; 3]> {
    let hex = text.strip_prefix('#')?;
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let v = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some([v(0)?, v(2)?, v(4)?])
}

/// RVB -> Y'CbCr 8 bits BT.709, plage limitée (les valeurs de l'image I420 du moteur).
pub fn rgb_to_yuv(rgb: [u8; 3]) -> [u8; 3] {
    let [r, g, b] = rgb.map(|v| f64::from(v) / 255.0);
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    [
        (16.0 + 219.0 * y).round() as u8,
        (128.0 + 224.0 * (b - y) / 1.8556).round() as u8,
        (128.0 + 224.0 * (r - y) / 1.5748).round() as u8,
    ]
}

impl Stick {
    pub fn validate(&self) -> Result<()> {
        let bad = |what: &str| Err(export_err(format!("Réglage de la perche : {what}")));
        if !(RADIUS_RANGE.0..=RADIUS_RANGE.1).contains(&self.radius) {
            return bad(&format!("le rayon doit être compris entre {} et {}°.", RADIUS_RANGE.0, RADIUS_RANGE.1));
        }
        if !(0.0..=1.0).contains(&self.softness) {
            return bad("l'adoucissement doit être compris entre 0 et 1.");
        }
        if !self.yaw.is_finite() || !(-180.0..=180.0).contains(&self.yaw) {
            return bad("le cap de l'axe doit être compris entre -180 et 180°.");
        }
        if !self.pitch.is_finite() || !(-90.0..=90.0).contains(&self.pitch) {
            return bad("l'inclinaison de l'axe doit être comprise entre -90 et 90°.");
        }
        if parse_color(&self.color).is_none() {
            return bad("la couleur s'écrit #rrggbb.");
        }
        Ok(())
    }

    /// Géométrie prête à servir au rendu ; `None` si la fonction est désactivée.
    pub fn geometry(&self) -> Option<StickGeom> {
        if self.mode == StickMode::Off {
            return None;
        }
        let (yaw, pitch) = (self.yaw.to_radians(), self.pitch.to_radians());
        // repère de la caméra : x à droite, y en haut, z devant ; l'inclinaison -90° est le nadir
        let axis = [pitch.cos() * yaw.sin(), pitch.sin(), pitch.cos() * yaw.cos()];
        let helper = if axis[0].abs() < 0.9 { [1.0, 0.0, 0.0] } else { [0.0, 0.0, 1.0] };
        let perp = normalize(cross(axis, helper));
        let rho = self.radius.clamp(RADIUS_RANGE.0, RADIUS_RANGE.1).to_radians();
        let soft = self.softness.clamp(MIN_SOFTNESS, 1.0);
        Some(StickGeom {
            mode: self.mode,
            axis,
            perp,
            rho,
            rho_in: rho * (1.0 - soft),
            yuv: rgb_to_yuv(parse_color(&self.color).unwrap_or([32, 32, 32])),
        })
    }
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn normalize(v: [f64; 3]) -> [f64; 3] {
    let n = dot(v, v).sqrt();
    [v[0] / n, v[1] / n, v[2] / n]
}

/// Cône de la perche prêt à l'emploi (angles en radians).
#[derive(Debug, Clone, Copy)]
pub struct StickGeom {
    pub mode: StickMode,
    pub axis: [f64; 3],
    /// Vecteur unitaire orthogonal à l'axe, pour le point exactement sur l'axe (azimut indéfini).
    perp: [f64; 3],
    pub rho: f64,
    /// Rayon en deçà duquel le disque de couleur est opaque.
    pub rho_in: f64,
    /// Couleur du disque en Y'CbCr limité.
    pub yuv: [u8; 3],
}

impl StickGeom {
    /// Angle (rad) entre la direction `d` (unitaire, repère de la caméra) et l'axe.
    pub fn angle(&self, d: [f64; 3]) -> f64 {
        dot(d, self.axis).clamp(-1.0, 1.0).acos()
    }

    /// Direction à échantillonner pour la direction `d` (repère de la caméra) et opacité du disque de couleur.
    #[inline]
    pub fn apply(&self, d: [f64; 3]) -> ([f64; 3], f32) {
        let c = dot(d, self.axis).clamp(-1.0, 1.0);
        let a = c.acos();
        match self.mode {
            StickMode::Off => (d, 0.0),
            StickMode::Mirror => {
                if a >= self.rho {
                    return (d, 0.0);
                }
                let e = [d[0] - c * self.axis[0], d[1] - c * self.axis[1], d[2] - c * self.axis[2]];
                let s = dot(e, e).sqrt();
                let u = if s > 1e-9 { [e[0] / s, e[1] / s, e[2] / s] } else { self.perp };
                let a2 = 2.0 * self.rho - a;
                let (ca, sa) = (a2.cos(), a2.sin());
                ([ca * self.axis[0] + sa * u[0], ca * self.axis[1] + sa * u[1], ca * self.axis[2] + sa * u[2]], 0.0)
            }
            StickMode::Cover => {
                let t = ((a - self.rho_in) / (self.rho - self.rho_in)).clamp(0.0, 1.0);
                (d, (1.0 - t * t * (3.0 - 2.0 * t)) as f32)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(yaw: f64, pitch: f64) -> [f64; 3] {
        let (y, p) = (yaw.to_radians(), pitch.to_radians());
        [p.cos() * y.sin(), p.sin(), p.cos() * y.cos()]
    }

    fn geom(mode: StickMode, radius: f64) -> StickGeom {
        Stick { mode, radius, ..Default::default() }.geometry().unwrap()
    }

    #[test]
    fn off_has_no_geometry_and_the_default_points_down() {
        assert!(Stick::default().geometry().is_none());
        let g = geom(StickMode::Mirror, 20.0);
        assert!(g.axis[0].abs() < 1e-12 && (g.axis[1] + 1.0).abs() < 1e-12 && g.axis[2].abs() < 1e-12);
        let tilted = Stick { mode: StickMode::Cover, yaw: 90.0, pitch: 0.0, ..Default::default() }.geometry().unwrap();
        assert!((tilted.axis[0] - 1.0).abs() < 1e-12, "cap 90° : vers la droite");
    }

    #[test]
    fn mirror_reflects_the_angle_about_the_cone_edge_and_keeps_the_azimuth() {
        let g = geom(StickMode::Mirror, 20.0);
        for azimuth in [0.0f64, 37.0, 120.0, -75.0, 180.0] {
            for angle in [2.0f64, 8.0, 15.0, 19.5] {
                // direction à `angle` degrés du nadir, dans le plan d'azimut donné
                let a = angle.to_radians();
                let az = azimuth.to_radians();
                let d = [a.sin() * az.cos(), -a.cos(), a.sin() * az.sin()];
                let (m, cover) = g.apply(d);
                assert_eq!(cover, 0.0);
                let want = (40.0f64 - angle).to_radians();
                assert!((g.angle(m) - want).abs() < 1e-9, "azimut {azimuth}°, {angle}° -> {}", g.angle(m).to_degrees());
                // même plan d'azimut : le produit vectoriel avec l'axe garde la même orientation
                let (cx, cz) = (d[0], d[2]);
                let (mx, mz) = (m[0], m[2]);
                assert!((cx * mz - cz * mx).abs() < 1e-9, "l'azimut est conservé");
                assert!(cx * mx + cz * mz > 0.0);
                let n = (m[0] * m[0] + m[1] * m[1] + m[2] * m[2]).sqrt();
                assert!((n - 1.0).abs() < 1e-12);
            }
        }
        // continu au bord, identité au-delà
        let edge = unit(10.0, -70.0 + 0.0); // 20° du nadir
        let (m, _) = g.apply(edge);
        assert!((m[0] - edge[0]).abs() < 1e-9 && (m[1] - edge[1]).abs() < 1e-9 && (m[2] - edge[2]).abs() < 1e-9);
        for d in [unit(0.0, 0.0), unit(80.0, -45.0), unit(0.0, 90.0), unit(0.0, -60.0)] {
            assert_eq!(g.apply(d).0, d, "hors du cône : inchangé");
        }
        // le nadir exact est envoyé sur l'anneau à 2ρ
        let (m, _) = g.apply([0.0, -1.0, 0.0]);
        assert!((g.angle(m) - 40.0f64.to_radians()).abs() < 1e-9);
    }

    #[test]
    fn cover_is_opaque_inside_soft_at_the_edge_and_absent_outside() {
        let g = Stick { mode: StickMode::Cover, radius: 20.0, softness: 0.5, ..Default::default() }.geometry().unwrap();
        let at = |angle: f64| g.apply(unit(0.0, -90.0 + angle)).1;
        assert_eq!(at(0.0), 1.0);
        assert_eq!(at(9.9), 1.0, "opaque jusqu'à 10° (rayon moins moitié d'adoucissement)");
        assert!(at(15.0) > 0.4 && at(15.0) < 0.6, "à mi-bord : {}", at(15.0));
        assert_eq!(at(20.0), 0.0);
        assert_eq!(at(45.0), 0.0);
        let (a, b, c) = (at(11.0), at(13.0), at(17.0));
        assert!(a > b && b > c, "décroissant : {a} {b} {c}");
        // une zone sans adoucissement garde un bord adouci minimal
        let hard = Stick { mode: StickMode::Cover, radius: 20.0, softness: 0.0, ..Default::default() };
        let g = hard.geometry().unwrap();
        assert!(g.rho - g.rho_in > 0.005);
    }

    #[test]
    fn colors_parse_and_convert_to_limited_range_yuv() {
        assert_eq!(parse_color("#ff8000"), Some([255, 128, 0]));
        assert_eq!(parse_color("#FFFFFF"), Some([255, 255, 255]));
        for bad in ["ff8000", "#ff80", "#gg0000", "#ff80000", "", "#ééé"] {
            assert_eq!(parse_color(bad), None, "{bad}");
        }
        assert_eq!(rgb_to_yuv([0, 0, 0]), [16, 128, 128]);
        assert_eq!(rgb_to_yuv([255, 255, 255]), [235, 128, 128]);
        let red = rgb_to_yuv([255, 0, 0]);
        assert!(red[2] > 200 && red[1] < 128, "{red:?}");
    }

    #[test]
    fn validation_names_the_faulty_setting() {
        assert!(Stick::default().validate().is_ok());
        for (bad, word) in [
            (Stick { radius: 0.5, ..Default::default() }, "rayon"),
            (Stick { radius: 61.0, ..Default::default() }, "rayon"),
            (Stick { softness: 1.5, ..Default::default() }, "adoucissement"),
            (Stick { yaw: 200.0, ..Default::default() }, "cap"),
            (Stick { pitch: -91.0, ..Default::default() }, "inclinaison"),
            (Stick { pitch: f64::NAN, ..Default::default() }, "inclinaison"),
            (Stick { color: "rouge".into(), ..Default::default() }, "couleur"),
        ] {
            let e = bad.validate().unwrap_err();
            assert!(crate::error::is_user_error(&e) && e.to_string().contains(word), "{e}");
        }
    }

    #[test]
    fn json_uses_lowercase_modes_and_defaults_for_missing_fields() {
        let s: Stick = serde_json::from_str(r##"{"mode": "mirror", "radius": 12.5}"##).unwrap();
        assert_eq!((s.mode, s.radius, s.pitch, s.color.as_str()), (StickMode::Mirror, 12.5, -90.0, "#202020"));
        assert_eq!(serde_json::to_value(StickMode::Cover).unwrap(), "cover");
        assert!(serde_json::from_str::<Stick>(r#"{"mode": "gomme"}"#).is_err());
    }
}
