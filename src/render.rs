//! Moteur de rendu direct « objectifs -> vue » (Rust pur, `rayon`).
//!
//! Pour chaque image de sortie : direction de chaque pixel → rotation (stabilisation, horizon,
//! keyframes) → pixel correspondant dans chacun des deux fisheye (interpolation bilinéaire) → fondu
//! de la couture. Les tables de correspondance sont calculées sur une grille grossière (division 4 à
//! 8) puis interpolées bilinéairement à la volée (sémantique de `cv2.resize` INTER_LINEAR). Spec :
//! `x3studio/render.py` (mêmes formules, même résultat à quelques niveaux de gris près ; vérifiées
//! contre `v360` de FFmpeg dans `tests/render.rs`).
//!
//! Formats : images **I420** (planaire 4:2:0, 8 bits) ; plans Y, U, V contigus.

use crate::calibration::Calibration;
use crate::error::{export_err, Result};
use crate::geometry::Mat3;
use rayon::prelude::*;
use std::f64::consts::PI;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Projection {
    /// Rectiligne (`flat`).
    Flat,
    /// Fisheye équidistant.
    Fisheye,
    /// Petite planète (stéréographique `sg`).
    Planet,
    /// Équirectangulaire 360° (export 360, analyse du mouvement).
    Equirect,
}

impl FromStr for Projection {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "flat" => Ok(Projection::Flat),
            "fisheye" => Ok(Projection::Fisheye),
            "planet" => Ok(Projection::Planet),
            "equirect" => Ok(Projection::Equirect),
            other => Err(export_err(format!("Projection inconnue : {other}"))),
        }
    }
}

/// Disposition de l'image d'entrée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// Deux fisheye côte à côte `[flux 0 | flux 1]`, chacun `lens x lens` : image `2*lens x lens`.
    /// `calib.swap_lenses` est géré par le rendu (l'objectif A « avant » est alors à droite).
    Dual { lens: u32 },
    /// Vidéo déjà assemblée : image équirectangulaire `w x h` (`w == 2h`).
    Equirect { w: u32, h: u32 },
}

// ---------------------------------------------------------------------------
// Rayons de sortie
// ---------------------------------------------------------------------------
/// Distorsion maximale acceptée (au-delà, la projection se replie sur elle-même).
pub const MAX_DISTORTION: f64 = 2.0;

/// Demi-angle maximal (rad) de la perspective générale de distorsion `d` : au-delà, la formule diverge
/// (`d < 1` : asymptote, `d = 0` : 90°) ou se replie (`d > 1` : la sphère vue de l'extérieur).
pub fn perspective_limit(d: f64) -> f64 {
    if d >= 1.0 {
        (-1.0 / d).acos()
    } else {
        (-d.max(0.0)).acos()
    }
}

/// Perspective générale (point de vue à la distance `d` du centre de la sphère) : un rayon d'angle `θ` à l'axe
/// arrive à `r = (d+1)·sin θ / (d + cos θ)` ; `d = 0` est la projection rectiligne, `d = 1` la stéréographique
/// (petite planète), `d → ∞` l'orthographique. `r` est normalisé pour que le bord horizontal de l'image (`|u| = 1`)
/// corresponde à `fov / 2` quelle que soit la distorsion : la distorsion change la forme sans changer le champ.
/// Inverse en forme close : `θ = atan2(r, d+1) + asin(r·d / √((d+1)² + r²))`.
#[inline]
fn general_ray(u: f64, v: f64, fov: f64, aspect: f64, d: f64) -> [f64; 3] {
    let half = (fov / 2.0).min(perspective_limit(d) * 0.999);
    let ge = (d + 1.0) * half.sin() / (d + half.cos());
    let (x, y) = (u * ge, v * aspect * ge);
    let r = x.hypot(y);
    let big_r = ((d + 1.0).powi(2) + r * r).sqrt();
    let th = r.atan2(d + 1.0) + (r * d / big_r).clamp(-1.0, 1.0).asin();
    let s = if r > 1e-9 { th.sin() / r } else { 1.0 };
    [x * s, -y * s, th.cos()]
}

/// Direction (normalisée) du pixel de coordonnées normalisées `(u, v)` ∈ [-1, 1]² (v vers le bas) ;
/// `fov` = champ horizontal en radians, `aspect` = h/w. Même calcul (en f64) que `output_rays` de Python.
/// `dist` : distorsion de la projection `Flat` (0 = rectiligne), ignorée par les autres.
#[inline]
fn ray_at(proj: Projection, u: f64, v: f64, fov: f64, aspect: f64, dist: f64) -> [f64; 3] {
    match proj {
        Projection::Flat if dist > 1e-6 => general_ray(u, v, fov, aspect, dist.min(MAX_DISTORTION)),
        Projection::Flat => {
            let t = (fov / 2.0).tan();
            let (x, y, z) = (u * t, -v * t * aspect, 1.0);
            let n = (x * x + y * y + z * z).sqrt();
            [x / n, y / n, z / n]
        }
        Projection::Fisheye => {
            let xa = u * fov / 2.0;
            let ya = v * aspect * fov / 2.0;
            let th = xa.hypot(ya);
            let sinc = if th > 1e-9 { th.sin() / th } else { 1.0 };
            [xa * sinc, -ya * sinc, th.cos()]
        }
        Projection::Planet => {
            // stéréographique : échelles tan(fov/4) distinctes en h et v (comme v360)
            let kx = (fov / 4.0).tan();
            let ky = (fov * aspect / 4.0).tan();
            let (xs, ys) = (u * kx, v * ky);
            let r = xs.hypot(ys);
            let th = 2.0 * r.atan();
            let s = if r > 1e-9 { th.sin() / r } else { 2.0 };
            [xs * s, -ys * s, th.cos()]
        }
        Projection::Equirect => {
            let (lon, lat) = (u * PI, -v * PI / 2.0);
            [lat.cos() * lon.sin(), lat.sin(), lat.cos() * lon.cos()]
        }
    }
}

/// Rayons de sortie d'une grille grossière `gw x gh` (centres de cellules) couvrant les lignes
/// `[y0, y1)` d'une image `out_w x out_h` ; `fov_deg` = champ horizontal. Un rayon `(x, y, z)` normalisé
/// par cellule, ligne par ligne. Exposé pour les tests de conformité.
#[allow(clippy::too_many_arguments)]
pub fn output_rays(
    proj: Projection,
    gw: usize,
    gh: usize,
    fov_deg: f64,
    out_w: u32,
    out_h: u32,
    y0: u32,
    y1: u32,
) -> Vec<[f32; 3]> {
    output_rays_with(proj, 0.0, gw, gh, fov_deg, out_w, out_h, y0, y1)
}

/// Comme [`output_rays`], avec la distorsion `dist` de la projection `Flat`.
#[allow(clippy::too_many_arguments)]
pub fn output_rays_with(
    proj: Projection,
    dist: f64,
    gw: usize,
    gh: usize,
    fov_deg: f64,
    out_w: u32,
    out_h: u32,
    y0: u32,
    y1: u32,
) -> Vec<[f32; 3]> {
    let aspect = out_h as f64 / out_w as f64;
    let fov = fov_deg.to_radians();
    let mut out = Vec::with_capacity(gw * gh);
    for gy in 0..gh {
        let ypix = y0 as f64 + (gy as f64 + 0.5) / gh as f64 * (y1 - y0) as f64;
        let v = ypix / out_h as f64 * 2.0 - 1.0;
        for gx in 0..gw {
            let u = (gx as f64 + 0.5) / gw as f64 * 2.0 - 1.0;
            let r = ray_at(proj, u, v, fov, aspect, dist);
            out.push([r[0] as f32, r[1] as f32, r[2] as f32]);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Interpolation de la grille grossière (sémantique de cv2.resize INTER_LINEAR)
// ---------------------------------------------------------------------------
#[derive(Clone, Copy, Debug)]
struct Tap {
    i0: u32,
    i1: u32,
    f: f32,
}

/// Pour chaque pixel de destination `0..dst`, indices (voisin bas, voisin haut) et poids dans une source
/// de taille `src` : centres de pixels alignés, bords répliqués.
fn taps(dst: usize, src: usize) -> Vec<Tap> {
    (0..dst)
        .map(|i| {
            let s = (i as f64 + 0.5) * src as f64 / dst as f64 - 0.5;
            let fl = s.floor();
            let (mut i0, mut f) = (fl as i64, (s - fl) as f32);
            if i0 < 0 {
                i0 = 0;
                f = 0.0;
            }
            if i0 >= src as i64 - 1 {
                i0 = src as i64 - 1;
                f = 0.0;
            }
            Tap { i0: i0 as u32, i1: ((i0 + 1).min(src as i64 - 1)) as u32, f }
        })
        .collect()
}

/// Cellule de la table grossière : coordonnées dans l'objectif A (x, y), dans l'objectif B (x, y), poids de A.
type Cell = [f32; 5];

// ---------------------------------------------------------------------------
// Moteur
// ---------------------------------------------------------------------------
/// Rend une vue à partir d'une image I420. `Sync` : un même moteur est partagé entre threads.
pub struct LensRenderer {
    calib: Calibration,
    src: SourceKind,
    proj: Projection,
    out_w: usize,
    out_h: usize,
    gw: usize,
    gh: usize,
    luma_cols: Vec<Tap>,
    luma_rows: Vec<Tap>,
    chroma_cols: Vec<Tap>,
    chroma_rows: Vec<Tap>,
    frame_in: usize,
    frame_out: usize,
}

/// Lignes de sortie traitées par tâche rayon (paire : la chrominance a la moitié).
const CHUNK_ROWS: usize = 16;

impl LensRenderer {
    /// `out_w`/`out_h` pairs. `grid_div` : 4 (≤ 4 Mpx) ou 8. Erreur `ExportError` sinon.
    pub fn new(
        calib: &Calibration,
        src: SourceKind,
        out_w: u32,
        out_h: u32,
        proj: Projection,
        grid_div: u32,
    ) -> Result<Self> {
        if out_w == 0 || out_h == 0 || !out_w.is_multiple_of(2) || !out_h.is_multiple_of(2) {
            return Err(export_err("La taille de sortie doit être paire."));
        }
        let frame_in = match src {
            SourceKind::Dual { lens } => {
                if lens == 0 || !lens.is_multiple_of(2) {
                    return Err(export_err("La taille d'un objectif doit être paire."));
                }
                (2 * lens as usize) * lens as usize * 3 / 2
            }
            SourceKind::Equirect { w, h } => {
                if w == 0 || h == 0 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
                    return Err(export_err("La taille de l'image équirectangulaire doit être paire."));
                }
                w as usize * h as usize * 3 / 2
            }
        };
        let (out_w, out_h) = (out_w as usize, out_h as usize);
        let div = grid_div.max(1) as usize;
        let (gw, gh) = ((out_w / div).max(2), (out_h / div).max(2));
        Ok(LensRenderer {
            calib: calib.clone(),
            src,
            proj,
            out_w,
            out_h,
            gw,
            gh,
            luma_cols: taps(out_w, gw),
            luma_rows: taps(out_h, gh),
            chroma_cols: taps(out_w / 2, gw),
            chroma_rows: taps(out_h / 2, gh),
            frame_in,
            frame_out: out_w * out_h * 3 / 2,
        })
    }

    /// Octets d'une image d'entrée I420.
    pub fn frame_bytes_in(&self) -> usize {
        self.frame_in
    }

    /// Octets d'une image de sortie I420 (`out_w * out_h * 3 / 2`).
    pub fn frame_bytes_out(&self) -> usize {
        self.frame_out
    }

    pub fn out_size(&self) -> (u32, u32) {
        (self.out_w as u32, self.out_h as u32)
    }

    /// `m` : sortie -> sphère de la caméra (`stab · V`), `fov_deg` : champ horizontal (ignoré pour
    /// `Equirect`). `out.len() == frame_bytes_out()`, `frame.len() == frame_bytes_in()`.
    pub fn render(&self, frame: &[u8], m: &Mat3, fov_deg: f64, out: &mut [u8]) {
        self.render_with(frame, m, fov_deg, 0.0, out);
    }

    /// Comme [`LensRenderer::render`], avec la distorsion `dist` (0 = rectiligne, 1 = stéréographique ; projection
    /// `Flat` seulement).
    pub fn render_with(&self, frame: &[u8], m: &Mat3, fov_deg: f64, dist: f64, out: &mut [u8]) {
        assert_eq!(frame.len(), self.frame_in, "taille d'image d'entrée inattendue");
        assert_eq!(out.len(), self.frame_out, "taille d'image de sortie inattendue");
        match self.src {
            SourceKind::Dual { lens } => self.render_dual(frame, lens as usize, m, fov_deg, dist, out),
            SourceKind::Equirect { w, h } => self.render_equirect(frame, w as usize, h as usize, m, fov_deg, dist, out),
        }
    }

    // -- objectifs fisheye ---------------------------------------------------
    /// Table grossière : pour chaque cellule, coordonnées (luma) dans A et B et poids de A.
    fn coarse_table(&self, lens: usize, m: &Mat3, fov_deg: f64, dist: f64) -> Vec<Cell> {
        let (ra, rb) = self.calib.lens_matrices();
        let (rot_a, rot_b) = (ra * m, rb * m);
        let fov_out = fov_deg.to_radians();
        let aspect = self.out_h as f64 / self.out_w as f64;
        let half_lens = self.calib.fov.to_radians() / 2.0;
        let cov_limit = half_lens - 0.3f64.to_radians();
        let blend = self.calib.effective_blend();
        let size = lens as f64;
        let (gw, gh) = (self.gw, self.gh);
        let mut table = vec![[0f32; 5]; gw * gh];
        table.par_chunks_mut(gw).enumerate().for_each(|(gy, row)| {
            let v = (gy as f64 + 0.5) / gh as f64 * 2.0 - 1.0;
            for (gx, cell) in row.iter_mut().enumerate() {
                let u = (gx as f64 + 0.5) / gw as f64 * 2.0 - 1.0;
                let a = ray_at(self.proj, u, v, fov_out, aspect, dist);
                let (xa, ya, ta) = lens_coords(&rot_a, &a, half_lens, size);
                let (xb, yb, tb) = lens_coords(&rot_b, &a, half_lens, size);
                let (cov_a, cov_b) = ((ta <= cov_limit) as u8 as f64, (tb <= cov_limit) as u8 as f64);
                let th = ta.to_degrees();
                let wa = if blend <= 0.0 {
                    (th < 90.0) as u8 as f64
                } else {
                    let x = ((th - (90.0 - blend / 2.0)) / blend).clamp(0.0, 1.0);
                    1.0 - x * x * (3.0 - 2.0 * x)
                };
                let (wa_c, wb_c) = (wa * cov_a, (1.0 - wa) * cov_b);
                let s = wa_c + wb_c;
                let w = if s > 1e-6 { wa_c / s } else { 0.5 };
                *cell = [xa as f32, ya as f32, xb as f32, yb as f32, w as f32];
            }
        });
        table
    }

    fn render_dual(&self, frame: &[u8], lens: usize, m: &Mat3, fov_deg: f64, dist: f64, out: &mut [u8]) {
        let table = self.coarse_table(lens, m, fov_deg, dist);
        let (w, h) = (self.out_w, self.out_h);
        let ysz = 2 * lens * lens;
        let csz = (lens / 2) * lens;
        let (y_src, rest) = frame.split_at(ysz);
        let (u_src, v_src) = rest.split_at(csz);
        let swap = self.calib.swap_lenses;
        // décalage de chaque objectif dans l'image côte à côte (luma puis chroma)
        let luma = PlaneCtx::new(y_src, 2 * lens, lens, 1.0, 0.0, swap);
        let chroma_u = PlaneCtx::new(u_src, lens, lens / 2, 0.5, -0.25, swap);
        let chroma_v = PlaneCtx::new(v_src, lens, lens / 2, 0.5, -0.25, swap);

        let (y_out, uv_out) = out.split_at_mut(w * h);
        let (u_out, v_out) = uv_out.split_at_mut(w * h / 4);
        let cw = w / 2;
        y_out
            .par_chunks_mut(CHUNK_ROWS * w)
            .zip(u_out.par_chunks_mut(CHUNK_ROWS / 2 * cw))
            .zip(v_out.par_chunks_mut(CHUNK_ROWS / 2 * cw))
            .enumerate()
            .for_each(|(ci, ((yc, uc), vc))| {
                let row0 = ci * CHUNK_ROWS;
                fill_luma(&luma, &table, self.gw, &self.luma_cols, &self.luma_rows, row0, yc, w);
                fill_chroma(
                    &chroma_u,
                    &chroma_v,
                    &table,
                    self.gw,
                    &self.chroma_cols,
                    &self.chroma_rows,
                    row0 / 2,
                    uc,
                    vc,
                    cw,
                );
            });
    }

    // -- source équirectangulaire ----------------------------------------------
    #[allow(clippy::too_many_arguments)]
    fn render_equirect(&self, frame: &[u8], sw: usize, sh: usize, m: &Mat3, fov_deg: f64, dist: f64, out: &mut [u8]) {
        let (w, h) = (self.out_w, self.out_h);
        let (y_src, rest) = frame.split_at(sw * sh);
        let (u_src, v_src) = rest.split_at(sw * sh / 4);
        let fov = fov_deg.to_radians();
        let aspect = h as f64 / w as f64;
        let mm =
            [[m[(0, 0)], m[(0, 1)], m[(0, 2)]], [m[(1, 0)], m[(1, 1)], m[(1, 2)]], [m[(2, 0)], m[(2, 1)], m[(2, 2)]]];
        let proj = self.proj;
        let (y_out, uv_out) = out.split_at_mut(w * h);
        let (u_out, v_out) = uv_out.split_at_mut(w * h / 4);
        let cw = w / 2;

        // un plan : `pw x ph` pixels de sortie, source `(src, sw_p x sh_p)`
        let render_rows = |dst: &mut [u8], row0: usize, pw: usize, ph: usize, src: &[u8], sw_p: usize, sh_p: usize| {
            for (r, line) in dst.chunks_mut(pw).enumerate() {
                let v = ((row0 + r) as f64 + 0.5) / ph as f64 * 2.0 - 1.0;
                for (i, px) in line.iter_mut().enumerate() {
                    let u = (i as f64 + 0.5) / pw as f64 * 2.0 - 1.0;
                    let a = ray_at(proj, u, v, fov, aspect, dist);
                    let d = [
                        mm[0][0] * a[0] + mm[0][1] * a[1] + mm[0][2] * a[2],
                        mm[1][0] * a[0] + mm[1][1] * a[1] + mm[1][2] * a[2],
                        mm[2][0] * a[0] + mm[2][1] * a[1] + mm[2][2] * a[2],
                    ];
                    let lon = d[0].atan2(d[2]);
                    let lat = d[1].clamp(-1.0, 1.0).asin();
                    let x = (lon / (2.0 * PI) + 0.5) * sw_p as f64 - 0.5;
                    let y = (0.5 - lat / PI) * sh_p as f64 - 0.5;
                    *px = bilerp_wrap_x(src, sw_p, sh_p, x as f32, y as f32);
                }
            }
        };
        y_out.par_chunks_mut(CHUNK_ROWS * w).enumerate().for_each(|(ci, yc)| {
            render_rows(yc, ci * CHUNK_ROWS, w, h, y_src, sw, sh);
        });
        u_out.par_chunks_mut(CHUNK_ROWS / 2 * cw).enumerate().for_each(|(ci, uc)| {
            render_rows(uc, ci * CHUNK_ROWS / 2, cw, h / 2, u_src, sw / 2, sh / 2);
        });
        v_out.par_chunks_mut(CHUNK_ROWS / 2 * cw).enumerate().for_each(|(ci, vc)| {
            render_rows(vc, ci * CHUNK_ROWS / 2, cw, h / 2, v_src, sw / 2, sh / 2);
        });
    }
}

/// Coordonnées pixel `(x, y)` dans un fisheye carré de côté `size` + angle à l'axe (rad) pour la direction
/// `a` : `d = rot · a`, rayon normalisé `θ / (fov/2)` (modèle `fisheye` de v360, équidistant).
fn lens_coords(rot: &Mat3, a: &[f64; 3], half_fov: f64, size: f64) -> (f64, f64, f64) {
    let dx = rot[(0, 0)] * a[0] + rot[(0, 1)] * a[1] + rot[(0, 2)] * a[2];
    let dy = rot[(1, 0)] * a[0] + rot[(1, 1)] * a[1] + rot[(1, 2)] * a[2];
    let dz = rot[(2, 0)] * a[0] + rot[(2, 1)] * a[1] + rot[(2, 2)] * a[2];
    let rho = dx.hypot(dy);
    let theta = rho.atan2(dz);
    let scale = if rho > 1e-7 {
        theta / rho / half_fov
    } else if dz < 0.0 {
        0.0
    } else {
        1.0 / half_fov
    };
    let (u, v) = (dx * scale, -dy * scale);
    ((u + 1.0) * 0.5 * size - 0.5, (v + 1.0) * 0.5 * size - 0.5, theta)
}

/// Un plan de l'image source (Y, U ou V) côte à côte et sa géométrie.
struct PlaneCtx<'a> {
    data: &'a [u8],
    stride: usize,
    /// Taille d'un objectif dans ce plan (côté, en pixels du plan).
    lens: usize,
    /// Conversion des coordonnées luma de la table vers les coordonnées de ce plan : `x * scale + shift`.
    scale: f32,
    shift: f32,
    off_a: usize,
    off_b: usize,
}

impl<'a> PlaneCtx<'a> {
    fn new(data: &'a [u8], stride: usize, lens: usize, scale: f32, shift: f32, swap: bool) -> Self {
        // sans permutation : A à gauche, B à droite ; `swap` : l'objectif « avant » est dans le flux 1 (à droite)
        let (off_a, off_b) = if swap { (stride / 2, 0) } else { (0, stride / 2) };
        PlaneCtx { data, stride, lens, scale, shift, off_a, off_b }
    }
}

/// Interpole la table grossière pour la ligne de sortie de poids vertical `fy` entre les lignes `top` et
/// `bot` : `row[gx]` = valeurs de la cellule à cette hauteur (reste l'interpolation horizontale par pixel).
#[inline]
fn blend_rows(top: &[Cell], bot: &[Cell], fy: f32, row: &mut [Cell]) {
    for ((r, t), b) in row.iter_mut().zip(top).zip(bot) {
        for k in 0..5 {
            r[k] = t[k] + (b[k] - t[k]) * fy;
        }
    }
}

/// Valeurs `[xa, ya, xb, yb, w]` du pixel : interpolation horizontale de la ligne `row`.
#[inline(always)]
fn pixel_cell(row: &[Cell], ct: &Tap) -> ([f32; 4], i32) {
    let (a, b) = (&row[ct.i0 as usize], &row[ct.i1 as usize]);
    let fx = ct.f;
    let at = |k: usize| a[k] + (b[k] - a[k]) * fx;
    ([at(0), at(1), at(2), at(3)], ((at(4) * 256.0).round() as i32).clamp(0, 256))
}

/// Échantillon bilinéaire (arrondi) de l'objectif de décalage `off` aux coordonnées de la table `(cx, cy)`.
#[inline(always)]
fn sample_lens(p: &PlaneCtx, cx: f32, cy: f32, off: usize) -> i32 {
    let lim = (p.lens - 1) as f32;
    let x = (cx * p.scale + p.shift).clamp(0.0, lim);
    let y = (cy * p.scale + p.shift).clamp(0.0, lim);
    let (x0, y0) = (x as usize, y as usize);
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let (x1, y1) = ((x0 + 1).min(p.lens - 1), (y0 + 1).min(p.lens - 1));
    let (r0, r1) = (y0 * p.stride + off, y1 * p.stride + off);
    let (p00, p01) = (p.data[r0 + x0] as f32, p.data[r0 + x1] as f32);
    let (p10, p11) = (p.data[r1 + x0] as f32, p.data[r1 + x1] as f32);
    let t = p00 + (p01 - p00) * fx;
    let b = p10 + (p11 - p10) * fx;
    (t + (b - t) * fy + 0.5) as i32
}

/// Fond A et B avec le poids `w16` (0..=256), comme `(yb*(256-w) + ya*w + 128) >> 8` de la version Python ;
/// les pixels entièrement dans un seul objectif n'échantillonnent que celui-là.
#[inline(always)]
fn blend_pixel(p: &PlaneCtx, v: &[f32; 4], w16: i32) -> u8 {
    if w16 == 0 {
        sample_lens(p, v[2], v[3], p.off_b) as u8
    } else if w16 == 256 {
        sample_lens(p, v[0], v[1], p.off_a) as u8
    } else {
        let ya = sample_lens(p, v[0], v[1], p.off_a);
        let yb = sample_lens(p, v[2], v[3], p.off_b);
        ((yb * (256 - w16) + ya * w16 + 128) >> 8) as u8
    }
}

/// Remplit les lignes de luminance `dst` (lignes de `dw` pixels, à partir de la ligne `row0` du plan de sortie).
#[allow(clippy::too_many_arguments)]
fn fill_luma(
    p: &PlaneCtx,
    table: &[Cell],
    gw: usize,
    cols: &[Tap],
    rows: &[Tap],
    row0: usize,
    dst: &mut [u8],
    dw: usize,
) {
    let mut row = vec![[0f32; 5]; gw];
    for (r, line) in dst.chunks_mut(dw).enumerate() {
        let ry = rows[row0 + r];
        blend_rows(&table[ry.i0 as usize * gw..][..gw], &table[ry.i1 as usize * gw..][..gw], ry.f, &mut row);
        for (px, ct) in line.iter_mut().zip(cols) {
            let (v, w16) = pixel_cell(&row, ct);
            *px = blend_pixel(p, &v, w16);
        }
    }
}

/// Idem pour les deux plans de chrominance (mêmes cartes : l'interpolation n'est faite qu'une fois).
#[allow(clippy::too_many_arguments)]
fn fill_chroma(
    pu: &PlaneCtx,
    pv: &PlaneCtx,
    table: &[Cell],
    gw: usize,
    cols: &[Tap],
    rows: &[Tap],
    row0: usize,
    du: &mut [u8],
    dv: &mut [u8],
    dw: usize,
) {
    let mut row = vec![[0f32; 5]; gw];
    for (r, (lu, lv)) in du.chunks_mut(dw).zip(dv.chunks_mut(dw)).enumerate() {
        let ry = rows[row0 + r];
        blend_rows(&table[ry.i0 as usize * gw..][..gw], &table[ry.i1 as usize * gw..][..gw], ry.f, &mut row);
        for ((u, v), ct) in lu.iter_mut().zip(lv.iter_mut()).zip(cols) {
            let (m, w16) = pixel_cell(&row, ct);
            *u = blend_pixel(pu, &m, w16);
            *v = blend_pixel(pv, &m, w16);
        }
    }
}

/// Bilinéaire avec enroulement en x (équirectangulaire) et bords répliqués en y.
fn bilerp_wrap_x(src: &[u8], w: usize, h: usize, x: f32, y: f32) -> u8 {
    let (xf, yf) = (x.floor(), y.floor());
    let (fx, fy) = (x - xf, y - yf);
    let x0 = (xf as i64).rem_euclid(w as i64) as usize;
    let x1 = (x0 + 1) % w;
    let y0 = (yf as i64).clamp(0, h as i64 - 1) as usize;
    let y1 = (yf as i64 + 1).clamp(0, h as i64 - 1) as usize;
    let (p00, p01) = (src[y0 * w + x0] as f32, src[y0 * w + x1] as f32);
    let (p10, p11) = (src[y1 * w + x0] as f32, src[y1 * w + x1] as f32);
    let t = p00 + (p01 - p00) * fx;
    let b = p10 + (p11 - p10) * fx;
    (t + (b - t) * fy + 0.5) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{ypr_to_matrix, Mat3};

    /// Image source lisse et déterministe (dégradés + sinus) : insensible à une erreur de 1/32 px.
    fn smooth_dual(lens: usize) -> Vec<u8> {
        let (w, h) = (2 * lens, lens);
        let mut f = vec![0u8; w * h * 3 / 2];
        for y in 0..h {
            for x in 0..w {
                let v = 128.0 + 60.0 * ((x as f32) * 0.05).sin() + 50.0 * ((y as f32) * 0.07).cos();
                f[y * w + x] = v.clamp(0.0, 255.0) as u8;
            }
        }
        let ysz = w * h;
        for i in 0..ysz / 4 {
            f[ysz + i] = (100 + (i % 50)) as u8;
            f[ysz + ysz / 4 + i] = (150 - (i % 40)) as u8;
        }
        f
    }

    #[test]
    fn rejects_odd_sizes() {
        let c = Calibration::default();
        let src = SourceKind::Dual { lens: 64 };
        assert!(LensRenderer::new(&c, src, 33, 18, Projection::Flat, 4).is_err());
        assert!(LensRenderer::new(&c, src, 32, 17, Projection::Flat, 4).is_err());
        assert!(LensRenderer::new(&c, src, 0, 18, Projection::Flat, 4).is_err());
        assert!(LensRenderer::new(&c, SourceKind::Dual { lens: 63 }, 32, 18, Projection::Flat, 4).is_err());
        let e = LensRenderer::new(&c, src, 33, 18, Projection::Flat, 4).err().unwrap();
        assert!(crate::error::is_user_error(&e));
        assert_eq!(e.to_string(), "La taille de sortie doit être paire.");
        assert!("cube".parse::<Projection>().is_err());
        assert_eq!("planet".parse::<Projection>().unwrap(), Projection::Planet);
    }

    #[test]
    fn rays_are_unit_vectors_with_the_expected_centre() {
        for proj in [Projection::Flat, Projection::Fisheye, Projection::Planet, Projection::Equirect] {
            let r = output_rays(proj, 9, 5, 120.0, 90, 50, 0, 50);
            assert_eq!(r.len(), 45);
            for v in &r {
                let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
                assert!((n - 1.0).abs() < 1e-5, "{proj:?} : norme {n}");
            }
            // la cellule centrale (4, 2) regarde droit devant (sauf équirect : lon=0, lat=0 → z = 1)
            let c = r[2 * 9 + 4];
            assert!((c[2] - 1.0).abs() < 1e-5 && c[0].abs() < 1e-5 && c[1].abs() < 1e-5, "{proj:?} : {c:?}");
        }
        // plat, fov 90°, image 2x2 : cellules aux coordonnées ±0.5 → atan(0.5) = 26,565° du centre
        let r = output_rays(Projection::Flat, 2, 2, 90.0, 2, 2, 0, 2);
        let ang = |v: [f32; 3]| (v[0] as f64).atan2(v[2] as f64).to_degrees();
        assert!((ang(r[1]) - 26.565_051).abs() < 1e-3 && (ang(r[0]) + 26.565_051).abs() < 1e-3);
        // en haut de l'image, y > 0 (vers le haut)
        assert!(r[0][1] > 0.0 && r[2][1] < 0.0);
        // bande [y0, y1) : la grille couvre exactement la bande
        let band = output_rays(Projection::Flat, 2, 1, 90.0, 2, 4, 0, 2);
        let full = output_rays(Projection::Flat, 2, 4, 90.0, 2, 4, 0, 4);
        assert!(
            band[0][1] > full[2][1] && band[0][1] < full[0][1],
            "le centre de la bande est entre les lignes 0 et 1"
        );
    }

    /// Rayon attendu pour un angle `th` à l'axe et un azimut `az` (y vers le bas), par la formule *directe*
    /// `r = (d+1) sin θ / (d + cos θ)` écrite ici indépendamment de la formule inverse du moteur.
    fn forward_uv(th: f64, az: f64, half: f64, d: f64) -> (f64, f64) {
        let g = |t: f64| (d + 1.0) * t.sin() / (d + t.cos());
        let r = g(th) / g(half);
        (r * az.cos(), r * az.sin())
    }

    #[test]
    fn general_perspective_inverts_its_forward_formula() {
        for d in [0.05, 0.3, 0.5, 0.8, 1.0, 1.4, 2.0] {
            for fov_deg in [40.0, 90.0, 140.0] {
                let half = (fov_deg / 2.0f64).to_radians().min(perspective_limit(d) * 0.99);
                for k in 1..=8 {
                    let th = half * k as f64 / 8.0;
                    for az in [0.0, 0.7, 2.0, -2.5] {
                        let (u, v) = forward_uv(th, az, half, d);
                        let ray = ray_at(Projection::Flat, u, v, 2.0 * half, 1.0, d);
                        let want = [th.sin() * az.cos(), -th.sin() * az.sin(), th.cos()];
                        for i in 0..3 {
                            assert!(
                                (ray[i] - want[i]).abs() < 1e-9,
                                "d={d} fov={fov_deg} θ={th} : {ray:?} vs {want:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn distortion_changes_the_shape_not_the_field_and_contains_both_classic_projections() {
        let fov = 120.0f64.to_radians();
        // le bord horizontal de l'image reste à fov / 2 quelle que soit la distorsion
        for d in [0.0, 0.2, 0.7, 1.0, 1.6] {
            let r = ray_at(Projection::Flat, 1.0, 0.0, fov, 0.5625, d);
            assert!((r[0].atan2(r[2]) - fov / 2.0).abs() < 1e-9, "d={d}");
            let r = ray_at(Projection::Flat, -1.0, 0.0, fov, 0.5625, d);
            assert!((r[0].atan2(r[2]) + fov / 2.0).abs() < 1e-9, "d={d}");
        }
        // d → 0 : rectiligne ; d = 1 : stéréographique (la petite planète, image carrée)
        for (u, v) in [(0.3, -0.4), (0.9, 0.2), (-0.6, 0.7)] {
            let flat = ray_at(Projection::Flat, u, v, fov, 1.0, 0.0);
            let near0 = ray_at(Projection::Flat, u, v, fov, 1.0, 1e-5);
            let planet = ray_at(Projection::Planet, u, v, fov, 1.0, 0.0);
            let d1 = ray_at(Projection::Flat, u, v, fov, 1.0, 1.0);
            for i in 0..3 {
                assert!((flat[i] - near0[i]).abs() < 1e-4, "continuité en 0 : {flat:?} {near0:?}");
                assert!((planet[i] - d1[i]).abs() < 1e-9, "d = 1 est la petite planète : {planet:?} {d1:?}");
            }
        }
        // plus la distorsion augmente, plus le centre est agrandi par rapport aux bords : l'angle à mi-chemin décroît
        let mid = |d: f64| ray_at(Projection::Flat, 0.5, 0.0, fov, 1.0, d);
        let ang = |r: [f64; 3]| r[0].atan2(r[2]);
        assert!(ang(mid(0.0)) > ang(mid(0.5)) && ang(mid(0.5)) > ang(mid(1.0)), "angle à mi-chemin décroissant");
        // la distorsion demandée au-delà de la limite est bornée (pas de NaN)
        let r = ray_at(Projection::Flat, 1.0, 0.9, 340.0f64.to_radians(), 1.0, 0.3);
        assert!(r.iter().all(|c| c.is_finite()));
        assert!((perspective_limit(0.0) - PI / 2.0).abs() < 1e-12);
        assert!((perspective_limit(1.0) - PI).abs() < 1e-12);
        assert!((perspective_limit(2.0).to_degrees() - 120.0).abs() < 1e-9);
    }

    #[test]
    fn render_with_distortion_matches_the_classic_projections() {
        let lens = 64;
        let frame = smooth_dual(lens);
        let c = Calibration::default();
        let m = ypr_to_matrix(20.0, 10.0, 5.0);
        // distorsion 0 = rendu historique ; distorsion 1 = petite planète sur une image carrée
        let flat = LensRenderer::new(&c, SourceKind::Dual { lens: lens as u32 }, 48, 28, Projection::Flat, 4).unwrap();
        let (mut a, mut b) = (vec![0u8; flat.frame_bytes_out()], vec![0u8; flat.frame_bytes_out()]);
        flat.render(&frame, &m, 90.0, &mut a);
        flat.render_with(&frame, &m, 90.0, 0.0, &mut b);
        assert_eq!(a, b);
        flat.render_with(&frame, &m, 90.0, 0.5, &mut b);
        assert_ne!(a, b, "la distorsion modifie l'image");
        let planet =
            LensRenderer::new(&c, SourceKind::Dual { lens: lens as u32 }, 40, 40, Projection::Planet, 4).unwrap();
        let square =
            LensRenderer::new(&c, SourceKind::Dual { lens: lens as u32 }, 40, 40, Projection::Flat, 4).unwrap();
        let (mut p, mut q) = (vec![0u8; planet.frame_bytes_out()], vec![0u8; planet.frame_bytes_out()]);
        planet.render(&frame, &m, 250.0, &mut p);
        square.render_with(&frame, &m, 250.0, 1.0, &mut q);
        let diff = p.iter().zip(&q).map(|(x, y)| (*x as i32 - *y as i32).abs()).max().unwrap();
        assert!(diff <= 1, "planète = perspective de distorsion 1 : écart max {diff}");
    }

    #[test]
    fn taps_follow_the_resize_convention() {
        // agrandissement ×4 d'une source de 4 : s = (i + 0.5) / 4 - 0.5, bords répliqués
        let t = taps(16, 4);
        let expect = |i: usize, i0: u32, i1: u32, f: f32| {
            assert_eq!((t[i].i0, t[i].i1), (i0, i1), "pixel {i}");
            assert!((t[i].f - f).abs() < 1e-6, "pixel {i} : {} au lieu de {f}", t[i].f);
        };
        expect(0, 0, 1, 0.0); // s = -0.375 → bord
        expect(1, 0, 1, 0.0); // s = -0.125 → bord
        expect(2, 0, 1, 0.125);
        expect(6, 1, 2, 0.125);
        expect(10, 2, 3, 0.125);
        expect(14, 3, 3, 0.0); // s = 3.375 ≥ 3 → bord
        expect(15, 3, 3, 0.0);
        // identité quand dst == src
        for (i, tap) in taps(8, 8).iter().enumerate() {
            assert_eq!(tap.i0 as usize, i);
            assert!(tap.f.abs() < 1e-6);
        }
        // réduction ×2 : chaque pixel de destination moyenne deux voisins
        let t = taps(4, 8);
        assert!((t[1].f - 0.5).abs() < 1e-6 && t[1].i0 == 2 && t[1].i1 == 3);
    }

    #[test]
    fn front_view_of_a_constant_image_is_constant() {
        let lens = 64;
        let mut frame = vec![90u8; 2 * lens * lens * 3 / 2];
        // chaque objectif d'une valeur distincte (luma) pour repérer le fondu
        for y in 0..lens {
            for x in 0..lens {
                frame[y * 2 * lens + x] = 60;
                frame[y * 2 * lens + lens + x] = 200;
            }
        }
        let c = Calibration::default();
        let r = LensRenderer::new(&c, SourceKind::Dual { lens: lens as u32 }, 32, 18, Projection::Flat, 4).unwrap();
        let mut out = vec![0u8; r.frame_bytes_out()];
        r.render(&frame, &Mat3::identity(), 90.0, &mut out);
        assert!(out[..32 * 18].iter().all(|&v| v == 60), "vue de face : uniquement l'objectif A");
        // vue vers l'arrière (yaw 180) : uniquement l'objectif B
        r.render(&frame, &ypr_to_matrix(180.0, 0.0, 0.0), 90.0, &mut out);
        assert!(out[..32 * 18].iter().all(|&v| v == 200), "vue arrière : uniquement l'objectif B");
        // swap_lenses : les rôles s'échangent
        let swapped = Calibration { swap_lenses: true, ..Calibration::default() };
        let r =
            LensRenderer::new(&swapped, SourceKind::Dual { lens: lens as u32 }, 32, 18, Projection::Flat, 4).unwrap();
        r.render(&frame, &Mat3::identity(), 90.0, &mut out);
        assert!(out[..32 * 18].iter().all(|&v| v == 200));
        // sur la couture (yaw 90), les deux objectifs se mélangent
        let r = LensRenderer::new(&c, SourceKind::Dual { lens: lens as u32 }, 32, 18, Projection::Flat, 4).unwrap();
        r.render(&frame, &ypr_to_matrix(90.0, 0.0, 0.0), 40.0, &mut out);
        let mid = out[9 * 32 + 16];
        assert!((60..=200).contains(&mid), "{mid}");
    }

    #[test]
    fn equirect_source_roundtrip() {
        // une équirect « lat/lon » : valeur = f(lon) ; une vue plate vers yaw=+90° doit voir lon ≈ 90°
        let (w, h) = (128usize, 64usize);
        let mut frame = vec![128u8; w * h * 3 / 2];
        for y in 0..h {
            for x in 0..w {
                let lon = (x as f32 + 0.5) / w as f32 * 360.0 - 180.0;
                frame[y * w + x] = ((lon + 180.0) / 360.0 * 255.0) as u8;
            }
        }
        let r = LensRenderer::new(
            &Calibration::default(),
            SourceKind::Equirect { w: w as u32, h: h as u32 },
            16,
            16,
            Projection::Flat,
            4,
        )
        .unwrap();
        let mut out = vec![0u8; r.frame_bytes_out()];
        r.render(&frame, &ypr_to_matrix(90.0, 0.0, 0.0), 20.0, &mut out);
        let centre = out[8 * 16 + 8] as f32;
        assert!((centre - (270.0 / 360.0 * 255.0)).abs() < 4.0, "valeur au centre : {centre}");
        // enroulement : yaw = 180° regarde la couture (valeurs extrêmes des deux côtés)
        r.render(&frame, &ypr_to_matrix(180.0, 0.0, 0.0), 20.0, &mut out);
        let (left, right) = (out[8 * 16 + 1], out[8 * 16 + 14]);
        assert!(left > 200 && right < 60 || left < 60 && right > 200, "{left} {right}");
    }

    #[test]
    fn smooth_source_is_sampled_consistently_across_projections() {
        let lens = 64;
        let frame = smooth_dual(lens);
        let c = Calibration::default();
        for (proj, w, h, fov) in [
            (Projection::Flat, 32u32, 18u32, 90.0),
            (Projection::Fisheye, 24, 24, 180.0),
            (Projection::Planet, 24, 24, 250.0),
            (Projection::Equirect, 48, 24, 0.0),
        ] {
            let r = LensRenderer::new(&c, SourceKind::Dual { lens: lens as u32 }, w, h, proj, 4).unwrap();
            let mut out = vec![0u8; r.frame_bytes_out()];
            r.render(&frame, &ypr_to_matrix(20.0, 10.0, 5.0), fov, &mut out);
            // les sorties ne sont ni vides ni saturées
            let y = &out[..(w * h) as usize];
            let (mn, mx) = (*y.iter().min().unwrap(), *y.iter().max().unwrap());
            assert!(mx > mn + 20, "{proj:?} : image plate ({mn}..{mx})");
        }
    }

    #[test]
    fn render_is_deterministic_and_thread_safe() {
        let lens = 64;
        let frame = smooth_dual(lens);
        let r = std::sync::Arc::new(
            LensRenderer::new(
                &Calibration::default(),
                SourceKind::Dual { lens: lens as u32 },
                64,
                36,
                Projection::Flat,
                4,
            )
            .unwrap(),
        );
        let m = ypr_to_matrix(33.0, -12.0, 4.0);
        let mut reference = vec![0u8; r.frame_bytes_out()];
        r.render(&frame, &m, 100.0, &mut reference);
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let (r, frame, reference) = (r.clone(), frame.clone(), reference.clone());
                std::thread::spawn(move || {
                    let mut out = vec![0u8; r.frame_bytes_out()];
                    for _ in 0..5 {
                        r.render(&frame, &m, 100.0, &mut out);
                        assert_eq!(out, reference);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    fn unhex(h: &str) -> Vec<u8> {
        (0..h.len() / 2).map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap()).collect()
    }

    /// Valeurs produites par la version Python (OpenCV) : `tools/golden/gen_render.py`.
    #[test]
    fn matches_the_python_reference() {
        let g: serde_json::Value = serde_json::from_str(include_str!("../tests/golden/render.json")).unwrap();
        let lens = g["lens"].as_u64().unwrap() as usize;
        let frame = unhex(g["frame"].as_str().unwrap());
        assert_eq!(frame.len(), 2 * lens * lens * 3 / 2);

        for c in g["renders"].as_array().unwrap() {
            let name = c["name"].as_str().unwrap();
            let calib: Calibration = serde_json::from_value(c["calib"].clone()).unwrap();
            let (w, h) = (c["w"].as_u64().unwrap() as u32, c["h"].as_u64().unwrap() as u32);
            let ypr: Vec<f64> = c["ypr"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
            let proj: Projection = c["proj"].as_str().unwrap().parse().unwrap();
            let r = LensRenderer::new(&calib, SourceKind::Dual { lens: lens as u32 }, w, h, proj, 4).unwrap();
            let mut out = vec![0u8; r.frame_bytes_out()];
            r.render(&frame, &ypr_to_matrix(ypr[0], ypr[1], ypr[2]), c["fov"].as_f64().unwrap(), &mut out);
            let expected = unhex(c["expected"].as_str().unwrap());
            assert_eq!(out.len(), expected.len(), "{name}");
            let ysz = (w * h) as usize;
            for (plane, range) in [("Y", 0..ysz), ("U", ysz..ysz * 5 / 4), ("V", ysz * 5 / 4..ysz * 3 / 2)] {
                let diffs: Vec<i32> = range.map(|i| (out[i] as i32 - expected[i] as i32).abs()).collect();
                let max = *diffs.iter().max().unwrap();
                let mean = diffs.iter().sum::<i32>() as f64 / diffs.len() as f64;
                eprintln!("{name:16} plan {plane} : écart max {max}, moyen {mean:.3}");
                assert!(max <= 1 && mean < 0.05, "{name} plan {plane} : écart max {max}, moyen {mean:.3}");
            }
        }

        for c in g["rays"].as_array().unwrap() {
            let name = c["proj"].as_str().unwrap();
            let u = |k: &str| c[k].as_u64().unwrap();
            let rays = output_rays(
                name.parse().unwrap(),
                u("gw") as usize,
                u("gh") as usize,
                c["fov"].as_f64().unwrap(),
                u("w") as u32,
                u("h") as u32,
                u("y0") as u32,
                u("y1") as u32,
            );
            let expected = c["expected"].as_array().unwrap();
            assert_eq!(rays.len(), expected.len(), "{name}");
            for (got, want) in rays.iter().zip(expected) {
                for k in 0..3 {
                    let w = want[k].as_f64().unwrap();
                    assert!((got[k] as f64 - w).abs() < 2e-6, "{name} : {got:?} vs {want}");
                }
            }
        }
    }

    /// Mesure de performance (à lancer à la main) :
    /// `cargo test --release --lib -- --ignored --nocapture bench_render`
    #[test]
    #[ignore]
    fn bench_render() {
        let lens = 2880usize;
        let mut frame = vec![0u8; 2 * lens * lens * 3 / 2];
        for (i, v) in frame.iter_mut().enumerate() {
            *v = ((i * 7 + i / 1000) % 251) as u8;
        }
        let c = Calibration::default();
        for (proj, w, h, div, fov) in [
            (Projection::Flat, 1920u32, 1080u32, 4u32, 90.0),
            (Projection::Flat, 3840, 2160, 8, 90.0),
            (Projection::Equirect, 5760, 2880, 8, 0.0),
        ] {
            let r = LensRenderer::new(&c, SourceKind::Dual { lens: lens as u32 }, w, h, proj, div).unwrap();
            let mut out = vec![0u8; r.frame_bytes_out()];
            r.render(&frame, &ypr_to_matrix(30.0, 5.0, 2.0), fov, &mut out);
            let n = 10;
            let t = std::time::Instant::now();
            for i in 0..n {
                r.render(&frame, &ypr_to_matrix(30.0 + i as f64, 5.0, 2.0), fov, &mut out);
            }
            eprintln!("{proj:?} {w}x{h} : {:.1} ms/image", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
        }
    }
}
