//! Génération d'une vidéo « type Insta360 X3 » synthétique : pour essayer le logiciel sans caméra
//! (`x3studio demo`) et pour les tests (mouvement de caméra connu → vérité terrain).
//! Spec : `x3studio/synth.py`. Une scène sphérique texturée est « filmée » par une caméra virtuelle
//! qui tremble ; les deux objectifs fisheye (champ 200°) sont écrits comme deux flux vidéo d'un
//! fichier `.insv` (conteneur MP4) avec une piste audio, comme le fait l'X3.
//!
//! Le rendu des objectifs est fait ici (pas de filtre `v360` : FFmpeg ne fait qu'encoder).

use crate::error::{export_err, Result};
use crate::geometry::{rot_yaw, ypr_to_matrix, Mat3, Vec3};
use crate::pipeline::Proc;
use rayon::prelude::*;
use std::f64::consts::PI;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Static,
    Shake,
    Pan,
}

impl FromStr for Motion {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "static" => Ok(Motion::Static),
            "shake" => Ok(Motion::Shake),
            "pan" => Ok(Motion::Pan),
            other => Err(export_err(format!("Mouvement inconnu : {other} (static, shake ou pan)"))),
        }
    }
}

/// Générateur pseudo-aléatoire reproductible (SplitMix64).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Flottant dans [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
    /// Entier dans [lo, hi).
    pub fn int(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.unit() * (hi - lo).max(1) as f64) as i64
    }
}

/// Texture équirectangulaire RGB riche en coins et repères colorés (rouge 0°, vert +90°, bleu 180°,
/// jaune −90°, ligne d'horizon blanche).
pub struct Texture {
    pub w: usize,
    pub h: usize,
    /// RGB 8 bits, ligne par ligne.
    pub rgb: Vec<u8>,
}

pub fn make_texture(width: usize, seed: u64) -> Texture {
    let w = (width.max(64) / 2) * 2;
    let h = w / 2;
    let mut rng = Rng::new(seed);
    let mut img = vec![0f32; w * h * 3];
    let (sky, ground) = ([0.35f32, 0.55, 0.9], [0.35f32, 0.28, 0.2]);
    for y in 0..h {
        let t = y as f32 / (h - 1) as f32;
        for x in 0..w {
            for c in 0..3 {
                img[(y * w + x) * 3 + c] = if t < 0.5 { sky[c] * (1.0 - t * 0.6) } else { ground[c] * (0.6 + t * 0.4) };
            }
        }
    }
    // bruit multi-échelle : blocs de couleur aléatoire (les arêtes donnent des coins à suivre)
    for (scale, amp) in [(64usize, 0.25f32), (24, 0.2), (8, 0.15)] {
        let (gw, gh) = (w / scale + 1, h / scale + 1);
        let grid: Vec<f32> = (0..gw * gh * 3).map(|_| rng.unit() as f32).collect();
        img.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
            for x in 0..w {
                for c in 0..3 {
                    row[x * 3 + c] += (grid[((y / scale) * gw + x / scale) * 3 + c] - 0.5) * amp;
                }
            }
        });
    }
    // formes
    for _ in 0..(w * h / 4096).max(40) {
        let cx = rng.int(0, w as i64);
        let cy = rng.int((h as f64 * 0.12) as i64, (h as f64 * 0.88) as i64);
        let (sw, sh) = (rng.int(8, 70), rng.int(8, 70));
        let col = [rng.unit() as f32, rng.unit() as f32, rng.unit() as f32];
        let ellipse = rng.unit() >= 0.5;
        for y in (cy - sh).max(0)..(cy + sh).min(h as i64) {
            for x in (cx - sw).max(0)..(cx + sw).min(w as i64) {
                if ellipse {
                    let (dx, dy) = ((x - cx) as f64 / sw as f64, (y - cy) as f64 / sh as f64);
                    if dx * dx + dy * dy >= 1.0 {
                        continue;
                    }
                }
                img[(y as usize * w + x as usize) * 3..][..3].copy_from_slice(&col);
            }
        }
    }
    // repères : bandes verticales aux 4 points cardinaux + horizon
    for (lon, col) in
        [(0.0, [1.0f32, 0.0, 0.0]), (90.0, [0.0, 1.0, 0.0]), (180.0, [0.0, 0.2, 1.0]), (-90.0, [1.0, 1.0, 0.0])]
    {
        let cx = (((lon + 180.0f64) % 360.0) / 360.0 * w as f64) as usize;
        for y in 0..h {
            for dx in 0..12 {
                let x = (cx + w + dx - 6) % w; // la bande à 180° est coupée par le bord de l'image : on la prolonge
                img[(y * w + x) * 3..][..3].copy_from_slice(&col);
            }
        }
    }
    for y in h / 2 - 2..h / 2 + 2 {
        img[y * w * 3..(y + 1) * w * 3].fill(1.0);
    }
    let rgb = img.iter().map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8).collect();
    Texture { w, h, rgb }
}

/// Orientations caméra -> monde pour `n` images : identité (`Static`), tremblement de quelques degrés
/// (`Shake`), tremblement + rotation lente de 12°/s en cap (`Pan`).
pub fn camera_motion(n: usize, fps: f64, motion: Motion, seed: u64) -> Vec<Mat3> {
    let mut rng = Rng::new(seed ^ 0x0053_4841_4B45);
    let mut shake = |amp: f64| -> Vec<f64> {
        let comps: Vec<(f64, f64, f64)> =
            [0.9, 2.3, 4.1].iter().map(|&f| (f, rng.uniform(0.4, 1.0), rng.uniform(0.0, 2.0 * PI))).collect();
        (0..n)
            .map(|i| {
                let t = i as f64 / fps;
                amp * comps.iter().map(|&(f, a, p)| a * (2.0 * PI * f * t + p).sin()).sum::<f64>() / 2.0
            })
            .collect()
    };
    if motion == Motion::Static {
        return vec![Mat3::identity(); n];
    }
    let (yaw, pitch, roll) = (shake(3.0), shake(2.5), shake(3.5));
    (0..n)
        .map(|i| {
            let pan = if motion == Motion::Pan { 12.0 * i as f64 / fps } else { 0.0 };
            ypr_to_matrix(pan + yaw[i], pitch[i], roll[i])
        })
        .collect()
}

/// Réglages d'une vidéo synthétique.
#[derive(Debug, Clone)]
pub struct SynthOptions {
    pub seconds: f64,
    pub fps: f64,
    /// Côté d'une image d'objectif (px, pair).
    pub lens: u32,
    /// Champ de vision de chaque objectif (degrés).
    pub fov: f64,
    pub motion: Motion,
    pub seed: u64,
    pub with_audio: bool,
    /// Bruit additif (écart-type en niveaux sur 255), différent à chaque image : pour tester la réduction du bruit.
    pub noise: f64,
    /// Demi-angle (degrés) d'une « perche » magenta peinte dans les deux objectifs autour du nadir de la caméra
    /// (axe -Y du repère de la caméra, fixe par rapport à elle) ; 0 = aucune.
    pub stick: f64,
}

impl Default for SynthOptions {
    fn default() -> Self {
        SynthOptions {
            seconds: 3.0,
            fps: 30.0,
            lens: 960,
            fov: 200.0,
            motion: Motion::Shake,
            seed: 3,
            with_audio: true,
            noise: 0.0,
            stick: 0.0,
        }
    }
}

/// Directions (repère de l'objectif) de chaque pixel d'un fisheye équidistant `lens x lens`.
struct LensGrid {
    lens: usize,
    /// `None` hors du cercle image.
    dirs: Vec<Option<[f32; 3]>>,
}

fn lens_grid(lens: usize, fov_deg: f64) -> LensGrid {
    let half = fov_deg.to_radians() / 2.0;
    let mut dirs = Vec::with_capacity(lens * lens);
    for j in 0..lens {
        let v = (j as f64 + 0.5) / lens as f64 * 2.0 - 1.0;
        for i in 0..lens {
            let u = (i as f64 + 0.5) / lens as f64 * 2.0 - 1.0;
            let r = u.hypot(v);
            dirs.push(if r > 1.0 {
                None
            } else {
                let th = r * half;
                let s = if r > 1e-12 { th.sin() / r } else { half };
                Some([(u * s) as f32, (-v * s) as f32, th.cos() as f32])
            });
        }
    }
    LensGrid { lens, dirs }
}

/// Couleur de la texture dans la direction monde `(x, y, z)` (interpolation bilinéaire, raccord en longitude).
fn sample_texture(tex: &Texture, x: f32, y: f32, z: f32) -> [u8; 3] {
    let (tw, th) = (tex.w, tex.h);
    let lon = x.atan2(z);
    let lat = y.clamp(-1.0, 1.0).asin();
    let fx = (lon / (2.0 * std::f32::consts::PI) + 0.5) * tw as f32 - 0.5;
    let fy = ((0.5 - lat / std::f32::consts::PI) * th as f32 - 0.5).clamp(0.0, th as f32 - 1.0);
    let (x0f, y0f) = (fx.floor(), fy.floor());
    let (ax, ay) = (fx - x0f, fy - y0f);
    let x0 = (x0f as i64).rem_euclid(tw as i64) as usize;
    let x1 = (x0 + 1) % tw;
    let (y0, y1) = (y0f as usize, (y0f as usize + 1).min(th - 1));
    let mut px = [0u8; 3];
    for (c, out) in px.iter_mut().enumerate() {
        let t = |xx: usize, yy: usize| f32::from(tex.rgb[(yy * tw + xx) * 3 + c]);
        let v = (t(x0, y0) * (1.0 - ax) + t(x1, y0) * ax) * (1.0 - ay) + (t(x0, y1) * (1.0 - ax) + t(x1, y1) * ax) * ay;
        *out = (v + 0.5) as u8;
    }
    px
}

fn mat_f32(m: &Mat3) -> [f32; 9] {
    let mut a = [0f32; 9];
    for r in 0..3 {
        for c in 0..3 {
            a[r * 3 + c] = m[(r, c)] as f32;
        }
    }
    a
}

/// Couleur de la perche peinte par [`SynthOptions::stick`].
pub const STICK_COLOR: [u8; 3] = [255, 0, 255];

/// Une image « deux objectifs côte à côte » (RGB 8 bits, `2*lens x lens`) vue par la caméra `cam`
/// (caméra -> monde) ; l'objectif B regarde vers l'arrière. Avec une perche, les directions à moins de `stick_deg`
/// degrés du nadir de la caméra (repère de la caméra, donc sans rapport avec le monde) sont peintes en [`STICK_COLOR`].
fn render_pair(tex: &Texture, grid: &LensGrid, cam: &Mat3, stick_deg: Option<f64>, out: &mut [u8]) {
    let l = grid.lens;
    let stick_cos = stick_deg.map(|d| d.to_radians().cos() as f32);
    let ms = [mat_f32(cam), mat_f32(&(cam * rot_yaw(PI)))];
    out.par_chunks_mut(2 * l * 3).enumerate().for_each(|(y, row)| {
        for (k, m) in ms.iter().enumerate() {
            for x in 0..l {
                let px = &mut row[(k * l + x) * 3..][..3];
                let Some(d) = grid.dirs[y * l + x] else {
                    px.fill(0);
                    continue;
                };
                // l'axe y du repère de l'objectif est celui de la caméra pour les deux objectifs (B est tourné en cap)
                if stick_cos.is_some_and(|c| -d[1] > c) {
                    px.copy_from_slice(&STICK_COLOR);
                    continue;
                }
                let wx = m[0] * d[0] + m[1] * d[1] + m[2] * d[2];
                let wy = m[3] * d[0] + m[4] * d[1] + m[5] * d[2];
                let wz = m[6] * d[0] + m[7] * d[1] + m[8] * d[2];
                px.copy_from_slice(&sample_texture(tex, wx, wy, wz));
            }
        }
    });
}

/// Bruit additif reproductible, à peu près gaussien (somme de quatre tirages uniformes), d'écart-type `sigma` niveaux ;
/// il dépend de la graine, du numéro d'image et de la position : deux images successives n'ont pas le même.
fn add_noise(frame: &mut [u8], sigma: f64, seed: u64, index: usize) {
    if sigma <= 0.0 {
        return;
    }
    frame.par_chunks_mut(3 * 1024).enumerate().for_each(|(c, chunk)| {
        let mut rng = Rng::new(seed ^ ((index as u64) << 24) ^ (c as u64).wrapping_mul(0x9E37_79B9));
        for p in chunk.iter_mut() {
            let g = (rng.unit() + rng.unit() + rng.unit() + rng.unit() - 2.0) * 3.0f64.sqrt();
            *p = (f64::from(*p) + g * sigma).round().clamp(0.0, 255.0) as u8;
        }
    });
}

/// Image équirectangulaire RGB `w x h` vue par la caméra `cam` (caméra -> monde) : le centre est
/// l'axe de la caméra, `x` croît vers la droite.
pub fn render_equirect(tex: &Texture, cam: &Mat3, w: usize, h: usize, out: &mut [u8]) {
    let m = mat_f32(cam);
    out.par_chunks_mut(w * 3).enumerate().for_each(|(j, row)| {
        let lat = (0.5 - (j as f32 + 0.5) / h as f32) * std::f32::consts::PI;
        for i in 0..w {
            let lon = ((i as f32 + 0.5) / w as f32 - 0.5) * 2.0 * std::f32::consts::PI;
            let d = [lat.cos() * lon.sin(), lat.sin(), lat.cos() * lon.cos()];
            let wx = m[0] * d[0] + m[1] * d[1] + m[2] * d[2];
            let wy = m[3] * d[0] + m[4] * d[1] + m[5] * d[2];
            let wz = m[6] * d[0] + m[7] * d[1] + m[8] * d[2];
            row[i * 3..i * 3 + 3].copy_from_slice(&sample_texture(tex, wx, wy, wz));
        }
    });
}

/// Lance ffmpeg (`args` : entrée brute sur stdin + sorties) et lui envoie `n` images rendues par `render`
/// dans un tampon de `frame_bytes` octets. La sortie `out` est supprimée en cas d'échec.
fn feed_ffmpeg(
    args: Vec<String>,
    out: &Path,
    n: usize,
    frame_bytes: usize,
    mut render: impl FnMut(usize, &mut [u8]),
) -> Result<()> {
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut cmd: Vec<String> = ["ffmpeg", "-hide_banner", "-nostdin", "-y", "-v", "error"].map(String::from).to_vec();
    cmd.extend(args);
    cmd.push(out.to_string_lossy().into_owned());
    let mut proc = Proc::spawn(&cmd, Stdio::piped(), Stdio::null())?;
    let mut stdin = proc.take_stdin();
    let mut frame = vec![0u8; frame_bytes];
    let mut write_err = None;
    for i in 0..n {
        render(i, &mut frame);
        if let Some(w) = stdin.as_mut() {
            if let Err(e) = w.write_all(&frame) {
                write_err = Some(e);
                break;
            }
        }
    }
    drop(stdin);
    if write_err.is_some() {
        proc.kill();
    }
    let (ok, tail) = proc.wait();
    if write_err.is_some() || !ok {
        let _ = std::fs::remove_file(out);
        return Err(export_err(format!("ffmpeg a échoué : {tail}")));
    }
    Ok(())
}

fn check_options(opt: &SynthOptions) -> Result<usize> {
    if !(opt.fps > 0.0 && opt.seconds > 0.0) {
        return Err(export_err("Durée et cadence doivent être positives."));
    }
    let n = (opt.seconds * opt.fps).round() as usize;
    if n == 0 {
        return Err(export_err("La vidéo ne contiendrait aucune image."));
    }
    Ok(n)
}

/// Arguments ffmpeg communs : entrée brute RGB sur stdin, audio optionnel (sinusoïde de 440 Hz).
fn raw_input_args(size: &str, opt: &SynthOptions) -> Vec<String> {
    let mut a: Vec<String> =
        ["-f", "rawvideo", "-pix_fmt", "rgb24", "-s", size, "-framerate"].map(String::from).to_vec();
    a.extend([format!("{}", opt.fps), "-i".into(), "-".into()]);
    if opt.with_audio {
        a.extend(["-f".to_string(), "lavfi".into(), "-i".into()]);
        a.push(format!("sine=frequency=440:sample_rate=48000:duration={}", opt.seconds));
    }
    a
}

fn encode_args(opt: &SynthOptions, n: usize) -> Vec<String> {
    let mut a: Vec<String> = vec![];
    if opt.with_audio {
        a.extend(["-map", "1:a", "-c:a", "aac", "-b:a", "96k"].map(String::from));
    }
    a.extend(
        ["-c:v", "libx264", "-crf", "18", "-preset", "veryfast", "-pix_fmt", "yuv420p", "-frames:v"].map(String::from),
    );
    a.extend([n.to_string(), "-r".into(), format!("{}", opt.fps), "-f".into(), "mp4".into()]);
    // comme la caméra : BT.709, plage limitée
    a.extend(["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709"].map(String::from));
    a
}

/// Écrit le `.insv` synthétique (ffmpeg requis : encodage seulement) ; retourne les orientations
/// vraies (caméra -> monde, une par image).
pub fn make_synthetic_insv(path: &Path, opt: &SynthOptions, texture: Option<&Texture>) -> Result<Vec<Mat3>> {
    let lens = opt.lens as usize;
    if lens < 16 || !lens.is_multiple_of(2) {
        return Err(export_err("La taille d'un objectif doit être paire et d'au moins 16 px."));
    }
    let n = check_options(opt)?;
    let mats = camera_motion(n, opt.fps, opt.motion, opt.seed);
    let own;
    let tex = match texture {
        Some(t) => t,
        None => {
            own = make_texture(2048, opt.seed);
            &own
        }
    };
    let grid = lens_grid(lens, opt.fov);
    let mut args = raw_input_args(&format!("{}x{}", 2 * lens, lens), opt);
    args.extend(
        [
            "-filter_complex",
            "[0:v]split[s1][s2];\
             [s1]crop=iw/2:ih:0:0,scale=out_color_matrix=bt709:out_range=tv,format=yuv420p[a];\
             [s2]crop=iw/2:ih:iw/2:0,scale=out_color_matrix=bt709:out_range=tv,format=yuv420p[b]",
            "-map",
            "[a]",
            "-map",
            "[b]",
        ]
        .map(String::from),
    );
    args.extend(encode_args(opt, n));
    feed_ffmpeg(args, path, n, 2 * lens * lens * 3, |i, frame| {
        render_pair(tex, &grid, &mats[i], (opt.stick > 0.0).then_some(opt.stick), frame);
        add_noise(frame, opt.noise, opt.seed, i);
    })?;
    Ok(mats)
}

/// Écrit une vidéo déjà assemblée (équirectangulaire `width x width/2`, un seul flux vidéo) : pour tester
/// les sources 360° « toutes faites ». `opt.lens` et `opt.fov` sont ignorés.
pub fn make_synthetic_equirect(
    path: &Path,
    width: u32,
    opt: &SynthOptions,
    texture: Option<&Texture>,
) -> Result<Vec<Mat3>> {
    if width < 32 || !width.is_multiple_of(4) {
        return Err(export_err("La largeur doit être un multiple de 4 d'au moins 32 px."));
    }
    let n = check_options(opt)?;
    let mats = camera_motion(n, opt.fps, opt.motion, opt.seed);
    let own;
    let tex = match texture {
        Some(t) => t,
        None => {
            own = make_texture(2048, opt.seed);
            &own
        }
    };
    let (w, h) = (width as usize, width as usize / 2);
    let mut args = raw_input_args(&format!("{w}x{h}"), opt);
    args.extend(["-vf", "scale=out_color_matrix=bt709:out_range=tv", "-map", "0:v"].map(String::from));
    args.extend(encode_args(opt, n));
    feed_ffmpeg(args, path, n, w * h * 3, |i, frame| {
        render_equirect(tex, &mats[i], w, h, frame);
        // la perche est autour du nadir de la caméra, donc en bas de l'image : des lignes entières
        let stick_lat = -(PI / 2.0 - opt.stick.to_radians());
        for (j, row) in frame.chunks_mut(w * 3).enumerate().filter(|_| opt.stick > 0.0) {
            if (0.5 - (j as f64 + 0.5) / h as f64) * PI < stick_lat {
                row.chunks_mut(3).for_each(|p| p.copy_from_slice(&STICK_COLOR));
            }
        }
        add_noise(frame, opt.noise, opt.seed, i);
    })?;
    Ok(mats)
}

/// Direction monde d'un pixel d'objectif — utilitaire de test : `cam · direction de l'objectif`.
pub fn lens_pixel_world_dir(cam: &Mat3, back_lens: bool, u: f64, v: f64, fov_deg: f64) -> Option<Vec3> {
    let r = u.hypot(v);
    if r > 1.0 {
        return None;
    }
    let th = r * fov_deg.to_radians() / 2.0;
    let s = if r > 1e-12 { th.sin() / r } else { fov_deg.to_radians() / 2.0 };
    let d = Vec3::new(u * s, -v * s, th.cos());
    let m = if back_lens { cam * rot_yaw(PI) } else { *cam };
    Some(m * d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_reproducible_and_uniform() {
        let (mut a, mut b) = (Rng::new(5), Rng::new(5));
        assert_eq!((a.next_u64(), a.next_u64()), (b.next_u64(), b.next_u64()));
        assert_ne!(Rng::new(5).next_u64(), Rng::new(6).next_u64());
        let mut r = Rng::new(1);
        let v: Vec<f64> = (0..10_000).map(|_| r.unit()).collect();
        assert!(v.iter().all(|x| (0.0..1.0).contains(x)));
        let mean = v.iter().sum::<f64>() / v.len() as f64;
        assert!((mean - 0.5).abs() < 0.02, "{mean}");
        assert!((0..1000).map(|_| r.int(3, 7)).all(|x| (3..7).contains(&x)));
    }

    #[test]
    fn texture_has_the_landmarks_and_is_reproducible() {
        let t = make_texture(512, 3);
        assert_eq!((t.w, t.h, t.rgb.len()), (512, 256, 512 * 256 * 3));
        assert_eq!(t.rgb, make_texture(512, 3).rgb);
        assert_ne!(t.rgb, make_texture(512, 4).rgb);
        let px = |lon: f64, y: usize| {
            let x = (((lon + 180.0) % 360.0) / 360.0 * 512.0) as usize + 1;
            let i = (y * 512 + x) * 3;
            [t.rgb[i], t.rgb[i + 1], t.rgb[i + 2]]
        };
        assert_eq!(px(0.0, 10), [255, 0, 0]);
        assert_eq!(px(90.0, 10), [0, 255, 0]);
        assert_eq!(px(180.0, 10), [0, 51, 255]);
        assert_eq!(px(-90.0, 10), [255, 255, 0]);
        assert_eq!(px(30.0, 127), [255, 255, 255], "horizon blanc");
    }

    #[test]
    fn camera_motion_shapes() {
        assert!(camera_motion(10, 30.0, Motion::Static, 1).iter().all(|m| (m - Mat3::identity()).abs().max() == 0.0));
        let shake = camera_motion(90, 30.0, Motion::Shake, 3);
        assert_eq!(shake, camera_motion(90, 30.0, Motion::Shake, 3));
        let max_angle =
            shake.iter().map(|m| ((m.trace() - 1.0) / 2.0).clamp(-1.0, 1.0).acos().to_degrees()).fold(0.0, f64::max);
        assert!(max_angle > 2.0 && max_angle < 15.0, "{max_angle}");
        let pan = camera_motion(61, 30.0, Motion::Pan, 3);
        let (yaw_end, ..) = crate::geometry::matrix_to_ypr(&pan[60]);
        assert!(yaw_end > 12.0, "2 s de rotation à 12°/s : {yaw_end}");
        assert!("pan".parse::<Motion>().is_ok() && "danse".parse::<Motion>().is_err());
    }

    #[test]
    fn lens_pixels_see_the_expected_texture() {
        // caméra penchée de 40° vers le haut : le centre de A regarde (lon 0°, lat +40°) = bande rouge, celui de B
        // (lon 180°, lat -40°) = bande bleue (l'horizon blanc ne traverse pas le centre)
        let t = make_texture(512, 3);
        let grid = lens_grid(64, 200.0);
        let mut out = vec![0u8; 2 * 64 * 64 * 3];
        render_pair(&t, &grid, &ypr_to_matrix(0.0, 40.0, 0.0), None, &mut out);
        let px = |x: usize, y: usize| [out[(y * 128 + x) * 3], out[(y * 128 + x) * 3 + 1], out[(y * 128 + x) * 3 + 2]];
        let a = px(32, 32);
        assert!(a[0] > 200 && a[1] < 60 && a[2] < 60, "A : rouge au centre {a:?}");
        let b = px(64 + 32, 32);
        assert!(b[2] > 200 && b[0] < 60, "B : bleu au centre {b:?}");
        assert_eq!(px(0, 0), [0, 0, 0], "coin hors du cercle image");
        // même direction vue par l'utilitaire de test
        let d = lens_pixel_world_dir(&Mat3::identity(), true, 0.0, 0.0, 200.0).unwrap();
        assert!((d.z + 1.0).abs() < 1e-12);
        let up = lens_pixel_world_dir(&Mat3::identity(), false, 0.0, -0.5, 200.0).unwrap();
        assert!(up.y > 0.7, "le haut de l'image regarde vers le haut : {up:?}");
    }

    #[test]
    fn the_painted_stick_is_a_cone_fixed_to_the_camera() {
        let t = make_texture(512, 3);
        let grid = lens_grid(128, 200.0);
        let magenta = |out: &[u8]| out.chunks(3).filter(|p| *p == STICK_COLOR).count();
        let mut plain = vec![0u8; 2 * 128 * 128 * 3];
        render_pair(&t, &grid, &Mat3::identity(), None, &mut plain);
        assert_eq!(magenta(&plain), 0);
        let mut a = vec![0u8; plain.len()];
        render_pair(&t, &grid, &Mat3::identity(), Some(15.0), &mut a);
        // les deux objectifs voient la perche au bord de leur image (le nadir est à 90° de leur axe)
        let n = magenta(&a);
        assert!(n > 300, "{n} pixels de perche");
        let (left, right): (usize, usize) = (0..128).fold((0, 0), |(l, r), y| {
            let row = &a[y * 256 * 3..(y + 1) * 256 * 3];
            (l + magenta(&row[..128 * 3]), r + magenta(&row[128 * 3..]))
        });
        assert!(left > 100 && right > 100, "perche dans A ({left}) et dans B ({right})");
        // la perche est liée à la caméra : une caméra penchée en voit la même image, la scène change autour
        let mut b = vec![0u8; plain.len()];
        render_pair(&t, &grid, &ypr_to_matrix(40.0, 25.0, 10.0), Some(15.0), &mut b);
        assert_eq!(magenta(&b), n);
        let mask = |img: &[u8]| img.chunks(3).map(|p| p == STICK_COLOR).collect::<Vec<_>>();
        assert_eq!(mask(&a), mask(&b));
    }

    #[test]
    fn writes_a_two_stream_insv_that_probes_like_an_x3_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("démo vidéo.insv");
        let opt = SynthOptions { seconds: 1.0, fps: 24.0, lens: 96, motion: Motion::Shake, ..Default::default() };
        let tex = make_texture(512, 3);
        let mats = make_synthetic_insv(&p, &opt, Some(&tex)).unwrap();
        assert_eq!(mats.len(), 24);
        let src = crate::media::probe_source(&p).unwrap();
        assert_eq!(src.layout, crate::media::Layout::TwoStreams);
        assert_eq!(src.lens_size, (96, 96));
        assert!((src.fps - 24.0).abs() < 0.01 && (src.duration - 1.0).abs() < 0.1, "{src:?}");
        assert!(src.has_audio);
        assert_eq!(src.frames(), 24);
        // sans audio
        let q = dir.path().join("muet.insv");
        make_synthetic_insv(&q, &SynthOptions { with_audio: false, ..opt }, Some(&tex)).unwrap();
        assert!(!crate::media::probe_source(&q).unwrap().has_audio);
    }

    #[test]
    fn writes_an_equirect_video_probed_as_already_stitched() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("360.mp4");
        let opt =
            SynthOptions { seconds: 1.0, fps: 10.0, motion: Motion::Static, with_audio: false, ..Default::default() };
        let tex = make_texture(512, 3);
        let mats = make_synthetic_equirect(&p, 256, &opt, Some(&tex)).unwrap();
        assert_eq!(mats.len(), 10);
        let src = crate::media::probe_source(&p).unwrap();
        assert_eq!((src.layout, src.lens_size, src.frames()), (crate::media::Layout::Equirect, (256, 128), 10));
        for w in [0, 31, 258] {
            let e = make_synthetic_equirect(&p, w, &opt, None).unwrap_err();
            assert!(crate::error::is_user_error(&e), "{e}");
        }
        // l'axe de la caméra est au centre de l'image : bande rouge (lon 0°) à gauche de l'horizon, vert à 3/4 de la largeur
        let mut img = vec![0u8; 256 * 128 * 3];
        render_equirect(&tex, &Mat3::identity(), 256, 128, &mut img);
        let px = |img: &[u8], x: usize, y: usize| {
            [img[(y * 256 + x) * 3], img[(y * 256 + x) * 3 + 1], img[(y * 256 + x) * 3 + 2]]
        };
        assert_eq!(px(&img, 128, 20), [255, 0, 0]);
        assert_eq!(px(&img, 192, 20), [0, 255, 0], "lon +90° à droite");
        assert_eq!(px(&img, 64, 20), [255, 255, 0], "lon -90° à gauche");
        // la caméra tourne de 90° vers la droite : le vert arrive au centre
        render_equirect(&tex, &crate::geometry::ypr_to_matrix(90.0, 0.0, 0.0), 256, 128, &mut img);
        assert_eq!(px(&img, 128, 20), [0, 255, 0]);
    }

    #[test]
    fn invalid_options_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.insv");
        for opt in [
            SynthOptions { lens: 15, ..Default::default() },
            SynthOptions { lens: 33, ..Default::default() },
            SynthOptions { seconds: 0.0, ..Default::default() },
            SynthOptions { fps: -1.0, ..Default::default() },
            SynthOptions { seconds: 0.001, fps: 1.0, ..Default::default() },
        ] {
            let e = make_synthetic_insv(&p, &opt, None).unwrap_err();
            assert!(crate::error::is_user_error(&e), "{e}");
        }
        assert!(!p.exists());
    }
}
