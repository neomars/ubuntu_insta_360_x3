//! Stabilisation 360° et mise à niveau de l'horizon, calculées localement (Rust pur, sans OpenCV).
//!
//! 1. *Estimation* : sur un proxy équirectangulaire basse résolution (960x480, gris), suivi de
//!    points (Shi-Tomasi + Lucas-Kanade pyramidal avec contrôle aller-retour) sur cinq vues
//!    rectilignes (avant, droite, arrière, gauche, haut), conversion en directions 3D, rotation de
//!    Kabsch avec rejet des aberrants → orientation `Q_t` de la caméra (caméra -> monde de l'image 0).
//! 2. *Cible* : `T_t = Q_t⁻¹ · Z_t` (sortie -> caméra) selon le mode (lissage, verrouillage,
//!    horizon à plat). Spec complète et valeurs de référence : `x3studio/stabilize.py`.

use crate::calibration::Calibration;
use crate::error::{export_err, Cancelled, Result};
use crate::geometry::{matrix_from_quat, matrix_to_ypr, quat_from_matrix, rot_yaw, Mat3, Quat, Vec3, Ypr};
use crate::media::Source;
use crate::pipeline;
use crate::render::{LensRenderer, Projection, SourceKind};
use nalgebra::{Matrix4, SymmetricEigen};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StabMode {
    Off,
    Smooth,
    Lock,
    Horizon,
}

impl FromStr for StabMode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "off" => Ok(StabMode::Off),
            "smooth" => Ok(StabMode::Smooth),
            "lock" => Ok(StabMode::Lock),
            "horizon" => Ok(StabMode::Horizon),
            other => Err(export_err(format!("Mode de stabilisation inconnu : {other}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// Orientation de la caméra
// ---------------------------------------------------------------------------
/// Orientation de la caméra image par image.
#[derive(Debug, Clone, PartialEq)]
pub struct Orientation {
    /// `[w, x, y, z]` : caméra(t) -> monde (image 0), signes continus.
    pub quats: Vec<Quat>,
    pub fps: f64,
    /// Nombre de points valides ayant servi à estimer chaque image (0 pour la première).
    pub inliers: Vec<u32>,
    /// Empreinte de la calibration utilisée à l'analyse (voir [`calibration_key`]).
    pub key: String,
}

/// Signature du fichier d'orientation (format binaire little-endian, version 1).
const ORIENT_MAGIC: &[u8; 8] = b"X3ORIE01";

impl Orientation {
    pub fn n(&self) -> usize {
        self.quats.len()
    }

    pub fn matrices(&self) -> Vec<Mat3> {
        self.quats.iter().map(matrix_from_quat).collect()
    }

    /// Écrit le fichier binaire (écriture atomique : fichier temporaire puis renommage).
    pub fn save(&self, path: &Path) -> Result<()> {
        let key = self.key.as_bytes();
        let mut buf = Vec::with_capacity(32 + key.len() + self.n() * (4 * 8 + 4));
        buf.extend_from_slice(ORIENT_MAGIC);
        buf.extend_from_slice(&(self.n() as u64).to_le_bytes());
        buf.extend_from_slice(&self.fps.to_le_bytes());
        buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
        buf.extend_from_slice(key);
        for q in &self.quats {
            for v in q {
                buf.extend_from_slice(&v.to_le_bytes());
            }
        }
        for i in 0..self.n() {
            buf.extend_from_slice(&self.inliers.get(i).copied().unwrap_or(0).to_le_bytes());
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &buf)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Relit un fichier écrit par [`Orientation::save`] ; erreur (jamais de panique) s'il est tronqué ou corrompu.
    pub fn load(path: &Path) -> Result<Self> {
        let data = std::fs::read(path)?;
        let bad = || anyhow::anyhow!("Fichier d'orientation illisible : {}", path.display());
        let mut r = Reader { data: &data, pos: 0 };
        if r.take(8).ok_or_else(bad)? != ORIENT_MAGIC {
            return Err(bad());
        }
        let n = u64::from_le_bytes(r.array().ok_or_else(bad)?) as usize;
        let fps = f64::from_le_bytes(r.array().ok_or_else(bad)?);
        let key_len = u32::from_le_bytes(r.array().ok_or_else(bad)?) as usize;
        let key = String::from_utf8(r.take(key_len).ok_or_else(bad)?.to_vec()).map_err(|_| bad())?;
        // taille attendue : n * (4 * 8 + 4) octets ; vérifiée avant toute allocation
        if r.remaining() / 36 != n || r.remaining() % 36 != 0 {
            return Err(bad());
        }
        let mut quats = Vec::with_capacity(n);
        for _ in 0..n {
            let mut q = [0f64; 4];
            for v in q.iter_mut() {
                *v = f64::from_le_bytes(r.array().ok_or_else(bad)?);
            }
            quats.push(q);
        }
        let mut inliers = Vec::with_capacity(n);
        for _ in 0..n {
            inliers.push(u32::from_le_bytes(r.array().ok_or_else(bad)?));
        }
        if !fps.is_finite() || quats.iter().flatten().any(|v| !v.is_finite()) {
            return Err(bad());
        }
        Ok(Orientation { quats, fps, inliers, key })
    }
}

/// Lecture séquentielle d'un tampon sans jamais dépasser sa fin.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.data.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(s)
    }
    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }
    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
}

/// Empreinte de la calibration (hors `blend`) : l'analyse devient « périmée » si elle change.
pub fn calibration_key(c: &Calibration) -> String {
    format!(
        "{:.2}|{:.2}|{:.2}|{:.2}|{:.2}|{:.2}|{:.2}|{}",
        c.a_pitch, c.a_roll, c.a_yaw, c.b_pitch, c.b_roll, c.b_yaw, c.fov, c.swap_lenses
    )
}

// ---------------------------------------------------------------------------
// Images, pyramides et flux optique (Lucas-Kanade)
// ---------------------------------------------------------------------------
/// Image grise 8 bits.
#[derive(Debug, Clone)]
pub struct Gray {
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
}

/// Demi-fenêtre du suivi (fenêtre de 21 x 21) et nombre de niveaux de la pyramide.
const LK_HALF: i32 = 10;
const LK_WIN: usize = 2 * LK_HALF as usize + 1;
const LK_LEVELS: usize = 4;
const LK_MAX_ITER: usize = 20;
/// Une itération s'arrête quand la correction passe sous 0,01 pixel.
const LK_EPS2: f32 = 1e-4;
/// Seuil de la plus petite valeur propre (par pixel) de la matrice des gradients.
const LK_MIN_EIG: f32 = 1e-4;
/// Écart maximal (px) du contrôle aller-retour.
const FB_MAX_ERR: f32 = 0.6;

/// Indice avec miroir sans répétition du bord (`BORDER_REFLECT_101`).
#[inline]
fn reflect101(i: isize, n: usize) -> usize {
    if n == 1 {
        return 0;
    }
    let n = n as isize;
    let mut i = i;
    while i < 0 || i >= n {
        i = if i < 0 { -i } else { 2 * (n - 1) - i };
    }
    i as usize
}

struct Level {
    w: usize,
    h: usize,
    img: Vec<f32>,
    /// Gradient (Scharr normalisé : niveaux de gris par pixel).
    ix: Vec<f32>,
    iy: Vec<f32>,
}

/// Pyramide gaussienne d'une image avec ses gradients.
pub struct Pyramid {
    levels: Vec<Level>,
}

fn scharr(img: &[f32], w: usize, h: usize) -> (Vec<f32>, Vec<f32>) {
    let mut ix = vec![0f32; w * h];
    let mut iy = vec![0f32; w * h];
    for y in 0..h {
        let (ym, yp) = (reflect101(y as isize - 1, h) * w, reflect101(y as isize + 1, h) * w);
        let y0 = y * w;
        for x in 0..w {
            let (xm, xp) = (reflect101(x as isize - 1, w), reflect101(x as isize + 1, w));
            let dx = 3.0 * (img[ym + xp] - img[ym + xm])
                + 10.0 * (img[y0 + xp] - img[y0 + xm])
                + 3.0 * (img[yp + xp] - img[yp + xm]);
            let dy = 3.0 * (img[yp + xm] - img[ym + xm])
                + 10.0 * (img[yp + x] - img[ym + x])
                + 3.0 * (img[yp + xp] - img[ym + xp]);
            ix[y0 + x] = dx / 32.0;
            iy[y0 + x] = dy / 32.0;
        }
    }
    (ix, iy)
}

/// Réduction par deux (noyau gaussien 1-4-6-4-1, comme `pyrDown`).
fn pyr_down(img: &[f32], w: usize, h: usize) -> (Vec<f32>, usize, usize) {
    const K: [f32; 5] = [1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0];
    let (nw, nh) = (w.div_ceil(2), h.div_ceil(2));
    // passe horizontale (toutes les lignes, colonnes paires seulement)
    let mut tmp = vec![0f32; nw * h];
    for y in 0..h {
        let row = &img[y * w..(y + 1) * w];
        for nx in 0..nw {
            let c = (2 * nx) as isize;
            let mut s = 0.0;
            for (k, wt) in K.iter().enumerate() {
                s += wt * row[reflect101(c + k as isize - 2, w)];
            }
            tmp[y * nw + nx] = s;
        }
    }
    let mut out = vec![0f32; nw * nh];
    for ny in 0..nh {
        let c = (2 * ny) as isize;
        for x in 0..nw {
            let mut s = 0.0;
            for (k, wt) in K.iter().enumerate() {
                s += wt * tmp[reflect101(c + k as isize - 2, h) * nw + x];
            }
            out[ny * nw + x] = s;
        }
    }
    (out, nw, nh)
}

impl Pyramid {
    pub fn new(g: &Gray) -> Self {
        let mut levels: Vec<Level> = Vec::with_capacity(LK_LEVELS);
        let mut img: Vec<f32> = g.data.iter().map(|&v| f32::from(v)).collect();
        let (mut w, mut h) = (g.w, g.h);
        loop {
            let (ix, iy) = scharr(&img, w, h);
            let next =
                if levels.len() + 1 < LK_LEVELS && w >= 16 && h >= 16 { Some(pyr_down(&img, w, h)) } else { None };
            levels.push(Level { w, h, img, ix, iy });
            match next {
                Some((i, nw, nh)) => {
                    img = i;
                    w = nw;
                    h = nh;
                }
                None => break,
            }
        }
        Pyramid { levels }
    }
}

/// Échantillons bilinéaires d'une fenêtre `LK_WIN x LK_WIN` centrée en `(cx, cy)` (sous-pixel, bords répliqués).
fn gather(img: &[f32], w: usize, h: usize, cx: f32, cy: f32, out: &mut [f32; LK_WIN * LK_WIN]) {
    let (fx0, fy0) = (cx.floor(), cy.floor());
    let (fx, fy) = (cx - fx0, cy - fy0);
    let (x0, y0) = (fx0 as i32 - LK_HALF, fy0 as i32 - LK_HALF);
    let mut cols = [0usize; LK_WIN + 1];
    let mut rows = [0usize; LK_WIN + 1];
    for i in 0..=LK_WIN {
        cols[i] = (x0 + i as i32).clamp(0, w as i32 - 1) as usize;
        rows[i] = ((y0 + i as i32).clamp(0, h as i32 - 1) as usize) * w;
    }
    let (gx, gy) = (1.0 - fx, 1.0 - fy);
    for j in 0..LK_WIN {
        let (r0, r1) = (rows[j], rows[j + 1]);
        for i in 0..LK_WIN {
            let (c0, c1) = (cols[i], cols[i + 1]);
            out[j * LK_WIN + i] =
                (img[r0 + c0] * gx + img[r0 + c1] * fx) * gy + (img[r1 + c0] * gx + img[r1 + c1] * fx) * fy;
        }
    }
}

/// Suit un point de `prev` vers `next` (pyramidal, additif inverse). `None` si le suivi échoue.
fn track_point(prev: &Pyramid, next: &Pyramid, pt: (f32, f32)) -> Option<(f32, f32)> {
    let n = prev.levels.len().min(next.levels.len());
    let mut flow = (0f32, 0f32);
    let mut tpl = [0f32; LK_WIN * LK_WIN];
    let mut tix = [0f32; LK_WIN * LK_WIN];
    let mut tiy = [0f32; LK_WIN * LK_WIN];
    let mut cur = [0f32; LK_WIN * LK_WIN];
    for l in (0..n).rev() {
        let (lp, ln) = (&prev.levels[l], &next.levels[l]);
        let s = 1.0 / (1u32 << l) as f32;
        let p = (pt.0 * s, pt.1 * s);
        if l + 1 < n {
            flow = (flow.0 * 2.0, flow.1 * 2.0);
        }
        gather(&lp.img, lp.w, lp.h, p.0, p.1, &mut tpl);
        gather(&lp.ix, lp.w, lp.h, p.0, p.1, &mut tix);
        gather(&lp.iy, lp.w, lp.h, p.0, p.1, &mut tiy);
        let (mut a11, mut a12, mut a22) = (0f32, 0f32, 0f32);
        for k in 0..LK_WIN * LK_WIN {
            a11 += tix[k] * tix[k];
            a12 += tix[k] * tiy[k];
            a22 += tiy[k] * tiy[k];
        }
        let area = (LK_WIN * LK_WIN) as f32;
        let min_eig = 0.5 * (a11 + a22 - ((a11 - a22) * (a11 - a22) + 4.0 * a12 * a12).sqrt()) / area;
        let det = a11 * a22 - a12 * a12;
        if min_eig < LK_MIN_EIG || det < f32::EPSILON {
            if l == 0 {
                return None;
            }
            continue;
        }
        let inv_det = 1.0 / det;
        let (lim_x, lim_y) = ((ln.w as f32) + LK_HALF as f32, (ln.h as f32) + LK_HALF as f32);
        let mut last = (0f32, 0f32);
        for it in 0..LK_MAX_ITER {
            let q = (p.0 + flow.0, p.1 + flow.1);
            if q.0 < -(LK_HALF as f32) || q.1 < -(LK_HALF as f32) || q.0 >= lim_x || q.1 >= lim_y {
                if l == 0 {
                    return None;
                }
                break;
            }
            gather(&ln.img, ln.w, ln.h, q.0, q.1, &mut cur);
            let (mut b1, mut b2) = (0f32, 0f32);
            for k in 0..LK_WIN * LK_WIN {
                let d = tpl[k] - cur[k];
                b1 += d * tix[k];
                b2 += d * tiy[k];
            }
            let dx = (a22 * b1 - a12 * b2) * inv_det;
            let dy = (a11 * b2 - a12 * b1) * inv_det;
            flow.0 += dx;
            flow.1 += dy;
            if dx * dx + dy * dy <= LK_EPS2 {
                break;
            }
            // oscillation entre deux positions : on s'arrête au milieu
            if it > 0 && (dx + last.0).abs() < 0.01 && (dy + last.1).abs() < 0.01 {
                flow.0 -= dx * 0.5;
                flow.1 -= dy * 0.5;
                break;
            }
            last = (dx, dy);
        }
    }
    let r = (pt.0 + flow.0, pt.1 + flow.1);
    let top = &next.levels[0];
    if r.0 < 0.0
        || r.1 < 0.0
        || r.0 > (top.w - 1) as f32
        || r.1 > (top.h - 1) as f32
        || !r.0.is_finite()
        || !r.1.is_finite()
    {
        return None;
    }
    Some(r)
}

// ---------------------------------------------------------------------------
// Points d'intérêt (Shi-Tomasi, comme `goodFeaturesToTrack`)
// ---------------------------------------------------------------------------
/// Somme sur une fenêtre `k x k` (bords en miroir).
fn box_sum(src: &[f32], w: usize, h: usize, k: usize) -> Vec<f32> {
    let r = (k / 2) as isize;
    let mut tmp = vec![0f32; w * h];
    for y in 0..h {
        let row = &src[y * w..(y + 1) * w];
        for x in 0..w {
            let mut s = 0.0;
            for d in -r..=r {
                s += row[reflect101(x as isize + d, w)];
            }
            tmp[y * w + x] = s;
        }
    }
    let mut out = vec![0f32; w * h];
    for y in 0..h {
        for d in -r..=r {
            let sy = reflect101(y as isize + d, h) * w;
            for x in 0..w {
                out[y * w + x] += tmp[sy + x];
            }
        }
    }
    out
}

/// Les `max_corners` meilleurs coins (valeur propre minimale du tenseur de structure, maxima locaux
/// au-dessus de `quality x maximum`, distance mutuelle d'au moins `min_dist`), du plus fort au plus faible.
fn good_features(g: &Gray, max_corners: usize, quality: f32, min_dist: f32, block: usize) -> Vec<(f32, f32)> {
    let (w, h) = (g.w, g.h);
    if w < 3 || h < 3 {
        return vec![];
    }
    let px = |x: isize, y: isize| f32::from(g.data[reflect101(y, h) * w + reflect101(x, w)]);
    let mut ixx = vec![0f32; w * h];
    let mut ixy = vec![0f32; w * h];
    let mut iyy = vec![0f32; w * h];
    for y in 0..h as isize {
        for x in 0..w as isize {
            let dx = (px(x + 1, y - 1) + 2.0 * px(x + 1, y) + px(x + 1, y + 1))
                - (px(x - 1, y - 1) + 2.0 * px(x - 1, y) + px(x - 1, y + 1));
            let dy = (px(x - 1, y + 1) + 2.0 * px(x, y + 1) + px(x + 1, y + 1))
                - (px(x - 1, y - 1) + 2.0 * px(x, y - 1) + px(x + 1, y - 1));
            let i = y as usize * w + x as usize;
            ixx[i] = dx * dx;
            ixy[i] = dx * dy;
            iyy[i] = dy * dy;
        }
    }
    let (a, b, c) = (box_sum(&ixx, w, h, block), box_sum(&ixy, w, h, block), box_sum(&iyy, w, h, block));
    let eig: Vec<f32> =
        (0..w * h).map(|i| 0.5 * (a[i] + c[i] - ((a[i] - c[i]) * (a[i] - c[i]) + 4.0 * b[i] * b[i]).sqrt())).collect();
    let max = eig.iter().copied().fold(0f32, f32::max);
    if max <= 0.0 {
        return vec![];
    }
    let thresh = quality * max;
    let mut cand: Vec<(f32, usize)> = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let v = eig[y * w + x];
            if v <= thresh {
                continue;
            }
            let mut is_max = true;
            'nb: for dy in -1isize..=1 {
                for dx in -1isize..=1 {
                    let (nx, ny) = (x as isize + dx, y as isize + dy);
                    if (dx == 0 && dy == 0) || nx < 0 || ny < 0 || nx >= w as isize || ny >= h as isize {
                        continue;
                    }
                    if eig[ny as usize * w + nx as usize] > v {
                        is_max = false;
                        break 'nb;
                    }
                }
            }
            if is_max {
                cand.push((v, y * w + x));
            }
        }
    }
    cand.sort_by(|p, q| q.0.partial_cmp(&p.0).unwrap_or(std::cmp::Ordering::Equal).then(p.1.cmp(&q.1)));
    let md2 = min_dist * min_dist;
    let mut out: Vec<(f32, f32)> = Vec::new();
    for (_, i) in cand {
        let (x, y) = ((i % w) as f32, (i / w) as f32);
        if out.iter().all(|&(ox, oy)| (ox - x) * (ox - x) + (oy - y) * (oy - y) >= md2) {
            out.push((x, y));
            if out.len() >= max_corners {
                break;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Estimation de la rotation entre deux images
// ---------------------------------------------------------------------------
/// Une vue rectiligne et sa pyramide.
pub struct Face {
    gray: Gray,
    pyr: Pyramid,
}

/// Estimateur de rotation entre deux images équirectangulaires grises.
pub struct RotationEstimator {
    width: usize,
    height: usize,
    /// Côté d'une vue (px) et `tan(champ/2)`.
    n: usize,
    t: f64,
    min_inliers: usize,
    rots: Vec<Mat3>,
    /// Pour chaque vue et chaque pixel : coordonnées `(x, y)` dans l'image équirectangulaire.
    maps: Vec<Vec<[f32; 2]>>,
}

/// Directions des vues : (cap, inclinaison) — avant, droite, arrière, gauche, haut.
const FACES: [(f64, f64); 5] = [(0.0, 0.0), (90.0, 0.0), (180.0, 0.0), (-90.0, 0.0), (0.0, 90.0)];

impl RotationEstimator {
    /// `width x height` : taille des images équirectangulaires (typiquement 960x480).
    pub fn new(width: usize, height: usize) -> Self {
        let n = 224usize;
        let t = (100f64.to_radians() / 2.0).tan();
        let rots: Vec<Mat3> = FACES.iter().map(|&(y, p)| crate::geometry::ypr_to_matrix(y, p, 0.0)).collect();
        let maps = rots
            .iter()
            .map(|rot| {
                let mut m = Vec::with_capacity(n * n);
                for j in 0..n {
                    let v = (j as f64 + 0.5) / n as f64 * 2.0 - 1.0;
                    for i in 0..n {
                        let u = (i as f64 + 0.5) / n as f64 * 2.0 - 1.0;
                        let r = Vec3::new(u * t, -v * t, 1.0).normalize();
                        let d = rot * r;
                        let lon = d.x.atan2(d.z);
                        let lat = d.y.clamp(-1.0, 1.0).asin();
                        let mx = (lon / (2.0 * std::f64::consts::PI) + 0.5) * width as f64 - 0.5;
                        let my = (0.5 - lat / std::f64::consts::PI) * height as f64 - 0.5;
                        m.push([mx as f32, my as f32]);
                    }
                }
                m
            })
            .collect();
        RotationEstimator { width, height, n, t, min_inliers: 14, rots, maps }
    }

    /// Les 5 vues rectilignes (224x224, champ 100°) d'une image équirectangulaire.
    pub fn faces(&self, gray: &[u8]) -> Vec<Gray> {
        assert_eq!(gray.len(), self.width * self.height, "taille d'image équirectangulaire inattendue");
        let (w, h) = (self.width, self.height);
        self.maps
            .iter()
            .map(|map| {
                let data = map
                    .iter()
                    .map(|&[mx, my]| {
                        let (fx, fy) = (mx.floor(), my.floor());
                        let (ax, ay) = (mx - fx, my - fy);
                        let x0 = (fx as i64).rem_euclid(w as i64) as usize;
                        let x1 = (x0 + 1) % w;
                        let y0 = (fy as i64).clamp(0, h as i64 - 1) as usize * w;
                        let y1 = (fy as i64 + 1).clamp(0, h as i64 - 1) as usize * w;
                        let p = |i: usize| f32::from(gray[i]);
                        let v = (p(y0 + x0) * (1.0 - ax) + p(y0 + x1) * ax) * (1.0 - ay)
                            + (p(y1 + x0) * (1.0 - ax) + p(y1 + x1) * ax) * ay;
                        (v + 0.5) as u8
                    })
                    .collect();
                Gray { w: self.n, h: self.n, data }
            })
            .collect()
    }

    /// Vues + pyramides, prêtes pour [`RotationEstimator::relative_rotation_faces`].
    pub fn prepare(&self, gray: &[u8]) -> Vec<Face> {
        self.faces(gray).into_iter().map(|g| Face { pyr: Pyramid::new(&g), gray: g }).collect()
    }

    fn dirs(&self, pts: &[(f32, f32)], rot: &Mat3) -> Vec<Vec3> {
        pts.iter()
            .map(|&(x, y)| {
                let u = (f64::from(x) + 0.5) / self.n as f64 * 2.0 - 1.0;
                let v = (f64::from(y) + 0.5) / self.n as f64 * 2.0 - 1.0;
                rot * Vec3::new(u * self.t, -v * self.t, 1.0).normalize()
            })
            .collect()
    }

    /// Rotation `R` telle que `b = R a` pour des points fixes du monde, et nombre de points retenus.
    pub fn relative_rotation(&self, a: &[Gray], b: &[Gray]) -> (Mat3, usize) {
        let mk =
            |v: &[Gray]| -> Vec<Face> { v.iter().map(|g| Face { pyr: Pyramid::new(g), gray: g.clone() }).collect() };
        self.relative_rotation_faces(&mk(a), &mk(b))
    }

    pub fn relative_rotation_faces(&self, fa: &[Face], fb: &[Face]) -> (Mat3, usize) {
        let mut pa: Vec<Vec3> = Vec::new();
        let mut pb: Vec<Vec3> = Vec::new();
        for (k, (a, b)) in fa.iter().zip(fb).enumerate() {
            let p0 = good_features(&a.gray, 120, 0.01, 7.0, 7);
            if p0.len() < 6 {
                continue;
            }
            let mut q0 = Vec::with_capacity(p0.len());
            let mut q1 = Vec::with_capacity(p0.len());
            for &p in &p0 {
                let Some(f) = track_point(&a.pyr, &b.pyr, p) else {
                    continue;
                };
                let Some(back) = track_point(&b.pyr, &a.pyr, f) else {
                    continue;
                };
                if ((back.0 - p.0).powi(2) + (back.1 - p.1).powi(2)).sqrt() < FB_MAX_ERR {
                    q0.push(p);
                    q1.push(f);
                }
            }
            if q0.len() < 4 {
                continue;
            }
            let (da, db) = (self.dirs(&q0, &self.rots[k]), self.dirs(&q1, &self.rots[k]));
            for (x, y) in da.into_iter().zip(db) {
                if x.y > -0.7 {
                    // ignore le nadir (souvent la personne / la perche)
                    pa.push(x);
                    pb.push(y);
                }
            }
        }
        if pa.is_empty() {
            return (Mat3::identity(), 0);
        }
        if pa.len() < self.min_inliers {
            return (Mat3::identity(), pa.len());
        }
        kabsch_robust(&pa, &pb, self.min_inliers)
    }
}

/// Rotation de Kabsch (`b ≈ R a`) avec élimination itérative des aberrants.
fn kabsch_robust(a: &[Vec3], b: &[Vec3], min_inliers: usize) -> (Mat3, usize) {
    let n = a.len();
    let mut mask = vec![true; n];
    let mut r = Mat3::identity();
    for _ in 0..4 {
        let mut h = Mat3::zeros();
        for i in (0..n).filter(|&i| mask[i]) {
            h += a[i] * b[i].transpose();
        }
        let svd = h.svd(true, true);
        let (Some(u), Some(vt)) = (svd.u, svd.v_t) else {
            return (Mat3::identity(), n);
        };
        let d = (vt.transpose() * u.transpose()).determinant();
        r = vt.transpose() * Mat3::from_diagonal(&Vec3::new(1.0, 1.0, d.signum())) * u.transpose();
        let ang: Vec<f64> = (0..n).map(|i| b[i].dot(&(r * a[i])).clamp(-1.0, 1.0).acos().to_degrees()).collect();
        let mut sel: Vec<f64> = (0..n).filter(|&i| mask[i]).map(|i| ang[i]).collect();
        sel.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
        let median =
            if sel.len() % 2 == 1 { sel[sel.len() / 2] } else { 0.5 * (sel[sel.len() / 2 - 1] + sel[sel.len() / 2]) };
        let thr = (2.5 * median).max(0.12);
        let new: Vec<bool> = ang.iter().map(|&x| x < thr).collect();
        let count = new.iter().filter(|&&v| v).count();
        if count < min_inliers || new == mask {
            if count >= min_inliers {
                mask = new;
            }
            break;
        }
        mask = new;
    }
    (r, mask.iter().filter(|&&v| v).count())
}

// ---------------------------------------------------------------------------
// Analyse d'une vidéo
// ---------------------------------------------------------------------------
/// Nombre d'images traitées ensemble (rayon) avant de revenir au flux de décodage.
const BATCH: usize = 24;

/// Accumule les rotations : `frames` = images équirectangulaires grises `width*height` octets.
pub fn analyze_frames(
    frames: &mut dyn Iterator<Item = Vec<u8>>,
    width: usize,
    height: usize,
    fps: f64,
    n_hint: usize,
    on_progress: &dyn Fn(f64),
    cancel: &AtomicBool,
) -> Result<Orientation> {
    let est = RotationEstimator::new(width, height);
    let mut quats: Vec<Quat> = vec![[1.0, 0.0, 0.0, 0.0]];
    let mut inliers: Vec<u32> = vec![0];
    let mut q = Mat3::identity();
    let mut prev: Option<Vec<Face>> = None;
    let mut seen = 0usize;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        let mut batch: Vec<Vec<u8>> = Vec::with_capacity(BATCH);
        while batch.len() < BATCH {
            match frames.next() {
                Some(f) if f.len() == width * height => batch.push(f),
                Some(_) => return Err(export_err("Analyse impossible : image décodée de taille inattendue.")),
                None => break,
            }
        }
        if batch.is_empty() {
            break;
        }
        let mut faces: Vec<Vec<Face>> = batch.par_iter().map(|g| est.prepare(g)).collect();
        // images voisines : (dernière du lot précédent, lot...) ; chaque paire est indépendante
        if let Some(p) = prev.take() {
            faces.insert(0, p);
        }
        let rots: Vec<(Mat3, usize)> = (0..faces.len() - 1)
            .into_par_iter()
            .map(|i| est.relative_rotation_faces(&faces[i], &faces[i + 1]))
            .collect();
        for (r, n) in rots {
            q *= r.transpose(); // Q_{t+1} = Q_t · R⁻¹
            let mut quat = quat_from_matrix(&q);
            if let Some(last) = quats.last() {
                if last.iter().zip(&quat).map(|(a, b)| a * b).sum::<f64>() < 0.0 {
                    quat.iter_mut().for_each(|v| *v = -*v);
                }
            }
            quats.push(quat);
            inliers.push(n as u32);
            if quats.len() % 256 == 0 {
                q = matrix_from_quat(&quat); // évite la dérive d'orthogonalité
            }
        }
        seen += batch.len();
        prev = faces.pop();
        if n_hint > 0 {
            on_progress((seen as f64 / n_hint as f64).min(1.0));
        }
    }
    on_progress(1.0);
    Ok(Orientation { quats, fps, inliers, key: String::new() })
}

/// Largeur des images d'analyse (équirectangulaire gris).
const ANALYSIS_W: usize = 960;

/// Analyse complète : décode le proxy (`pipeline::analysis_decoder`), assemble en équirectangulaire
/// gris 960x480 avec `render::LensRenderer`, puis [`analyze_frames`]. Renseigne `key`.
pub fn analyze_video(
    src: &Source,
    calib: &Calibration,
    proxy: &Path,
    on_progress: &dyn Fn(f64),
    cancel: &AtomicBool,
) -> Result<Orientation> {
    let dec = pipeline::analysis_decoder(src, proxy);
    let (width, height) = (ANALYSIS_W, ANALYSIS_W / 2);
    let renderer = if dec.dual {
        Some(LensRenderer::new(
            calib,
            SourceKind::Dual { lens: dec.frame_h },
            width as u32,
            height as u32,
            Projection::Equirect,
            4,
        )?)
    } else {
        None
    };
    let frame_bytes = match &renderer {
        Some(r) => r.frame_bytes_in(),
        None => width * height,
    };
    let (program, args) = dec.cmd.split_first().ok_or_else(|| export_err("Commande de décodage vide."))?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| export_err(format!("Impossible de lancer ffmpeg : {e}")))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let errs: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    let drain = stderr.map(|mut s| {
        let errs = errs.clone();
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = s.read_to_string(&mut text);
            let mut q = errs.lock().unwrap_or_else(|e| e.into_inner());
            for l in text.lines().rev().take(10).collect::<Vec<_>>().into_iter().rev() {
                q.push_back(l.to_string());
            }
        })
    });
    let result = std::thread::scope(|scope| -> Result<Orientation> {
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(6);
        if let Some(mut out) = stdout {
            let renderer = renderer.as_ref();
            scope.spawn(move || {
                let identity = Mat3::identity();
                let mut buf = vec![0u8; frame_bytes];
                let mut equi = renderer.map(|r| vec![0u8; r.frame_bytes_out()]);
                while out.read_exact(&mut buf).is_ok() {
                    let gray = match (renderer, equi.as_mut()) {
                        (Some(r), Some(o)) => {
                            r.render(&buf, &identity, 0.0, o);
                            o[..width * height].to_vec()
                        }
                        _ => buf.clone(),
                    };
                    if tx.send(gray).is_err() {
                        return;
                    }
                }
            });
        }
        let mut it = rx.into_iter();
        let res = analyze_frames(&mut it, width, height, src.fps, src.frames(), on_progress, cancel);
        drop(it); // débloque le lecteur si l'analyse s'est arrêtée avant la fin
        let _ = child.kill();
        let _ = child.wait();
        res
    });
    if let Some(t) = drain {
        let _ = t.join();
    }
    let mut o = result?;
    if o.n() < 2 {
        let tail: Vec<String> = errs.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect();
        return Err(export_err(format!("Analyse impossible : aucune image décodée.\n{}", tail.join("\n"))));
    }
    o.key = calibration_key(calib);
    Ok(o)
}

// ---------------------------------------------------------------------------
// Lissage et cibles
// ---------------------------------------------------------------------------
/// Noyau gaussien normalisé de rayon `ceil(3σ)` (au moins 1).
fn gauss_kernel(sigma_frames: f64) -> Vec<f64> {
    let r = (3.0 * sigma_frames).ceil().max(1.0) as isize;
    let k: Vec<f64> = (-r..=r).map(|x| (-0.5 * (x as f64 / sigma_frames).powi(2)).exp()).collect();
    let s: f64 = k.iter().sum();
    k.into_iter().map(|v| v / s).collect()
}

/// Lissage gaussien le long de l'axe des images d'un signal à `ch` canaux (disposition `n x ch`),
/// avec normalisation aux bords.
fn smooth_signal(x: &[f64], ch: usize, sigma_frames: f64) -> Vec<f64> {
    if sigma_frames < 1e-3 || x.is_empty() {
        return x.to_vec();
    }
    let n = x.len() / ch;
    let k = gauss_kernel(sigma_frames);
    let r = (k.len() / 2) as isize;
    let mut out = vec![0f64; x.len()];
    out.par_chunks_mut(ch).enumerate().for_each(|(i, row)| {
        let lo = (i as isize - r).max(0) as usize;
        let hi = ((i as isize + r) as usize).min(n - 1);
        let mut wsum = 0.0;
        for j in lo..=hi {
            let w = k[(j as isize - i as isize + r) as usize];
            wsum += w;
            for (c, v) in row.iter_mut().enumerate() {
                *v += w * x[j * ch + c];
            }
        }
        row.iter_mut().for_each(|v| *v /= wsum);
    });
    out
}

/// Moyenne gaussienne de quaternions (méthode des valeurs propres de Markley), écart-type en images.
pub fn smooth_quats(qs: &[Quat], sigma_frames: f64) -> Vec<Quat> {
    if sigma_frames < 1e-3 || qs.is_empty() {
        return qs.to_vec();
    }
    let mut outer = Vec::with_capacity(qs.len() * 16);
    for q in qs {
        for i in 0..4 {
            for j in 0..4 {
                outer.push(q[i] * q[j]);
            }
        }
    }
    let avg = smooth_signal(&outer, 16, sigma_frames);
    avg.par_chunks(16)
        .zip(qs.par_iter())
        .map(|(m, q0)| {
            let eig = SymmetricEigen::new(Matrix4::from_row_slice(m));
            let best = (0..4)
                .max_by(|&a, &b| {
                    eig.eigenvalues[a].partial_cmp(&eig.eigenvalues[b]).unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or(0);
            let v = eig.eigenvectors.column(best);
            let mut q = [v[0], v[1], v[2], v[3]];
            if q.iter().zip(q0).map(|(a, b)| a * b).sum::<f64>() < 0.0 {
                q.iter_mut().for_each(|x| *x = -*x);
            }
            let n = q.iter().map(|x| x * x).sum::<f64>().sqrt();
            q.map(|x| x / n)
        })
        .collect()
}

/// Équivalent de `numpy.unwrap` (discontinuité π).
fn unwrap_phase(p: &[f64]) -> Vec<f64> {
    use std::f64::consts::PI;
    let mut out = Vec::with_capacity(p.len());
    let mut offset = 0.0;
    for (i, &v) in p.iter().enumerate() {
        if i > 0 {
            let dd = v - p[i - 1];
            let mut ddmod = (dd + PI).rem_euclid(2.0 * PI) - PI;
            if ddmod == -PI && dd > 0.0 {
                ddmod = PI;
            }
            if dd.abs() >= PI {
                offset += ddmod - dd;
            }
        }
        out.push(v + offset);
    }
    out
}

/// Matrices `T_t` (sortie -> caméra), une par image d'analyse (`n_frames` copies de `O⁻¹` si pas
/// d'orientation ou mode `Off`). `strength` : écart-type du lissage (s) ; `heading_strength` : idem
/// pour le cap en mode horizon (`None` = `strength`). `user_orientation` : recentrage / horizon.
pub fn stabilization_matrices(
    orient: Option<&Orientation>,
    mode: StabMode,
    strength: f64,
    user_orientation: &Ypr,
    ref_frame: usize,
    heading_strength: Option<f64>,
    n_frames: usize,
) -> Vec<Mat3> {
    let o = user_orientation.matrix();
    let o_inv = o.transpose();
    let orient = match orient {
        Some(x) if x.n() > 0 && mode != StabMode::Off => x,
        _ => return vec![o_inv; n_frames.max(1)],
    };
    let q: Vec<Mat3> = orient.matrices(); // caméra -> monde
    let qi: Vec<Mat3> = q.iter().map(|m| m.transpose()).collect();
    let fps = orient.fps;
    match mode {
        StabMode::Off => vec![o_inv; n_frames.max(1)],
        StabMode::Smooth => {
            let qs = smooth_quats(&orient.quats, strength * fps);
            qi.iter().zip(&qs).map(|(qi, s)| qi * matrix_from_quat(s) * o_inv).collect()
        }
        StabMode::Lock => {
            let r = q[ref_frame.min(q.len() - 1)];
            let target = r * o_inv;
            qi.iter().map(|qi| qi * target).collect()
        }
        StabMode::Horizon => {
            // orientation de la caméra dans le monde à plat
            let yaws: Vec<f64> = q.iter().map(|m| matrix_to_ypr(&(o * m)).0.to_radians()).collect();
            let yaws = unwrap_phase(&yaws);
            let hs = heading_strength.unwrap_or(strength);
            let ys = smooth_signal(&yaws, 1, hs * fps);
            qi.iter().zip(&ys).map(|(qi, &a)| qi * (o_inv * rot_yaw(a))).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::ypr_to_matrix;
    use serde_json::Value;

    fn angle_deg(a: &Mat3, b: &Mat3) -> f64 {
        let m = a.transpose() * b;
        ((m.trace() - 1.0) / 2.0).clamp(-1.0, 1.0).acos().to_degrees()
    }

    fn mat_from_json(v: &Value) -> Mat3 {
        let f: Vec<f64> = v.as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
        Mat3::from_row_slice(&f)
    }

    fn quats_from_json(v: &Value) -> Vec<Quat> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|q| {
                let f: Vec<f64> = q.as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
                [f[0], f[1], f[2], f[3]]
            })
            .collect()
    }

    // -- textures et images synthétiques ----------------------------------------
    /// Générateur pseudo-aléatoire (xorshift) reproductible.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// Texture équirectangulaire grise `w x h`, riche en coins : bruit multi-échelle + formes.
    fn texture(w: usize, h: usize, seed: u64) -> Vec<f32> {
        let mut rng = Rng(seed | 1);
        let mut img = vec![110f32; w * h];
        for (scale, amp) in [(64usize, 50.0f32), (24, 40.0), (8, 30.0)] {
            let (gw, gh) = (w / scale + 2, h / scale + 2);
            let grid: Vec<f32> = (0..gw * gh).map(|_| rng.next() as f32 - 0.5).collect();
            for y in 0..h {
                for x in 0..w {
                    let (fx, fy) = (x as f32 / scale as f32, y as f32 / scale as f32);
                    let (x0, y0) = (fx as usize, fy as usize);
                    let (ax, ay) = (fx - x0 as f32, fy - y0 as f32);
                    let g = |i: usize, j: usize| grid[j * gw + i];
                    let v = (g(x0, y0) * (1.0 - ax) + g(x0 + 1, y0) * ax) * (1.0 - ay)
                        + (g(x0, y0 + 1) * (1.0 - ax) + g(x0 + 1, y0 + 1) * ax) * ay;
                    img[y * w + x] += v * amp;
                }
            }
        }
        for _ in 0..(w * h / 900) {
            let (cx, cy) =
                ((rng.next() * w as f64) as isize, (rng.next() * (h as f64 * 0.76) + h as f64 * 0.12) as isize);
            let (sw, sh) = ((rng.next() * 30.0) as isize + 4, (rng.next() * 30.0) as isize + 4);
            let col = (rng.next() * 255.0) as f32;
            for y in (cy - sh).max(0)..(cy + sh).min(h as isize) {
                for x in (cx - sw).max(0)..(cx + sw).min(w as isize) {
                    img[y as usize * w + x as usize] = col;
                }
            }
        }
        img.iter_mut().for_each(|v| *v = v.clamp(0.0, 255.0));
        img
    }

    /// Image équirectangulaire vue par une caméra d'orientation `cam` (caméra -> monde) : pour chaque
    /// pixel, la direction `d` dans la caméra devient `cam · d` dans le monde, lue dans la texture.
    fn view_of(tex: &[f32], tw: usize, th: usize, cam: &Mat3, w: usize, h: usize) -> Vec<u8> {
        let mut out = vec![0u8; w * h];
        out.par_chunks_mut(w).enumerate().for_each(|(j, row)| {
            let lat = std::f64::consts::FRAC_PI_2 - (j as f64 + 0.5) / h as f64 * std::f64::consts::PI;
            for (i, px) in row.iter_mut().enumerate() {
                let lon = (i as f64 + 0.5) / w as f64 * 2.0 * std::f64::consts::PI - std::f64::consts::PI;
                let d = cam * Vec3::new(lat.cos() * lon.sin(), lat.sin(), lat.cos() * lon.cos());
                let lon2 = d.x.atan2(d.z);
                let lat2 = d.y.clamp(-1.0, 1.0).asin();
                let fx = (lon2 / (2.0 * std::f64::consts::PI) + 0.5) * tw as f64 - 0.5;
                let fy = ((0.5 - lat2 / std::f64::consts::PI) * th as f64 - 0.5).clamp(0.0, th as f64 - 1.0);
                let (x0f, y0f) = (fx.floor(), fy.floor());
                let (ax, ay) = ((fx - x0f) as f32, (fy - y0f) as f32);
                let x0 = (x0f as i64).rem_euclid(tw as i64) as usize;
                let x1 = (x0 + 1) % tw;
                let y0 = y0f as usize;
                let y1 = (y0 + 1).min(th - 1);
                let t = |x: usize, y: usize| tex[y * tw + x];
                let v = (t(x0, y0) * (1.0 - ax) + t(x1, y0) * ax) * (1.0 - ay)
                    + (t(x0, y1) * (1.0 - ax) + t(x1, y1) * ax) * ay;
                *px = (v + 0.5) as u8;
            }
        });
        out
    }

    fn shaking_cameras(n: usize, fps: f64) -> Vec<Mat3> {
        (0..n)
            .map(|i| {
                let t = i as f64 / fps;
                let s = |f: f64, p: f64| (2.0 * std::f64::consts::PI * f * t + p).sin();
                ypr_to_matrix(
                    3.0 * (s(0.9, 0.3) + 0.5 * s(2.3, 1.1)),
                    2.5 * (s(1.3, 2.0) + 0.4 * s(3.1, 0.2)),
                    3.5 * (s(0.7, 4.0) + 0.5 * s(2.9, 5.0)),
                )
            })
            .collect()
    }

    // -- orientation, empreinte -----------------------------------------------
    #[test]
    fn calibration_key_ignores_blend_only() {
        let a = Calibration::default();
        let b = Calibration { blend: 3.0, ..Calibration::default() };
        assert_eq!(calibration_key(&a), calibration_key(&b));
        assert_ne!(calibration_key(&a), calibration_key(&Calibration { fov: 195.0, ..Calibration::default() }));
        assert_ne!(calibration_key(&a), calibration_key(&Calibration { swap_lenses: true, ..Calibration::default() }));
        assert_ne!(calibration_key(&a), calibration_key(&Calibration { a_roll: 0.5, ..Calibration::default() }));
    }

    #[test]
    fn orientation_save_load_roundtrip_and_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let o = Orientation {
            quats: vec![[1.0, 0.0, 0.0, 0.0], [0.99, 0.01, -0.02, 0.1], [0.9, 0.1, 0.2, 0.3]],
            fps: 29.97,
            inliers: vec![0, 120, 118],
            key: "abc|é".into(),
        };
        let p = dir.path().join("o.bin");
        o.save(&p).unwrap();
        assert_eq!(Orientation::load(&p).unwrap(), o);
        // fichier tronqué, mauvais en-tête, vide, taille annoncée démesurée : erreurs, pas de panique
        let bytes = std::fs::read(&p).unwrap();
        for cut in [0, 5, 8, 20, bytes.len() - 1] {
            std::fs::write(&p, &bytes[..cut]).unwrap();
            assert!(Orientation::load(&p).is_err(), "coupé à {cut}");
        }
        let mut bad = bytes.clone();
        bad[0] = b'Z';
        std::fs::write(&p, &bad).unwrap();
        assert!(Orientation::load(&p).is_err());
        let mut huge = bytes.clone();
        huge[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        std::fs::write(&p, &huge).unwrap();
        assert!(Orientation::load(&p).is_err());
        assert!(Orientation::load(&dir.path().join("absent.bin")).is_err());
    }

    #[test]
    fn stab_mode_parsing() {
        assert_eq!("horizon".parse::<StabMode>().unwrap(), StabMode::Horizon);
        let e = "zoom".parse::<StabMode>().unwrap_err();
        assert!(crate::error::is_user_error(&e));
        assert_eq!(serde_json::to_string(&StabMode::Smooth).unwrap(), "\"smooth\"");
    }

    // -- vecteurs de référence de la version Python ---------------------------------
    #[test]
    fn matches_the_python_reference() {
        let g: Value = serde_json::from_str(include_str!("../tests/golden/stabilize.json")).unwrap();
        let fps = g["fps"].as_f64().unwrap();
        let quats = quats_from_json(&g["quats"]);
        let orient = Orientation { inliers: vec![100; quats.len()], quats: quats.clone(), fps, key: String::new() };

        let sm = quats_from_json(&g["smooth"]["quats"]);
        let mine = smooth_quats(&quats, g["smooth"]["sigma_frames"].as_f64().unwrap());
        for (a, b) in mine.iter().zip(&sm) {
            assert!(a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9), "{a:?} != {b:?}");
        }

        for c in g["cases"].as_array().unwrap() {
            let name = c["name"].as_str().unwrap();
            let mode: StabMode = c["mode"].as_str().unwrap().parse().unwrap();
            let u = &c["user"];
            let user = Ypr {
                yaw: u["yaw"].as_f64().unwrap(),
                pitch: u["pitch"].as_f64().unwrap(),
                roll: u["roll"].as_f64().unwrap(),
            };
            let t = stabilization_matrices(
                Some(&orient),
                mode,
                c["strength"].as_f64().unwrap(),
                &user,
                c["ref_frame"].as_u64().unwrap() as usize,
                c["heading_strength"].as_f64(),
                quats.len(),
            );
            let want: Vec<Mat3> = c["matrices"].as_array().unwrap().iter().map(mat_from_json).collect();
            assert_eq!(t.len(), want.len(), "{name}");
            let worst = t.iter().zip(&want).map(|(a, b)| (a - b).abs().max()).fold(0.0, f64::max);
            assert!(worst < 1e-9, "{name} : écart {worst}");
        }
    }

    // -- lissage et modes ---------------------------------------------------------
    #[test]
    fn smoothing_keeps_constants_and_reduces_noise() {
        let q0 = quat_from_matrix(&ypr_to_matrix(20.0, 5.0, -3.0));
        let constant = vec![q0; 40];
        for q in smooth_quats(&constant, 6.0) {
            assert!(q.iter().zip(&q0).all(|(a, b)| (a - b).abs() < 1e-12));
        }
        assert_eq!(smooth_quats(&constant, 0.0), constant);
        assert!(smooth_quats(&[], 5.0).is_empty());
        let cams = shaking_cameras(120, 30.0);
        let qs: Vec<Quat> = cams.iter().map(quat_from_matrix).collect();
        let sm = smooth_quats(&qs, 15.0);
        let dev = |v: &[Quat]| -> f64 {
            let ms: Vec<Mat3> = v.iter().map(matrix_from_quat).collect();
            ms.iter().map(|m| angle_deg(&Mat3::identity(), m).powi(2)).sum::<f64>().sqrt()
        };
        assert!(dev(&sm) < 0.4 * dev(&qs), "{} vs {}", dev(&sm), dev(&qs));
        assert!(sm.iter().all(|q| (q.iter().map(|x| x * x).sum::<f64>() - 1.0).abs() < 1e-9));
    }

    #[test]
    fn off_without_orientation_is_the_user_orientation_only() {
        let user = Ypr { yaw: 30.0, pitch: 0.0, roll: 5.0 };
        let want = ypr_to_matrix(30.0, 0.0, 5.0).transpose();
        for t in stabilization_matrices(None, StabMode::Lock, 1.0, &user, 0, None, 4) {
            assert!((t - want).abs().max() < 1e-12);
        }
        assert_eq!(stabilization_matrices(None, StabMode::Off, 1.0, &user, 0, None, 4).len(), 4);
        assert_eq!(stabilization_matrices(None, StabMode::Off, 1.0, &user, 0, None, 0).len(), 1);
        let o =
            Orientation { quats: vec![[1.0, 0.0, 0.0, 0.0]; 3], fps: 30.0, inliers: vec![0; 3], key: String::new() };
        let t = stabilization_matrices(Some(&o), StabMode::Off, 1.0, &user, 0, None, 7);
        assert_eq!(t.len(), 7);
        assert!((t[6] - want).abs().max() < 1e-12);
    }

    #[test]
    fn horizon_mode_levels_a_tilted_camera() {
        // caméra immobile (identité) et roulis utilisateur de 20° : l'horizon de sortie est tourné de -20°
        let o = Orientation {
            quats: vec![[1.0, 0.0, 0.0, 0.0]; 30],
            fps: 30.0,
            inliers: vec![100; 30],
            key: String::new(),
        };
        let user = Ypr { yaw: 0.0, pitch: 0.0, roll: 20.0 };
        let t = stabilization_matrices(Some(&o), StabMode::Horizon, 0.5, &user, 0, None, 30);
        assert_eq!(t.len(), 30);
        assert!((matrix_to_ypr(&t[0]).2 + 20.0).abs() < 1e-6);
    }

    #[test]
    fn lock_ref_frame_is_clamped_and_unwrap_follows_numpy() {
        let o =
            Orientation { quats: vec![[1.0, 0.0, 0.0, 0.0]; 5], fps: 30.0, inliers: vec![1; 5], key: String::new() };
        let t = stabilization_matrices(Some(&o), StabMode::Lock, 1.0, &Ypr::default(), 99, None, 5);
        assert!((t[4] - Mat3::identity()).abs().max() < 1e-12);
        let u = unwrap_phase(&[3.0, -3.0, -2.5, 3.1]);
        assert!((u[1] - (-3.0 + 2.0 * std::f64::consts::PI)).abs() < 1e-12);
        assert!(u.windows(2).all(|w| (w[1] - w[0]).abs() < std::f64::consts::PI));
    }

    // -- briques de l'estimation --------------------------------------------------------
    #[test]
    fn shi_tomasi_finds_the_corners_of_a_square() {
        let (w, h) = (64usize, 64usize);
        let mut data = vec![20u8; w * h];
        for y in 20..44 {
            for x in 16..40 {
                data[y * w + x] = 220;
            }
        }
        let pts = good_features(&Gray { w, h, data }, 50, 0.1, 7.0, 7);
        assert_eq!(pts.len(), 4, "{pts:?}");
        // la fenêtre 7x7 est entièrement dans la zone de contraste à ~3 px à l'intérieur de chaque coin
        for (cx, cy) in [(16.0, 20.0), (39.0, 20.0), (16.0, 43.0), (39.0, 43.0)] {
            assert!(
                pts.iter().any(|p| (p.0 - cx).abs() <= 3.5 && (p.1 - cy).abs() <= 3.5),
                "coin ({cx}, {cy}) absent : {pts:?}"
            );
        }
        // une image plate n'a aucun coin
        assert!(good_features(&Gray { w, h, data: vec![90; w * h] }, 50, 0.01, 7.0, 7).is_empty());
    }

    #[test]
    fn corner_limits_are_respected() {
        let tex = texture(256, 256, 5);
        let g = Gray { w: 256, h: 256, data: tex.iter().map(|&v| v as u8).collect() };
        let pts = good_features(&g, 120, 0.01, 7.0, 7);
        assert!(pts.len() > 60 && pts.len() <= 120, "{}", pts.len());
        for (i, a) in pts.iter().enumerate() {
            for b in &pts[i + 1..] {
                assert!((a.0 - b.0).hypot(a.1 - b.1) >= 7.0 - 1e-4);
            }
        }
    }

    #[test]
    fn lucas_kanade_recovers_subpixel_shifts() {
        let (w, h) = (224usize, 224usize);
        let tex = texture(w, h, 9);
        let shifted = |dx: f32, dy: f32| -> Vec<u8> {
            let mut o = vec![0u8; w * h];
            for y in 0..h {
                for x in 0..w {
                    let (sx, sy) =
                        ((x as f32 - dx).clamp(0.0, w as f32 - 1.001), (y as f32 - dy).clamp(0.0, h as f32 - 1.001));
                    let (x0, y0) = (sx as usize, sy as usize);
                    let (ax, ay) = (sx - x0 as f32, sy - y0 as f32);
                    let t = |x: usize, y: usize| tex[y * w + x];
                    let v = (t(x0, y0) * (1.0 - ax) + t(x0 + 1, y0) * ax) * (1.0 - ay)
                        + (t(x0, y0 + 1) * (1.0 - ax) + t(x0 + 1, y0 + 1) * ax) * ay;
                    o[y * w + x] = (v + 0.5) as u8;
                }
            }
            o
        };
        let a = Gray { w, h, data: shifted(0.0, 0.0) };
        let pa = Pyramid::new(&a);
        let pts: Vec<(f32, f32)> = good_features(&a, 120, 0.01, 7.0, 7)
            .into_iter()
            .filter(|p| p.0 > 30.0 && p.0 < 190.0 && p.1 > 30.0 && p.1 < 190.0)
            .collect();
        assert!(pts.len() > 20);
        for (dx, dy) in [(1.3f32, -0.7f32), (-2.4, 3.1), (7.0, 5.0)] {
            let b = Gray { w, h, data: shifted(dx, dy) };
            let pb = Pyramid::new(&b);
            let mut errs = vec![];
            for &p in &pts {
                if let Some(q) = track_point(&pa, &pb, p) {
                    errs.push(((q.0 - p.0 - dx).powi(2) + (q.1 - p.1 - dy).powi(2)).sqrt());
                }
            }
            assert!(errs.len() * 10 >= pts.len() * 8, "trop d'échecs de suivi : {} / {}", errs.len(), pts.len());
            errs.sort_by(|x, y| x.partial_cmp(y).unwrap());
            let median = errs[errs.len() / 2];
            assert!(median < 0.08, "décalage ({dx}, {dy}) : erreur médiane {median} px");
        }
    }

    #[test]
    fn kabsch_recovers_a_rotation_despite_outliers() {
        let r = ypr_to_matrix(2.0, -1.0, 1.5);
        let mut rng = Rng(77);
        let a: Vec<Vec3> =
            (0..200).map(|_| Vec3::new(rng.next() - 0.5, rng.next() - 0.5, rng.next() + 0.2).normalize()).collect();
        let mut b: Vec<Vec3> = a.iter().map(|v| r * v).collect();
        for k in (0..200).step_by(5) {
            b[k] = Vec3::new(rng.next() - 0.5, rng.next() - 0.5, 1.0).normalize();
            // 20 % d'aberrants
        }
        let (est, inl) = kabsch_robust(&a, &b, 14);
        assert!(angle_deg(&est, &r) < 1e-6, "{}", angle_deg(&est, &r));
        assert!((150..=165).contains(&inl), "{inl}");
    }

    // -- estimation sur des images synthétiques --------------------------------------------
    #[test]
    fn estimator_measures_small_rotations() {
        let (w, h) = (960usize, 480usize);
        let tex = texture(1920, 960, 3);
        let est = RotationEstimator::new(w, h);
        let base = ypr_to_matrix(15.0, 5.0, 0.0);
        let f0 = view_of(&tex, 1920, 960, &base, w, h);
        let faces0 = est.faces(&f0);
        assert_eq!(faces0.len(), 5);
        assert!(faces0.iter().all(|f| f.w == 224 && f.h == 224 && f.data.len() == 224 * 224));
        for delta in [ypr_to_matrix(0.6, 0.0, 0.0), ypr_to_matrix(0.0, -0.8, 0.5), ypr_to_matrix(-2.0, 1.0, -1.5)] {
            let cam = base * delta;
            let f1 = view_of(&tex, 1920, 960, &cam, w, h);
            let (r, n) = est.relative_rotation(&faces0, &est.faces(&f1));
            // les directions de la caméra 0 deviennent celles de la caméra 1 : b = R a avec R = (C0ᵀ C1)ᵀ
            let want = (base.transpose() * cam).transpose();
            assert!(n > 100, "points retenus : {n}");
            assert!(angle_deg(&r, &want) < 0.06, "écart {}° pour {:?}", angle_deg(&r, &want), delta);
        }
    }

    #[test]
    fn estimator_ignores_featureless_and_unrelated_frames() {
        let (w, h) = (960usize, 480usize);
        let est = RotationEstimator::new(w, h);
        let flat = est.faces(&vec![128u8; w * h]);
        let (r, n) = est.relative_rotation(&flat, &flat);
        assert_eq!(n, 0);
        assert!((r - Mat3::identity()).abs().max() < 1e-12);
        // deux scènes sans rapport : peu de points cohérents, jamais de panique
        let a = est.faces(&view_of(&texture(1920, 960, 1), 1920, 960, &Mat3::identity(), w, h));
        let b = est.faces(&view_of(&texture(1920, 960, 2), 1920, 960, &Mat3::identity(), w, h));
        let (r, n) = est.relative_rotation(&a, &b);
        assert!(n < 100, "{n}");
        assert!(r.determinant() > 0.99);
    }

    #[test]
    fn analyze_frames_follows_the_camera() {
        let (w, h, fps) = (960usize, 480usize, 30.0);
        let tex = texture(1920, 960, 3);
        let cams = shaking_cameras(30, fps);
        let mut frames = cams.iter().map(|c| view_of(&tex, 1920, 960, c, w, h));
        let seen = std::cell::Cell::new(0.0);
        let o = analyze_frames(&mut frames, w, h, fps, cams.len(), &|p| seen.set(p), &AtomicBool::new(false)).unwrap();
        assert_eq!(o.n(), cams.len());
        assert_eq!(seen.get(), 1.0);
        assert_eq!(o.fps, fps);
        let est = o.matrices();
        let worst = (0..cams.len()).map(|i| angle_deg(&(cams[0].transpose() * cams[i]), &est[i])).fold(0.0, f64::max);
        assert!(worst < 0.2, "erreur max {worst}°");
        let mut inl = o.inliers[1..].to_vec();
        inl.sort_unstable();
        assert!(inl[inl.len() / 2] > 100, "{inl:?}");
        // continuité des signes
        assert!(o.quats.windows(2).all(|p| p[0].iter().zip(&p[1]).map(|(a, b)| a * b).sum::<f64>() > 0.0));
    }

    #[test]
    fn analysis_can_be_cancelled_and_rejects_bad_frames() {
        let cancel = AtomicBool::new(true);
        let mut it = std::iter::repeat_with(|| vec![0u8; 960 * 480]);
        let e = analyze_frames(&mut it, 960, 480, 30.0, 0, &|_| {}, &cancel).unwrap_err();
        assert!(crate::error::is_cancelled(&e));
        let mut bad = std::iter::once(vec![0u8; 10]);
        let e = analyze_frames(&mut bad, 960, 480, 30.0, 0, &|_| {}, &AtomicBool::new(false)).unwrap_err();
        assert!(crate::error::is_user_error(&e));
        // flux vide : une seule orientation (identité)
        let mut none = std::iter::empty::<Vec<u8>>();
        let o = analyze_frames(&mut none, 960, 480, 30.0, 0, &|_| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(o.n(), 1);
    }

    #[test]
    fn stabilizing_with_the_estimate_removes_the_shake() {
        let (w, h, fps) = (960usize, 480usize, 30.0);
        let tex = texture(1920, 960, 3);
        let cams = shaking_cameras(36, fps);
        let mut frames = cams.iter().map(|c| view_of(&tex, 1920, 960, c, w, h));
        let o = analyze_frames(&mut frames, w, h, fps, 0, &|_| {}, &AtomicBool::new(false)).unwrap();
        let shake = cams.iter().map(|c| angle_deg(&cams[0], c).powi(2)).sum::<f64>() / cams.len() as f64;
        assert!(shake.sqrt() > 2.0, "la scène doit trembler : {}", shake.sqrt());
        // direction « monde » d'un pixel de sortie : C_t · T_t
        let resid = |t: &[Mat3]| -> f64 {
            let world: Vec<Mat3> = cams.iter().zip(t).map(|(c, t)| c * t).collect();
            (world.iter().map(|m| angle_deg(&world[0], m).powi(2)).sum::<f64>() / world.len() as f64).sqrt()
        };
        // orientation vraie : ce que donnerait une estimation parfaite (le lissage laisse un résidu propre)
        let truth = Orientation {
            quats: cams.iter().map(|c| quat_from_matrix(&(cams[0].transpose() * c))).collect(),
            fps,
            inliers: vec![0; cams.len()],
            key: String::new(),
        };
        let user = Ypr::default();
        let compare = |mode: StabMode, strength: f64| -> (f64, f64) {
            let got = resid(&stabilization_matrices(Some(&o), mode, strength, &user, 0, None, o.n()));
            let ideal = resid(&stabilization_matrices(Some(&truth), mode, strength, &user, 0, None, truth.n()));
            (got, ideal)
        };
        let (got, ideal) = compare(StabMode::Lock, 1.0);
        assert!(ideal < 1e-3 && got < 0.2, "verrouillé : {got}° (idéal {ideal}°)");
        let (got, ideal) = compare(StabMode::Horizon, 1.0);
        assert!(got < ideal + 0.15, "horizon : {got}° (idéal {ideal}°)");
        let mut res = vec![];
        for s in [0.1, 0.3, 1.0] {
            let (got, ideal) = compare(StabMode::Smooth, s);
            assert!(got < ideal + 0.15, "lissage {s} s : {got}° (idéal {ideal}°)");
            res.push(got);
        }
        assert!(res[0] > res[1] && res[1] > res[2], "lissage non monotone : {res:?}");
    }
}
