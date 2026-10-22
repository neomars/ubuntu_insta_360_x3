//! Utilitaires partagés par les tests d'intégration : médias synthétiques mis en cache, lecture
//! d'images décodées par ffmpeg, mesures (écart angulaire, tremblement, écart d'images).
//!
//! Les tests ne dépendent d'aucun fichier extérieur : les vidéos sont générées par `x3studio::synth`
//! (un `.insv` à deux objectifs fisheye vu par une caméra qui tremble, vérité terrain connue) et gardées
//! dans `target/tmp` ; le nom du cache contient une empreinte du code de `synth.rs` et des options, donc
//! toute modification du générateur régénère les fichiers.
#![allow(dead_code)] // chaque binaire de test n'utilise qu'une partie de ces outils

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use x3studio::geometry::{ypr_to_matrix, Mat3, Vec3};
use x3studio::synth::{make_synthetic_equirect, make_synthetic_insv, make_texture, SynthOptions, Texture};

/// Largeur et graine de la texture de toutes les vidéos de test.
pub const TEX_WIDTH: usize = 1024;
pub const TEX_SEED: u64 = 3;

/// Les tests ont besoin de ffmpeg et ffprobe : on ne « saute » jamais silencieusement.
pub fn require_ffmpeg() {
    for tool in ["ffmpeg", "ffprobe"] {
        let ok = Command::new(tool).arg("-version").output().map(|o| o.status.success()).unwrap_or(false);
        assert!(ok, "{tool} est requis pour ces tests (sudo apt install ffmpeg)");
    }
}

pub fn texture() -> &'static Texture {
    static TEX: OnceLock<Texture> = OnceLock::new();
    TEX.get_or_init(|| make_texture(TEX_WIDTH, TEX_SEED))
}

fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3))
}

pub fn cache_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("x3-test-media");
    std::fs::create_dir_all(&dir).expect("dossier de cache des médias de test");
    dir
}

/// Vidéo synthétique et orientation vraie de la caméra (caméra -> monde) à chaque image.
pub struct Clip {
    pub path: PathBuf,
    pub truth: Vec<Mat3>,
}

fn cached(name: &str, ext: &str, key: &str, generate: impl FnOnce(&Path) -> Vec<Mat3>) -> Clip {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path =
        cache_dir().join(format!("{name}-{:016x}.{ext}", fnv(&format!("{}{key}", include_str!("../../src/synth.rs")))));
    let truth_file = path.with_extension("truth");
    if path.is_file() && truth_file.is_file() {
        let text = std::fs::read_to_string(&truth_file).expect("vérité terrain");
        let v: Vec<f64> = text.split_whitespace().map(|x| x.parse().expect("flottant")).collect();
        return Clip { path, truth: v.chunks(9).map(Mat3::from_row_slice).collect() };
    }
    let tmp = cache_dir().join(format!("tmp-{}-{name}.{ext}", std::process::id()));
    let truth = generate(&tmp);
    let text: Vec<String> =
        truth.iter().flat_map(|m| (0..9).map(move |i| format!("{:e}", m[(i / 3, i % 3)]))).collect();
    std::fs::write(&truth_file, text.join(" ")).expect("écriture de la vérité terrain");
    std::fs::rename(&tmp, &path).expect("mise en cache de la vidéo de test");
    Clip { path, truth }
}

/// `.insv` synthétique à deux objectifs (mis en cache).
pub fn clip(name: &str, opt: SynthOptions) -> Clip {
    require_ffmpeg();
    cached(name, "insv", &format!("{opt:?}{TEX_WIDTH}{TEX_SEED}"), |p| {
        make_synthetic_insv(p, &opt, Some(texture())).expect("génération du .insv de test")
    })
}

/// Vidéo équirectangulaire synthétique (mise en cache).
pub fn equirect_clip(name: &str, width: u32, opt: SynthOptions) -> Clip {
    require_ffmpeg();
    cached(name, "mp4", &format!("{opt:?}{width}{TEX_WIDTH}{TEX_SEED}"), |p| {
        make_synthetic_equirect(p, width, &opt, Some(texture())).expect("génération de la vidéo équirectangulaire")
    })
}

// ---------------------------------------------------------------------------
// Lecture de vidéos
// ---------------------------------------------------------------------------
pub struct VideoInfo {
    pub w: usize,
    pub h: usize,
    pub frames: usize,
    pub fps: f64,
    pub duration: f64,
    pub codec: String,
}

pub fn probe_video(path: &Path) -> VideoInfo {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-count_frames"])
        .args(["-show_entries", "stream=width,height,nb_read_frames,r_frame_rate,codec_name:format=duration"])
        .args(["-of", "default=nw=1"])
        .arg(path)
        .output()
        .expect("ffprobe");
    assert!(out.status.success(), "ffprobe : {}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let get = |k: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{k}=")))
            .unwrap_or_else(|| panic!("{k} absent : {text}"))
            .to_string()
    };
    let (n, d) = get("r_frame_rate").split_once('/').map(|(a, b)| (a.to_string(), b.to_string())).expect("cadence");
    VideoInfo {
        w: get("width").parse().unwrap(),
        h: get("height").parse().unwrap(),
        frames: get("nb_read_frames").parse().unwrap(),
        fps: n.parse::<f64>().unwrap() / d.parse::<f64>().unwrap(),
        duration: get("duration").parse().unwrap(),
        codec: get("codec_name"),
    }
}

/// Images décodées (`pix_fmt` : `gray` ou `rgb24`), toutes ou les `n` premières à partir de `ss` secondes.
pub fn read_frames(path: &Path, w: usize, h: usize, pix_fmt: &str, ss: Option<f64>, n: Option<usize>) -> Vec<Vec<u8>> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error"]);
    if let Some(s) = ss {
        cmd.args(["-ss", &s.to_string()]);
    }
    cmd.arg("-i").arg(path);
    if let Some(n) = n {
        cmd.args(["-frames:v", &n.to_string()]);
    }
    cmd.args(["-f", "rawvideo", "-pix_fmt", pix_fmt, "-"]);
    let out = cmd.output().expect("ffmpeg");
    assert!(out.status.success(), "ffmpeg : {}", String::from_utf8_lossy(&out.stderr));
    let size = w * h * if pix_fmt == "rgb24" { 3 } else { 1 };
    assert!(
        out.stdout.len().is_multiple_of(size),
        "taille de flux inattendue : {} octets pour des images de {size}",
        out.stdout.len()
    );
    out.stdout.chunks(size).map(<[u8]>::to_vec).collect()
}

/// Image fixe (JPEG, PNG) lue en RGB.
pub fn read_image(path: &Path) -> (Vec<u8>, usize, usize) {
    let v = probe_image_size(path);
    let img = read_frames(path, v.0, v.1, "rgb24", None, Some(1)).remove(0);
    (img, v.0, v.1)
}

fn probe_image_size(path: &Path) -> (usize, usize) {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height", "-of", "csv=p=0:s=x"])
        .arg(path)
        .output()
        .expect("ffprobe");
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (w, h) = text.split_once('x').unwrap_or_else(|| panic!("taille illisible : {text:?}"));
    (w.parse().unwrap(), h.parse().unwrap())
}

// ---------------------------------------------------------------------------
// Mesures
// ---------------------------------------------------------------------------
/// Angle (°) de la rotation `aᵀ b`.
pub fn angle_between(a: &Mat3, b: &Mat3) -> f64 {
    let m = a.transpose() * b;
    ((m.trace() - 1.0) / 2.0).clamp(-1.0, 1.0).acos().to_degrees()
}

/// Écart absolu moyen entre deux images de même taille.
pub fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).map(|(&x, &y)| (i32::from(x) - i32::from(y)).unsigned_abs() as f64).sum::<f64>() / a.len() as f64
}

/// Réduction par moyenne de blocs `k x k` d'une image RGB.
pub fn downscale_rgb(img: &[u8], w: usize, h: usize, k: usize) -> (Vec<u8>, usize, usize) {
    let (nw, nh) = (w / k, h / k);
    let mut out = vec![0u8; nw * nh * 3];
    for y in 0..nh {
        for x in 0..nw {
            for c in 0..3 {
                let mut s = 0u32;
                for dy in 0..k {
                    for dx in 0..k {
                        s += u32::from(img[((y * k + dy) * w + x * k + dx) * 3 + c]);
                    }
                }
                out[(y * nw + x) * 3 + c] = (s / (k * k) as u32) as u8;
            }
        }
    }
    (out, nw, nh)
}

fn fft(re: &mut [f64], im: &mut [f64], inverse: bool) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = 2.0 * std::f64::consts::PI / len as f64 * if inverse { 1.0 } else { -1.0 };
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0, 0.0);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let (tr, ti) = (re[b] * cr - im[b] * ci, re[b] * ci + im[b] * cr);
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let nr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = nr;
            }
        }
        len <<= 1;
    }
}

fn fft2(re: &mut [f64], im: &mut [f64], n: usize, inverse: bool) {
    for r in 0..n {
        fft(&mut re[r * n..(r + 1) * n], &mut im[r * n..(r + 1) * n], inverse);
    }
    let (mut cr, mut ci) = (vec![0.0; n], vec![0.0; n]);
    for c in 0..n {
        for r in 0..n {
            cr[r] = re[r * n + c];
            ci[r] = im[r * n + c];
        }
        fft(&mut cr, &mut ci, inverse);
        for r in 0..n {
            re[r * n + c] = cr[r];
            im[r * n + c] = ci[r];
        }
    }
}

/// Décalage `(dx, dy)` en pixels de `b` par rapport à `a` (corrélation de phase fenêtrée sur le carré central,
/// pic localisé au centroïde 5x5) — mesure indépendante du code testé.
pub fn phase_correlate(a: &[u8], b: &[u8], w: usize, h: usize) -> (f64, f64) {
    let n = (1usize << (usize::BITS - 1 - w.min(h).leading_zeros())).min(256);
    let (x0, y0) = ((w - n) / 2, (h - n) / 2);
    let prep = |img: &[u8]| {
        let mut re = vec![0.0; n * n];
        for y in 0..n {
            for x in 0..n {
                let hann = |i: usize| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * (i as f64 + 0.5) / n as f64).cos();
                re[y * n + x] = f64::from(img[(y0 + y) * w + x0 + x]) * hann(x) * hann(y);
            }
        }
        (re, vec![0.0; n * n])
    };
    let ((mut ar, mut ai), (mut br, mut bi)) = (prep(a), prep(b));
    fft2(&mut ar, &mut ai, n, false);
    fft2(&mut br, &mut bi, n, false);
    let (mut rr, mut ri) = (vec![0.0; n * n], vec![0.0; n * n]);
    for i in 0..n * n {
        let (cr, ci) = (ar[i] * br[i] + ai[i] * bi[i], ai[i] * br[i] - ar[i] * bi[i]); // A · conj(B)
        let m = cr.hypot(ci).max(1e-12);
        rr[i] = cr / m;
        ri[i] = ci / m;
    }
    fft2(&mut rr, &mut ri, n, true);
    let peak = (0..n * n).max_by(|&i, &j| rr[i].partial_cmp(&rr[j]).unwrap()).unwrap();
    let (py, px) = ((peak / n) as isize, (peak % n) as isize);
    let (mut sx, mut sy, mut sw) = (0.0, 0.0, 0.0);
    for dy in -2isize..=2 {
        for dx in -2isize..=2 {
            let v =
                rr[(py + dy).rem_euclid(n as isize) as usize * n + (px + dx).rem_euclid(n as isize) as usize].max(0.0);
            sx += v * dx as f64;
            sy += v * dy as f64;
            sw += v;
        }
    }
    let wrap = |p: isize, c: f64| {
        let v = p as f64 + c;
        if v > n as f64 / 2.0 {
            v - n as f64
        } else {
            v
        }
    };
    (-wrap(px, sx / sw), -wrap(py, sy / sw))
}

/// Tremblement : décalage moyen (px) entre images consécutives d'une vidéo.
pub fn mean_shift(path: &Path, w: usize, h: usize) -> f64 {
    let frames = read_frames(path, w, h, "gray", None, None);
    assert!(frames.len() > 2, "vidéo trop courte pour mesurer le tremblement");
    let shifts: Vec<f64> = frames
        .windows(2)
        .map(|p| {
            let (dx, dy) = phase_correlate(&p[0], &p[1], w, h);
            dx.hypot(dy)
        })
        .collect();
    shifts.iter().sum::<f64>() / shifts.len() as f64
}

// ---------------------------------------------------------------------------
// Vérité terrain géométrique
// ---------------------------------------------------------------------------
/// Direction (monde) vue par le pixel `(u, v)` ∈ [-1, 1]² (v vers le bas) d'une projection de sortie, formules de la
/// spécification (`docs/ARCHITECTURE.md`) : écrites ici indépendamment du moteur de rendu.
pub fn output_ray(proj: &str, u: f64, v: f64, fov_deg: f64, aspect: f64) -> Vec3 {
    let fov = fov_deg.to_radians();
    match proj {
        "flat" => {
            let t = (fov / 2.0).tan();
            Vec3::new(u * t, -v * t * aspect, 1.0).normalize()
        }
        "fisheye" => {
            let theta = (u * u + v * aspect * v * aspect).sqrt() * fov / 2.0;
            let phi = (-v * aspect).atan2(u);
            Vec3::new(theta.sin() * phi.cos(), theta.sin() * phi.sin(), theta.cos())
        }
        "planet" => {
            let (x, y) = (u * (fov / 4.0).tan(), v * (fov * aspect / 4.0).tan());
            let theta = 2.0 * x.hypot(y).atan();
            let phi = (-y).atan2(x);
            Vec3::new(theta.sin() * phi.cos(), theta.sin() * phi.sin(), theta.cos())
        }
        other => panic!("projection inconnue : {other}"),
    }
}

/// Couleur de la texture dans la direction `d` (bilinéaire, raccord en longitude).
pub fn texture_at(tex: &Texture, d: &Vec3) -> [f64; 3] {
    let lon = d.x.atan2(d.z);
    let lat = d.y.clamp(-1.0, 1.0).asin();
    let fx = (lon / (2.0 * std::f64::consts::PI) + 0.5) * tex.w as f64 - 0.5;
    let fy = ((0.5 - lat / std::f64::consts::PI) * tex.h as f64 - 0.5).clamp(0.0, tex.h as f64 - 1.0);
    let (x0, y0) = (fx.floor(), fy.floor());
    let (ax, ay) = (fx - x0, fy - y0);
    let xi = |x: f64| (x as i64).rem_euclid(tex.w as i64) as usize;
    let yi = |y: f64| (y as usize).min(tex.h - 1);
    let mut px = [0.0; 3];
    for (c, out) in px.iter_mut().enumerate() {
        let t = |x: f64, y: f64| f64::from(tex.rgb[(yi(y) * tex.w + xi(x)) * 3 + c]);
        *out = (t(x0, y0) * (1.0 - ax) + t(x0 + 1.0, y0) * ax) * (1.0 - ay)
            + (t(x0, y0 + 1.0) * (1.0 - ax) + t(x0 + 1.0, y0 + 1.0) * ax) * ay;
    }
    px
}

/// Ce que doit montrer une vue `proj` de `w x h` pixels (RGB) : la texture vue par une caméra d'orientation `cam`
/// (caméra -> monde) à travers la vue `(yaw, pitch, roll, fov)`.
pub fn expected_view(tex: &Texture, proj: &str, w: usize, h: usize, cam: &Mat3, view: (f64, f64, f64, f64)) -> Vec<u8> {
    let m = cam * ypr_to_matrix(view.0, view.1, view.2);
    let aspect = h as f64 / w as f64;
    let mut out = vec![0u8; w * h * 3];
    for j in 0..h {
        for i in 0..w {
            let (u, v) = ((i as f64 + 0.5) / w as f64 * 2.0 - 1.0, (j as f64 + 0.5) / h as f64 * 2.0 - 1.0);
            let c = texture_at(tex, &(m * output_ray(proj, u, v, view.3, aspect)));
            for k in 0..3 {
                out[(j * w + i) * 3 + k] = (c[k] + 0.5) as u8;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Dérivés de vidéos et inspection
// ---------------------------------------------------------------------------
/// Fichier dérivé d'un clip par ffmpeg (mis en cache) : `args` = tout ce qui suit `-i clip` jusqu'au fichier de
/// sortie exclu, p. ex. `["-map", "0:v:1", "-c", "copy", "-f", "mp4"]`. Le fichier est dans un sous-dossier propre
/// à `(clip, subdir)` : utile pour les noms imposés (`VID_..._00_001.insv`).
pub fn derive(clip: &Clip, subdir: &str, filename: &str, args: &[&str]) -> PathBuf {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let stem = clip.path.file_stem().unwrap().to_string_lossy().into_owned();
    let dir = cache_dir().join(format!("{stem}-{subdir}"));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join(filename);
    if !out.is_file() {
        let tmp = dir.join(format!("tmp-{}-{filename}", std::process::id()));
        let st = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-i"])
            .arg(&clip.path)
            .args(args)
            .arg(&tmp)
            .output()
            .expect("ffmpeg");
        assert!(st.status.success(), "dérivation {subdir}/{filename} : {}", String::from_utf8_lossy(&st.stderr));
        std::fs::rename(&tmp, &out).unwrap();
    }
    out
}

/// `(type, codec)` de chaque flux : `[("video", "h264"), ("audio", "aac")]`.
pub fn streams(path: &Path) -> Vec<(String, String)> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_type,codec_name", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("ffprobe");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once(',').map(|(c, t)| (t.to_string(), c.to_string())))
        .collect()
}

/// Étiquette de codec du flux vidéo (`avc1`, `hvc1`, …).
pub fn video_tag(path: &Path) -> String {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=codec_tag_string", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("ffprobe");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Couleur moyenne du carré de rayon `r` centré en `(x, y)` d'une image RGB de largeur `w`.
pub fn avg_rgb(img: &[u8], w: usize, x: usize, y: usize, r: usize) -> [f64; 3] {
    let mut s = [0.0; 3];
    let mut n = 0.0;
    for yy in y - r..=y + r {
        for xx in x - r..=x + r {
            for c in 0..3 {
                s[c] += f64::from(img[(yy * w + xx) * 3 + c]);
            }
            n += 1.0;
        }
    }
    s.map(|v| v / n)
}

/// Vrai si la couleur est « presque » `target` (écart max par canal).
pub fn near(c: [f64; 3], target: [f64; 3], tol: f64) -> bool {
    c.iter().zip(&target).all(|(a, b)| (a - b).abs() <= tol)
}

/// Processus dont la ligne de commande contient `needle` (Linux : /proc).
pub fn processes_mentioning(needle: &str) -> Vec<String> {
    let mut found = vec![];
    if let Ok(dir) = std::fs::read_dir("/proc") {
        for e in dir.flatten() {
            if e.file_name().to_string_lossy().chars().all(|c| c.is_ascii_digit()) {
                if let Ok(cmd) = std::fs::read(e.path().join("cmdline")) {
                    let text = String::from_utf8_lossy(&cmd).replace('\0', " ");
                    if text.contains(needle) {
                        found.push(text);
                    }
                }
            }
        }
    }
    found
}
