//! Analyse du mouvement et stabilisation de bout en bout : vidéo synthétique qui tremble (orientation vraie de
//! la caméra connue) → aperçu VP9 → estimation → matrices de stabilisation → export.
mod common;

use common::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use x3studio::calibration::Calibration;
use x3studio::error::{is_cancelled, is_user_error};
use x3studio::geometry::{Mat3, Ypr};
use x3studio::media::{probe_source, Source};
use x3studio::pipeline::{build_plan, build_proxy_cmd, run_ffmpeg, run_plan, ExportSettings};
use x3studio::stabilize::{analyze_video, calibration_key, stabilization_matrices, Orientation, StabMode};
use x3studio::synth::{Motion, SynthOptions};

const LENS: u32 = 480;

fn shake_clip() -> Clip {
    clip("shake", SynthOptions { seconds: 2.5, fps: 30.0, lens: LENS, motion: Motion::Shake, ..Default::default() })
}

fn make_proxy(src: &Source, dir: &std::path::Path) -> PathBuf {
    let proxy = dir.join("proxy.webm");
    run_ffmpeg(&build_proxy_cmd(src, &proxy, LENS), src.duration, &|_| {}, &AtomicBool::new(false)).unwrap();
    proxy
}

/// Clip, orientation estimée et vérité terrain (analysés une seule fois pour tous les tests du fichier).
fn analysed() -> &'static (Source, Orientation, Vec<Mat3>, tempfile::TempDir) {
    static A: OnceLock<(Source, Orientation, Vec<Mat3>, tempfile::TempDir)> = OnceLock::new();
    A.get_or_init(|| {
        let c = shake_clip();
        let src = probe_source(&c.path).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let proxy = make_proxy(&src, dir.path());
        let o = analyze_video(&src, &Calibration::default(), &proxy, &|_| {}, &AtomicBool::new(false)).unwrap();
        (src, o, c.truth, dir)
    })
}

/// Rotation « monde » d'un pixel de sortie : `C_t · T_t` doit rester constante si la vidéo est stable.
fn residual(truth: &[Mat3], t: &[Mat3]) -> f64 {
    let world: Vec<Mat3> = truth.iter().zip(t).map(|(c, t)| c * t).collect();
    (world.iter().map(|m| angle_between(&world[0], m).powi(2)).sum::<f64>() / world.len() as f64).sqrt()
}

#[test]
fn the_estimated_orientation_follows_the_true_camera() {
    let (src, o, truth, _) = analysed();
    assert!(o.n().abs_diff(truth.len()) <= 1, "{} orientations pour {} images", o.n(), truth.len());
    assert!((o.fps - src.fps).abs() < 1e-9);
    let n = o.n().min(truth.len());
    let est = o.matrices();
    let errors: Vec<f64> = (0..n).map(|i| angle_between(&(truth[0].transpose() * truth[i]), &est[i])).collect();
    let worst = errors.iter().cloned().fold(0.0, f64::max);
    println!("erreur max {worst:.3}°, moyenne {:.3}°", errors.iter().sum::<f64>() / n as f64);
    assert!(worst < 0.25, "erreur d'orientation max {worst:.3}°");
    let mut inliers = o.inliers[1..].to_vec();
    inliers.sort_unstable();
    assert!(inliers[inliers.len() / 2] > 100, "points retenus : {inliers:?}");
    assert_eq!(o.key, calibration_key(&Calibration::default()));
}

#[test]
fn stabilization_modes_remove_the_shake() {
    let (_, o, truth, _) = analysed();
    let shake = (truth.iter().map(|c| angle_between(&truth[0], c).powi(2)).sum::<f64>() / truth.len() as f64).sqrt();
    assert!(shake > 2.0, "la vidéo de test doit trembler réellement : {shake:.2}°");
    let user = Ypr::default();
    for (mode, strength, limit) in
        [(StabMode::Lock, 1.0, 0.25), (StabMode::Smooth, 1.0, 0.5), (StabMode::Horizon, 1.0, 0.45)]
    {
        let t = stabilization_matrices(Some(o), mode, strength, &user, 0, None, o.n());
        assert_eq!(t.len(), o.n());
        let r = residual(truth, &t[..truth.len().min(t.len())]);
        println!("{mode:?} : résidu {r:.3}°");
        assert!(r < limit, "{mode:?} : résidu {r:.3}° (limite {limit}°)");
    }
}

#[test]
fn smoothing_strength_is_monotonic() {
    let (_, o, truth, _) = analysed();
    let res: Vec<f64> = [0.1, 0.3, 1.0]
        .iter()
        .map(|&s| {
            let t = stabilization_matrices(Some(o), StabMode::Smooth, s, &Ypr::default(), 0, None, o.n());
            residual(truth, &t[..truth.len().min(t.len())])
        })
        .collect();
    assert!(res[0] > res[1] && res[1] > res[2], "{res:?}");
}

#[test]
fn a_stabilized_export_is_steady() {
    let (src, o, _, _) = analysed();
    let dir = tempfile::tempdir().unwrap();
    let s = ExportSettings {
        width: 480,
        height: 270,
        view: serde_json::json!({"yaw": 20, "fov": 90}),
        crf: 26,
        preset: "ultrafast".into(),
        ..Default::default()
    };
    let (shaky, steady) = (dir.path().join("shaky.mp4"), dir.path().join("steady.mp4"));
    let plan = build_plan(src, &Calibration::default(), &s, &shaky, None).unwrap();
    run_plan(plan, &|_| {}, &AtomicBool::new(false)).unwrap();
    let lock = stabilization_matrices(Some(o), StabMode::Lock, 1.0, &Ypr::default(), 0, None, o.n());
    let plan = build_plan(src, &Calibration::default(), &s, &steady, Some(&lock)).unwrap();
    run_plan(plan, &|_| {}, &AtomicBool::new(false)).unwrap();
    let (a, b) = (mean_shift(&shaky, 480, 270), mean_shift(&steady, 480, 270));
    println!("décalage moyen entre images : {a:.2} px (sans) → {b:.2} px (verrouillé)");
    assert!(a > 2.0, "la version non stabilisée doit trembler : {a:.2} px");
    assert!(b < 0.5 && b < a / 8.0, "{a:.2} px → {b:.2} px");
}

/// Ordonnée (px, sous-pixel) de la ligne d'horizon blanche autour de la colonne `x` : le maximum de « blancheur »
/// (plus petit des trois canaux), moyennée sur 17 colonnes, puis centroïde autour du pic.
fn horizon_row(frame: &[u8], w: usize, h: usize, x: usize) -> f64 {
    let rows: Vec<f64> = (0..h)
        .map(|y| {
            (x - 8..=x + 8).map(|xx| f64::from(*frame[(y * w + xx) * 3..][..3].iter().min().unwrap())).sum::<f64>()
                / 17.0
        })
        .collect();
    let peak = (0..h).max_by(|&a, &b| rows[a].partial_cmp(&rows[b]).unwrap()).unwrap();
    let (lo, hi) = (peak.saturating_sub(3), (peak + 3).min(h - 1));
    let weights: Vec<f64> = (lo..=hi).map(|y| (rows[y] - 100.0).max(0.0)).collect();
    (lo..=hi).zip(&weights).map(|(y, w)| y as f64 * w).sum::<f64>() / weights.iter().sum::<f64>()
}

#[test]
fn the_horizon_mode_keeps_the_horizon_level_end_to_end() {
    // Vidéo qui tremble (roulis jusqu'à ±3,5°). Avec l'attitude de la première image comme « orientation » (c'est ce
    // que règle l'utilisateur pour mettre l'horizon à plat) et le mode horizon, la ligne d'horizon de la scène
    // reste horizontale et au centre de la vue ; sans stabilisation elle penche et bouge.
    let (src, o, truth, _) = analysed();
    let (yaw, pitch, roll) = x3studio::geometry::matrix_to_ypr(&truth[0]);
    let user = Ypr { yaw, pitch, roll };
    let t = stabilization_matrices(Some(o), StabMode::Horizon, 1.0, &user, 0, None, o.n());
    let dir = tempfile::tempdir().unwrap();
    let (w, h) = (480usize, 270usize);
    let s = ExportSettings {
        width: w as u32,
        height: h as u32,
        end: 2.0,
        crf: 20,
        preset: "ultrafast".into(),
        ..Default::default()
    };
    let (level, shaky) = (dir.path().join("level.mp4"), dir.path().join("shaky.mp4"));
    run_plan(build_plan(src, &Calibration::default(), &s, &level, Some(&t)).unwrap(), &|_| {}, &AtomicBool::new(false))
        .unwrap();
    run_plan(build_plan(src, &Calibration::default(), &s, &shaky, None).unwrap(), &|_| {}, &AtomicBool::new(false))
        .unwrap();
    let spread = |video: &std::path::Path| -> (f64, f64) {
        let frames = read_frames(video, w, h, "rgb24", None, None);
        let (mut tilt, mut off) = (0.0f64, 0.0f64);
        for f in frames.iter().step_by(4) {
            let (l, r) = (horizon_row(f, w, h, w / 5), horizon_row(f, w, h, 4 * w / 5));
            tilt = tilt.max((l - r).abs());
            off = off.max(((l + r) / 2.0 - h as f64 / 2.0).abs());
        }
        (tilt, off)
    };
    let (lvl_tilt, lvl_off) = spread(&level);
    let (shk_tilt, shk_off) = spread(&shaky);
    println!("horizon stabilisé : inclinaison {lvl_tilt:.2} px, décalage {lvl_off:.2} px ; sans : {shk_tilt:.2} px, {shk_off:.2} px");
    assert!(
        lvl_tilt < 2.5 && lvl_off < 4.0,
        "horizon non stabilisé : {lvl_tilt:.2} px de pente, {lvl_off:.2} px de décalage"
    );
    assert!(
        shk_tilt > 2.0 * lvl_tilt + 2.0 && shk_off > lvl_off,
        "la vidéo non stabilisée doit trembler plus : {shk_tilt:.2}/{shk_off:.2}"
    );
}

#[test]
fn the_lens_swap_applies_to_the_analysis_too() {
    let c = shake_clip();
    let swapped = derive(
        &c,
        "swap",
        "swapped.insv",
        &["-map", "0:v:1", "-map", "0:v:0", "-map", "0:a", "-c", "copy", "-f", "mp4"],
    );
    let src = probe_source(&swapped).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let proxy = make_proxy(&src, dir.path());
    let err = |calib: &Calibration| -> f64 {
        let o = analyze_video(&src, calib, &proxy, &|_| {}, &AtomicBool::new(false)).unwrap();
        let est = o.matrices();
        (0..o.n().min(c.truth.len()))
            .map(|i| angle_between(&(c.truth[0].transpose() * c.truth[i]), &est[i]))
            .fold(0.0, f64::max)
    };
    let good = err(&Calibration { swap_lenses: true, ..Default::default() });
    let bad = err(&Calibration::default());
    println!("avec swap : {good:.3}°, sans : {bad:.3}°");
    assert!(good < 0.3, "{good}");
    assert!(bad > 4.0 * good, "sans l'échange des objectifs l'estimation devrait être fausse : {bad} contre {good}");
}

#[test]
fn an_equirect_source_can_be_analysed() {
    let c = equirect_clip(
        "equirect-shake",
        1024,
        SynthOptions { seconds: 2.0, fps: 30.0, motion: Motion::Shake, ..Default::default() },
    );
    let src = probe_source(&c.path).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let proxy = make_proxy(&src, dir.path());
    let o = analyze_video(&src, &Calibration::default(), &proxy, &|_| {}, &AtomicBool::new(false)).unwrap();
    let est = o.matrices();
    let worst = (0..o.n().min(c.truth.len()))
        .map(|i| angle_between(&(c.truth[0].transpose() * c.truth[i]), &est[i]))
        .fold(0.0, f64::max);
    assert!(worst < 0.3, "erreur max {worst:.3}°");
}

#[test]
fn analysis_can_be_cancelled_and_reports_bad_proxies() {
    let (src, ..) = analysed();
    let dir = tempfile::tempdir().unwrap();
    let needle = format!("apercu-annule-{}", std::process::id());
    let proxy = dir.path().join(format!("{needle}.webm"));
    run_ffmpeg(&build_proxy_cmd(src, &proxy, LENS), src.duration, &|_| {}, &AtomicBool::new(false)).unwrap();
    let cancel = AtomicBool::new(false);
    let e = analyze_video(
        src,
        &Calibration::default(),
        &proxy,
        &|p| {
            if p > 0.1 {
                cancel.store(true, Ordering::Relaxed)
            }
        },
        &cancel,
    )
    .unwrap_err();
    assert!(is_cancelled(&e), "{e}");
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(processes_mentioning(&needle).is_empty(), "ffmpeg orphelin : {:?}", processes_mentioning(&needle));
    // aperçu illisible
    let junk = dir.path().join("junk.webm");
    std::fs::write(&junk, b"ceci n'est pas une video").unwrap();
    let e = analyze_video(src, &Calibration::default(), &junk, &|_| {}, &AtomicBool::new(false)).unwrap_err();
    assert!(is_user_error(&e) && e.to_string().contains("Analyse impossible"), "{e}");
    // aperçu absent
    let e =
        analyze_video(src, &Calibration::default(), &dir.path().join("absent.webm"), &|_| {}, &AtomicBool::new(false))
            .unwrap_err();
    assert!(is_user_error(&e), "{e}");
}
