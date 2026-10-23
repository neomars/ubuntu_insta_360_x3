//! Suivi de sujet de bout en bout : vidéo synthétique (un sujet mobile devant un décor, caméra qui tremble) → aperçu
//! VP9 → analyse du mouvement → suivi → keyframes → export dont le centre doit montrer le sujet.
mod common;

use common::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use x3studio::calibration::Calibration;
use x3studio::error::is_cancelled;
use x3studio::geometry::Ypr;
use x3studio::geometry::{Mat3, Vec3};
use x3studio::media::{probe_source, Source};
use x3studio::pipeline::{build_plan, build_proxy_cmd, run_ffmpeg, run_plan, ExportSettings};
use x3studio::stabilize::{analyze_video, stabilization_matrices, StabMode};
use x3studio::synth::{Motion, Subject, SynthOptions};
use x3studio::tracker::{samples_to_keyframes, track_video, Track, TrackParams, KEYFRAME_TOLERANCE};

const LENS: u32 = 480;
const SECONDS: f64 = 3.0;

fn subject() -> Subject {
    Subject { radius: 7.0, from: (15.0, 5.0), to: (85.0, 15.0), wobble: 4.0, until: f64::INFINITY }
}

fn make_proxy(src: &Source, dir: &Path) -> PathBuf {
    let proxy = dir.join("proxy.webm");
    run_ffmpeg(&build_proxy_cmd(src, &proxy, LENS), src.duration, &|_| {}, &AtomicBool::new(false)).unwrap();
    proxy
}

fn dir_of(yaw: f64, pitch: f64) -> Vec3 {
    let (y, p) = (yaw.to_radians(), pitch.to_radians());
    Vec3::new(p.cos() * y.sin(), p.sin(), p.cos() * y.cos())
}

fn yaw_pitch(d: &Vec3) -> (f64, f64) {
    (d.x.atan2(d.z).to_degrees(), d.y.asin().to_degrees())
}

fn angle(a: (f64, f64), b: (f64, f64)) -> f64 {
    dir_of(a.0, a.1).dot(&dir_of(b.0, b.1)).clamp(-1.0, 1.0).acos().to_degrees()
}

/// Tout ce qu'il faut pour suivre : clip qui tremble, aperçu, stabilisation « verrouillée » (la vue reste dans le
/// repère de la première image), et ce que le suivi doit trouver (direction vraie du sujet dans ce repère).
struct Scene {
    src: Source,
    proxy: PathBuf,
    stab: Vec<Mat3>,
    truth: Vec<Mat3>,
    _dir: tempfile::TempDir,
}

fn shaking_scene() -> &'static Scene {
    static S: OnceLock<Scene> = OnceLock::new();
    S.get_or_init(|| {
        let c = clip(
            "subject-shake",
            SynthOptions {
                seconds: SECONDS,
                fps: 30.0,
                lens: LENS,
                motion: Motion::Shake,
                subject: Some(subject()),
                ..Default::default()
            },
        );
        let src = probe_source(&c.path).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let proxy = make_proxy(&src, dir.path());
        let o = analyze_video(&src, &Calibration::default(), &proxy, &|_| {}, &AtomicBool::new(false)).unwrap();
        let stab = stabilization_matrices(Some(&o), StabMode::Lock, 1.0, &Ypr::default(), 0, None, o.n());
        Scene { src, proxy, stab, truth: c.truth, _dir: dir }
    })
}

/// Direction vraie du sujet à l'image `i`, dans le repère de la vue (celui de la première image : mode verrouillé).
fn expected(s: &Scene, i: usize) -> (f64, f64) {
    let (yaw, pitch) = subject().at(i as f64 / s.src.fps, SECONDS);
    yaw_pitch(&(s.truth[0].transpose() * dir_of(yaw, pitch)))
}

fn run_track(s: &Scene, p: &TrackParams) -> Track {
    track_video(&s.src, &Calibration::default(), &s.proxy, Some(&s.stab), p, &|_| {}, &AtomicBool::new(false)).unwrap()
}

#[test]
fn the_subject_is_followed_through_the_preview_video_with_the_stabilization() {
    let s = shaking_scene();
    let i0 = 20;
    let (yaw, pitch) = expected(s, i0);
    // le point désigné est à 1° du centre du sujet : l'écart doit rester celui-là
    let p = TrackParams {
        t0: i0 as f64 / 30.0,
        yaw: yaw + 0.7,
        pitch: pitch - 0.7,
        size: 12.0,
        start: 0.0,
        end: SECONDS,
        backward: true,
    };
    let t = run_track(s, &p);
    assert_eq!((t.lost_after, t.lost_before), (None, None), "{t:?}");
    assert!(
        t.samples.len() >= 85 && t.samples.len() <= 91,
        "{} échantillons pour {} images",
        t.samples.len(),
        s.src.frames()
    );
    let errors: Vec<f64> = t
        .samples
        .iter()
        .map(|x| angle((x.yaw - 0.7, x.pitch + 0.7), expected(s, (x.t * 30.0).round() as usize)))
        .collect();
    let worst = errors.iter().cloned().fold(0.0, f64::max);
    let mean = errors.iter().sum::<f64>() / errors.len() as f64;
    println!("suivi : écart moyen {mean:.2}°, maximal {worst:.2}°");
    assert!(mean < 1.3 && worst < 3.0, "écart moyen {mean:.2}°, maximal {worst:.2}° avec la trajectoire vraie");
    // sans stabilisation, le suivi se fait dans le repère de la caméra : il retrouve aussi le sujet (la caméra tremble peu)
    let free =
        track_video(&s.src, &Calibration::default(), &s.proxy, None, &p, &|_| {}, &AtomicBool::new(false)).unwrap();
    assert_eq!(free.lost_after, None);
    assert!(free.samples.len() >= 85);
}

#[test]
fn tracked_keyframes_keep_the_subject_at_the_centre_of_the_export() {
    let s = shaking_scene();
    let i0 = 10;
    let (yaw, pitch) = expected(s, i0);
    let p = TrackParams { t0: i0 as f64 / 30.0, yaw, pitch, size: 12.0, start: 0.0, end: SECONDS, backward: true };
    let track = run_track(s, &p);
    let keyframes = samples_to_keyframes(&track.samples, KEYFRAME_TOLERANCE);
    assert!(keyframes.len() >= 3 && keyframes.len() < 40, "{} keyframes", keyframes.len());
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("suivi.mp4");
    let settings = ExportSettings {
        width: 640,
        height: 360,
        crf: 20,
        preset: "ultrafast".into(),
        audio: false,
        keyframes,
        view: serde_json::json!({"fov": 60}),
        ..Default::default()
    };
    let plan = build_plan(&s.src, &Calibration::default(), &settings, &out, Some(&s.stab)).unwrap();
    run_plan(plan, &|_| {}, &AtomicBool::new(false)).unwrap();
    let frames = read_frames(&out, 640, 360, "rgb24", None, None);
    // fraction de pixels « couleur du sujet » (orange ou cyan) dans une fenêtre de 80 x 80 pixels : environ 0,67 sur le sujet
    // (certaines nuances sombres, la compression et les bords sortent des seuils), presque 0 sur le décor
    let sprite = |img: &[u8], (x0, y0, w, h): (usize, usize, usize, usize)| {
        let mut n = 0usize;
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                let p = &img[(y * 640 + x) * 3..][..3];
                let orange = p[0] > 90 && p[1] > 35 && f64::from(p[0]) > 1.4 * f64::from(p[1]) && p[2] < 50;
                let cyan = p[0] < 50 && p[1] > 70 && p[2] > 100;
                n += usize::from(orange || cyan);
            }
        }
        n as f64 / (w * h) as f64
    };
    let centred: Vec<f64> = frames.iter().map(|f| sprite(f, (280, 140, 80, 80))).collect();
    let good = centred.iter().filter(|&&f| f > 0.5).count();
    assert!(
        good as f64 >= 0.9 * frames.len() as f64,
        "sujet au centre sur {good} images sur {} : {centred:.2?}",
        frames.len()
    );
    // sans le suivi (vue fixe), le sujet traverse l'image et n'est presque jamais au centre
    let still = ExportSettings { keyframes: vec![], ..settings };
    let out2 = dir.path().join("fixe.mp4");
    run_plan(
        build_plan(&s.src, &Calibration::default(), &still, &out2, Some(&s.stab)).unwrap(),
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    let still_frames = read_frames(&out2, 640, 360, "rgb24", None, None);
    let still_good = still_frames.iter().filter(|f| sprite(f, (280, 140, 80, 80)) > 0.5).count();
    assert!(
        still_good < frames.len() / 3,
        "vue fixe : sujet au centre sur {still_good} images (test non discriminant)"
    );
}

#[test]
fn the_subject_is_lost_when_it_leaves_the_scene_and_the_job_can_be_cancelled() {
    let c = clip(
        "subject-static",
        SynthOptions {
            seconds: SECONDS,
            fps: 30.0,
            lens: LENS,
            motion: Motion::Static,
            subject: Some(Subject { until: 1.5, ..subject() }),
            ..Default::default()
        },
    );
    let src = probe_source(&c.path).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let proxy = make_proxy(&src, dir.path());
    let (yaw, pitch) = subject().at(0.3, SECONDS);
    let p = TrackParams { t0: 0.3, yaw, pitch, size: 12.0, start: 0.0, end: SECONDS, backward: false };
    let t = track_video(&src, &Calibration::default(), &proxy, None, &p, &|_| {}, &AtomicBool::new(false)).unwrap();
    let lost = t.lost_after.expect("sujet perdu");
    assert!((1.5..2.0).contains(&lost), "perte à {lost:.2} s (le sujet disparaît à 1,5 s)");
    assert!(t.samples.iter().all(|s| s.t < lost) && t.samples.first().unwrap().t == 0.3, "{:?}", t.samples.first());
    assert_eq!(t.lost_before, None, "pas de recul demandé");
    // annulation : le suivi s'arrête tout de suite avec l'erreur d'annulation
    let cancel = AtomicBool::new(false);
    let seen = std::cell::Cell::new(0usize);
    let e = track_video(
        &src,
        &Calibration::default(),
        &proxy,
        None,
        &p,
        &|f| {
            seen.set(seen.get() + 1);
            if f > 0.2 {
                cancel.store(true, Ordering::Relaxed);
            }
        },
        &cancel,
    )
    .expect_err("annulé");
    assert!(is_cancelled(&e), "{e}");
    assert!(seen.get() < 60);
}
