//! Construction et exécution des commandes FFmpeg + rendu (tout est traité localement).
//!
//! FFmpeg ne fait que décoder / encoder : `ffmpeg` (décodeur, deux flux fisheye -> I420 brut côte à
//! côte) → [`crate::render::LensRenderer`] (rotation, projection, fondu) → `ffmpeg` (encodeur, I420
//! brut → H.264/H.265 + audio). Spec : `x3studio/pipeline.py` (partie moteur `cv`, sans le moteur
//! v360) et `x3studio/render.py::render_stream`.

use crate::calibration::Calibration;
use crate::error::{export_err, media_err, Cancelled, Result};
use crate::geometry::{interpolate_view, Keyframe, Mat3, View, Ypr};
use crate::media::{Layout, Source};
use crate::render::{LensRenderer, Projection, SourceKind};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

/// Vue fixe (sans keyframe) : champs optionnels, les absents reprennent la valeur par défaut.
pub type ViewOverride = serde_json::Value;

/// Paramètres d'un export. JSON identique à celui de l'interface (`web/app.js::exportSettings`
/// + état du projet) ; champs inconnus ignorés.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportSettings {
    /// `reframe` (vidéo plate) | `equirect` (360°).
    pub mode: String,
    /// `flat` | `fisheye` | `planet` (mode `reframe`).
    pub projection: String,
    pub width: u32,
    pub height: u32,
    /// 0 = identique à la source.
    pub fps: f64,
    pub start: f64,
    /// 0 = jusqu'à la fin.
    pub end: f64,
    pub speed: f64,
    /// `h264` | `h265`.
    pub codec: String,
    pub crf: u32,
    pub preset: String,
    /// Largeur de l'équirectangulaire en mode `equirect` (0 = 2 x taille d'objectif).
    pub stitch_width: u32,
    pub audio: bool,
    pub keyframes: Vec<Keyframe>,
    /// Vue fixe si pas de keyframe (champs `yaw`, `pitch`, `roll`, `fov` optionnels).
    pub view: ViewOverride,
    /// Recentrage / horizon de l'utilisateur.
    pub orientation: Ypr,
    /// Balises « vidéo sphérique » dans le MP4 (mode `equirect`).
    pub spherical_metadata: bool,
}

impl Default for ExportSettings {
    fn default() -> Self {
        ExportSettings {
            mode: "reframe".into(),
            projection: "flat".into(),
            width: 1920,
            height: 1080,
            fps: 0.0,
            start: 0.0,
            end: 0.0,
            speed: 1.0,
            codec: "h264".into(),
            crf: 20,
            preset: "medium".into(),
            stitch_width: 0,
            audio: true,
            keyframes: vec![],
            view: serde_json::json!({}),
            orientation: Ypr::default(),
            spherical_metadata: true,
        }
    }
}

pub const MAX_FOV_FLAT: f64 = 170.0;
pub const MAX_FOV_FISHEYE: f64 = 360.0;
pub const MAX_FOV_PLANET: f64 = 350.0;

/// Côté maximal d'une image de sortie (px) : 8K (7680x4320, 8192x4096) tient ; au-delà, une seule image
/// dépasserait 100 Mo et le pipeline en garde une demi-douzaine en mémoire.
pub const MAX_OUTPUT_SIDE: u32 = 8192;
const PROJECTIONS: [&str; 3] = ["flat", "fisheye", "planet"];
const PRESETS: [&str; 10] =
    ["ultrafast", "superfast", "veryfast", "faster", "fast", "medium", "slow", "slower", "veryslow", "placebo"];

/// Champ de vision par défaut d'une projection (`flat` 90, `fisheye` 180, `planet` 250, `equirect` 0).
pub fn default_fov(projection: &str) -> f64 {
    match projection {
        "fisheye" => 180.0,
        "planet" => 250.0,
        "equirect" => 0.0,
        _ => 90.0,
    }
}

fn max_fov(projection: &str) -> f64 {
    match projection {
        "fisheye" => MAX_FOV_FISHEYE,
        "planet" => MAX_FOV_PLANET,
        "equirect" => 0.0,
        _ => MAX_FOV_FLAT,
    }
}

impl ExportSettings {
    /// Vérifie les paramètres (erreurs `ExportError` : valeurs inconnues, tailles impaires ou démesurées…).
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.mode.as_str(), "reframe" | "equirect") {
            return Err(export_err(format!("Mode inconnu : {}", self.mode)));
        }
        if self.mode == "reframe" {
            if !PROJECTIONS.contains(&self.projection.as_str()) {
                return Err(export_err(format!("Projection inconnue : {}", self.projection)));
            }
            if !self.width.is_multiple_of(2) || !self.height.is_multiple_of(2) {
                return Err(export_err("La largeur et la hauteur de sortie doivent être paires."));
            }
            if self.width == 0 || self.height == 0 || self.width > MAX_OUTPUT_SIDE || self.height > MAX_OUTPUT_SIDE {
                return Err(export_err(format!(
                    "Taille de sortie invalide : {}x{} (de 2 à {MAX_OUTPUT_SIDE} px de côté).",
                    self.width, self.height
                )));
            }
        }
        if !matches!(self.codec.as_str(), "h264" | "h265") {
            return Err(export_err(format!("Codec inconnu : {} (h264 ou h265)", self.codec)));
        }
        if !PRESETS.contains(&self.preset.as_str()) {
            return Err(export_err(format!("Préréglage x264/x265 inconnu : {}", self.preset)));
        }
        if self.crf > 51 {
            return Err(export_err("Le CRF doit être compris entre 0 et 51."));
        }
        if ![self.fps, self.start, self.end, self.speed].iter().all(|v| v.is_finite()) {
            return Err(export_err("Cadence, plage et vitesse doivent être des nombres."));
        }
        if self.fps < 0.0 || self.fps > 240.0 {
            return Err(export_err("La cadence de sortie doit être comprise entre 0 (source) et 240 i/s."));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanKind {
    Video,
    Image,
}

/// Tout ce qu'il faut pour exécuter un export (ou une capture).
pub struct Plan {
    pub kind: PlanKind,
    pub out_path: PathBuf,
    /// Durée de la sortie (s), pour la progression.
    pub out_duration: f64,
    /// FFmpeg décodeur : stdout = images I420 brutes (deux objectifs côte à côte, ou équirect).
    pub decoder: Vec<String>,
    /// FFmpeg encodeur : stdin = images I420 brutes de la taille de sortie.
    pub encoder: Vec<String>,
    pub renderer: LensRenderer,
    /// Une matrice de vue et un champ de vision par image à rendre.
    pub mats: Vec<Mat3>,
    pub fovs: Vec<f64>,
    /// Injecter les métadonnées sphériques après l'encodage.
    pub spherical: bool,
}

/// Argument protégé pour un affichage « copiable dans un terminal » (comme `shlex.quote`).
fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty() && arg.chars().all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c));
    if safe {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\"'\"'"))
    }
}

fn shell_line(cmd: &[String]) -> String {
    cmd.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
}

impl Plan {
    /// Ligne de commande lisible (pour `--dry-run`).
    pub fn shell(&self) -> String {
        format!("{}  |  <rendu x3studio>  |  {}", shell_line(&self.decoder), shell_line(&self.encoder))
    }
}

// ---------------------------------------------------------------------------
// Paramètres de vue
// ---------------------------------------------------------------------------
fn projection_of(s: &ExportSettings) -> &str {
    if s.mode == "reframe" {
        &s.projection
    } else {
        "equirect"
    }
}

/// Vue de base d'un `ExportSettings` (`view` JSON + champ par défaut de la projection).
pub fn base_view(s: &ExportSettings) -> View {
    let get =
        |k: &str, d: f64| s.view.get(k).and_then(serde_json::Value::as_f64).filter(|v| v.is_finite()).unwrap_or(d);
    View {
        yaw: get("yaw", 0.0),
        pitch: get("pitch", 0.0),
        roll: get("roll", 0.0),
        fov: get("fov", default_fov(projection_of(s))),
    }
}

/// Matrice de vue et champ de vision par image (spec : `view_params` de pipeline.py). `times_abs` :
/// instants dans la source ; `stab` : une matrice par image *source* (orientation utilisateur
/// incluse) sinon seule l'orientation de `s.orientation` s'applique.
pub fn view_params(
    src: &Source,
    s: &ExportSettings,
    times_abs: &[f64],
    stab: Option<&[Mat3]>,
) -> (Vec<Mat3>, Vec<f64>) {
    let o_inv = s.orientation.matrix().transpose();
    let proj = projection_of(s);
    let base = base_view(s);
    let stab = stab.filter(|m| !m.is_empty());
    let mut mats = Vec::with_capacity(times_abs.len());
    let mut fovs = Vec::with_capacity(times_abs.len());
    for &t in times_abs {
        let stabilized = match stab {
            Some(m) => m[((t * src.fps).round_ties_even().max(0.0) as usize).min(m.len() - 1)],
            None => o_inv,
        };
        if s.mode == "reframe" {
            let v = interpolate_view(&s.keyframes, t, &base);
            mats.push(stabilized * v.matrix());
            fovs.push(v.fov.clamp(5.0, max_fov(proj)));
        } else {
            mats.push(stabilized);
            fovs.push(0.0);
        }
    }
    (mats, fovs)
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------
fn clip_range(src: &Source, s: &ExportSettings) -> Result<(f64, f64)> {
    let start = s.start.max(0.0);
    if start >= src.duration - 0.05 {
        return Err(export_err("Le début de la plage dépasse la durée de la vidéo."));
    }
    let end = if s.end > start { s.end } else { src.duration }.min(src.duration);
    if end - start < 0.05 {
        return Err(export_err("Plage de lecture vide."));
    }
    Ok((start, end))
}

/// (cadence de traitement, cadence de sortie).
fn process_fps(src: &Source, s: &ExportSettings) -> (f64, f64) {
    let ofps = if s.fps > 0.0 { s.fps } else { src.fps };
    let mut pfps = src.fps;
    if s.speed >= 1.0 && ofps * s.speed < src.fps - 0.01 {
        pfps = ofps * s.speed;
    }
    (pfps, ofps)
}

/// Taille de l'image équirectangulaire produite (mode `equirect`).
pub fn stitch_size(src: &Source, s: &ExportSettings) -> (u32, u32) {
    let w = if s.stitch_width > 0 {
        s.stitch_width
    } else if src.layout == Layout::Equirect {
        src.lens_size.0
    } else {
        2 * src.lens_size.0
    };
    let w = w.clamp(256, 8192) / 2 * 2;
    (w, w / 2)
}

fn output_size(src: &Source, s: &ExportSettings) -> (u32, u32) {
    if s.mode == "reframe" {
        (s.width, s.height)
    } else {
        stitch_size(src, s)
    }
}

/// Enchaîne `atempo` (chaque filtre accepte de 0,5 à 2).
fn atempo_chain(speed: f64) -> String {
    let mut parts = vec![];
    let mut s = speed;
    while s > 2.0 {
        parts.push("atempo=2.0".to_string());
        s /= 2.0;
    }
    while s < 0.5 {
        parts.push("atempo=0.5".to_string());
        s /= 0.5;
    }
    parts.push(format!("atempo={s:.5}"));
    parts.join(",")
}

fn encoder_args(s: &ExportSettings) -> Vec<String> {
    let v = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let crf = s.crf.to_string();
    if s.codec == "h265" {
        v(&[
            "-c:v",
            "libx265",
            "-preset",
            &s.preset,
            "-crf",
            &crf,
            "-tag:v",
            "hvc1",
            "-pix_fmt",
            "yuv420p",
            "-x265-params",
            "log-level=error",
        ])
    } else {
        v(&["-c:v", "libx264", "-preset", &s.preset, "-crf", &crf, "-pix_fmt", "yuv420p"])
    }
}

fn stream_rank(src: &Source, i: usize) -> Result<usize> {
    src.lens_streams.get(i).copied().ok_or_else(|| media_err("Flux vidéo introuvable dans ce fichier."))
}

/// Entrées FFmpeg du décodeur (`-ss`/`-t` par fichier). Le décodage commence une demi-image avant `start` :
/// `-ss` écarte toute image dont l'horodatage est inférieur (la 44e image d'une vidéo à 30 i/s, à 1,466666 s,
/// serait perdue par `-ss 1.4667`) ; on retient ainsi l'image la plus proche de `start`.
fn input_args(src: &Source, start: f64, dur: f64) -> Vec<String> {
    let margin = 0.5 / src.fps.max(1.0);
    let ss = (start - margin).max(0.0);
    let t = dur + (start - ss) + margin;
    let files = if src.layout == Layout::TwoFiles {
        &src.lens_files[..]
    } else {
        &src.lens_files[..1.min(src.lens_files.len())]
    };
    let mut args = vec![];
    for f in files {
        args.extend([
            "-ss".to_string(),
            format!("{ss:.4}"),
            "-t".to_string(),
            format!("{t:.4}"),
            "-i".to_string(),
            f.clone(),
        ]);
    }
    args
}

/// Graphe du décodeur : images I420 `[flux 0 | flux 1]` (ou équirectangulaires déjà assemblées). L'échange
/// éventuel des objectifs (`swap_lenses`) est fait par le moteur de rendu, pas ici.
fn decoder_graph(src: &Source, fps_filter: &str) -> Result<String> {
    let (a, f) = (stream_rank(src, 0)?, fps_filter);
    Ok(match src.layout {
        Layout::TwoStreams => {
            let b = stream_rank(src, 1)?;
            format!("[0:v:{a}]{f}format=yuv420p[fa];[0:v:{b}]{f}format=yuv420p[fb];[fa][fb]hstack=inputs=2[v]")
        }
        Layout::TwoFiles => {
            let b = stream_rank(src, 1)?;
            format!("[0:v:{a}]{f}format=yuv420p[fa];[1:v:{b}]{f}format=yuv420p[fb];[fa][fb]hstack=inputs=2[v]")
        }
        Layout::SbsFisheye | Layout::Equirect => format!("[0:v:{a}]{f}format=yuv420p[v]"),
    })
}

fn source_kind(src: &Source) -> SourceKind {
    match src.layout {
        Layout::Equirect => SourceKind::Equirect { w: src.lens_size.0, h: src.lens_size.1 },
        _ => SourceKind::Dual { lens: src.lens_size.0 },
    }
}

fn make_renderer(src: &Source, calib: &Calibration, s: &ExportSettings) -> Result<LensRenderer> {
    let (ow, oh) = output_size(src, s);
    let proj = if s.mode == "reframe" { s.projection.parse::<Projection>()? } else { Projection::Equirect };
    let div = if u64::from(ow) * u64::from(oh) <= 4_200_000 { 4 } else { 8 };
    LensRenderer::new(calib, source_kind(src), ow, oh, proj, div)
}

fn base_command(overwrite: bool) -> Vec<String> {
    let mut c = vec!["ffmpeg".to_string(), "-hide_banner".into(), "-nostdin".into()];
    if overwrite {
        c.push("-y".into());
    }
    c.extend(["-v".to_string(), "error".into()]);
    c
}

/// Plan d'un export vidéo. Erreurs `ExportError` : mode/projection inconnus, taille impaire, plage vide.
pub fn build_plan(
    src: &Source,
    calib: &Calibration,
    s: &ExportSettings,
    out: &Path,
    stab: Option<&[Mat3]>,
) -> Result<Plan> {
    s.validate()?;
    let (start, end) = clip_range(src, s)?;
    let dur = end - start;
    let (pfps, ofps) = process_fps(src, s);
    let speed = s.speed.clamp(0.05, 100.0);
    let n_proc = ((dur * pfps).round_ties_even() as usize).max(1);
    let times: Vec<f64> = (0..n_proc).map(|i| start + i as f64 / pfps).collect();
    let (mats, fovs) = view_params(src, s, &times, stab);
    let out_dur = dur / speed;
    let spherical = s.mode == "equirect" && s.spherical_metadata;
    let renderer = make_renderer(src, calib, s)?;
    let (ow, oh) = output_size(src, s);

    let fps_f = if (pfps - src.fps).abs() > 0.01 { format!("fps={pfps:.5},") } else { String::new() };
    let mut decoder = base_command(false);
    decoder.extend(input_args(src, start, dur));
    decoder.extend(["-filter_complex".into(), decoder_graph(src, &fps_f)?]);
    decoder.extend(["-map", "[v]", "-an", "-f", "rawvideo", "-pix_fmt", "yuv420p", "-"].map(String::from));

    let mut post = vec![];
    if (speed - 1.0).abs() > 1e-3 {
        post.push(format!("setpts=PTS/{speed:.5}"));
    }
    if (ofps - pfps * speed).abs() > 0.01 {
        post.push(format!("fps={ofps:.5}"));
    }
    post.push("format=yuv420p".into());

    let mut encoder = base_command(true);
    encoder.extend(["-f", "rawvideo", "-pix_fmt", "yuv420p", "-s"].map(String::from));
    encoder.extend([format!("{ow}x{oh}"), "-framerate".into(), format!("{pfps:.5}"), "-i".into(), "-".into()]);
    let use_audio = s.audio && src.has_audio;
    if use_audio {
        let audio = src.audio_file.clone().or_else(|| src.lens_files.first().cloned()).unwrap_or_default();
        encoder.extend([
            "-ss".to_string(),
            format!("{start:.4}"),
            "-t".into(),
            format!("{dur:.4}"),
            "-i".into(),
            audio,
        ]);
    }
    encoder.extend([
        "-filter_complex".to_string(),
        format!("[0:v]{}[vout]", post.join(",")),
        "-map".into(),
        "[vout]".into(),
    ]);
    if use_audio {
        encoder.extend(["-map".to_string(), "1:a:0?".into()]);
        if (speed - 1.0).abs() > 1e-3 {
            encoder.extend(["-af".to_string(), atempo_chain(speed)]);
        }
        encoder.extend(["-c:a", "aac", "-b:a", "192k"].map(String::from));
    } else {
        encoder.push("-an".into());
    }
    encoder.extend(encoder_args(s));
    encoder.extend(["-t".to_string(), format!("{out_dur:.4}")]);
    if !spherical {
        encoder.extend(["-movflags", "+faststart"].map(String::from));
    }
    encoder.push(out.to_string_lossy().into_owned());
    Ok(Plan {
        kind: PlanKind::Video,
        out_path: out.to_path_buf(),
        out_duration: out_dur,
        decoder,
        encoder,
        renderer,
        mats,
        fovs,
        spherical,
    })
}

/// Plan d'une image fixe (`.jpg`/`.png`) à l'instant `t` : un seul rendu, encodé par FFmpeg.
pub fn build_snapshot_plan(
    src: &Source,
    calib: &Calibration,
    s: &ExportSettings,
    t: f64,
    out: &Path,
    stab: Option<&[Mat3]>,
) -> Result<Plan> {
    s.validate()?;
    if !t.is_finite() {
        return Err(export_err("Instant de capture invalide."));
    }
    let t = t.clamp(0.0, (src.duration - 1.0 / src.fps.max(1.0)).max(0.0));
    let end = src.duration.min(t + 1.0);
    let s2 = ExportSettings { start: t, end, speed: 1.0, ..s.clone() };
    let (mats, fovs) = view_params(src, &s2, &[t], stab);
    let renderer = make_renderer(src, calib, &s2)?;
    let (ow, oh) = output_size(src, &s2);

    let mut decoder = base_command(false);
    decoder.extend(input_args(src, t, (end - t).max(0.05)));
    decoder.extend(["-filter_complex".into(), decoder_graph(src, "")?]);
    decoder.extend(
        ["-map", "[v]", "-an", "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "yuv420p", "-"].map(String::from),
    );

    let ext = out.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let is_jpg = ext == "jpg" || ext == "jpeg";
    let mut encoder = base_command(true);
    encoder.extend(["-f", "rawvideo", "-pix_fmt", "yuv420p", "-s"].map(String::from));
    encoder.extend([format!("{ow}x{oh}"), "-framerate".into(), "1".into(), "-i".into(), "-".into()]);
    encoder.extend([
        "-filter_complex".to_string(),
        format!("[0:v]format={}[vout]", if is_jpg { "yuvj420p" } else { "rgb24" }),
        "-map".into(),
        "[vout]".into(),
        "-an".into(),
        "-frames:v".into(),
        "1".into(),
    ]);
    if is_jpg {
        encoder.extend(["-q:v", "2"].map(String::from));
    }
    encoder.push(out.to_string_lossy().into_owned());
    Ok(Plan {
        kind: PlanKind::Image,
        out_path: out.to_path_buf(),
        out_duration: 1.0,
        decoder,
        encoder,
        renderer,
        mats,
        fovs,
        spherical: false,
    })
}

// ---------------------------------------------------------------------------
// Aperçu (proxy basse résolution pour le lecteur web) et analyse
// ---------------------------------------------------------------------------
/// Proxy d'aperçu WebM (VP9) : deux fisheye `lens_px` côte à côte, ou équirect `2*lens_px x lens_px`.
/// La sortie `-progress pipe:1` permet de suivre l'avancement avec [`run_ffmpeg`].
pub fn build_proxy_cmd(src: &Source, out: &Path, lens_px: u32) -> Vec<String> {
    let mut cmd = base_command(true);
    cmd.extend(["-progress", "pipe:1", "-nostats"].map(String::from));
    let sc = format!("scale={lens_px}:{lens_px}:flags=bilinear");
    let rank = |i: usize| src.lens_streams.get(i).copied().unwrap_or(0);
    let file = |i: usize| src.lens_files.get(i).or(src.lens_files.first()).cloned().unwrap_or_default();
    let graph = match src.layout {
        Layout::TwoFiles => {
            cmd.extend(["-i".to_string(), file(0), "-i".into(), file(1)]);
            format!("[0:v:{}]{sc}[a];[1:v:{}]{sc}[b];[a][b]hstack=inputs=2,format=yuv420p[v]", rank(0), rank(1))
        }
        Layout::TwoStreams => {
            cmd.extend(["-i".to_string(), file(0)]);
            format!("[0:v:{}]{sc}[a];[0:v:{}]{sc}[b];[a][b]hstack=inputs=2,format=yuv420p[v]", rank(0), rank(1))
        }
        Layout::SbsFisheye | Layout::Equirect => {
            cmd.extend(["-i".to_string(), file(0)]);
            format!("[0:v:{}]scale={}:{lens_px}:flags=bilinear,format=yuv420p[v]", rank(0), 2 * lens_px)
        }
    };
    cmd.extend(["-filter_complex".to_string(), graph, "-map".into(), "[v]".into()]);
    if src.has_audio {
        cmd.extend(["-map", "0:a:0?", "-c:a", "libopus", "-b:a", "96k"].map(String::from));
    } else {
        cmd.push("-an".into());
    }
    cmd.extend(
        [
            "-c:v",
            "libvpx-vp9",
            "-b:v",
            "0",
            "-crf",
            "36",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-row-mt",
            "1",
            "-tile-columns",
            "2",
            "-g",
            "15",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "webm",
        ]
        .map(String::from),
    );
    cmd.push(out.to_string_lossy().into_owned());
    cmd
}

/// Décodage du proxy pour l'analyse du mouvement.
pub struct AnalysisDecoder {
    pub cmd: Vec<String>,
    /// Vrai : images I420 `frame_w x frame_h` à deux objectifs côte à côte (à assembler) ;
    /// faux : images équirectangulaires grises 960x480 déjà prêtes.
    pub dual: bool,
    pub frame_w: u32,
    pub frame_h: u32,
}

/// Taille d'un objectif dans les images d'analyse (le proxy est réduit à cette taille).
const ANALYSIS_LENS_PX: u32 = 640;

pub fn analysis_decoder(src: &Source, proxy: &Path) -> AnalysisDecoder {
    let mut cmd = base_command(false);
    cmd.extend(["-i".to_string(), proxy.to_string_lossy().into_owned()]);
    let (dual, w, h, graph, pix) = if src.layout == Layout::Equirect {
        (false, 960, 480, "[0:v:0]scale=960:480:flags=bilinear,format=gray[v]".to_string(), "gray")
    } else {
        let l = ANALYSIS_LENS_PX;
        (true, 2 * l, l, format!("[0:v:0]scale={}:{l}:flags=bilinear,format=yuv420p[v]", 2 * l), "yuv420p")
    };
    cmd.extend(["-filter_complex".to_string(), graph, "-map".into(), "[v]".into(), "-an".into()]);
    cmd.extend(["-f", "rawvideo", "-pix_fmt", pix, "-"].map(String::from));
    AnalysisDecoder { cmd, dual, frame_w: w, frame_h: h }
}

// ---------------------------------------------------------------------------
// Processus enfants
// ---------------------------------------------------------------------------
/// Dernières lignes d'erreur d'un processus (lues dans un thread : sans cela le processus se bloque
/// quand le tube est plein).
struct StderrTail {
    handle: Option<JoinHandle<VecDeque<String>>>,
}

impl StderrTail {
    const KEEP: usize = 40;

    fn start(stderr: ChildStderr) -> StderrTail {
        let handle = std::thread::spawn(move || {
            let mut tail = VecDeque::with_capacity(Self::KEEP + 1);
            let mut reader = BufReader::new(stderr);
            let mut line = Vec::new();
            loop {
                line.clear();
                // une ligne démesurée (sortie binaire) est tronquée à 64 Kio
                match (&mut reader).take(65536).read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        tail.push_back(String::from_utf8_lossy(&line).trim_end().to_string());
                        if tail.len() > Self::KEEP {
                            tail.pop_front();
                        }
                    }
                }
            }
            tail
        });
        StderrTail { handle: Some(handle) }
    }

    /// À appeler une fois le processus terminé (le tube est alors fermé).
    fn finish(mut self) -> String {
        self.handle
            .take()
            .and_then(|h| h.join().ok())
            .map(|t| t.into_iter().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default()
    }
}

/// Processus enfant tué (et attendu) à la destruction : jamais d'orphelin, quel que soit le chemin de sortie.
pub(crate) struct Proc {
    child: Child,
    tail: Option<StderrTail>,
}

impl Proc {
    /// Lance `cmd[0]` avec ses arguments (jamais de shell). stderr est toujours capturé.
    pub(crate) fn spawn(cmd: &[String], stdin: Stdio, stdout: Stdio) -> Result<Proc> {
        let (program, args) = cmd.split_first().ok_or_else(|| export_err("Commande vide."))?;
        let mut child =
            Command::new(program).args(args).stdin(stdin).stdout(stdout).stderr(Stdio::piped()).spawn().map_err(
                |e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        export_err(format!("{program} est introuvable : installez-le (sudo apt install ffmpeg)."))
                    } else {
                        export_err(format!("Impossible de lancer {program} : {e}"))
                    }
                },
            )?;
        let tail = child.stderr.take().map(StderrTail::start);
        Ok(Proc { child, tail })
    }

    pub(crate) fn kill(&mut self) {
        let _ = self.child.kill();
    }

    /// Attend la fin du processus : (réussite, dernières lignes de stderr).
    pub(crate) fn wait(&mut self) -> (bool, String) {
        let ok = self.child.wait().map(|s| s.success()).unwrap_or(false);
        (ok, self.tail.take().map(StderrTail::finish).unwrap_or_default())
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Lit exactement `buf.len()` octets ; `Ok(false)` si le flux se termine avant (image incomplète ignorée).
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<bool> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) => return Ok(false),
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// Exécute FFmpeg avec `-progress pipe:1` : progression 0..1 d'après `out_time_us` / `total_seconds`,
/// annulation par `cancel` (processus tué). Erreur `ExportError` avec la fin de stderr si échec.
pub fn run_ffmpeg(cmd: &[String], total_seconds: f64, on_progress: &dyn Fn(f64), cancel: &AtomicBool) -> Result<()> {
    let mut proc = Proc::spawn(cmd, Stdio::null(), Stdio::piped())?;
    let stdout = proc.child.stdout.take().ok_or_else(|| export_err("Sortie de ffmpeg indisponible."))?;
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            line.clear();
            match (&mut r).take(65536).read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(String::from_utf8_lossy(&line).trim().to_string()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut info: HashMap<String, String> = HashMap::new();
    let mut cancelled = false;
    loop {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            proc.kill();
            break;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                let (k, v) = line.split_once('=').unwrap_or((line.as_str(), ""));
                info.insert(k.to_string(), v.to_string());
                if k == "progress" {
                    let us = info.get("out_time_us").filter(|v| !v.is_empty()).or(info.get("out_time_ms"));
                    let sec = us.and_then(|v| v.parse::<i64>().ok()).map_or(0.0, |v| (v as f64 / 1e6).max(0.0));
                    on_progress(if total_seconds > 0.0 { (sec / total_seconds).min(1.0) } else { 0.0 });
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let (ok, tail) = proc.wait();
    let _ = reader.join();
    if cancelled || cancel.load(Ordering::Relaxed) {
        return Err(Cancelled.into());
    }
    if !ok {
        return Err(export_err(format!("FFmpeg a échoué :\n{tail}")));
    }
    on_progress(1.0);
    Ok(())
}

/// Nombre d'images lues d'avance / rendues en attente d'écriture.
const PIPELINE_DEPTH: usize = 4;

/// Décodeur → rendu → encodeur. Les trois étapes s'exécutent en parallèle (lecture et écriture dans
/// leurs propres threads, rendu parallélisé par `rayon`).
fn render_stream(plan: &Plan, on_progress: &(dyn Fn(f64) + Sync), cancel: &AtomicBool) -> Result<()> {
    let n = plan.mats.len().min(plan.fovs.len());
    if n == 0 {
        return Err(export_err("Aucune image à produire."));
    }
    let (n_in, n_out) = (plan.renderer.frame_bytes_in(), plan.renderer.frame_bytes_out());
    let mut dec = Proc::spawn(&plan.decoder, Stdio::null(), Stdio::piped())?;
    let mut enc = Proc::spawn(&plan.encoder, Stdio::piped(), Stdio::null())?;
    let mut dec_out = dec.child.stdout.take().ok_or_else(|| export_err("Sortie du décodeur indisponible."))?;
    let mut enc_in = enc.child.stdin.take().ok_or_else(|| export_err("Entrée de l'encodeur indisponible."))?;

    let (outcome, done, write_res) = std::thread::scope(|scope| {
        let (full_tx, full_rx) = mpsc::sync_channel::<Vec<u8>>(PIPELINE_DEPTH);
        let (free_tx, free_rx) = mpsc::channel::<Vec<u8>>();
        let (out_tx, out_rx) = mpsc::sync_channel::<Vec<u8>>(PIPELINE_DEPTH);
        let (out_free_tx, out_free_rx) = mpsc::channel::<Vec<u8>>();
        let reader = scope.spawn(move || {
            for _ in 0..n {
                let mut buf = free_rx.try_recv().unwrap_or_else(|_| vec![0u8; n_in]);
                if !matches!(read_full(&mut dec_out, &mut buf), Ok(true)) || full_tx.send(buf).is_err() {
                    break;
                }
            }
        });
        let writer = scope.spawn(move || -> std::io::Result<()> {
            for frame in out_rx {
                enc_in.write_all(&frame)?;
                let _ = out_free_tx.send(frame);
            }
            enc_in.flush() // `enc_in` est fermé à la sortie : l'encodeur voit la fin du flux
        });
        let mut done = 0usize;
        let mut outcome: Result<()> = Ok(());
        'frames: while done < n {
            let frame = loop {
                if cancel.load(Ordering::Relaxed) {
                    outcome = Err(Cancelled.into());
                    break 'frames;
                }
                match full_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(f) => break f,
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break 'frames,
                }
            };
            let mut out = out_free_rx.try_recv().unwrap_or_else(|_| vec![0u8; n_out]);
            plan.renderer.render(&frame, &plan.mats[done], plan.fovs[done], &mut out);
            let _ = free_tx.send(frame);
            if out_tx.send(out).is_err() {
                break; // le thread d'écriture s'est arrêté : l'encodeur est mort
            }
            done += 1;
            on_progress(done as f64 / n as f64);
        }
        drop(full_rx); // débloque le lecteur s'il attend de la place
        dec.kill(); // plus rien à lire (fin normale, annulation ou erreur)
        if outcome.is_err() {
            enc.kill();
        }
        drop(out_tx);
        let _ = reader.join();
        (outcome, done, writer.join())
    });
    let (enc_ok, enc_err) = enc.wait();
    let (_, dec_err) = dec.wait();
    outcome?;
    if !matches!(write_res, Ok(Ok(()))) {
        return Err(export_err(format!("L'encodeur s'est arrêté :\n{enc_err}")));
    }
    if done == 0 {
        return Err(export_err(format!("Aucune image décodée :\n{dec_err}")));
    }
    if !enc_ok {
        return Err(export_err(format!("FFmpeg (encodage) a échoué :\n{enc_err}")));
    }
    on_progress(1.0);
    Ok(())
}

/// Exécute le plan (décodage → rendu parallèle → encodage), supprime la sortie partielle en cas
/// d'échec ou d'annulation (`Cancelled`), injecte les métadonnées sphériques si demandé.
pub fn run_plan(plan: Plan, on_progress: &(dyn Fn(f64) + Sync), cancel: &AtomicBool) -> Result<PathBuf> {
    let result = render_stream(&plan, on_progress, cancel).and_then(|()| {
        if plan.spherical {
            crate::spherical::inject_spherical_v1(&plan.out_path)?;
        }
        Ok(())
    });
    match result {
        Ok(()) => Ok(plan.out_path),
        Err(e) => {
            let _ = std::fs::remove_file(&plan.out_path);
            // Ctrl+C atteint aussi les processus enfants : leur arrêt n'est pas une erreur de l'utilisateur
            Err(if cancel.load(Ordering::Relaxed) { Cancelled.into() } else { e })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{is_cancelled, is_user_error};
    use serde_json::json;
    use std::cell::RefCell;

    fn x3(duration: f64, fps: f64) -> Source {
        Source {
            path: "/v/VID_00_001.insv".into(),
            layout: Layout::TwoStreams,
            lens_files: vec!["/v/VID_00_001.insv".into(), "/v/VID_00_001.insv".into()],
            lens_streams: vec![0, 1],
            lens_size: (2880, 2880),
            fps,
            duration,
            codec: "h264".into(),
            has_audio: true,
            audio_file: None,
            has_trailer: true,
            size_bytes: 1,
            camera: "Insta360 X3".into(),
            notes: vec![],
        }
    }

    fn arg_after<'a>(cmd: &'a [String], flag: &str) -> Option<&'a str> {
        cmd.iter().position(|a| a == flag).and_then(|i| cmd.get(i + 1)).map(String::as_str)
    }

    /// Valeur après la *dernière* occurrence de l'option (ex. `-t` d'entrée audio puis `-t` de sortie).
    fn last_arg_after<'a>(cmd: &'a [String], flag: &str) -> Option<&'a str> {
        cmd.iter().rposition(|a| a == flag).and_then(|i| cmd.get(i + 1)).map(String::as_str)
    }

    fn close(a: &Mat3, b: &Mat3) -> bool {
        (a - b).abs().max() < 1e-9
    }

    #[test]
    fn fov_tables_and_base_view() {
        assert_eq!(
            (default_fov("flat"), default_fov("fisheye"), default_fov("planet"), default_fov("equirect")),
            (90.0, 180.0, 250.0, 0.0)
        );
        assert_eq!(
            (max_fov("flat"), max_fov("fisheye"), max_fov("planet")),
            (MAX_FOV_FLAT, MAX_FOV_FISHEYE, MAX_FOV_PLANET)
        );
        let s = ExportSettings {
            projection: "fisheye".into(),
            view: json!({"yaw": 12, "fov": "large", "pitch": null}),
            ..Default::default()
        };
        let v = base_view(&s);
        assert_eq!((v.yaw, v.pitch, v.fov), (12.0, 0.0, 180.0), "valeurs non numériques ignorées");
        assert_eq!(base_view(&ExportSettings { mode: "equirect".into(), ..Default::default() }).fov, 0.0);
    }

    #[test]
    fn view_params_follow_keyframes_stabilization_and_orientation() {
        let src = x3(10.0, 30.0);
        let s = ExportSettings {
            keyframes: vec![
                Keyframe { t: 1.0, yaw: Some(0.0), pitch: None, roll: None, fov: Some(60.0) },
                Keyframe { t: 3.0, yaw: Some(40.0), pitch: None, roll: None, fov: Some(400.0) },
            ],
            orientation: Ypr { yaw: 10.0, pitch: 0.0, roll: 0.0 },
            ..Default::default()
        };
        let (m, f) = view_params(&src, &s, &[0.0, 2.0, 5.0], None);
        let o_inv = s.orientation.matrix().transpose();
        assert!(close(&m[0], &(o_inv * crate::geometry::ypr_to_matrix(0.0, 0.0, 0.0))));
        assert!(close(&m[2], &(o_inv * crate::geometry::ypr_to_matrix(40.0, 0.0, 0.0))));
        assert!(close(&m[1], &(o_inv * crate::geometry::ypr_to_matrix(20.0, 0.0, 0.0))), "milieu de l'interpolation");
        // champ : 60 avant le premier keyframe, 230 au milieu (smoothstep) puis 400 après, tous bornés à 170°
        assert_eq!((f[0], f[1], f[2]), (60.0, 170.0, 170.0));
        // stabilisation : une matrice par image source, indexée par round(t * fps) (arrondi pair)
        let stab: Vec<Mat3> = (0..400).map(|i| crate::geometry::ypr_to_matrix(i as f64 * 0.1, 0.0, 0.0)).collect();
        let plain = ExportSettings::default();
        let (m, _) = view_params(&src, &plain, &[0.5 / 30.0, 1.5 / 30.0, 2.0, 99.0], Some(&stab));
        assert!(close(&m[0], &stab[0]) && close(&m[1], &stab[2]), "0,5 → 0 et 1,5 → 2 (arrondi au pair)");
        assert!(close(&m[2], &stab[60]) && close(&m[3], &stab[399]), "borné à la dernière matrice");
        // mode équirectangulaire : pas de vue, seulement la stabilisation
        let eq = ExportSettings { mode: "equirect".into(), ..Default::default() };
        let (m, f) = view_params(&src, &eq, &[1.0], Some(&stab));
        assert!(close(&m[0], &stab[30]) && f[0] == 0.0);
        // tableau vide = pas de stabilisation
        let (m, _) = view_params(&src, &plain, &[1.0], Some(&[]));
        assert!(close(&m[0], &Mat3::identity()));
    }

    #[test]
    fn settings_are_validated() {
        let ok = ExportSettings::default();
        assert!(ok.validate().is_ok());
        for (s, needle) in [
            (ExportSettings { mode: "cube".into(), ..Default::default() }, "Mode inconnu"),
            (ExportSettings { projection: "cube".into(), ..Default::default() }, "Projection inconnue"),
            (ExportSettings { projection: "equirect".into(), ..Default::default() }, "Projection inconnue"),
            (ExportSettings { width: 1921, ..Default::default() }, "paires"),
            (ExportSettings { height: 0, ..Default::default() }, "Taille de sortie"),
            (ExportSettings { width: 20000, ..Default::default() }, "Taille de sortie"),
            (ExportSettings { codec: "vp9".into(), ..Default::default() }, "Codec inconnu"),
            (ExportSettings { preset: "-f".into(), ..Default::default() }, "Préréglage"),
            (ExportSettings { crf: 52, ..Default::default() }, "CRF"),
            (ExportSettings { fps: f64::NAN, ..Default::default() }, "nombres"),
            (ExportSettings { fps: 1000.0, ..Default::default() }, "cadence"),
            (ExportSettings { speed: f64::INFINITY, ..Default::default() }, "nombres"),
        ] {
            let e = s.validate().unwrap_err();
            assert!(is_user_error(&e) && e.to_string().contains(needle), "{needle} : {e}");
        }
        // une largeur impaire n'a pas d'importance en mode 360°
        assert!(ExportSettings { mode: "equirect".into(), width: 1921, ..Default::default() }.validate().is_ok());
        let e: ExportSettings =
            serde_json::from_value(json!({"width": 640, "inconnu": 1, "keyframes": [{"t": 1, "yaw": 5}]})).unwrap();
        assert_eq!((e.width, e.height, e.keyframes.len()), (640, 1080, 1));
    }

    #[test]
    fn range_fps_and_sizes() {
        let src = x3(10.0, 30.0);
        let r = |s: ExportSettings| clip_range(&src, &s);
        assert_eq!(r(ExportSettings::default()).unwrap(), (0.0, 10.0));
        assert_eq!(r(ExportSettings { start: 2.0, end: 4.5, ..Default::default() }).unwrap(), (2.0, 4.5));
        assert_eq!(
            r(ExportSettings { start: 2.0, end: 1.0, ..Default::default() }).unwrap(),
            (2.0, 10.0),
            "fin invalide = jusqu'à la fin"
        );
        assert_eq!(r(ExportSettings { start: -3.0, end: 99.0, ..Default::default() }).unwrap(), (0.0, 10.0));
        assert!(r(ExportSettings { start: 9.99, ..Default::default() }).unwrap_err().to_string().contains("dépasse"));
        assert!(r(ExportSettings { start: 5.0, end: 5.01, ..Default::default() })
            .unwrap_err()
            .to_string()
            .contains("vide"));
        // 60 i/s demandés sur une source à 30 : cadence de traitement = source ; 10 i/s à vitesse 4 : 40 i/s > 30 → source
        assert_eq!(process_fps(&src, &ExportSettings { fps: 60.0, ..Default::default() }), (30.0, 60.0));
        assert_eq!(process_fps(&src, &ExportSettings { fps: 5.0, speed: 2.0, ..Default::default() }), (10.0, 5.0));
        assert_eq!(process_fps(&src, &ExportSettings { fps: 10.0, speed: 4.0, ..Default::default() }), (30.0, 10.0));
        assert_eq!(process_fps(&src, &ExportSettings { fps: 10.0, speed: 0.5, ..Default::default() }), (30.0, 10.0));
        let eq = ExportSettings { mode: "equirect".into(), ..Default::default() };
        assert_eq!(stitch_size(&src, &eq), (5760, 2880));
        assert_eq!(stitch_size(&src, &ExportSettings { stitch_width: 3001, ..eq.clone() }), (3000, 1500));
        assert_eq!(stitch_size(&src, &ExportSettings { stitch_width: 10, ..eq.clone() }), (256, 128));
        assert_eq!(stitch_size(&src, &ExportSettings { stitch_width: 99999, ..eq.clone() }), (8192, 4096));
        let flat = Source { layout: Layout::Equirect, lens_size: (4096, 2048), ..src.clone() };
        assert_eq!(stitch_size(&flat, &eq), (4096, 2048));
        assert_eq!(output_size(&src, &ExportSettings::default()), (1920, 1080));
    }

    #[test]
    fn atempo_and_encoders() {
        assert_eq!(atempo_chain(1.5), "atempo=1.50000");
        assert_eq!(atempo_chain(4.0), "atempo=2.0,atempo=2.00000");
        assert_eq!(atempo_chain(8.0), "atempo=2.0,atempo=2.0,atempo=2.00000");
        assert_eq!(atempo_chain(0.25), "atempo=0.5,atempo=0.50000");
        let h264 = encoder_args(&ExportSettings { crf: 18, preset: "slow".into(), ..Default::default() });
        assert_eq!(h264.join(" "), "-c:v libx264 -preset slow -crf 18 -pix_fmt yuv420p");
        let h265 = encoder_args(&ExportSettings { codec: "h265".into(), ..Default::default() }).join(" ");
        assert!(
            h265.contains("-c:v libx265")
                && h265.contains("-tag:v hvc1")
                && h265.contains("-x265-params log-level=error"),
            "{h265}"
        );
    }

    #[test]
    fn build_plan_shapes_the_two_ffmpeg_commands() {
        let src = x3(10.0, 30.0);
        let calib = Calibration::default();
        let s = ExportSettings { width: 640, height: 360, start: 1.0, end: 3.0, crf: 23, ..Default::default() };
        let plan = build_plan(&src, &calib, &s, Path::new("/tmp/sortie vidéo.mp4"), None).unwrap();
        assert_eq!((plan.kind, plan.mats.len(), plan.fovs.len(), plan.spherical), (PlanKind::Video, 60, 60, false));
        assert!((plan.out_duration - 2.0).abs() < 1e-9);
        assert_eq!(plan.renderer.out_size(), (640, 360));
        let dec = &plan.decoder;
        assert_eq!(dec[0], "ffmpeg");
        assert_eq!(
            (arg_after(dec, "-ss"), arg_after(dec, "-t"), arg_after(dec, "-i")),
            (Some("0.9833"), Some("2.0333"), Some("/v/VID_00_001.insv"))
        );
        assert_eq!(
            arg_after(dec, "-filter_complex"),
            Some("[0:v:0]format=yuv420p[fa];[0:v:1]format=yuv420p[fb];[fa][fb]hstack=inputs=2[v]")
        );
        assert_eq!(&dec[dec.len() - 8..], ["-map", "[v]", "-an", "-f", "rawvideo", "-pix_fmt", "yuv420p", "-"]);
        let enc = &plan.encoder;
        assert_eq!(arg_after(enc, "-s"), Some("640x360"));
        assert_eq!(arg_after(enc, "-framerate"), Some("30.00000"));
        assert_eq!(arg_after(enc, "-filter_complex"), Some("[0:v]format=yuv420p[vout]"));
        assert!(enc.iter().any(|a| a == "-y") && enc.windows(2).any(|w| w == ["-movflags", "+faststart"]));
        assert_eq!(
            (arg_after(enc, "-c:a"), last_arg_after(enc, "-t"), arg_after(enc, "-crf")),
            (Some("aac"), Some("2.0000"), Some("23"))
        );
        assert_eq!(enc.last().unwrap(), "/tmp/sortie vidéo.mp4");
        assert!(plan.shell().contains("'/tmp/sortie vidéo.mp4'") && plan.shell().contains("<rendu x3studio>"));
        // audio coupé / source sans audio
        let mute =
            build_plan(&src, &calib, &ExportSettings { audio: false, ..s.clone() }, Path::new("o.mp4"), None).unwrap();
        assert!(mute.encoder.iter().any(|a| a == "-an") && !mute.encoder.iter().any(|a| a == "-c:a"));
        let silent = Source { has_audio: false, ..src.clone() };
        assert!(build_plan(&silent, &calib, &s, Path::new("o.mp4"), None).unwrap().encoder.iter().any(|a| a == "-an"));
        // vitesse x2 et cadence de sortie plus basse : moins d'images traitées, setpts + atempo
        let fast = ExportSettings { speed: 2.0, fps: 10.0, start: 0.0, end: 0.0, ..s.clone() };
        let plan = build_plan(&src, &calib, &fast, Path::new("o.mp4"), None).unwrap();
        assert_eq!(plan.mats.len(), 200, "10 s traitées à 20 i/s");
        assert!(arg_after(&plan.decoder, "-filter_complex").unwrap().starts_with("[0:v:0]fps=20.00000,format=yuv420p"));
        assert_eq!(
            arg_after(&plan.encoder, "-filter_complex"),
            Some("[0:v]setpts=PTS/2.00000,fps=10.00000,format=yuv420p[vout]")
        );
        assert_eq!(arg_after(&plan.encoder, "-af"), Some("atempo=2.00000"));
        assert_eq!(last_arg_after(&plan.encoder, "-t"), Some("5.0000"));
        assert_eq!(arg_after(&plan.encoder, "-t"), Some("10.0000"), "l'entrée audio garde la durée source");
    }

    #[test]
    fn build_plan_handles_every_layout_and_mode() {
        let calib = Calibration::default();
        // deux fichiers : une entrée par objectif, audio du premier
        let two_files = Source {
            layout: Layout::TwoFiles,
            lens_files: vec!["/v/a_00_1.insv".into(), "/v/a_10_1.insv".into()],
            lens_streams: vec![0, 0],
            audio_file: Some("/v/a_00_1.insv".into()),
            ..x3(10.0, 30.0)
        };
        let p = build_plan(&two_files, &calib, &ExportSettings::default(), Path::new("o.mp4"), None).unwrap();
        assert_eq!(p.decoder.iter().filter(|a| *a == "-i").count(), 2);
        assert!(arg_after(&p.decoder, "-filter_complex").unwrap().contains("[1:v:0]format=yuv420p[fb]"));
        // côte à côte et équirectangulaire : un seul flux ; les objectifs permutés restent dans l'ordre du flux
        for layout in [Layout::SbsFisheye, Layout::Equirect] {
            let src = Source { layout, lens_streams: vec![0], lens_files: vec!["/v/x.mp4".into()], ..x3(10.0, 30.0) };
            let swapped = Calibration { swap_lenses: true, ..calib.clone() };
            let p = build_plan(&src, &swapped, &ExportSettings::default(), Path::new("o.mp4"), None).unwrap();
            assert_eq!(arg_after(&p.decoder, "-filter_complex"), Some("[0:v:0]format=yuv420p[v]"), "{layout:?}");
        }
        // mode 360° : pas de faststart (métadonnées sphériques ajoutées ensuite), sauf si désactivées
        let eq = ExportSettings { mode: "equirect".into(), stitch_width: 1024, ..Default::default() };
        let p = build_plan(&x3(4.0, 30.0), &calib, &eq, Path::new("o.mp4"), None).unwrap();
        assert!(p.spherical && !p.encoder.iter().any(|a| a == "+faststart"));
        assert_eq!(p.renderer.out_size(), (1024, 512));
        assert!(p.fovs.iter().all(|&f| f == 0.0));
        let p = build_plan(
            &x3(4.0, 30.0),
            &calib,
            &ExportSettings { spherical_metadata: false, ..eq },
            Path::new("o.mp4"),
            None,
        )
        .unwrap();
        assert!(!p.spherical && p.encoder.iter().any(|a| a == "+faststart"));
        // erreurs de paramètres
        let src = x3(10.0, 30.0);
        for bad in
            [ExportSettings { width: 1921, ..Default::default() }, ExportSettings { start: 11.0, ..Default::default() }]
        {
            assert!(is_user_error(&build_plan(&src, &calib, &bad, Path::new("o.mp4"), None).err().unwrap()));
        }
        let odd = Source { lens_size: (2881, 2881), ..src.clone() };
        assert!(build_plan(&odd, &calib, &ExportSettings::default(), Path::new("o.mp4"), None).is_err());
        let broken = Source { lens_streams: vec![0], ..src };
        assert!(build_plan(&broken, &calib, &ExportSettings::default(), Path::new("o.mp4"), None).is_err());
    }

    #[test]
    fn snapshot_plan_clamps_the_instant_and_picks_the_format() {
        let src = x3(10.0, 30.0);
        let calib = Calibration::default();
        let s = ExportSettings { width: 320, height: 180, ..Default::default() };
        let jpg = build_snapshot_plan(&src, &calib, &s, 99.0, Path::new("/tmp/c.JPG"), None).unwrap();
        assert_eq!((jpg.kind, jpg.mats.len()), (PlanKind::Image, 1));
        assert_eq!(
            arg_after(&jpg.decoder, "-ss"),
            Some("9.9500"),
            "borné à la dernière image, une demi-image d'avance"
        );
        assert!(jpg.decoder.windows(2).any(|w| w == ["-frames:v", "1"]));
        assert_eq!(arg_after(&jpg.encoder, "-filter_complex"), Some("[0:v]format=yuvj420p[vout]"));
        assert_eq!(arg_after(&jpg.encoder, "-q:v"), Some("2"));
        let png = build_snapshot_plan(&src, &calib, &s, -5.0, Path::new("/tmp/c.png"), None).unwrap();
        assert_eq!(arg_after(&png.decoder, "-ss"), Some("0.0000"));
        assert_eq!(arg_after(&png.encoder, "-filter_complex"), Some("[0:v]format=rgb24[vout]"));
        assert!(arg_after(&png.encoder, "-q:v").is_none());
        assert!(build_snapshot_plan(&src, &calib, &s, f64::NAN, Path::new("c.png"), None).is_err());
        // l'instant sert aussi à choisir les keyframes et la stabilisation
        let kf = ExportSettings {
            keyframes: vec![Keyframe { t: 5.0, yaw: Some(33.0), pitch: None, roll: None, fov: None }],
            ..s
        };
        let p = build_snapshot_plan(&src, &calib, &kf, 2.0, Path::new("c.jpg"), None).unwrap();
        assert!(close(&p.mats[0], &crate::geometry::ypr_to_matrix(33.0, 0.0, 0.0)));
    }

    #[test]
    fn proxy_and_analysis_commands() {
        let src = x3(10.0, 30.0);
        let cmd = build_proxy_cmd(&src, Path::new("/tmp/p.part"), 960);
        assert_eq!(
            arg_after(&cmd, "-filter_complex"),
            Some("[0:v:0]scale=960:960:flags=bilinear[a];[0:v:1]scale=960:960:flags=bilinear[b];[a][b]hstack=inputs=2,format=yuv420p[v]")
        );
        assert!(
            cmd.windows(2).any(|w| w == ["-progress", "pipe:1"]) && cmd.windows(2).any(|w| w == ["-c:a", "libopus"])
        );
        assert_eq!(cmd.last().unwrap(), "/tmp/p.part");
        assert_eq!(arg_after(&cmd, "-f"), Some("webm"));
        let two = Source {
            layout: Layout::TwoFiles,
            lens_files: vec!["a".into(), "b".into()],
            lens_streams: vec![0, 0],
            ..src.clone()
        };
        let cmd = build_proxy_cmd(&two, Path::new("p"), 480);
        assert_eq!(cmd.iter().filter(|a| *a == "-i").count(), 2);
        let eq = Source {
            layout: Layout::Equirect,
            lens_files: vec!["e.mp4".into()],
            lens_streams: vec![0],
            has_audio: false,
            ..src.clone()
        };
        let cmd = build_proxy_cmd(&eq, Path::new("p"), 480);
        assert_eq!(arg_after(&cmd, "-filter_complex"), Some("[0:v:0]scale=960:480:flags=bilinear,format=yuv420p[v]"));
        assert!(cmd.iter().any(|a| a == "-an"));
        let d = analysis_decoder(&src, Path::new("/tmp/p.webm"));
        assert!(d.dual && (d.frame_w, d.frame_h) == (1280, 640));
        assert_eq!(arg_after(&d.cmd, "-pix_fmt"), Some("yuv420p"));
        assert_eq!(arg_after(&d.cmd, "-i"), Some("/tmp/p.webm"));
        let d = analysis_decoder(&eq, Path::new("p"));
        assert!(!d.dual && (d.frame_w, d.frame_h) == (960, 480));
        assert_eq!(arg_after(&d.cmd, "-pix_fmt"), Some("gray"));
        // seuls des filtres simples sont utilisés (pas de v360, remap, overlay)
        for c in [&build_proxy_cmd(&src, Path::new("p"), 960), &d.cmd] {
            let text = c.join(" ");
            assert!(!text.contains("v360") && !text.contains("remap") && !text.contains("overlay"), "{text}");
        }
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(shell_quote("abc-1.mp4"), "abc-1.mp4");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("l'été"), "'l'\"'\"'été'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("[0:v:0]scale"), "'[0:v:0]scale'");
    }

    // -- exécution réelle de ffmpeg -----------------------------------------------------
    fn ff(args: &[&str]) -> Vec<String> {
        let mut c = base_command(true);
        c.extend(["-progress", "pipe:1", "-nostats"].map(String::from));
        c.extend(args.iter().map(|s| s.to_string()));
        c
    }

    #[test]
    fn run_ffmpeg_reports_progress_and_failures() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("t.mp4");
        let seen = RefCell::new(vec![]);
        let cmd = ff(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=2:size=64x64:rate=10",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            out.to_str().unwrap(),
        ]);
        run_ffmpeg(&cmd, 2.0, &|p| seen.borrow_mut().push(p), &AtomicBool::new(false)).unwrap();
        let seen = seen.into_inner();
        assert!(out.metadata().unwrap().len() > 100);
        assert_eq!(*seen.last().unwrap(), 1.0);
        assert!(seen.windows(2).all(|w| w[0] <= w[1]) && seen.iter().all(|p| (0.0..=1.0).contains(p)), "{seen:?}");
        // échec : le message de ffmpeg est remonté
        let bad = ff(&["-i", "/nulle/part.mp4", out.to_str().unwrap()]);
        let e = run_ffmpeg(&bad, 1.0, &|_| {}, &AtomicBool::new(false)).unwrap_err();
        assert!(
            is_user_error(&e) && e.to_string().starts_with("FFmpeg a échoué") && e.to_string().contains("part.mp4"),
            "{e}"
        );
        // programme absent
        let e = run_ffmpeg(&["ffmpeg-introuvable-xyz".to_string()], 1.0, &|_| {}, &AtomicBool::new(false)).unwrap_err();
        assert!(is_user_error(&e) && e.to_string().contains("introuvable"), "{e}");
        assert!(run_ffmpeg(&[], 1.0, &|_| {}, &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn run_ffmpeg_can_be_cancelled_quickly() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("long.mp4");
        // `-re` : lecture en temps réel, 60 s de vidéo
        let cmd = ff(&[
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=60:size=64x64:rate=10",
            "-c:v",
            "libx264",
            out.to_str().unwrap(),
        ]);
        let cancel = AtomicBool::new(false);
        let t0 = std::time::Instant::now();
        let e = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(700));
                cancel.store(true, Ordering::Relaxed);
            });
            run_ffmpeg(&cmd, 60.0, &|_| {}, &cancel).unwrap_err()
        });
        assert!(is_cancelled(&e), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(8), "annulation lente : {:?}", t0.elapsed());
    }
}
