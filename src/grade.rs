//! Réglages d'image : luminosité, contraste, tons moyens (gamma), saturation, chaleur, teinte, et (à l'export
//! seulement) netteté et réduction du bruit.
//!
//! Les réglages de couleur sont appliqués à l'image de sortie I420, par des tables de 256 valeurs (une pour la
//! luminance, une par plan de chrominance) : aucun calcul par pixel au-delà d'une lecture de table. Le calcul se
//! fait dans l'espace Y'CbCr normalisé (Y' dans 0..1, Cb/Cr dans -0.5..0.5, BT.709) :
//!
//! ```text
//! Y'  = clamp((clamp(Y)^(1/gamma) - 0.5) * contraste + 0.5 + luminosité, 0, 1)
//! Cb' = clamp(Cb * saturation - 0.08 * chaleur + 0.051 * teinte, -0.5, 0.5)
//! Cr' = clamp(Cr * saturation + 0.07 * chaleur + 0.06 * teinte, -0.5, 0.5)
//! ```
//!
//! `web/viewer.js` refait ce même calcul sur les pixels de l'aperçu (RGB -> Y'CbCr -> RGB), de sorte que l'aperçu
//! et l'export montrent la même image. La netteté (`unsharp`) et le débruitage (`hqdn3d`) passent par les filtres
//! de FFmpeg ; l'aperçu ne les montre pas.

use crate::error::{export_err, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

/// Bornes des réglages (l'interface les reprend dans `web/geometry.js`).
pub const BRIGHTNESS_RANGE: (f64, f64) = (-0.5, 0.5);
pub const CONTRAST_RANGE: (f64, f64) = (0.5, 2.0);
pub const SATURATION_RANGE: (f64, f64) = (0.0, 2.0);
pub const GAMMA_RANGE: (f64, f64) = (0.5, 2.0);
pub const WARMTH_RANGE: (f64, f64) = (-1.0, 1.0);
pub const TINT_RANGE: (f64, f64) = (-1.0, 1.0);
pub const UNIT_RANGE: (f64, f64) = (0.0, 1.0);

/// Décalage de chrominance (Cb, Cr) pour une chaleur de +1 (orangé) et une teinte de +1 (magenta).
const WARMTH_SHIFT: (f64, f64) = (-0.08, 0.07);
const TINT_SHIFT: (f64, f64) = (0.051, 0.06);

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Grade {
    /// Décalage de luminance, -0.5..0.5 (0 = inchangé).
    pub brightness: f64,
    /// Facteur autour du gris moyen, 0.5..2 (1 = inchangé).
    pub contrast: f64,
    /// Facteur de chrominance, 0..2 (1 = inchangé, 0 = noir et blanc).
    pub saturation: f64,
    /// Tons moyens, 0.5..2 (1 = inchangé, > 1 = plus clair).
    pub gamma: f64,
    /// -1 (bleuté) .. 1 (orangé).
    pub warmth: f64,
    /// -1 (vert) .. 1 (magenta).
    pub tint: f64,
    /// Netteté, 0..1 (export seulement).
    pub sharpen: f64,
    /// Réduction du bruit, 0..1 (export seulement).
    pub denoise: f64,
}

impl Default for Grade {
    fn default() -> Self {
        Grade {
            brightness: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            gamma: 1.0,
            warmth: 0.0,
            tint: 0.0,
            sharpen: 0.0,
            denoise: 0.0,
        }
    }
}

impl Grade {
    /// Vérifie que les valeurs sont des nombres dans leurs bornes (erreur « utilisateur » sinon).
    pub fn validate(&self) -> Result<()> {
        for (name, v, (lo, hi)) in [
            ("luminosité", self.brightness, BRIGHTNESS_RANGE),
            ("contraste", self.contrast, CONTRAST_RANGE),
            ("saturation", self.saturation, SATURATION_RANGE),
            ("tons moyens", self.gamma, GAMMA_RANGE),
            ("chaleur", self.warmth, WARMTH_RANGE),
            ("teinte", self.tint, TINT_RANGE),
            ("netteté", self.sharpen, UNIT_RANGE),
            ("réduction du bruit", self.denoise, UNIT_RANGE),
        ] {
            if !v.is_finite() || v < lo || v > hi {
                return Err(export_err(format!("Réglage d'image « {name} » hors limites : {v} (de {lo} à {hi}).")));
            }
        }
        Ok(())
    }

    /// Les réglages de couleur ne changent rien (la netteté et le bruit ne sont pas des réglages de couleur).
    pub fn color_is_neutral(&self) -> bool {
        let d = Grade::default();
        self.brightness == d.brightness
            && self.contrast == d.contrast
            && self.saturation == d.saturation
            && self.gamma == d.gamma
            && self.warmth == d.warmth
            && self.tint == d.tint
    }

    /// Table de la luminance : valeur I420 (16..235 utile) -> valeur corrigée.
    fn luma_lut(&self) -> [u8; 256] {
        let (b, c, e) = (self.brightness, self.contrast, 1.0 / self.gamma);
        let mut lut = [0u8; 256];
        for (i, out) in lut.iter_mut().enumerate() {
            let y = ((i as f64 - 16.0) / 219.0).clamp(0.0, 1.0).powf(e);
            let y = ((y - 0.5) * c + 0.5 + b).clamp(0.0, 1.0);
            *out = (16.0 + 219.0 * y).round() as u8;
        }
        lut
    }

    /// Table d'un plan de chrominance : `shift` = décalage de chaleur et de teinte pour ce plan.
    fn chroma_lut(&self, shift: f64) -> [u8; 256] {
        let mut lut = [0u8; 256];
        for (i, out) in lut.iter_mut().enumerate() {
            let c = ((i as f64 - 128.0) / 224.0).clamp(-0.5, 0.5);
            let c = (c * self.saturation + shift).clamp(-0.5, 0.5);
            *out = (128.0 + 224.0 * c).round() as u8;
        }
        lut
    }

    /// Décalages de chrominance (Cb, Cr) dus à la chaleur et à la teinte.
    pub fn chroma_shift(&self) -> (f64, f64) {
        (
            WARMTH_SHIFT.0 * self.warmth + TINT_SHIFT.0 * self.tint,
            WARMTH_SHIFT.1 * self.warmth + TINT_SHIFT.1 * self.tint,
        )
    }

    /// Applique les réglages de couleur à une image I420 `w x h` (`frame.len() == w * h * 3 / 2`).
    pub fn apply_i420(&self, frame: &mut [u8], w: usize, h: usize) {
        if self.color_is_neutral() {
            return;
        }
        let (y_plane, uv) = frame.split_at_mut(w * h);
        let (u_plane, v_plane) = uv.split_at_mut(w * h / 4);
        let (shift_u, shift_v) = self.chroma_shift();
        let (ly, lu, lv) = (self.luma_lut(), self.chroma_lut(shift_u), self.chroma_lut(shift_v));
        let map = |plane: &mut [u8], lut: &[u8; 256]| {
            plane.par_chunks_mut(64 * 1024).for_each(|chunk| chunk.iter_mut().for_each(|p| *p = lut[*p as usize]));
        };
        map(y_plane, &ly);
        map(u_plane, &lu);
        map(v_plane, &lv);
    }

    /// Filtre FFmpeg de réduction du bruit (`hqdn3d`), ou `None` si le réglage est nul.
    pub fn denoise_filter(&self) -> Option<String> {
        (self.denoise > 1e-6).then(|| {
            let d = self.denoise;
            format!("hqdn3d={:.3}:{:.3}:{:.3}:{:.3}", 8.0 * d, 6.0 * d, 12.0 * d, 9.0 * d)
        })
    }

    /// Filtre FFmpeg de netteté (`unsharp`, luminance seulement), ou `None` si le réglage est nul.
    pub fn sharpen_filter(&self) -> Option<String> {
        (self.sharpen > 1e-6).then(|| format!("unsharp=5:5:{:.3}:5:5:0", 1.5 * self.sharpen))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Référence indépendante : le calcul en RGB flottant (BT.709 plein champ), tel que le fait le shader.
    fn reference(g: &Grade, rgb: [f64; 3]) -> [f64; 3] {
        let [r, gr, b] = rgb;
        let y = 0.2126 * r + 0.7152 * gr + 0.0722 * b;
        let (cb, cr) = ((b - y) / 1.8556, (r - y) / 1.5748);
        let y = (y.clamp(0.0, 1.0).powf(1.0 / g.gamma) - 0.5) * g.contrast + 0.5 + g.brightness;
        let y = y.clamp(0.0, 1.0);
        let (sb, sr) = g.chroma_shift();
        let cb = (cb * g.saturation + sb).clamp(-0.5, 0.5);
        let cr = (cr * g.saturation + sr).clamp(-0.5, 0.5);
        [
            (y + 1.5748 * cr).clamp(0.0, 1.0),
            (y - 0.187324 * cb - 0.468124 * cr).clamp(0.0, 1.0),
            (y + 1.8556 * cb).clamp(0.0, 1.0),
        ]
    }

    /// RGB (0..1) -> octets I420 limités.
    fn to_yuv(rgb: [f64; 3]) -> [u8; 3] {
        let [r, g, b] = rgb;
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        [
            (16.0 + 219.0 * y).round() as u8,
            (128.0 + 224.0 * (b - y) / 1.8556).round() as u8,
            (128.0 + 224.0 * (r - y) / 1.5748).round() as u8,
        ]
    }

    fn to_rgb(yuv: [u8; 3]) -> [f64; 3] {
        let y = (f64::from(yuv[0]) - 16.0) / 219.0;
        let (cb, cr) = ((f64::from(yuv[1]) - 128.0) / 224.0, (f64::from(yuv[2]) - 128.0) / 224.0);
        [
            (y + 1.5748 * cr).clamp(0.0, 1.0),
            (y - 0.187324 * cb - 0.468124 * cr).clamp(0.0, 1.0),
            (y + 1.8556 * cb).clamp(0.0, 1.0),
        ]
    }

    /// Image I420 2x2 d'un seul pixel de couleur `yuv` (un pixel de chrominance).
    fn flat_frame(yuv: [u8; 3]) -> Vec<u8> {
        vec![yuv[0], yuv[0], yuv[0], yuv[0], yuv[1], yuv[2]]
    }

    fn graded(g: &Grade, yuv: [u8; 3]) -> [u8; 3] {
        let mut f = flat_frame(yuv);
        g.apply_i420(&mut f, 2, 2);
        [f[0], f[4], f[5]]
    }

    #[test]
    fn neutral_grade_changes_nothing_and_is_the_default() {
        let g = Grade::default();
        assert!(g.color_is_neutral() && g.validate().is_ok());
        assert!(g.denoise_filter().is_none() && g.sharpen_filter().is_none());
        // même les valeurs hors de la plage légale passent telles quelles
        let mut f: Vec<u8> = (0..24).map(|i| (i * 11) as u8).collect();
        let before = f.clone();
        g.apply_i420(&mut f, 4, 4);
        assert_eq!(f, before);
        // et les tables, appliquées, seraient l'identité sur la plage légale
        let (ly, lu) = (g.luma_lut(), g.chroma_lut(0.0));
        assert!((16..=235).all(|i| usize::from(ly[i]) == i) && (16..=240).all(|i| usize::from(lu[i]) == i));
        assert_eq!((ly[0], ly[255], lu[0], lu[255]), (16, 235, 16, 240), "hors plage : ramené dans la plage légale");
    }

    /// Écart d'au plus un niveau (l'arrondi des tables peut tomber d'un côté ou de l'autre).
    fn near(a: u8, b: u8) -> bool {
        (i32::from(a) - i32::from(b)).abs() <= 1
    }

    #[test]
    fn the_luma_curve_follows_its_formula() {
        let level = |y: f64| 16 + (219.0 * y).round() as u8;
        // luminosité : décalage de la valeur normalisée
        let g = Grade { brightness: 0.1, ..Default::default() };
        assert!(near(graded(&g, [level(0.5), 128, 128])[0], level(0.6)));
        // contraste 2 : l'écart au gris moyen double (0,25 -> 0)
        let g = Grade { contrast: 2.0, ..Default::default() };
        assert!(near(graded(&g, [level(0.25), 128, 128])[0], level(0.0)));
        assert!(near(graded(&g, [level(0.6), 128, 128])[0], level(0.7)));
        // contraste 0,5 : l'écart diminue de moitié
        let g = Grade { contrast: 0.5, ..Default::default() };
        assert!(near(graded(&g, [level(0.9), 128, 128])[0], level(0.7)));
        // gamma 2 : 0,25 -> 0,5 (racine carrée) ; gamma 0,5 : 0,5 -> 0,25 (carré)
        let g = Grade { gamma: 2.0, ..Default::default() };
        assert!(near(graded(&g, [level(0.25), 128, 128])[0], level(0.5)));
        let g = Grade { gamma: 0.5, ..Default::default() };
        assert!(near(graded(&g, [level(0.5), 128, 128])[0], level(0.25)));
        // les valeurs qui dépassent sont écrêtées dans la plage légale (16..235)
        let g = Grade { brightness: 0.5, ..Default::default() };
        assert_eq!((graded(&g, [16, 128, 128])[0], graded(&g, [235, 128, 128])[0]), (level(0.5), 235));
        let g = Grade { brightness: -0.5, ..Default::default() };
        assert_eq!((graded(&g, [16, 128, 128])[0], graded(&g, [235, 128, 128])[0]), (16, level(0.5)));
        let g = Grade { contrast: 2.0, ..Default::default() };
        assert_eq!((graded(&g, [16, 128, 128])[0], graded(&g, [235, 128, 128])[0]), (16, 235));
    }

    #[test]
    fn saturation_warmth_and_tint_act_on_the_chroma_only() {
        let grey = [120u8, 128, 128];
        assert_eq!(graded(&Grade { saturation: 0.0, ..Default::default() }, [100, 200, 60]), [100, 128, 128]);
        // saturation 2 : l'écart au neutre double
        let out = graded(&Grade { saturation: 2.0, ..Default::default() }, [100, 148, 108]);
        assert_eq!((out[0], out[1], out[2]), (100, 168, 88));
        // chaleur +1 : moins de bleu (Cb), plus de rouge (Cr) ; -1 : l'inverse ; la luminance ne bouge pas
        let warm = graded(&Grade { warmth: 1.0, ..Default::default() }, grey);
        let cool = graded(&Grade { warmth: -1.0, ..Default::default() }, grey);
        assert_eq!((warm[0], cool[0]), (120, 120));
        assert!(warm[1] < 128 && warm[2] > 128 && cool[1] > 128 && cool[2] < 128, "{warm:?} {cool:?}");
        let [r, _, b] = to_rgb(warm);
        assert!(r > b + 0.05, "gris réchauffé : rouge {r} > bleu {b}");
        // teinte +1 : magenta = moins de vert
        let mag = to_rgb(graded(&Grade { tint: 1.0, ..Default::default() }, grey));
        assert!(mag[1] < mag[0] && mag[1] < mag[2], "{mag:?}");
        let green = to_rgb(graded(&Grade { tint: -1.0, ..Default::default() }, grey));
        assert!(green[1] > green[0] && green[1] > green[2], "{green:?}");
    }

    #[test]
    fn i420_result_matches_the_rgb_reference_within_rounding() {
        let grades = [
            Grade { brightness: 0.12, contrast: 1.3, saturation: 1.4, gamma: 1.2, ..Default::default() },
            Grade { brightness: -0.2, contrast: 0.7, saturation: 0.5, gamma: 0.8, warmth: 0.6, ..Default::default() },
            Grade { contrast: 1.6, warmth: -0.8, tint: 0.5, ..Default::default() },
            Grade { saturation: 1.8, tint: -0.7, gamma: 1.7, ..Default::default() },
        ];
        let colors = [
            [0.8, 0.3, 0.2],
            [0.1, 0.5, 0.9],
            [0.5, 0.5, 0.5],
            [0.9, 0.9, 0.2],
            [0.2, 0.7, 0.4],
            [0.05, 0.05, 0.05],
            [0.95, 0.9, 0.85],
        ];
        for g in grades {
            for c in colors {
                let got = to_rgb(graded(&g, to_yuv(c)));
                // la référence part des mêmes octets d'entrée que le moteur
                let want = reference(&g, to_rgb(to_yuv(c)));
                for k in 0..3 {
                    assert!((got[k] - want[k]).abs() < 0.02, "{g:?} sur {c:?} canal {k} : {got:?} vs {want:?}");
                }
            }
        }
    }

    #[test]
    fn apply_touches_every_plane_of_a_larger_frame() {
        let (w, h) = (6, 4);
        let mut f = vec![0u8; w * h * 3 / 2];
        f[..w * h].fill(120);
        f[w * h..w * h * 5 / 4].fill(100);
        f[w * h * 5 / 4..].fill(160);
        let g = Grade { brightness: 0.1, saturation: 0.5, ..Default::default() };
        g.apply_i420(&mut f, w, h);
        assert!(f[..w * h].iter().all(|&y| y == f[0] && y > 120));
        assert!(f[w * h..w * h * 5 / 4].iter().all(|&u| u == 114));
        assert!(f[w * h * 5 / 4..].iter().all(|&v| v == 144));
    }

    #[test]
    fn ffmpeg_filters_scale_with_the_settings() {
        let g = Grade { sharpen: 0.5, denoise: 0.5, ..Default::default() };
        assert_eq!(g.sharpen_filter().unwrap(), "unsharp=5:5:0.750:5:5:0");
        assert_eq!(g.denoise_filter().unwrap(), "hqdn3d=4.000:3.000:6.000:4.500");
        assert!(g.color_is_neutral(), "netteté et bruit ne sont pas de la couleur");
        let full = Grade { sharpen: 1.0, denoise: 1.0, ..Default::default() };
        assert!(full.sharpen_filter().unwrap().contains("1.500") && full.denoise_filter().unwrap().contains("12.000"));
    }

    #[test]
    fn out_of_range_values_are_refused() {
        for bad in [
            Grade { brightness: 0.6, ..Default::default() },
            Grade { contrast: 0.2, ..Default::default() },
            Grade { saturation: 2.5, ..Default::default() },
            Grade { gamma: 0.0, ..Default::default() },
            Grade { warmth: -1.5, ..Default::default() },
            Grade { tint: f64::NAN, ..Default::default() },
            Grade { sharpen: 1.1, ..Default::default() },
            Grade { denoise: -0.1, ..Default::default() },
        ] {
            let e = bad.validate().unwrap_err();
            assert!(crate::error::is_user_error(&e), "{e}");
        }
        assert!(Grade {
            brightness: -0.5,
            contrast: 2.0,
            saturation: 2.0,
            gamma: 0.5,
            warmth: 1.0,
            ..Default::default()
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn json_round_trip_and_missing_fields_use_the_neutral_values() {
        let g: Grade = serde_json::from_str(r#"{"contrast": 1.2, "inconnu": 3}"#).unwrap();
        assert_eq!((g.contrast, g.saturation, g.gamma, g.brightness), (1.2, 1.0, 1.0, 0.0));
        let back: Grade = serde_json::from_value(serde_json::to_value(g).unwrap()).unwrap();
        assert_eq!(back, g);
    }
}
