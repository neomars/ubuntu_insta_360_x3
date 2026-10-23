//! Plusieurs perspectives dans une même vidéo : côte à côte, haut / bas, incrustation, une grande vue et deux petites,
//! quatre vues. Chaque vue a sa projection, sa caméra virtuelle et ses keyframes ; la première est celle du projet
//! (`projection`, `view`, `keyframes`), les suivantes sont dans [`Multi::views`].
//!
//! Les rectangles des vues sont calculés à partir de fractions de l'image, alignés sur des pixels pairs (le format
//! I420 sous-échantillonne la chrominance) ; `web/geometry.js::layoutRects` fait le même calcul pour l'aperçu.

use crate::error::{export_err, Result};
use crate::geometry::{wrap180, Keyframe, View};
use serde::{Deserialize, Serialize};

/// Plus petite largeur ou hauteur d'une vue (px) : en deçà, la disposition est refusée.
pub const MIN_TILE_SIDE: u32 = 64;
/// Écart maximal entre les vues (px, multiple de 4).
pub const MAX_GAP: u32 = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MultiLayout {
    /// Une seule vue (défaut).
    #[default]
    Single,
    /// Deux vues côte à côte.
    Side,
    /// Deux vues l'une au-dessus de l'autre.
    Stack,
    /// Une vue plein cadre et une incrustation en bas à droite.
    Pip,
    /// Une grande vue à gauche, deux petites empilées à droite.
    Trio,
    /// Quatre vues, deux par deux.
    Quad,
}

/// Fractions `[x0, y0, x1, y1]` de l'image de chaque vue.
const SINGLE: [[f64; 4]; 1] = [[0.0, 0.0, 1.0, 1.0]];
const SIDE: [[f64; 4]; 2] = [[0.0, 0.0, 0.5, 1.0], [0.5, 0.0, 1.0, 1.0]];
const STACK: [[f64; 4]; 2] = [[0.0, 0.0, 1.0, 0.5], [0.0, 0.5, 1.0, 1.0]];
const PIP: [[f64; 4]; 2] = [[0.0, 0.0, 1.0, 1.0], [0.67, 0.67, 0.97, 0.97]];
const TRIO: [[f64; 4]; 3] = [[0.0, 0.0, 2.0 / 3.0, 1.0], [2.0 / 3.0, 0.0, 1.0, 0.5], [2.0 / 3.0, 0.5, 1.0, 1.0]];
const QUAD: [[f64; 4]; 4] = [[0.0, 0.0, 0.5, 0.5], [0.5, 0.0, 1.0, 0.5], [0.0, 0.5, 0.5, 1.0], [0.5, 0.5, 1.0, 1.0]];

impl MultiLayout {
    fn fractions(self) -> &'static [[f64; 4]] {
        match self {
            MultiLayout::Single => &SINGLE,
            MultiLayout::Side => &SIDE,
            MultiLayout::Stack => &STACK,
            MultiLayout::Pip => &PIP,
            MultiLayout::Trio => &TRIO,
            MultiLayout::Quad => &QUAD,
        }
    }

    /// Nombre de vues de la disposition.
    pub fn tiles(self) -> usize {
        self.fractions().len()
    }
}

/// Rectangle d'une vue dans l'image de sortie (pixels, origine en haut à gauche, valeurs paires).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Arrondit à l'entier pair le plus proche.
fn even(v: f64) -> u32 {
    ((v / 2.0).round() * 2.0).max(0.0) as u32
}

/// Rectangles des vues d'une image `w x h`. `gap` (multiple de 4) : écart entre les vues, pris sur les bords communs ;
/// ignoré pour l'incrustation.
pub fn layout_rects(layout: MultiLayout, w: u32, h: u32, gap: u32) -> Vec<Rect> {
    let (wf, hf) = (f64::from(w), f64::from(h));
    let inset = if layout == MultiLayout::Pip { 0 } else { gap / 2 };
    layout
        .fractions()
        .iter()
        .map(|f| {
            let (x0, x1, y0, y1) = (even(f[0] * wf), even(f[2] * wf), even(f[1] * hf), even(f[3] * hf));
            // un bord n'est « partagé » que s'il n'est pas sur le bord de l'image
            let (x0, x1) = (x0 + if f[0] > 0.0 { inset } else { 0 }, x1 - if f[2] < 1.0 { inset } else { 0 });
            let (y0, y1) = (y0 + if f[1] > 0.0 { inset } else { 0 }, y1 - if f[3] < 1.0 { inset } else { 0 });
            Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
        })
        .collect()
}

/// Une vue supplémentaire : projection, caméra virtuelle fixe et keyframes (comme celles du projet).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TileView {
    pub projection: String,
    pub view: View,
    pub keyframes: Vec<Keyframe>,
}

impl Default for TileView {
    fn default() -> Self {
        TileView { projection: "flat".into(), view: View::default(), keyframes: vec![] }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Multi {
    pub layout: MultiLayout,
    /// Écart entre les vues (px, multiple de 4, 0 à 40).
    pub gap: u32,
    /// Vues 2, 3… (la vue 1 est celle du projet). Les vues absentes prennent [`Multi::default_tile`].
    pub views: Vec<TileView>,
}

impl Multi {
    /// Vue par défaut de la vue n° `index` (1 = la deuxième) d'une disposition de `count` vues : le cap se répartit
    /// régulièrement sur le tour (côte à côte : avant et arrière).
    pub fn default_tile(index: usize, count: usize) -> TileView {
        let yaw = wrap180(360.0 * index as f64 / count.max(1) as f64);
        TileView { view: View { yaw, ..View::default() }, ..TileView::default() }
    }

    /// La vue n° `index` (1 = la deuxième) de la disposition courante.
    pub fn tile(&self, index: usize) -> TileView {
        self.views.get(index - 1).cloned().unwrap_or_else(|| Multi::default_tile(index, self.layout.tiles()))
    }

    pub fn validate(&self) -> Result<()> {
        if self.gap > MAX_GAP || !self.gap.is_multiple_of(4) {
            return Err(export_err(format!(
                "L'écart entre les vues doit être un multiple de 4 entre 0 et {MAX_GAP} px."
            )));
        }
        for (i, v) in self.views.iter().enumerate() {
            if !matches!(v.projection.as_str(), "flat" | "fisheye" | "planet") {
                return Err(export_err(format!("Vue {} : projection inconnue : {}", i + 2, v.projection)));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: u32, y: u32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn layouts_split_a_1080p_frame_into_even_rectangles() {
        assert_eq!(layout_rects(MultiLayout::Single, 1920, 1080, 0), vec![r(0, 0, 1920, 1080)]);
        assert_eq!(layout_rects(MultiLayout::Side, 1920, 1080, 0), vec![r(0, 0, 960, 1080), r(960, 0, 960, 1080)]);
        assert_eq!(layout_rects(MultiLayout::Stack, 1920, 1080, 0), vec![r(0, 0, 1920, 540), r(0, 540, 1920, 540)]);
        assert_eq!(layout_rects(MultiLayout::Pip, 1920, 1080, 0), vec![r(0, 0, 1920, 1080), r(1286, 724, 576, 324)]);
        assert_eq!(
            layout_rects(MultiLayout::Trio, 1920, 1080, 0),
            vec![r(0, 0, 1280, 1080), r(1280, 0, 640, 540), r(1280, 540, 640, 540)]
        );
        assert_eq!(
            layout_rects(MultiLayout::Quad, 1920, 1080, 0),
            vec![r(0, 0, 960, 540), r(960, 0, 960, 540), r(0, 540, 960, 540), r(960, 540, 960, 540)]
        );
    }

    #[test]
    fn gaps_are_taken_on_shared_edges_only_and_tiles_stay_even() {
        assert_eq!(
            layout_rects(MultiLayout::Side, 1920, 1080, 8),
            vec![r(0, 0, 956, 1080), r(964, 0, 956, 1080)],
            "4 px de chaque côté de la jonction, rien sur les bords de l'image"
        );
        assert_eq!(
            layout_rects(MultiLayout::Quad, 1920, 1080, 8),
            vec![r(0, 0, 956, 536), r(964, 0, 956, 536), r(0, 544, 956, 536), r(964, 544, 956, 536)]
        );
        assert_eq!(layout_rects(MultiLayout::Pip, 1920, 1080, 8), layout_rects(MultiLayout::Pip, 1920, 1080, 0));
        // des tailles qui ne se divisent pas : les rectangles restent pairs et se touchent sans se recouvrir
        for (w, h) in [(1366u32, 768u32), (1080, 1920), (854, 480), (1000, 1000)] {
            for layout in [MultiLayout::Side, MultiLayout::Stack, MultiLayout::Trio, MultiLayout::Quad] {
                for gap in [0, 8] {
                    let rects = layout_rects(layout, w, h, gap);
                    assert_eq!(rects.len(), layout.tiles());
                    for t in &rects {
                        assert!([t.x, t.y, t.w, t.h].iter().all(|v| v % 2 == 0), "{layout:?} {w}x{h} : {t:?}");
                        assert!(t.x + t.w <= w && t.y + t.h <= h && t.w > 0 && t.h > 0);
                    }
                    for (i, a) in rects.iter().enumerate() {
                        for b in &rects[i + 1..] {
                            let apart = a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y;
                            assert!(apart, "{layout:?} {w}x{h} : {a:?} recouvre {b:?}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn missing_tiles_are_spread_around_the_circle() {
        let m = Multi { layout: MultiLayout::Quad, ..Default::default() };
        let yaws: Vec<f64> = (1..4).map(|i| m.tile(i).view.yaw).collect();
        assert_eq!(yaws, vec![90.0, -180.0, -90.0]);
        let side = Multi { layout: MultiLayout::Side, ..Default::default() };
        assert_eq!(side.tile(1).view.yaw, -180.0, "côte à côte : la deuxième vue regarde derrière");
        // une vue donnée est reprise telle quelle
        let custom = Multi {
            layout: MultiLayout::Side,
            views: vec![TileView {
                projection: "fisheye".into(),
                view: View { yaw: 33.0, ..View::default() },
                keyframes: vec![],
            }],
            ..Default::default()
        };
        assert_eq!((custom.tile(1).projection.as_str(), custom.tile(1).view.yaw), ("fisheye", 33.0));
    }

    #[test]
    fn validation_and_json() {
        assert!(Multi::default().validate().is_ok());
        for bad in [Multi { gap: 6, ..Default::default() }, Multi { gap: 44, ..Default::default() }] {
            assert!(crate::error::is_user_error(&bad.validate().unwrap_err()));
        }
        let m: Multi = serde_json::from_str(
            r#"{"layout": "quad", "gap": 8, "views": [{"projection": "planet", "view": {"yaw": 10, "fov": 200}}]}"#,
        )
        .unwrap();
        assert_eq!(
            (m.layout, m.gap, m.views[0].projection.as_str(), m.views[0].view.yaw),
            (MultiLayout::Quad, 8, "planet", 10.0)
        );
        assert_eq!(m.views[0].view.pitch, 0.0, "champs absents : valeurs par défaut");
        let bad =
            Multi { views: vec![TileView { projection: "cube".into(), ..Default::default() }], ..Default::default() };
        let e = bad.validate().unwrap_err();
        assert!(e.to_string().contains("Vue 2"), "{e}");
        assert!(serde_json::from_str::<Multi>(r#"{"layout": "mosaique"}"#).is_err());
    }
}
