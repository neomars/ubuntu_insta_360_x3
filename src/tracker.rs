//! Suivi d'un sujet (à la « Deep Track » d'Insta360, mais sans réseau de neurones) : l'utilisateur désigne un point de
//! l'image à un instant donné ; on le suit image par image et on en tire le cap et l'inclinaison qui le gardent au
//! centre de la vue, sous forme de keyframes.
//!
//! Principe : à chaque image on rend, à partir de l'image équirectangulaire grise de l'analyse, une petite vue
//! rectiligne **centrée sur la direction prévue** du sujet, dans le repère stabilisé (la stabilisation du projet est
//! donc prise en compte) ; on y cherche le motif du sujet par corrélation croisée normalisée (NCC, zéro-moyenne),
//! affinée au sous-pixel. Le motif suit lentement les changements d'apparence. Quand la corrélation reste faible, le
//! sujet est déclaré perdu et le suivi s'arrête. Les images viennent du proxy d'aperçu, comme pour l'analyse du
//! mouvement : tout est local.
//!
//! Limites : c'est un suivi de motif, pas une reconnaissance d'objet. Il suit bien un sujet qui garde le même aspect
//! (personne, véhicule, drone), moins bien s'il tourne sur lui-même, change beaucoup de taille ou passe derrière un
//! obstacle ; il peut alors décrocher vers le décor.

use crate::calibration::Calibration;
use crate::error::{export_err, Cancelled, Result};
use crate::geometry::{ypr_to_matrix, Keyframe, Mat3, Vec3};
use crate::media::Source;
use crate::pipeline::{analysis_decoder_range, Proc};
use crate::render::{LensRenderer, Projection, SourceKind};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::f64::consts::PI;
use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};

/// Côté de la vue de recherche (px) et du motif (px).
const CROP: usize = 144;
const PATCH: usize = 40;
/// Rayon de recherche (px) autour de la position prévue.
const RADIUS: i32 = 36;
/// Le motif couvre cette part de la taille du sujet : un peu moins que lui, pour ne pas y mettre le décor qui l'entoure
/// (qui change quand le sujet se déplace). La vue de recherche en couvre `CROP / PATCH` fois plus.
const PATCH_OVER_SUBJECT: f64 = 0.8;
const CROP_OVER_SUBJECT: f64 = PATCH_OVER_SUBJECT * CROP as f64 / PATCH as f64;
/// Corrélation au-dessous de laquelle une image « compte pour perdue », et nombre d'images de suite qui arrêtent le suivi.
const LOST_SCORE: f64 = 0.35;
const LOST_FRAMES: usize = 6;
/// Le motif ne se met à jour que si la corrélation dépasse ce seuil ; part du nouveau motif.
const UPDATE_SCORE: f64 = 0.7;
const UPDATE_RATE: f32 = 0.12;
/// Taille du sujet (degrés) : bornes.
pub const SIZE_RANGE: (f64, f64) = (3.0, 40.0);
/// Image équirectangulaire grise 960 x 480 de l'analyse.
pub const FRAME_W: usize = 960;

/// Image équirectangulaire en niveaux de gris.
#[derive(Clone)]
pub struct Equirect {
    pub w: usize,
    pub h: usize,
    pub gray: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TrackSample {
    /// Instant (s).
    pub t: f64,
    /// Cap et inclinaison (°) du sujet dans le repère de la vue (après la stabilisation).
    pub yaw: f64,
    pub pitch: f64,
    /// Corrélation du motif, de 0 à 1.
    pub score: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Track {
    /// Un échantillon par image, triés par instant.
    pub samples: Vec<TrackSample>,
    /// Instant à partir duquel le sujet est perdu en avançant (`None` : suivi jusqu'à la fin de la plage).
    pub lost_after: Option<f64>,
    /// Instant jusqu'auquel le sujet est perdu en reculant.
    pub lost_before: Option<f64>,
}

fn dir_of(yaw: f64, pitch: f64) -> Vec3 {
    let (y, p) = (yaw.to_radians(), pitch.to_radians());
    Vec3::new(p.cos() * y.sin(), p.sin(), p.cos() * y.cos())
}

fn yaw_pitch(d: &Vec3) -> (f64, f64) {
    (d.x.atan2(d.z).to_degrees(), d.y.clamp(-1.0, 1.0).asin().to_degrees())
}

/// Bilinéaire avec enroulement en x et bords répliqués en y.
fn sample_equirect(f: &Equirect, x: f32, y: f32) -> f32 {
    let (xf, yf) = (x.floor(), y.floor());
    let (ax, ay) = (x - xf, y - yf);
    let x0 = (xf as i64).rem_euclid(f.w as i64) as usize;
    let x1 = (x0 + 1) % f.w;
    let y0 = (yf as i64).clamp(0, f.h as i64 - 1) as usize;
    let y1 = (yf as i64 + 1).clamp(0, f.h as i64 - 1) as usize;
    let g = |xx: usize, yy: usize| f32::from(f.gray[yy * f.w + xx]);
    let top = g(x0, y0) * (1.0 - ax) + g(x1, y0) * ax;
    let bot = g(x0, y1) * (1.0 - ax) + g(x1, y1) * ax;
    top * (1.0 - ay) + bot * ay
}

/// Vue rectiligne grise `CROP x CROP` de champ `fov_deg`, orientée par `m` (sortie -> caméra).
fn render_crop(f: &Equirect, m: &Mat3, fov_deg: f64) -> Vec<f32> {
    let t = (fov_deg.to_radians() / 2.0).tan();
    let mut out = vec![0f32; CROP * CROP];
    out.par_chunks_mut(CROP).enumerate().for_each(|(j, row)| {
        let v = (j as f64 + 0.5) / CROP as f64 * 2.0 - 1.0;
        for (i, px) in row.iter_mut().enumerate() {
            let u = (i as f64 + 0.5) / CROP as f64 * 2.0 - 1.0;
            let d = m * Vec3::new(u * t, -v * t, 1.0).normalize();
            let lon = d.x.atan2(d.z);
            let lat = d.y.clamp(-1.0, 1.0).asin();
            let x = (lon / (2.0 * PI) + 0.5) * f.w as f64 - 0.5;
            let y = (0.5 - lat / PI) * f.h as f64 - 0.5;
            *px = sample_equirect(f, x as f32, y as f32);
        }
    });
    out
}

/// Direction (repère de la vue de recherche) du point `(x, y)` (pixels, centre de pixel = +0,5) d'une vue de champ `fov_deg`.
fn crop_ray(x: f64, y: f64, fov_deg: f64) -> Vec3 {
    let t = (fov_deg.to_radians() / 2.0).tan();
    let (u, v) = (x / CROP as f64 * 2.0 - 1.0, y / CROP as f64 * 2.0 - 1.0);
    Vec3::new(u * t, -v * t, 1.0).normalize()
}

/// Motif de référence : valeurs moins leur moyenne, et norme.
#[derive(Clone)]
struct Template {
    zero_mean: Vec<f32>,
    norm: f32,
}

impl Template {
    fn from_patch(patch: &[f32]) -> Template {
        let n = patch.len() as f32;
        let mean = patch.iter().sum::<f32>() / n;
        let zero_mean: Vec<f32> = patch.iter().map(|v| v - mean).collect();
        let norm = zero_mean.iter().map(|v| v * v).sum::<f32>().sqrt();
        Template { zero_mean, norm }
    }
}

/// Le motif de `PATCH x PATCH` pixels dont le coin est en `(x0, y0)` dans la vue `crop`.
fn patch_at(crop: &[f32], x0: usize, y0: usize) -> Vec<f32> {
    let mut p = Vec::with_capacity(PATCH * PATCH);
    for j in 0..PATCH {
        p.extend_from_slice(&crop[(y0 + j) * CROP + x0..][..PATCH]);
    }
    p
}

/// Images intégrales (somme et somme des carrés) de la vue de recherche, pour la moyenne et la variance de chaque fenêtre.
struct Integral {
    s1: Vec<f64>,
    s2: Vec<f64>,
}

impl Integral {
    fn new(crop: &[f32]) -> Integral {
        let stride = CROP + 1;
        let mut s1 = vec![0f64; stride * stride];
        let mut s2 = vec![0f64; stride * stride];
        for j in 0..CROP {
            let (mut r1, mut r2) = (0.0, 0.0);
            for i in 0..CROP {
                let v = f64::from(crop[j * CROP + i]);
                r1 += v;
                r2 += v * v;
                s1[(j + 1) * stride + i + 1] = s1[j * stride + i + 1] + r1;
                s2[(j + 1) * stride + i + 1] = s2[j * stride + i + 1] + r2;
            }
        }
        Integral { s1, s2 }
    }

    /// (somme, somme des carrés) de la fenêtre `PATCH x PATCH` de coin `(x0, y0)`.
    fn window(&self, x0: usize, y0: usize) -> (f64, f64) {
        let stride = CROP + 1;
        let at = |t: &Vec<f64>, x: usize, y: usize| t[y * stride + x];
        let (x1, y1) = (x0 + PATCH, y0 + PATCH);
        let sum = |t: &Vec<f64>| at(t, x1, y1) - at(t, x0, y1) - at(t, x1, y0) + at(t, x0, y0);
        (sum(&self.s1), sum(&self.s2))
    }
}

/// Corrélation croisée normalisée du motif avec la fenêtre de coin `(x0, y0)` ; 0 pour une fenêtre uniforme.
fn ncc_at(crop: &[f32], integral: &Integral, tpl: &Template, x0: usize, y0: usize) -> f64 {
    let mut num = 0f32;
    for j in 0..PATCH {
        let row = &crop[(y0 + j) * CROP + x0..][..PATCH];
        let t = &tpl.zero_mean[j * PATCH..][..PATCH];
        num += row.iter().zip(t).map(|(a, b)| a * b).sum::<f32>();
    }
    let n = (PATCH * PATCH) as f64;
    let (s1, s2) = integral.window(x0, y0);
    let var = (s2 - s1 * s1 / n).max(0.0);
    // une fenêtre quasi uniforme (écart-type inférieur à 2 niveaux) n'a pas de motif à comparer : sa corrélation serait
    // un quotient de deux erreurs d'arrondi
    if var < n * 4.0 {
        return 0.0;
    }
    (f64::from(num) / (var.sqrt() * f64::from(tpl.norm))).clamp(-1.0, 1.0)
}

/// Meilleure position du motif dans `crop` : décalage `(dx, dy)` en pixels (sous-pixel) par rapport au centre, et
/// corrélation.
fn match_template(crop: &[f32], tpl: &Template) -> (f64, f64, f64) {
    let integral = Integral::new(crop);
    let centre = (CROP - PATCH) / 2;
    let side = (2 * RADIUS + 1) as usize;
    let scores: Vec<f64> = (0..side * side)
        .into_par_iter()
        .map(|k| {
            let (ox, oy) = ((k % side) as i32 - RADIUS, (k / side) as i32 - RADIUS);
            ncc_at(crop, &integral, tpl, (centre as i32 + ox) as usize, (centre as i32 + oy) as usize)
        })
        .collect();
    let (best, &score) = scores.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).expect("fenêtre non vide");
    let (bx, by) = ((best % side) as i32, (best / side) as i32);
    // sommet de la parabole passant par les trois valeurs voisines, par axe
    let peak = |lo: f64, mid: f64, hi: f64| {
        let d = lo - 2.0 * mid + hi;
        if d.abs() < 1e-9 {
            0.0
        } else {
            (0.5 * (lo - hi) / d).clamp(-0.5, 0.5)
        }
    };
    let at = |x: i32, y: i32| scores[(y as usize) * side + x as usize];
    let sx = if bx > 0 && bx < side as i32 - 1 { peak(at(bx - 1, by), score, at(bx + 1, by)) } else { 0.0 };
    let sy = if by > 0 && by < side as i32 - 1 { peak(at(bx, by - 1), score, at(bx, by + 1)) } else { 0.0 };
    (f64::from(bx - RADIUS) + sx, f64::from(by - RADIUS) + sy, score)
}

/// Un suivi en cours : le motif, la direction courante (repère de la vue) et sa vitesse.
pub struct Tracker {
    initial: Template,
    tpl: Template,
    crop_fov: f64,
    /// Direction courante et précédente du sujet dans le repère de la vue.
    w: Vec3,
    w_prev: Vec3,
    w0: Vec3,
    misses: usize,
}

/// Résultat de l'avance d'une image.
pub struct Step {
    pub yaw: f64,
    pub pitch: f64,
    pub score: f64,
    /// Le sujet est perdu (corrélation faible depuis [`LOST_FRAMES`] images) : le suivi doit s'arrêter.
    pub lost: bool,
}

impl Tracker {
    /// Démarre sur l'image `first` : le sujet est dans la direction `(yaw, pitch)` du repère de la vue (celle de la vue
    /// affichée), `stab` est la stabilisation de cette image et `size_deg` la taille apparente du sujet.
    pub fn new(first: &Equirect, stab: &Mat3, yaw: f64, pitch: f64, size_deg: f64) -> Result<Tracker> {
        let size = size_deg.clamp(SIZE_RANGE.0, SIZE_RANGE.1);
        let crop_fov = (size * CROP_OVER_SUBJECT).clamp(8.0, 150.0);
        let w0 = dir_of(yaw, pitch);
        let crop = render_crop(first, &(stab * ypr_to_matrix(yaw, pitch, 0.0)), crop_fov);
        let centre = (CROP - PATCH) / 2;
        let tpl = Template::from_patch(&patch_at(&crop, centre, centre));
        if tpl.norm < 1e-3 {
            return Err(export_err(
                "Le point choisi est trop uni pour être suivi (ciel, mur) : choisissez un sujet texturé.",
            ));
        }
        Ok(Tracker { initial: tpl.clone(), tpl, crop_fov, w: w0, w_prev: w0, w0, misses: 0 })
    }

    /// Repart du point de départ (pour suivre dans l'autre sens).
    pub fn rewind(&mut self) {
        self.tpl = self.initial.clone();
        self.w = self.w0;
        self.w_prev = self.w0;
        self.misses = 0;
    }

    /// Avance d'une image : `stab` est la stabilisation de cette image.
    pub fn step(&mut self, frame: &Equirect, stab: &Mat3) -> Step {
        // prévision à vitesse constante, fortement amortie (une caméra qui tremble inverse le sens du mouvement d'une image à l'autre)
        let pred = (self.w + (self.w - self.w_prev) * 0.5).normalize();
        let (pyaw, ppitch) = yaw_pitch(&pred);
        let v = ypr_to_matrix(pyaw, ppitch, 0.0);
        let crop = render_crop(frame, &(stab * v), self.crop_fov);
        let (dx, dy, score) = match_template(&crop, &self.tpl);
        // le centre du motif est au centre de la vue ; (dx, dy) le déplace
        let found = v * crop_ray(CROP as f64 / 2.0 + dx, CROP as f64 / 2.0 + dy, self.crop_fov);
        if score >= LOST_SCORE {
            self.misses = 0;
            self.w_prev = self.w;
            self.w = found.normalize();
            if score >= UPDATE_SCORE {
                let at = ((CROP as f64 / 2.0 + dx).round() as i64 - (PATCH / 2) as i64).clamp(0, (CROP - PATCH) as i64)
                    as usize;
                let at_y = ((CROP as f64 / 2.0 + dy).round() as i64 - (PATCH / 2) as i64)
                    .clamp(0, (CROP - PATCH) as i64) as usize;
                let fresh = Template::from_patch(&patch_at(&crop, at, at_y));
                let blended: Vec<f32> = self
                    .tpl
                    .zero_mean
                    .iter()
                    .zip(&fresh.zero_mean)
                    .map(|(old, new)| old * (1.0 - UPDATE_RATE) + new * UPDATE_RATE)
                    .collect();
                self.tpl = Template::from_patch(&blended);
            }
        } else {
            // sujet momentanément introuvable : on garde la prévision (le sujet continue sa route)
            self.misses += 1;
            self.w_prev = self.w;
            self.w = pred;
        }
        let (yaw, pitch) = yaw_pitch(&self.w);
        Step { yaw, pitch, score, lost: self.misses >= LOST_FRAMES }
    }
}

/// Paramètres d'un suivi.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TrackParams {
    /// Instant où le sujet est désigné (s) et sa direction dans le repère de la vue.
    pub t0: f64,
    pub yaw: f64,
    pub pitch: f64,
    /// Taille apparente du sujet (°).
    pub size: f64,
    /// Plage suivie (s) ; `backward` : suivre aussi vers le début de la plage.
    pub start: f64,
    pub end: f64,
    pub backward: bool,
}

/// Suit le sujet dans des images fournies par `frame(index)` (`None` = fin) : `fps` images par seconde, `stab` donne la
/// stabilisation de chaque image (`None` = identité). `progress` reçoit une fraction de 0 à 1 ; `cancelled` interrompt.
pub fn track_with(
    p: &TrackParams,
    fps: f64,
    n_frames: usize,
    stab: &dyn Fn(usize) -> Mat3,
    frame: &mut dyn FnMut(usize) -> Result<Option<Equirect>>,
    progress: &dyn Fn(f64),
    cancelled: &dyn Fn() -> bool,
) -> Result<Track> {
    if n_frames == 0 || fps.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        return Err(export_err("Suivi impossible : la vidéo ne contient aucune image."));
    }
    let last = n_frames - 1;
    let idx = |t: f64| ((t * fps).round_ties_even().max(0.0) as usize).min(last);
    let (i0, i_start, i_end) = (idx(p.t0), idx(p.start.min(p.t0)), idx(p.end.max(p.t0)));
    let first = frame(i0)?.ok_or_else(|| export_err("Image de départ introuvable."))?;
    let mut tracker = Tracker::new(&first, &stab(i0), p.yaw, p.pitch, p.size)?;
    let mut track = Track::default();
    let total = (i_end - i0) + if p.backward { i0 - i_start } else { 0 };
    let mut done = 0usize;
    // vers l'avant puis, si demandé, vers l'arrière
    let mut passes: Vec<(Vec<usize>, bool)> = vec![((i0 + 1..=i_end).collect(), true)];
    if p.backward {
        passes.push(((i_start..i0).rev().collect(), false));
    }
    track.samples.push(TrackSample { t: i0 as f64 / fps, yaw: p.yaw, pitch: p.pitch, score: 1.0 });
    for (indices, forward) in passes {
        tracker.rewind();
        for i in indices {
            if cancelled() {
                return Err(crate::error::Cancelled.into());
            }
            let Some(f) = frame(i)? else { break };
            let s = tracker.step(&f, &stab(i));
            done += 1;
            progress(done as f64 / total.max(1) as f64);
            if s.lost {
                let t = i as f64 / fps;
                if forward {
                    track.lost_after = Some(t);
                } else {
                    track.lost_before = Some(t);
                }
                break;
            }
            track.samples.push(TrackSample { t: i as f64 / fps, yaw: s.yaw, pitch: s.pitch, score: s.score });
        }
    }
    track.samples.sort_by(|a, b| a.t.total_cmp(&b.t));
    // les images où la corrélation était faible avant la perte (prévision) restent : elles prolongent le mouvement
    // un instant ; la perte elle-même n'est pas incluse
    progress(1.0);
    Ok(track)
}

/// Nombre d'images décodées d'un coup (une quarantaine de Mo en mémoire).
const CHUNK_FRAMES: usize = 90;

/// Décode `count` images du proxy à partir de l'image `first` en images équirectangulaires grises 960 x 480.
fn decode_gray_frames(
    src: &Source,
    calib: &Calibration,
    proxy: &Path,
    first: usize,
    count: usize,
    cancel: &AtomicBool,
) -> Result<Vec<Equirect>> {
    let dec = analysis_decoder_range(src, proxy, first, count);
    let (w, h) = (FRAME_W, FRAME_W / 2);
    let renderer = if dec.dual {
        Some(LensRenderer::new(
            calib,
            SourceKind::Dual { lens: dec.frame_h },
            w as u32,
            h as u32,
            Projection::Equirect,
            4,
        )?)
    } else {
        None
    };
    let in_bytes = renderer.as_ref().map_or(w * h, LensRenderer::frame_bytes_in);
    let mut proc = Proc::spawn(&dec.cmd, Stdio::null(), Stdio::piped())?;
    let mut out = proc.take_stdout().ok_or_else(|| export_err("Sortie du décodeur indisponible."))?;
    let mut frames = Vec::with_capacity(count);
    let mut buf = vec![0u8; in_bytes];
    let mut equi = renderer.as_ref().map(|r| vec![0u8; r.frame_bytes_out()]);
    let identity = Mat3::identity();
    while frames.len() < count && out.read_exact(&mut buf).is_ok() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        let gray = match (&renderer, equi.as_mut()) {
            (Some(r), Some(o)) => {
                r.render(&buf, &identity, 0.0, o);
                o[..w * h].to_vec()
            }
            _ => buf.clone(),
        };
        frames.push(Equirect { w, h, gray });
    }
    proc.kill();
    let _ = proc.wait();
    Ok(frames)
}

/// Suit un sujet dans le proxy d'aperçu du projet. `stab` : la stabilisation de chaque image (comme à l'export).
pub fn track_video(
    src: &Source,
    calib: &Calibration,
    proxy: &Path,
    stab: Option<&[Mat3]>,
    p: &TrackParams,
    on_progress: &dyn Fn(f64),
    cancel: &AtomicBool,
) -> Result<Track> {
    let n = src.frames();
    let mut chunk: Vec<Equirect> = vec![];
    let (mut chunk_first, mut last_asked) = (0usize, None::<usize>);
    let stab_of = |i: usize| match stab {
        Some(m) if !m.is_empty() => m[i.min(m.len() - 1)],
        _ => Mat3::identity(),
    };
    let mut frame = |i: usize| -> Result<Option<Equirect>> {
        if i >= n {
            return Ok(None);
        }
        let hit = i >= chunk_first && i < chunk_first + chunk.len();
        if !hit {
            // dans le sens de la lecture on décode vers l'avant, en marche arrière vers le début
            let forward = last_asked.is_none_or(|l| i >= l);
            let first = if forward { i } else { (i + 1).saturating_sub(CHUNK_FRAMES) };
            let count = if forward { CHUNK_FRAMES.min(n - i) } else { i + 1 - first };
            chunk = decode_gray_frames(src, calib, proxy, first, count, cancel)?;
            chunk_first = first;
        }
        last_asked = Some(i);
        Ok(i.checked_sub(chunk_first).and_then(|k| chunk.get(k)).cloned())
    };
    track_with(p, src.fps, n, &stab_of, &mut frame, on_progress, &|| cancel.load(Ordering::Relaxed))
}

/// Écart toléré (°) entre le chemin simplifié et le chemin suivi.
pub const KEYFRAME_TOLERANCE: f64 = 0.8;

/// Chemin suivi -> keyframes linéaires (cap et inclinaison seulement) : lissage triangulaire, puis simplification de
/// Douglas-Peucker. Même algorithme que `web/recorder.js`. Le cap est rendu cyclique, jamais plus de 150° entre deux
/// keyframes.
pub fn samples_to_keyframes(samples: &[TrackSample], tolerance: f64) -> Vec<Keyframe> {
    let mut pts: Vec<&TrackSample> = vec![];
    for s in samples {
        if pts.last().is_none_or(|p| s.t > p.t + 1e-6) {
            pts.push(s);
        }
    }
    let n = pts.len();
    if n < 2 {
        return vec![];
    }
    // cap continu (sans saut de 360°)
    let mut yaw = Vec::with_capacity(n);
    for (i, p) in pts.iter().enumerate() {
        yaw.push(if i == 0 { p.yaw } else { yaw[i - 1] + crate::geometry::wrap180(p.yaw - yaw[i - 1]) });
    }
    let pitch: Vec<f64> = pts.iter().map(|p| p.pitch).collect();
    let (yaw, pitch) = (smooth(&smooth(&yaw)), smooth(&smooth(&pitch)));
    let t: Vec<f64> = pts.iter().map(|p| p.t).collect();
    let error = |i: usize, j: usize, k: usize| {
        let u = (t[k] - t[i]) / (t[j] - t[i]);
        let e = |c: &[f64]| (c[k] - (c[i] + (c[j] - c[i]) * u)).abs();
        e(&yaw).max(e(&pitch)) / tolerance
    };
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;
    let mut stack = vec![(0usize, n - 1)];
    while let Some((i, j)) = stack.pop() {
        if j - i < 2 {
            continue;
        }
        let (best, worst) =
            (i + 1..j).map(|k| (k, error(i, j, k))).max_by(|a, b| a.1.total_cmp(&b.1)).expect("intervalle non vide");
        if worst > 1.0 || (yaw[j] - yaw[i]).abs() > 150.0 {
            keep[best] = true;
            stack.push((i, best));
            stack.push((best, j));
        }
    }
    let round = |v: f64| (v * 1000.0).round() / 1000.0;
    (0..n)
        .filter(|&k| keep[k])
        .map(|k| Keyframe {
            t: round(t[k]),
            yaw: Some(round(crate::geometry::wrap180(yaw[k]))),
            pitch: Some(round(pitch[k])),
            ease: Some(crate::geometry::Ease::Linear),
            ..Default::default()
        })
        .collect()
}

/// Moyenne mobile triangulaire (1, 2, 3, 2, 1), prolongée par symétrie impaire aux extrémités.
fn smooth(values: &[f64]) -> Vec<f64> {
    let n = values.len();
    let at = |j: i64| -> f64 {
        if j < 0 {
            2.0 * values[0] - values[(-j).min(n as i64 - 1) as usize]
        } else if j >= n as i64 {
            2.0 * values[n - 1] - values[(2 * (n as i64 - 1) - j).max(0) as usize]
        } else {
            values[j as usize]
        }
    };
    let w = [1.0, 2.0, 3.0, 2.0, 1.0];
    (0..n as i64).map(|i| (-2..=2).map(|k| at(i + k) * w[(k + 2) as usize]).sum::<f64>() / 9.0).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{make_texture, Rng};

    /// Monde de test : un fond texturé en niveaux de gris (équirectangulaire) et un sujet — un disque à damier — qui se
    /// déplace ; `cam` oriente la caméra qui le filme.
    struct World {
        bg: Equirect,
        radius_deg: f64,
    }

    impl World {
        fn new(radius_deg: f64) -> World {
            let tex = make_texture(1024, 5);
            let gray = tex
                .rgb
                .chunks(3)
                .map(|p| (0.299 * f64::from(p[0]) + 0.587 * f64::from(p[1]) + 0.114 * f64::from(p[2])) as u8)
                .collect();
            World { bg: Equirect { w: tex.w, h: tex.h, gray }, radius_deg }
        }

        /// L'image que voit la caméra `cam` (caméra -> monde) quand le sujet est en `subject` (monde) ; `None` = absent.
        fn frame(&self, cam: &Mat3, subject: Option<(f64, f64)>) -> Equirect {
            let (w, h) = (FRAME_W, FRAME_W / 2);
            let sub = subject.map(|(y, p)| (dir_of(y, p), ypr_to_matrix(y, p, 0.0)));
            let mut gray = vec![0u8; w * h];
            gray.par_chunks_mut(w).enumerate().for_each(|(j, row)| {
                let lat = (0.5 - (j as f64 + 0.5) / h as f64) * PI;
                for (i, px) in row.iter_mut().enumerate() {
                    let lon = ((i as f64 + 0.5) / w as f64 - 0.5) * 2.0 * PI;
                    let d = cam * Vec3::new(lat.cos() * lon.sin(), lat.sin(), lat.cos() * lon.cos());
                    if let Some((c, frame)) = &sub {
                        let angle = d.dot(c).clamp(-1.0, 1.0).acos().to_degrees();
                        if angle < self.radius_deg {
                            // texture lié au sujet (repère local) : des cellules de gris tirés au hasard, sans répétition
                            // (un damier se confond avec lui-même quand on le décale d'une case en diagonale)
                            let l = frame.transpose() * d;
                            let (a, b) = (
                                (l.x / l.z.max(0.1) * 40.0).floor() as i64,
                                (l.y / l.z.max(0.1) * 40.0).floor() as i64,
                            );
                            let h = (a.wrapping_mul(73_856_093) ^ b.wrapping_mul(19_349_663))
                                .wrapping_mul(2_654_435_761)
                                >> 7;
                            *px = 30 + (h & 0xFF).min(200) as u8;
                            continue;
                        }
                    }
                    // le fond est lié au monde : on l'échantillonne dans la direction monde
                    let (lw, latw) = (d.x.atan2(d.z), d.y.clamp(-1.0, 1.0).asin());
                    let xw = (lw / (2.0 * PI) + 0.5) * self.bg.w as f64 - 0.5;
                    let yw = (0.5 - latw / PI) * self.bg.h as f64 - 0.5;
                    *px = sample_equirect(&self.bg, xw as f32, yw as f32).round() as u8;
                }
            });
            Equirect { w, h, gray }
        }
    }

    fn angle_between(a: (f64, f64), b: (f64, f64)) -> f64 {
        dir_of(a.0, a.1).dot(&dir_of(b.0, b.1)).clamp(-1.0, 1.0).acos().to_degrees()
    }

    /// Chemin du sujet : de (10°, 5°) à (70°, 15°) en `n` images, avec une ondulation d'inclinaison.
    fn path(i: usize, n: usize) -> (f64, f64) {
        let u = i as f64 / (n - 1) as f64;
        (10.0 + 60.0 * u, 5.0 + 10.0 * u + 4.0 * (u * 2.0 * PI * 1.5).sin())
    }

    fn run(world: &World, n: usize, cams: &[Mat3], stabilized: bool, subject_until: usize, p: TrackParams) -> Track {
        let fps = 30.0;
        let mut frame = |i: usize| -> Result<Option<Equirect>> {
            Ok((i < n).then(|| world.frame(&cams[i], (i < subject_until).then(|| path(i, n)))))
        };
        let stab = |i: usize| if stabilized { cams[i.min(n - 1)].transpose() } else { Mat3::identity() };
        track_with(&p, fps, n, &stab, &mut frame, &|_| {}, &|| false).unwrap()
    }

    fn params(n: usize, i0: usize, offset: (f64, f64), backward: bool) -> TrackParams {
        let (y, p) = path(i0, n);
        TrackParams {
            t0: i0 as f64 / 30.0,
            yaw: y + offset.0,
            pitch: p + offset.1,
            size: 12.0,
            start: 0.0,
            end: n as f64 / 30.0,
            backward,
        }
    }

    #[test]
    fn ncc_matches_a_brute_force_computation() {
        let mut rng = Rng::new(3);
        let crop: Vec<f32> = (0..CROP * CROP).map(|_| (rng.unit() * 255.0) as f32).collect();
        let tpl = Template::from_patch(&patch_at(&crop, 30, 50));
        let integral = Integral::new(&crop);
        // une fenêtre uniforme ne corrèle avec rien, même si l'arrondi donne un numérateur non nul
        let flat = vec![200f32; CROP * CROP];
        assert_eq!(ncc_at(&flat, &Integral::new(&flat), &tpl, 52, 52), 0.0);
        for (x0, y0) in [(30usize, 50usize), (40, 60), (16, 16), (88, 88), (52, 52)] {
            let win = patch_at(&crop, x0, y0);
            let n = win.len() as f64;
            let mean = win.iter().map(|v| f64::from(*v)).sum::<f64>() / n;
            let num: f64 = win.iter().zip(&tpl.zero_mean).map(|(w, t)| (f64::from(*w) - mean) * f64::from(*t)).sum();
            let var: f64 = win.iter().map(|w| (f64::from(*w) - mean).powi(2)).sum();
            let want = num / (var.sqrt() * f64::from(tpl.norm));
            let got = ncc_at(&crop, &integral, &tpl, x0, y0);
            assert!((got - want).abs() < 1e-3, "({x0}, {y0}) : {got} contre {want}");
            assert!(got.abs() <= 1.0 + 1e-6);
        }
    }

    #[test]
    fn follows_a_moving_subject_with_a_fixed_camera() {
        let (world, n) = (World::new(5.0), 60);
        let cams = vec![Mat3::identity(); n];
        let t = run(&world, n, &cams, false, n, params(n, 5, (0.0, 0.0), false));
        assert_eq!(t.lost_after, None);
        assert_eq!(t.samples.len(), n - 5, "une position par image, de l'instant désigné à la fin");
        let worst = t
            .samples
            .iter()
            .map(|s| angle_between((s.yaw, s.pitch), path((s.t * 30.0).round() as usize, n)))
            .fold(0.0, f64::max);
        assert!(worst < 1.0, "écart maximal {worst:.2}° avec la trajectoire vraie");
        assert!(t.samples.iter().skip(1).all(|s| s.score > 0.7), "corrélation haute sur toute la trajectoire");
        // les échantillons avancent dans le temps et portent l'instant de leur image
        assert!(t.samples.windows(2).all(|w| w[1].t > w[0].t));
        assert!((t.samples[0].t - 5.0 / 30.0).abs() < 1e-9);
    }

    #[test]
    fn a_shaking_camera_is_handled_by_the_stabilization_or_by_the_search() {
        let (world, n) = (World::new(5.0), 60);
        let cams: Vec<Mat3> = (0..n)
            .map(|i| {
                let a = i as f64 * 0.35; // un tremblement de main : environ 1,7 Hz
                ypr_to_matrix(3.0 * a.sin(), 2.0 * (a * 1.3).cos(), 2.0 * (a * 0.7).sin())
            })
            .collect();
        // stabilisation parfaite : le suivi donne la direction du sujet dans le monde
        let t = run(&world, n, &cams, true, n, params(n, 10, (0.0, 0.0), true));
        assert_eq!((t.lost_after, t.lost_before), (None, None));
        assert_eq!(t.samples.len(), n, "dans les deux sens, toute la plage");
        let worst = t
            .samples
            .iter()
            .map(|s| angle_between((s.yaw, s.pitch), path((s.t * 30.0).round() as usize, n)))
            .fold(0.0, f64::max);
        assert!(worst < 1.2, "stabilisé : écart maximal {worst:.2}°");
        // sans stabilisation : la direction est celle du repère de la caméra, que le suivi retrouve aussi
        let t = run(&world, n, &cams, false, n, params(n, 10, (0.0, 0.0), false));
        assert_eq!(t.lost_after, None);
        let worst = t
            .samples
            .iter()
            .map(|s| {
                let i = (s.t * 30.0).round() as usize;
                let (wy, wp) = path(i, n);
                let in_cam = cams[i].transpose() * dir_of(wy, wp);
                angle_between((s.yaw, s.pitch), yaw_pitch(&in_cam))
            })
            .fold(0.0, f64::max);
        assert!(worst < 2.0, "repère de la caméra : écart maximal {worst:.2}°");
    }

    #[test]
    fn the_clicked_point_is_followed_not_the_centre_of_the_subject() {
        let (world, n) = (World::new(5.0), 60);
        let cams = vec![Mat3::identity(); n];
        let click = (2.0, -1.5);
        let t = run(&world, n, &cams, false, n, params(n, 5, click, false));
        assert_eq!(t.lost_after, None);
        let worst = t
            .samples
            .iter()
            .map(|s| angle_between((s.yaw - click.0, s.pitch - click.1), path((s.t * 30.0).round() as usize, n)))
            .fold(0.0, f64::max);
        assert!(worst < 1.6, "l'écart avec le point désigné reste de {worst:.2}° au plus");
    }

    #[test]
    fn the_subject_is_declared_lost_when_it_disappears() {
        let (world, n) = (World::new(5.0), 90);
        let cams = vec![Mat3::identity(); n];
        let t = run(&world, n, &cams, false, 40, params(n, 5, (0.5, 0.5), false));
        let lost = t.lost_after.expect("sujet perdu");
        assert!(
            (40.0 / 30.0..=52.0 / 30.0).contains(&lost),
            "perte à {lost:.2} s (le sujet disparaît à {:.2} s)",
            40.0 / 30.0
        );
        assert!(t.samples.iter().all(|s| s.t < lost), "aucun échantillon au-delà de la perte");
        assert!(t.samples.len() >= 33);
    }

    #[test]
    fn a_featureless_start_point_is_refused() {
        // un ciel uni : rien à suivre
        let blank = Equirect { w: FRAME_W, h: FRAME_W / 2, gray: vec![128; FRAME_W * FRAME_W / 2] };
        let e = Tracker::new(&blank, &Mat3::identity(), 0.0, 0.0, 12.0).err().expect("refusé");
        assert!(crate::error::is_user_error(&e) && e.to_string().contains("trop uni"), "{e}");
    }

    #[test]
    fn template_matching_finds_a_known_shift_to_a_fraction_of_a_pixel() {
        // une tache gaussienne texturée placée au centre puis décalée de (7,3 ; -4,6) pixels
        let blob = |cx: f64, cy: f64| -> Vec<f32> {
            let mut rng = Rng::new(9);
            let noise: Vec<f32> = (0..CROP * CROP).map(|_| rng.unit() as f32).collect();
            (0..CROP * CROP)
                .map(|k| {
                    let (x, y) = ((k % CROP) as f64 + 0.5, (k / CROP) as f64 + 0.5);
                    let r2 = (x - cx).powi(2) + (y - cy).powi(2);
                    let g = (-r2 / (2.0 * 9.0f64.powi(2))).exp();
                    (60.0 + 140.0 * g + 20.0 * f64::from(noise[k]) * g) as f32
                })
                .collect()
        };
        let centre = CROP as f64 / 2.0;
        let tpl = Template::from_patch(&patch_at(&blob(centre, centre), (CROP - PATCH) / 2, (CROP - PATCH) / 2));
        let (dx, dy, score) = match_template(&blob(centre, centre), &tpl);
        assert!(dx.abs() < 0.05 && dy.abs() < 0.05 && score > 0.99, "{dx} {dy} {score}");
        // le bruit est lié à l'image : on le fixe pour comparer les mêmes motifs avec un léger flou de sous-pixel
        let (dx, dy, score) = match_template(&blob(centre + 7.0, centre - 4.0), &tpl);
        assert!((dx - 7.0).abs() < 1.0 && (dy + 4.0).abs() < 1.0 && score > 0.5, "{dx} {dy} {score}");
    }

    #[test]
    fn keyframes_follow_the_track_with_few_points() {
        let samples: Vec<TrackSample> = (0..120)
            .map(|i| {
                let t = i as f64 / 30.0;
                TrackSample { t, yaw: 20.0 + 15.0 * t, pitch: 5.0 + 6.0 * (t * 1.2).sin(), score: 0.9 }
            })
            .collect();
        let k = samples_to_keyframes(&samples, KEYFRAME_TOLERANCE);
        assert!(k.len() >= 3 && k.len() <= 20, "{} keyframes", k.len());
        assert!(k.iter().all(|k| k.ease == Some(crate::geometry::Ease::Linear) && k.roll.is_none() && k.fov.is_none()));
        assert_eq!(
            (k[0].t, k.last().unwrap().t),
            (0.0, samples.last().unwrap().t.mul_add(1000.0, 0.0).round() / 1000.0)
        );
        let base = crate::geometry::View::default();
        for s in &samples {
            let v = crate::geometry::interpolate_view(&k, s.t, &base);
            assert!(
                crate::geometry::wrap180(v.yaw - s.yaw).abs() < 1.1 && (v.pitch - s.pitch).abs() < 1.1,
                "t = {}",
                s.t
            );
        }
        assert!(samples_to_keyframes(&samples[..1], KEYFRAME_TOLERANCE).is_empty());
    }
}
