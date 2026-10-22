//! Export complet (décodage ffmpeg → rendu → encodage ffmpeg) sur des vidéos synthétiques à vérité terrain connue.
//!
//! Les vues exportées sont comparées à ce que *doit* montrer la texture de la scène (formules de la
//! spécification réécrites dans `common`) : une erreur de convention (signe d'un axe, objectif inversé,
//! ordre des rotations) se voit tout de suite, car un décalage de 8° multiplie l'écart par plus de 10.
mod common;

use common::*;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use x3studio::calibration::Calibration;
use x3studio::error::{is_cancelled, is_user_error};
use x3studio::geometry::{ypr_to_matrix, Mat3, Ypr};
use x3studio::media::{probe_source, Layout, Source};
use x3studio::pipeline::{build_plan, build_snapshot_plan, run_plan, ExportSettings};
use x3studio::synth::{render_equirect, Motion, SynthOptions};

const TEX: fn() -> &'static x3studio::synth::Texture = texture;

fn static_clip() -> Clip {
    clip("static", SynthOptions { seconds: 1.5, fps: 30.0, lens: 480, motion: Motion::Static, ..Default::default() })
}

fn fast(s: ExportSettings) -> ExportSettings {
    ExportSettings { crf: 20, preset: "ultrafast".into(), ..s }
}

fn export_src(src: &Source, calib: &Calibration, s: &ExportSettings, out: &Path) -> PathBuf {
    let plan = build_plan(src, calib, s, out, None).unwrap();
    run_plan(plan, &|_| {}, &AtomicBool::new(false)).unwrap()
}

fn export(clip: &Clip, calib: &Calibration, s: &ExportSettings, out: &Path) -> PathBuf {
    export_src(&probe_source(&clip.path).unwrap(), calib, s, out)
}

/// Écart moyen (images réduites 4x) entre l'image `got` et la vue attendue de la texture.
fn view_error(got: &[u8], proj: &str, w: usize, h: usize, view: (f64, f64, f64, f64)) -> f64 {
    let want = expected_view(TEX(), proj, w, h, &Mat3::identity(), view);
    mean_abs_diff(&downscale_rgb(got, w, h, 4).0, &downscale_rgb(&want, w, h, 4).0)
}

fn flat_settings(view: (f64, f64, f64, f64)) -> ExportSettings {
    fast(ExportSettings {
        width: 640,
        height: 360,
        end: 1.0,
        view: json!({"yaw": view.0, "pitch": view.1, "roll": view.2, "fov": view.3}),
        ..Default::default()
    })
}

/// La vue exportée correspond à la scène, et nettement mieux qu'à la même scène décalée de 8° en cap.
fn assert_matches_scene(video: &Path, proj: &str, view: (f64, f64, f64, f64), label: &str) {
    let frames = read_frames(video, 640, 360, "rgb24", None, Some(8));
    let got = &frames[frames.len() / 2];
    let good = view_error(got, proj, 640, 360, view);
    let shifted = view_error(got, proj, 640, 360, (view.0 + 8.0, view.1, view.2, view.3));
    assert!(good < 9.0, "{label} : écart {good:.2} avec la scène attendue");
    assert!(
        shifted > 4.0 * good,
        "{label} : écart {good:.2} contre {shifted:.2} pour une vue décalée de 8° (test non discriminant)"
    );
}

#[test]
fn views_match_the_scene_for_each_projection() {
    let c = static_clip();
    let dir = tempfile::tempdir().unwrap();
    for (proj, view) in [
        ("flat", (25.0, -10.0, 4.0, 80.0)),
        ("flat", (-100.0, 30.0, 0.0, 120.0)),
        ("fisheye", (0.0, 0.0, 0.0, 180.0)),
        ("fisheye", (160.0, -20.0, 10.0, 200.0)),
        ("planet", (0.0, 0.0, 0.0, 250.0)),
    ] {
        let out = dir.path().join(format!("{proj}-{}.mp4", view.0));
        let s = ExportSettings { projection: proj.into(), ..flat_settings(view) };
        export(&c, &Calibration::default(), &s, &out);
        assert_matches_scene(&out, proj, view, &format!("{proj} {view:?}"));
    }
}

#[test]
fn the_user_orientation_recentres_the_view() {
    // orientation « cap 90° » : le centre de la sortie regarde la direction O⁻¹ · z = -x (la bande jaune)
    let c = static_clip();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("orient.mp4");
    let s = ExportSettings {
        orientation: Ypr { yaw: 90.0, pitch: 0.0, roll: 0.0 },
        ..flat_settings((0.0, 0.0, 0.0, 70.0))
    };
    export(&c, &Calibration::default(), &s, &out);
    let frame = &read_frames(&out, 640, 360, "rgb24", None, Some(3))[1];
    assert!(near(avg_rgb(frame, 640, 320, 90, 4), [255.0, 255.0, 0.0], 60.0), "{:?}", avg_rgb(frame, 640, 320, 90, 4));
}

#[test]
fn equirect_export_is_a_360_video_with_metadata() {
    let c = static_clip();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("360.mp4");
    let s = fast(ExportSettings { mode: "equirect".into(), stitch_width: 1024, end: 1.0, ..Default::default() });
    export(&c, &Calibration::default(), &s, &out);
    let info = probe_video(&out);
    assert_eq!((info.w, info.h, info.frames, info.codec.as_str()), (1024, 512, 30, "h264"));
    assert!(x3studio::spherical::has_spherical_v1(&out), "balises vidéo sphérique absentes");
    // l'image est celle de la scène vue par la caméra immobile : rouge au centre, vert à droite, jaune à gauche
    let frame = &read_frames(&out, 1024, 512, "rgb24", None, Some(3))[1];
    for (x, target) in [(512, [255.0, 0.0, 0.0]), (768, [0.0, 255.0, 0.0]), (256, [255.0, 255.0, 0.0])] {
        let c = avg_rgb(frame, 1024, x, 90, 3);
        assert!(near(c, target, 70.0), "x = {x} : {c:?}");
    }
    let mut want = vec![0u8; 1024 * 512 * 3];
    render_equirect(TEX(), &Mat3::identity(), 1024, 512, &mut want);
    let err = mean_abs_diff(&downscale_rgb(frame, 1024, 512, 4).0, &downscale_rgb(&want, 1024, 512, 4).0);
    assert!(err < 9.0, "écart {err:.2}");
    // métadonnées désactivées
    let plain = dir.path().join("plain.mp4");
    export(&c, &Calibration::default(), &ExportSettings { spherical_metadata: false, ..s }, &plain);
    assert!(!x3studio::spherical::has_spherical_v1(&plain));
    // l'orientation tourne l'image : cap +90° → le centre montre la direction -x (jaune)
    let turned = dir.path().join("turned.mp4");
    let s = fast(ExportSettings {
        mode: "equirect".into(),
        stitch_width: 512,
        end: 0.5,
        orientation: Ypr { yaw: 90.0, pitch: 0.0, roll: 0.0 },
        ..Default::default()
    });
    export(&c, &Calibration::default(), &s, &turned);
    let frame = &read_frames(&turned, 512, 256, "rgb24", None, Some(2))[1];
    assert!(near(avg_rgb(frame, 512, 256, 45, 2), [255.0, 255.0, 0.0], 70.0));
}

#[test]
fn keyframes_move_the_view_over_time() {
    let c = static_clip();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("kf.mp4");
    let s = fast(ExportSettings {
        width: 320,
        height: 180,
        end: 1.2,
        view: json!({"fov": 60}),
        keyframes: vec![
            x3studio::geometry::Keyframe { t: 0.0, yaw: Some(0.0), pitch: Some(30.0), roll: None, fov: None },
            x3studio::geometry::Keyframe { t: 1.0, yaw: Some(90.0), pitch: Some(30.0), roll: None, fov: None },
        ],
        ..Default::default()
    });
    export(&c, &Calibration::default(), &s, &out);
    let frames = read_frames(&out, 320, 180, "rgb24", None, None);
    assert_eq!(frames.len(), 36);
    // première image : la bande rouge (cap 0°) au centre ; à partir de 1 s : la bande verte (cap 90°)
    assert!(
        near(avg_rgb(&frames[0], 320, 160, 90, 2), [255.0, 0.0, 0.0], 70.0),
        "{:?}",
        avg_rgb(&frames[0], 320, 160, 90, 2)
    );
    assert!(
        near(avg_rgb(&frames[35], 320, 160, 90, 2), [0.0, 255.0, 0.0], 70.0),
        "{:?}",
        avg_rgb(&frames[35], 320, 160, 90, 2)
    );
    // au milieu (0,5 s), la vue est à mi-chemin (smoothstep) : ni rouge ni verte au centre
    let mid = avg_rgb(&frames[15], 320, 160, 90, 2);
    assert!(!near(mid, [255.0, 0.0, 0.0], 70.0) && !near(mid, [0.0, 255.0, 0.0], 70.0), "{mid:?}");
}

#[test]
fn container_properties_follow_the_settings() {
    let c = static_clip();
    let dir = tempfile::tempdir().unwrap();
    let run = |name: &str, s: ExportSettings| {
        let out = dir.path().join(name);
        export(&c, &Calibration::default(), &fast(s), &out);
        (probe_video(&out), streams(&out), out)
    };
    let base = ExportSettings { width: 320, height: 180, ..Default::default() };
    let (v, st, _) = run("a.mp4", ExportSettings { end: 1.0, ..base.clone() });
    assert_eq!((v.w, v.h, v.frames, v.codec.as_str()), (320, 180, 30, "h264"));
    assert!((v.fps - 30.0).abs() < 1e-6 && (v.duration - 1.0).abs() < 0.1, "{} s", v.duration);
    assert_eq!(st, vec![("video".to_string(), "h264".to_string()), ("audio".into(), "aac".into())]);
    // plage
    let (v, ..) = run("range.mp4", ExportSettings { start: 0.5, end: 1.0, ..base.clone() });
    assert!((14..=16).contains(&v.frames), "{} images", v.frames);
    // vitesse x2 : moitié moins long (les images en trop sont écartées pour garder 30 i/s)
    let (v, ..) = run("speed.mp4", ExportSettings { speed: 2.0, ..base.clone() });
    assert!((v.duration - 0.75).abs() < 0.12 && (22..=24).contains(&v.frames), "{} s, {} images", v.duration, v.frames);
    // ralenti x0,5 : deux fois plus long, cadence conservée
    let (v, ..) = run("slow.mp4", ExportSettings { speed: 0.5, end: 1.0, ..base.clone() });
    assert!((v.duration - 2.0).abs() < 0.15 && (v.fps - 30.0).abs() < 1e-6, "{} s", v.duration);
    // cadence de sortie
    let (v, ..) = run("fps.mp4", ExportSettings { fps: 10.0, end: 1.0, ..base.clone() });
    assert!((v.fps - 10.0).abs() < 1e-6 && (9..=11).contains(&v.frames), "{} i/s, {} images", v.fps, v.frames);
    // sans audio
    let (_, st, _) = run("mute.mp4", ExportSettings { audio: false, end: 1.0, ..base.clone() });
    assert_eq!(st.len(), 1);
    // H.265 : étiquette hvc1 pour QuickTime / Apple
    let (v, _, out) = run("hevc.mp4", ExportSettings { codec: "h265".into(), end: 1.0, ..base.clone() });
    assert_eq!(v.codec, "hevc");
    assert_eq!(video_tag(&out), "hvc1");
    // faststart : le moov est avant le mdat
    let (_, _, out) = run("fs.mp4", ExportSettings { end: 0.5, ..base.clone() });
    let bytes = std::fs::read(&out).unwrap();
    let pos = |tag: &[u8]| bytes.windows(4).position(|w| w == tag).unwrap();
    assert!(pos(b"moov") < pos(b"mdat"), "moov doit précéder mdat (lecture en continu)");
    // source sans piste audio
    let silent = clip(
        "silent",
        SynthOptions {
            seconds: 1.0,
            fps: 30.0,
            lens: 96,
            motion: Motion::Static,
            with_audio: false,
            ..Default::default()
        },
    );
    let out = dir.path().join("silent.mp4");
    export(&silent, &Calibration::default(), &fast(base), &out);
    assert_eq!(streams(&out).len(), 1);
}

#[test]
fn snapshots_are_jpeg_or_png_of_the_requested_view() {
    let c = static_clip();
    let src = probe_source(&c.path).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let view = (30.0, 5.0, 0.0, 90.0);
    let s = ExportSettings {
        width: 640,
        height: 360,
        view: json!({"yaw": 30, "pitch": 5, "fov": 90}),
        ..Default::default()
    };
    for (name, codec) in [("s.jpg", "mjpeg"), ("s.png", "png")] {
        let out = dir.path().join(name);
        let plan = build_snapshot_plan(&src, &Calibration::default(), &s, 0.5, &out, None).unwrap();
        run_plan(plan, &|_| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(streams(&out), vec![("video".to_string(), codec.to_string())], "{name}");
        let (img, w, h) = read_image(&out);
        assert_eq!((w, h), (640, 360));
        assert!(view_error(&img, "flat", 640, 360, view) < 9.0, "{name}");
    }
    // instant hors de la vidéo : borné à la dernière image
    let out = dir.path().join("late.jpg");
    let plan = build_snapshot_plan(&src, &Calibration::default(), &s, 99.0, &out, None).unwrap();
    run_plan(plan, &|_| {}, &AtomicBool::new(false)).unwrap();
    assert!(out.is_file());
}

#[test]
fn lens_layouts_and_swapped_lenses_give_the_same_picture() {
    let c = static_clip();
    let dir = tempfile::tempdir().unwrap();
    let view = (40.0, 0.0, 0.0, 90.0);
    let s = flat_settings(view);
    // deux fichiers `_00_` / `_10_` (anciens modèles)
    let a = derive(
        &c,
        "pair",
        "VID_20240101_120000_00_001.insv",
        &["-map", "0:v:0", "-map", "0:a", "-c", "copy", "-f", "mp4"],
    );
    derive(&c, "pair", "VID_20240101_120000_10_001.insv", &["-map", "0:v:1", "-c", "copy", "-f", "mp4"]);
    let src = probe_source(&a).unwrap();
    assert_eq!(src.layout, Layout::TwoFiles);
    export_src(&src, &Calibration::default(), &s, &dir.path().join("pair.mp4"));
    assert_matches_scene(&dir.path().join("pair.mp4"), "flat", view, "deux fichiers");
    // deux fisheye côte à côte dans un seul flux
    let sbs = derive(
        &c,
        "sbs",
        "sbs.insv",
        &[
            "-filter_complex",
            "[0:v:0][0:v:1]hstack=inputs=2[v]",
            "-map",
            "[v]",
            "-map",
            "0:a",
            "-c:v",
            "libx264",
            "-crf",
            "14",
            "-preset",
            "veryfast",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "copy",
            "-f",
            "mp4",
        ],
    );
    let src = probe_source(&sbs).unwrap();
    assert_eq!(src.layout, Layout::SbsFisheye);
    export_src(&src, &Calibration::default(), &s, &dir.path().join("sbs.mp4"));
    assert_matches_scene(&dir.path().join("sbs.mp4"), "flat", view, "côte à côte");
    // flux dans l'ordre inverse : sans correction l'image est celle de l'arrière ; `swap_lenses` la remet d'aplomb
    let swapped = derive(
        &c,
        "swap",
        "swapped.insv",
        &["-map", "0:v:1", "-map", "0:v:0", "-map", "0:a", "-c", "copy", "-f", "mp4"],
    );
    let src = probe_source(&swapped).unwrap();
    let wrong = dir.path().join("wrong.mp4");
    export_src(&src, &Calibration::default(), &s, &wrong);
    let frame = &read_frames(&wrong, 640, 360, "rgb24", None, Some(4))[2];
    assert!(view_error(frame, "flat", 640, 360, view) > 25.0, "l'image doit être fausse sans swap_lenses");
    let fixed = dir.path().join("fixed.mp4");
    export_src(&src, &Calibration { swap_lenses: true, ..Default::default() }, &s, &fixed);
    assert_matches_scene(&fixed, "flat", view, "objectifs permutés");
}

#[test]
fn an_equirect_source_is_reframed_directly() {
    let c = equirect_clip(
        "equirect",
        1024,
        SynthOptions { seconds: 1.0, fps: 30.0, motion: Motion::Static, ..Default::default() },
    );
    let src = probe_source(&c.path).unwrap();
    assert_eq!((src.layout, src.lens_size), (Layout::Equirect, (1024, 512)));
    let dir = tempfile::tempdir().unwrap();
    for (proj, view) in [("flat", (-30.0, 10.0, 0.0, 100.0)), ("planet", (0.0, 0.0, 0.0, 250.0))] {
        let out = dir.path().join(format!("{proj}.mp4"));
        export_src(
            &src,
            &Calibration::default(),
            &ExportSettings { projection: proj.into(), ..flat_settings(view) },
            &out,
        );
        assert_matches_scene(&out, proj, view, &format!("source équirectangulaire, {proj}"));
    }
    // ré-export 360° d'une vidéo déjà assemblée : l'image est conservée
    let out = dir.path().join("again.mp4");
    export_src(
        &src,
        &Calibration::default(),
        &fast(ExportSettings { mode: "equirect".into(), end: 0.5, ..Default::default() }),
        &out,
    );
    let frame = &read_frames(&out, 1024, 512, "rgb24", None, Some(2))[1];
    let mut want = vec![0u8; 1024 * 512 * 3];
    render_equirect(TEX(), &Mat3::identity(), 1024, 512, &mut want);
    assert!(mean_abs_diff(&downscale_rgb(frame, 1024, 512, 4).0, &downscale_rgb(&want, 1024, 512, 4).0) < 9.0);
}

#[test]
fn the_lens_field_of_view_calibration_is_taken_into_account() {
    // une calibration d'objectifs légèrement fausse (champ 190° au lieu de 200°) déforme la scène :
    // l'écart avec la vérité augmente, ce qui prouve que le champ de l'objectif est bien pris en compte
    let c = static_clip();
    let dir = tempfile::tempdir().unwrap();
    let view = (0.0, 0.0, 0.0, 150.0);
    let (good, bad) = (dir.path().join("good.mp4"), dir.path().join("bad.mp4"));
    export(&c, &Calibration::default(), &flat_settings(view), &good);
    export(&c, &Calibration { fov: 185.0, ..Default::default() }, &flat_settings(view), &bad);
    let err = |p: &Path| view_error(&read_frames(p, 640, 360, "rgb24", None, Some(4))[2], "flat", 640, 360, view);
    assert!(err(&good) < 9.0 && err(&bad) > 1.5 * err(&good), "{} vs {}", err(&good), err(&bad));
}

#[test]
fn paths_with_spaces_and_accents_work() {
    let c = static_clip();
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("Été 2024 — clip n°1.insv");
    std::fs::copy(&c.path, &input).unwrap();
    let out_dir = dir.path().join("dossier de sortie é");
    std::fs::create_dir_all(&out_dir).unwrap();
    let out = out_dir.join("résultat (1).mp4");
    let src = probe_source(&input).unwrap();
    let s = fast(ExportSettings { width: 160, height: 90, end: 0.5, ..Default::default() });
    export_src(&src, &Calibration::default(), &s, &out);
    assert_eq!(probe_video(&out).frames, 15);
}

#[test]
fn cancelling_removes_the_partial_file_and_leaves_no_process() {
    let c = static_clip();
    let src = probe_source(&c.path).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let needle = format!("annule-{}", std::process::id());
    let out = dir.path().join(format!("{needle}.mp4"));
    let s = fast(ExportSettings { width: 640, height: 360, ..Default::default() });
    let plan = build_plan(&src, &Calibration::default(), &s, &out, None).unwrap();
    let cancel = AtomicBool::new(false);
    let e = run_plan(
        plan,
        &|p| {
            if p > 0.05 {
                cancel.store(true, Ordering::Relaxed)
            }
        },
        &cancel,
    )
    .unwrap_err();
    assert!(is_cancelled(&e), "{e}");
    assert!(!out.exists(), "la sortie partielle doit être supprimée");
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(processes_mentioning(&needle).is_empty(), "{:?}", processes_mentioning(&needle));
    // annulation avant même le début
    let plan = build_plan(&src, &Calibration::default(), &s, &out, None).unwrap();
    let e = run_plan(plan, &|_| {}, &AtomicBool::new(true)).unwrap_err();
    assert!(is_cancelled(&e));
    assert!(!out.exists());
}

#[test]
fn failures_are_reported_with_ffmpeg_messages() {
    let c = static_clip();
    let src = probe_source(&c.path).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let s = fast(ExportSettings { width: 160, height: 90, end: 0.5, ..Default::default() });
    // décodeur : le fichier a disparu
    let out = dir.path().join("a.mp4");
    let gone = Source { lens_files: vec!["/nulle/part.insv".into(); 2], ..src.clone() };
    let plan = build_plan(&gone, &Calibration::default(), &s, &out, None).unwrap();
    let e = run_plan(plan, &|_| {}, &AtomicBool::new(false)).unwrap_err();
    assert!(
        is_user_error(&e) && e.to_string().contains("Aucune image décodée") && e.to_string().contains("part.insv"),
        "{e}"
    );
    assert!(!out.exists());
    // encodeur : dossier de sortie inexistant
    let out = dir.path().join("pas/de/dossier/b.mp4");
    let plan = build_plan(&src, &Calibration::default(), &s, &out, None).unwrap();
    let e = run_plan(plan, &|_| {}, &AtomicBool::new(false)).unwrap_err();
    assert!(is_user_error(&e) && !e.to_string().is_empty(), "{e}");
    // paramètres invalides : erreurs claires avant tout lancement
    for (bad, needle) in [
        (ExportSettings { width: 161, ..s.clone() }, "paires"),
        (ExportSettings { start: 50.0, ..s.clone() }, "dépasse"),
        (ExportSettings { projection: "cube".into(), ..s.clone() }, "Projection"),
        (ExportSettings { codec: "av1".into(), ..s.clone() }, "Codec"),
    ] {
        let e = build_plan(&src, &Calibration::default(), &bad, &dir.path().join("c.mp4"), None).err().unwrap();
        assert!(is_user_error(&e) && e.to_string().contains(needle), "{needle} : {e}");
    }
}

#[test]
fn user_orientation_matrix_is_applied_in_the_plan() {
    // la matrice de vue d'un plan sans stabilisation est O⁻¹ · V (orientation utilisateur, puis vue)
    let c = static_clip();
    let src = probe_source(&c.path).unwrap();
    let s = ExportSettings {
        orientation: Ypr { yaw: 20.0, pitch: -5.0, roll: 3.0 },
        view: json!({"yaw": 10, "pitch": 4, "roll": 1, "fov": 70}),
        end: 0.2,
        ..Default::default()
    };
    let plan = build_plan(&src, &Calibration::default(), &s, Path::new("/tmp/x.mp4"), None).unwrap();
    let want = ypr_to_matrix(20.0, -5.0, 3.0).transpose() * ypr_to_matrix(10.0, 4.0, 1.0);
    assert!((plan.mats[0] - want).abs().max() < 1e-12);
    assert_eq!(plan.fovs[0], 70.0);
}
