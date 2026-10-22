//! Analyse des fichiers Insta360 (.insv) et d'autres vidéos 360° via `ffprobe`.
//! Spec : `x3studio/media.py`. Le JSON de `Source` est consommé tel quel par `web/app.js`.

use crate::error::{media_err, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Les 32 derniers octets des fichiers Insta360 (fin des métadonnées embarquées).
pub const INSTA360_TRAILER_MAGIC: &[u8; 32] = b"8db42d694ccc418790edff439fe026bf";
/// Extensions (sans point, minuscules) proposées dans le sélecteur de fichiers (`.lrv` : version basse
/// résolution que l'X3 enregistre à côté de chaque `.insv`).
pub const VIDEO_EXTS: &[&str] = &["insv", "lrv", "mp4", "mov", "mkv", "webm", "m4v"];

/// Organisation du fichier. Sérialisé en `two_streams` / `two_files` / `sbs_fisheye` / `equirect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    /// Deux flux vidéo carrés dans un même fichier (X3, X2, ONE R).
    TwoStreams,
    /// Deux fichiers `..._00_...insv` + `..._10_...insv` (ONE X).
    TwoFiles,
    /// Deux fisheye côte à côte dans un seul flux 2:1.
    SbsFisheye,
    /// Vidéo déjà assemblée (équirectangulaire 2:1).
    Equirect,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub path: String,
    pub layout: Layout,
    /// Un fichier par objectif (le même fichier deux fois si `two_streams`).
    pub lens_files: Vec<String>,
    /// Rang du flux vidéo (`v:N`) dans chaque fichier.
    pub lens_streams: Vec<usize>,
    /// Taille d'une image d'objectif (ou de l'équirectangulaire).
    pub lens_size: (u32, u32),
    pub fps: f64,
    pub duration: f64,
    pub codec: String,
    pub has_audio: bool,
    pub audio_file: Option<String>,
    pub has_trailer: bool,
    pub size_bytes: u64,
    #[serde(default)]
    pub camera: String,
    #[serde(default)]
    pub notes: Vec<String>,
}

impl Source {
    pub fn frames(&self) -> usize {
        (self.duration * self.fps).round_ties_even().max(0.0) as usize
    }

    /// JSON de l'API : les champs de la structure + `frames`.
    pub fn to_json(&self) -> Value {
        let mut v = serde_json::to_value(self).expect("Source sérialisable");
        v["frames"] = serde_json::json!(self.frames());
        v
    }
}

/// Lance `ffprobe` (JSON) ; réessaie avec `-f mov` si la détection du format échoue (`.insv` est un
/// conteneur MP4/QuickTime).
pub fn ffprobe(path: &Path) -> Result<Value> {
    let run = |force_mov: bool| -> Result<std::process::Output> {
        let mut cmd = Command::new("ffprobe");
        if force_mov {
            cmd.args(["-f", "mov"]);
        }
        cmd.args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"]).arg(path);
        cmd.stdin(std::process::Stdio::null()).output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                media_err("ffprobe introuvable : installez ffmpeg (sudo apt install ffmpeg)")
            } else {
                anyhow::Error::new(e).context("lancement de ffprobe")
            }
        })
    };
    let mut out = run(false)?;
    if !out.status.success() || String::from_utf8_lossy(&out.stdout).trim().is_empty() {
        out = run(true)?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let short: String = err.trim().chars().take(300).collect();
            return Err(media_err(format!("ffprobe a échoué sur {} : {short}", path.display())));
        }
    }
    serde_json::from_slice(&out.stdout).map_err(|e| media_err(format!("Réponse de ffprobe illisible : {e}")))
}

fn fps_of(stream: &Value) -> f64 {
    for key in ["avg_frame_rate", "r_frame_rate"] {
        let Some(v) = stream.get(key).and_then(Value::as_str) else { continue };
        let Some((n, d)) = v.split_once('/') else { continue };
        if let (Ok(n), Ok(d)) = (n.trim().parse::<f64>(), d.trim().parse::<f64>()) {
            if n != 0.0 && d != 0.0 {
                return n / d;
            }
        }
    }
    30.0
}

/// Flux vidéo « réels » (les vignettes jointes sont ignorées) avec leur rang parmi tous les flux vidéo.
fn video_streams(info: &Value) -> Vec<(usize, &Value)> {
    info.get("streams")
        .and_then(Value::as_array)
        .map(|streams| {
            streams
                .iter()
                .filter(|s| s.get("codec_type").and_then(Value::as_str) == Some("video"))
                .enumerate()
                .filter(|(_, s)| {
                    s.get("disposition").and_then(|d| d.get("attached_pic")).and_then(Value::as_i64).unwrap_or(0) == 0
                })
                .collect()
        })
        .unwrap_or_default()
}

fn dim(stream: &Value, key: &str) -> u32 {
    stream.get(key).and_then(Value::as_u64).unwrap_or(0).min(u32::MAX as u64) as u32
}

fn parse_secs(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// Fin de fichier Insta360 (métadonnées gyro, etc.).
pub fn has_insta360_trailer(path: &Path) -> bool {
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let mut buf = [0u8; 32];
    f.seek(SeekFrom::End(-32)).is_ok() && f.read_exact(&mut buf).is_ok() && &buf == INSTA360_TRAILER_MAGIC
}

/// Taille de l'image réduite examinée par [`is_dual_fisheye_frame`] (deux carrés de 128 px).
const SNIFF_W: usize = 256;
const SNIFF_H: usize = 128;

/// Vrai si l'image 2:1 en niveaux de gris (`w` = 2 × `h`) montre deux objectifs fisheye côte à côte : deux
/// cercles, donc hors des cercles (les quatre coins de chaque carré) un noir uniforme, alors que le centre ne
/// l'est pas. Un panorama équirectangulaire remplit tout le cadre : ces coins sont de l'image.
///
/// Prudent : une scène nocturne (centre et coins sombres), un objectif couvert ou des cercles plus grands que le
/// cadre (coins non noirs) donnent faux, comme un panorama.
pub fn is_dual_fisheye_frame(gray: &[u8], w: usize, h: usize) -> bool {
    const DARK: f64 = 28.0; // le noir d'une vidéo (16) plus un peu de bruit de compression
    const FLAT: f64 = 10.0; // écart type maximal de ce noir : un ciel de nuit ou un sol sombre a du relief
    const CONTRAST: f64 = 12.0;
    if h < 16 || w != 2 * h || gray.len() != w * h {
        return false;
    }
    let r = h as f64 / 2.0;
    (0..2).all(|i| {
        let (cx, cy) = ((i * h) as f64 + r, r);
        let (mut inner, mut n_in) = (0.0, 0u32);
        let (mut outer, mut outer_sq, mut n_out) = (0.0, 0.0, 0u32);
        for y in 0..h {
            for x in i * h..(i + 1) * h {
                let d = ((x as f64 + 0.5 - cx).powi(2) + (y as f64 + 0.5 - cy).powi(2)).sqrt() / r;
                let v = f64::from(gray[y * w + x]);
                if d < 0.6 {
                    inner += v;
                    n_in += 1;
                } else if d > 1.12 {
                    outer += v;
                    outer_sq += v * v;
                    n_out += 1;
                }
            }
        }
        let (inner, n_out) = (inner / f64::from(n_in.max(1)), f64::from(n_out.max(1)));
        let (outer_mean, outer_var) = (outer / n_out, outer_sq / n_out - (outer / n_out).powi(2));
        outer_mean <= DARK && outer_var.max(0.0).sqrt() <= FLAT && inner >= outer_mean + CONTRAST
    })
}

/// Image grise réduite (`SNIFF_W` × `SNIFF_H`) prise à l'instant `t` ; `None` si ffmpeg n'en tire rien.
fn sample_gray_frame(path: &Path, t: f64) -> Option<Vec<u8>> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-v", "error", "-ss", &format!("{t:.3}"), "-i"])
        .arg(path)
        .args(["-frames:v", "1", "-vf", &format!("scale={SNIFF_W}:{SNIFF_H}:flags=area,format=gray")])
        .args(["-f", "rawvideo", "-"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    (out.status.success() && out.stdout.len() == SNIFF_W * SNIFF_H).then_some(out.stdout)
}

/// Le fichier montre-t-il deux fisheye côte à côte ? Examine jusqu'à trois images (au quart, à la moitié, aux
/// trois quarts de la durée) : une seule suffit.
pub fn looks_like_dual_fisheye(path: &Path, duration: f64) -> bool {
    let times: Vec<f64> =
        if duration > 1.0 { [0.25, 0.5, 0.75].iter().map(|f| f * duration).collect() } else { vec![0.0] };
    times
        .into_iter()
        .filter_map(|t| sample_gray_frame(path, t))
        .any(|gray| is_dual_fisheye_frame(&gray, SNIFF_W, SNIFF_H))
}

/// `VID_20240101_120000_00_001.insv` ou `PRO_VID_...` (insensible à la casse) :
/// retourne (préfixe sans le `_00_`, objectif `00`|`10`, numéro, extension avec point).
fn parse_insv_name(name: &str) -> Option<(&str, &str, &str, &str)> {
    let dot = name.rfind('.')?;
    let (stem, ext) = name.split_at(dot);
    if !ext.eq_ignore_ascii_case(".insv") {
        return None;
    }
    let (rest, num) = stem.rsplit_once('_')?;
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (prefix, lens) = rest.rsplit_once('_')?;
    if lens != "00" && lens != "10" {
        return None;
    }
    // prefix = (PRO_)?VID_<8 chiffres>_<6 chiffres>
    let body = match prefix.get(..4) {
        Some(p) if p.eq_ignore_ascii_case("PRO_") => &prefix[4..],
        _ => prefix,
    };
    let mut parts = body.split('_');
    let (v, date, time) = (parts.next()?, parts.next()?, parts.next()?);
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    if parts.next().is_some() || !v.eq_ignore_ascii_case("VID") || !digits(date, 8) || !digits(time, 6) {
        return None;
    }
    Some((prefix, lens, num, ext))
}

/// Fichier d'objectif apparié d'un `..._00_...insv` (`..._10_...insv`) s'il existe (anciens modèles).
pub fn find_lens_pair(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let (prefix, lens, num, ext) = parse_insv_name(name)?;
    if lens != "00" {
        return None;
    }
    let other = path.with_file_name(format!("{prefix}_10_{num}{ext}"));
    other.exists().then_some(other)
}

fn path_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Cœur pur et testable de la détection : `info` = sortie ffprobe du fichier. `load_pair` n'est appelé
/// que si le fichier ressemble à un objectif seul avec un partenaire `_10_` : il retourne le chemin et la
/// sortie ffprobe de ce partenaire (`None` = pas de partenaire). `sniff(durée)` n'est appelé que pour un flux 2:1
/// sans aucune marque Insta360 (`.insv`, `.lrv`, signature de fin de fichier) : vrai si les images montrent deux
/// fisheye côte à côte. Erreurs : `MediaError`.
pub fn source_from_probe(
    path: &Path,
    info: &Value,
    load_pair: &mut dyn FnMut() -> Result<Option<(PathBuf, Value)>>,
    sniff: &mut dyn FnMut(f64) -> bool,
    has_trailer: bool,
    size_bytes: u64,
) -> Result<Source> {
    let vids = video_streams(info);
    let Some(&(first_rank, v)) = vids.first() else {
        return Err(media_err("Aucun flux vidéo dans ce fichier."));
    };
    let has_audio = info
        .get("streams")
        .and_then(Value::as_array)
        .is_some_and(|a| a.iter().any(|s| s.get("codec_type").and_then(Value::as_str) == Some("audio")));
    let duration = parse_secs(info.get("format").and_then(|f| f.get("duration")))
        .or_else(|| parse_secs(v.get("duration")))
        .unwrap_or(0.0);
    let fps = fps_of(v);
    let has_ext = |ext: &str| path.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext));
    let (is_insv, is_lrv) = (has_ext("insv"), has_ext("lrv"));
    let camera = if is_insv || is_lrv || has_trailer { "Insta360" } else { "" }.to_string();
    let mut notes: Vec<String> = vec![];
    let codec_of = |s: &Value| s.get("codec_name").and_then(Value::as_str).unwrap_or("").to_string();
    let me = path_string(path);

    let squares: Vec<&(usize, &Value)> = vids.iter().filter(|(_, s)| dim(s, "width") == dim(s, "height")).collect();
    let (layout, lens_files, lens_streams, size, codec);
    if squares.len() >= 2 && dim(squares[0].1, "width") == dim(squares[1].1, "width") && dim(squares[0].1, "width") > 0
    {
        // --- deux flux carrés dans un même fichier (X3, X2, ONE R, ...)
        let (a, b) = (squares[0], squares[1]);
        layout = Layout::TwoStreams;
        lens_files = vec![me.clone(), me.clone()];
        lens_streams = vec![a.0, b.0];
        size = (dim(a.1, "width"), dim(a.1, "height"));
        codec = codec_of(a.1);
    } else {
        let (w, h) = (dim(v, "width"), dim(v, "height"));
        let pair = if w == h && w > 0 { load_pair()? } else { None };
        if let Some((pair_path, pair_info)) = pair {
            // deux fichiers, un objectif chacun
            let vb = video_streams(&pair_info);
            let Some(&(rank_b, _)) = vb.first() else {
                return Err(media_err(format!("Aucun flux vidéo dans {}", pair_path.display())));
            };
            layout = Layout::TwoFiles;
            lens_files = vec![me.clone(), path_string(&pair_path)];
            lens_streams = vec![first_rank, rank_b];
            size = (w, h);
            codec = codec_of(v);
            notes.push("Deux fichiers d'objectif appariés (_00_ / _10_).".into());
        } else if h > 0 && w == 2 * h {
            // 2:1 : deux fisheye côte à côte (l'X3 les enregistre ainsi dans ses `.insv` comme dans ses `.lrv`) ou
            // vidéo déjà assemblée. Un fichier brut de la caméra (extension, signature de fin de fichier) est
            // toujours en fisheye ; les autres sont départagés par leurs images.
            let side_by_side = is_insv || is_lrv || has_trailer || sniff(duration);
            codec = codec_of(v);
            if side_by_side {
                layout = Layout::SbsFisheye;
                lens_files = vec![me.clone(), me.clone()];
                lens_streams = vec![first_rank, first_rank];
                size = (h, h);
                notes.push("Deux fisheye côte à côte dans un seul flux.".into());
            } else {
                layout = Layout::Equirect;
                lens_files = vec![me.clone()];
                lens_streams = vec![first_rank];
                size = (w, h);
                notes.push("Vidéo déjà assemblée (équirectangulaire) : seul le recadrage est disponible.".into());
            }
        } else {
            return Err(media_err(format!(
                "Format non reconnu ({w}x{h}). Attendu : deux flux carrés (.insv), deux fisheye côte à côte, \
                 ou une vidéo équirectangulaire 2:1."
            )));
        }
    }
    if is_lrv {
        notes
            .push("Version basse résolution du clip (.lrv) : pour exporter en pleine qualité, ouvrez le .insv.".into());
    }
    Ok(Source {
        path: me.clone(),
        layout,
        lens_files,
        lens_streams,
        lens_size: size,
        fps,
        duration,
        codec,
        has_audio,
        audio_file: has_audio.then_some(me),
        has_trailer,
        size_bytes,
        camera,
        notes,
    })
}

/// `~` en tête de chemin → dossier personnel.
fn expand_home(p: &Path) -> PathBuf {
    if let Ok(rest) = p.strip_prefix("~") {
        if let Some(home) = std::env::var_os("HOME") {
            return Path::new(&home).join(rest);
        }
    }
    p.to_path_buf()
}

pub fn probe_source(path: &Path) -> Result<Source> {
    let expanded = expand_home(path);
    let path = std::fs::canonicalize(&expanded).unwrap_or_else(|_| std::path::absolute(&expanded).unwrap_or(expanded));
    if !path.is_file() {
        return Err(media_err(format!("Fichier introuvable : {}", path.display())));
    }
    let info = ffprobe(&path)?;
    let size_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let has_trailer = has_insta360_trailer(&path);
    let mut load_pair = || -> Result<Option<(PathBuf, Value)>> {
        match find_lens_pair(&path) {
            Some(p) => {
                let info = ffprobe(&p)?;
                Ok(Some((p, info)))
            }
            None => Ok(None),
        }
    };
    let mut sniff = |duration: f64| looks_like_dual_fisheye(&path, duration);
    source_from_probe(&path, &info, &mut load_pair, &mut sniff, has_trailer, size_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stream(kind: &str, w: u64, h: u64) -> Value {
        json!({"codec_type": kind, "codec_name": "hevc", "width": w, "height": h,
               "avg_frame_rate": "30000/1001", "r_frame_rate": "30/1", "disposition": {"attached_pic": 0}})
    }

    fn no_pair() -> Result<Option<(PathBuf, Value)>> {
        Ok(None)
    }

    fn no_sniff(_: f64) -> bool {
        false
    }

    #[test]
    fn insv_names() {
        assert_eq!(
            parse_insv_name("VID_20240101_120000_00_001.insv"),
            Some(("VID_20240101_120000", "00", "001", ".insv"))
        );
        assert_eq!(
            parse_insv_name("pro_vid_20240101_120000_10_12.INSV"),
            Some(("pro_vid_20240101_120000", "10", "12", ".INSV"))
        );
        for bad in [
            "VID_20240101_120000_01_001.insv",
            "VID_2024_120000_00_001.insv",
            "VID_20240101_120000_00_.insv",
            "VID_20240101_120000_00_001.mp4",
            "XYZ_20240101_120000_00_001.insv",
            "VID_20240101_120000_00_00a.insv",
            "IMG_20240101_120000_00_001.insv",
        ] {
            assert!(parse_insv_name(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn two_streams_layout_with_thumbnail_and_audio() {
        let mut thumb = stream("video", 320, 240);
        thumb["disposition"]["attached_pic"] = json!(1);
        let info = json!({"streams": [stream("video", 2880, 2880), thumb, stream("video", 2880, 2880), stream("audio", 0, 0)],
                          "format": {"duration": "12.5"}});
        let s = source_from_probe(Path::new("/x/clip.insv"), &info, &mut no_pair, &mut no_sniff, true, 123).unwrap();
        assert_eq!(s.layout, Layout::TwoStreams);
        assert_eq!(s.lens_streams, vec![0, 2], "la vignette jointe garde son rang mais n'est pas retenue");
        assert_eq!(s.lens_size, (2880, 2880));
        assert!((s.fps - 29.97002997).abs() < 1e-6);
        assert_eq!((s.duration, s.has_audio, s.has_trailer, s.size_bytes), (12.5, true, true, 123));
        assert_eq!(s.audio_file.as_deref(), Some("/x/clip.insv"));
        assert_eq!(s.camera, "Insta360");
        assert_eq!(s.frames(), 375);
        let j = s.to_json();
        assert_eq!(j["layout"], "two_streams");
        assert_eq!(j["lens_size"], json!([2880, 2880]));
        assert_eq!(j["frames"], 375);
        assert_eq!(j["lens_files"], json!(["/x/clip.insv", "/x/clip.insv"]));
    }

    #[test]
    fn side_by_side_and_equirect() {
        let info = json!({"streams": [stream("video", 5760, 2880)], "format": {"duration": "3"}});
        let s = source_from_probe(Path::new("/x/a.insv"), &info, &mut no_pair, &mut no_sniff, false, 1).unwrap();
        assert_eq!((s.layout, s.lens_size), (Layout::SbsFisheye, (2880, 2880)));
        assert_eq!(s.lens_streams, vec![0, 0]);
        assert_eq!(s.notes, vec!["Deux fisheye côte à côte dans un seul flux."]);
        let s = source_from_probe(Path::new("/x/a.mp4"), &info, &mut no_pair, &mut no_sniff, false, 1).unwrap();
        assert_eq!((s.layout, s.lens_size, s.camera.as_str()), (Layout::Equirect, (5760, 2880), ""));
        assert_eq!(s.lens_files, vec!["/x/a.mp4"]);
        assert!(!s.has_audio && s.audio_file.is_none());
    }

    #[test]
    fn two_files_layout() {
        let info = json!({"streams": [stream("video", 1920, 1920)], "format": {"duration": "2"}});
        let pair_info = json!({"streams": [stream("audio", 0, 0), stream("video", 1920, 1920)]});
        let mut loader =
            || -> Result<Option<(PathBuf, Value)>> { Ok(Some((PathBuf::from("/x/B_10.insv"), pair_info.clone()))) };
        let s = source_from_probe(Path::new("/x/A_00.insv"), &info, &mut loader, &mut no_sniff, false, 1).unwrap();
        assert_eq!(s.layout, Layout::TwoFiles);
        assert_eq!(s.lens_files, vec!["/x/A_00.insv", "/x/B_10.insv"]);
        assert_eq!(
            s.lens_streams,
            vec![0, 0],
            "rang du premier flux vidéo du fichier apparié (le son compte dans stream index, pas dans v:N)"
        );
        assert_eq!(s.notes, vec!["Deux fichiers d'objectif appariés (_00_ / _10_)."]);
        let empty = json!({"streams": [stream("audio", 0, 0)]});
        let mut loader =
            || -> Result<Option<(PathBuf, Value)>> { Ok(Some((PathBuf::from("/x/B_10.insv"), empty.clone()))) };
        let e = source_from_probe(Path::new("/x/A_00.insv"), &info, &mut loader, &mut no_sniff, false, 1).unwrap_err();
        assert_eq!(e.to_string(), "Aucun flux vidéo dans /x/B_10.insv");
    }

    #[test]
    fn errors_have_the_python_messages() {
        let info = json!({"streams": [stream("audio", 0, 0)]});
        let e = source_from_probe(Path::new("/x/a.mp4"), &info, &mut no_pair, &mut no_sniff, false, 1).unwrap_err();
        assert_eq!(e.to_string(), "Aucun flux vidéo dans ce fichier.");
        assert!(crate::error::is_user_error(&e));
        let info = json!({"streams": [stream("video", 1920, 1080)]});
        let e = source_from_probe(Path::new("/x/a.mp4"), &info, &mut no_pair, &mut no_sniff, false, 1).unwrap_err();
        assert_eq!(
            e.to_string(),
            "Format non reconnu (1920x1080). Attendu : deux flux carrés (.insv), deux fisheye côte à côte, \
             ou une vidéo équirectangulaire 2:1."
        );
        // dimensions absentes : jamais de panique
        let info = json!({"streams": [{"codec_type": "video"}]});
        assert!(source_from_probe(Path::new("/x/a.mp4"), &info, &mut no_pair, &mut no_sniff, false, 1).is_err());
    }

    #[test]
    fn fps_and_duration_fallbacks() {
        assert_eq!(fps_of(&json!({"avg_frame_rate": "0/0", "r_frame_rate": "25/1"})), 25.0);
        assert_eq!(fps_of(&json!({"avg_frame_rate": "30/0"})), 30.0);
        assert_eq!(fps_of(&json!({})), 30.0);
        assert_eq!(fps_of(&json!({"avg_frame_rate": "abc"})), 30.0);
        let mut v = stream("video", 100, 100);
        v["duration"] = json!("4.0");
        let info = json!({"streams": [v.clone(), v.clone()], "format": {"duration": "N/A"}});
        assert_eq!(
            source_from_probe(Path::new("/a.mp4"), &info, &mut no_pair, &mut no_sniff, false, 0).unwrap().duration,
            4.0
        );
        let info = json!({"streams": [stream("video", 100, 100), stream("video", 100, 100)]});
        assert_eq!(
            source_from_probe(Path::new("/a.mp4"), &info, &mut no_pair, &mut no_sniff, false, 0).unwrap().duration,
            0.0
        );
    }

    #[test]
    fn trailer_detection() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.insv");
        std::fs::write(&p, b"xx").unwrap();
        assert!(!has_insta360_trailer(&p), "fichier trop court");
        let mut data = vec![0u8; 10];
        data.extend_from_slice(INSTA360_TRAILER_MAGIC);
        std::fs::write(&p, &data).unwrap();
        assert!(has_insta360_trailer(&p));
        assert!(!has_insta360_trailer(&dir.path().join("absent")));
    }

    #[test]
    fn lens_pair_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("VID_20240101_120000_00_001.insv");
        let b = dir.path().join("VID_20240101_120000_10_001.insv");
        std::fs::write(&a, b"a").unwrap();
        assert!(find_lens_pair(&a).is_none());
        std::fs::write(&b, b"b").unwrap();
        assert_eq!(find_lens_pair(&a), Some(b.clone()));
        assert!(find_lens_pair(&b).is_none(), "seul le fichier _00_ cherche son partenaire");
    }

    /// Image 256×128 : deux disques (rayon `radius` × 64 px) plus clairs au centre (`inner`) qu'au bord, `outside`
    /// ailleurs ; `covered` éteint l'objectif de gauche.
    fn two_discs(inner: u8, outside: u8, radius: f64, covered: bool) -> Vec<u8> {
        let mut img = vec![outside; 256 * 128];
        for y in 0..128 {
            for x in 0..256 {
                let (cx, cy) = (if x < 128 { 64.0 } else { 192.0 }, 64.0);
                let d = ((x as f64 + 0.5 - cx).powi(2) + (y as f64 + 0.5 - cy).powi(2)).sqrt();
                if d < 64.0 * radius && !(covered && x < 128) {
                    img[y * 256 + x] = (inner as f64 * (1.0 - 0.4 * d / 64.0)) as u8;
                    // vignetage
                }
            }
        }
        img
    }

    #[test]
    fn two_black_cornered_circles_are_dual_fisheye_and_nothing_else_is() {
        assert!(is_dual_fisheye_frame(&two_discs(120, 16, 0.98, false), 256, 128));
        // fond noir un peu plus haut (compression), cercles un peu plus petits que le carré
        assert!(is_dual_fisheye_frame(&two_discs(90, 22, 0.9, false), 256, 128));
        // un panorama : de l'image sur tout le cadre
        let pano: Vec<u8> = (0..256 * 128).map(|i| (90 + (i * 37 % 97)) as u8).collect();
        assert!(!is_dual_fisheye_frame(&pano, 256, 128));
        // un panorama de nuit dont le ciel et le sol sont presque noirs mais granuleux : pas un noir uniforme
        let mut night = two_discs(120, 16, 0.98, false);
        for (i, v) in night.iter_mut().enumerate() {
            if *v == 16 {
                *v = if i % 2 == 0 { 0 } else { 40 };
            }
        }
        assert!(!is_dual_fisheye_frame(&night, 256, 128));
        // image noire, scène nocturne (disques à peine plus clairs que le fond), un objectif couvert, cercles plus
        // grands que le cadre (coins non noirs), dimensions inattendues
        assert!(!is_dual_fisheye_frame(&vec![16; 256 * 128], 256, 128));
        assert!(!is_dual_fisheye_frame(&two_discs(24, 16, 0.98, false), 256, 128));
        assert!(!is_dual_fisheye_frame(&two_discs(120, 16, 0.98, true), 256, 128));
        assert!(!is_dual_fisheye_frame(&two_discs(120, 40, 1.6, false), 256, 128));
        assert!(!is_dual_fisheye_frame(&pano, 128, 256));
        assert!(!is_dual_fisheye_frame(&pano[..100], 256, 128));
    }

    #[test]
    fn camera_files_are_fisheye_and_other_two_to_one_files_are_told_apart_by_their_frames() {
        let info =
            json!({"streams": [stream("video", 1024, 512), stream("audio", 0, 0)], "format": {"duration": "10"}});
        let probe = |name: &str, trailer: bool, fisheye_frames: bool| {
            let mut seen = None;
            let mut sniff = |d: f64| {
                seen = Some(d);
                fisheye_frames
            };
            let s = source_from_probe(Path::new(name), &info, &mut no_pair, &mut sniff, trailer, 1).unwrap();
            (s, seen)
        };
        // le .lrv de l'X3 : deux fisheye de 512 px côte à côte ; la caméra le marque, les images ne sont pas examinées
        let (s, seen) = probe("/x/LRV_20261021_004354_11_001.lrv", true, false);
        assert_eq!((s.layout, s.lens_size, s.camera.as_str()), (Layout::SbsFisheye, (512, 512), "Insta360"));
        assert_eq!(seen, None);
        assert!(s.notes.iter().any(|n| n.contains(".lrv")), "{:?}", s.notes);
        for (name, trailer) in [("/x/a.insv", false), ("/x/a.LRV", false), ("/x/copie.mp4", true)] {
            let (s, seen) = probe(name, trailer, false);
            assert_eq!((s.layout, seen), (Layout::SbsFisheye, None), "{name}");
        }
        // sans marque Insta360 : les images décident (durée transmise)
        let (s, seen) = probe("/x/copie.mp4", false, true);
        assert_eq!((s.layout, s.lens_size, s.camera.as_str(), seen), (Layout::SbsFisheye, (512, 512), "", Some(10.0)));
        let (s, seen) = probe("/x/pano.mp4", false, false);
        assert_eq!((s.layout, s.lens_size, seen), (Layout::Equirect, (1024, 512), Some(10.0)));
    }
}
