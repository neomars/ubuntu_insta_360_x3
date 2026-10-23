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
use x3studio::geometry::{Ease, Keyframe, View};
use x3studio::grade::Grade;
use x3studio::media::{probe_source, Layout, Source};
use x3studio::multi::{layout_rects, Multi, MultiLayout, TileView};
use x3studio::pipeline::{build_plan, build_snapshot_plan, run_plan, ExportSettings};
use x3studio::stick::{Stick, StickMode};
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

/// Distorsion : vues fixes, puis fondu enchaîné rectiligne → petite planète par keyframes (champ et distorsion
/// interpolés ensemble) ; comparées à la scène par une formule écrite indépendamment du moteur.
#[test]
fn distortion_shapes_the_view_and_animates_between_keyframes() {
    let c = static_clip();
    let dir = tempfile::tempdir().unwrap();
    for (view, dist) in
        [((25.0, -10.0, 4.0, 110.0), 0.5), ((-100.0, 30.0, 0.0, 150.0), 0.8), ((0.0, -90.0, 0.0, 250.0), 1.0)]
    {
        let out = dir.path().join(format!("d{dist}.mp4"));
        let s = ExportSettings {
            view: json!({"yaw": view.0, "pitch": view.1, "roll": view.2, "fov": view.3, "distortion": dist}),
            ..flat_settings(view)
        };
        export(&c, &Calibration::default(), &s, &out);
        let frames = read_frames(&out, 640, 360, "rgb24", None, Some(8));
        let got = &frames[frames.len() / 2];
        let want = expected_view_d(TEX(), "flat", 640, 360, &Mat3::identity(), view, dist);
        let good = mean_abs_diff(&downscale_rgb(got, 640, 360, 4).0, &downscale_rgb(&want, 640, 360, 4).0);
        // la même vue sans distorsion est nettement différente : le test discrimine
        let plain = expected_view_d(
            TEX(),
            "flat",
            640,
            360,
            &Mat3::identity(),
            (view.0, view.1, view.2, view.3.min(165.0)),
            0.0,
        );
        let bad = mean_abs_diff(&downscale_rgb(got, 640, 360, 4).0, &downscale_rgb(&plain, 640, 360, 4).0);
        assert!(good < 9.0, "distorsion {dist}, vue {view:?} : écart {good:.2}");
        assert!(bad > 3.0 * good, "distorsion {dist} : écart {good:.2} contre {bad:.2} sans distorsion");
    }
    // animation : à mi-parcours (smoothstep = 0,5) champ 145° et distorsion 0,5
    let out = dir.path().join("morph.mp4");
    let s = ExportSettings {
        keyframes: vec![
            x3studio::geometry::Keyframe { t: 0.0, fov: Some(90.0), distortion: Some(0.0), ..Default::default() },
            x3studio::geometry::Keyframe { t: 1.0, fov: Some(200.0), distortion: Some(1.0), ..Default::default() },
        ],
        ..flat_settings((0.0, 0.0, 0.0, 90.0))
    };
    export(&c, &Calibration::default(), &s, &out);
    let frames = read_frames(&out, 640, 360, "rgb24", None, Some(30));
    let mid = &frames[15];
    for (frame, view, dist, label) in
        [(&frames[0], (0.0, 0.0, 0.0, 90.0), 0.0, "début"), (mid, (0.0, 0.0, 0.0, 145.0), 0.5, "milieu")]
    {
        let want = expected_view_d(TEX(), "flat", 640, 360, &Mat3::identity(), view, dist);
        let err = mean_abs_diff(&downscale_rgb(frame, 640, 360, 4).0, &downscale_rgb(&want, 640, 360, 4).0);
        assert!(err < 9.0, "{label} : écart {err:.2}");
    }
    let straight = expected_view_d(TEX(), "flat", 640, 360, &Mat3::identity(), (0.0, 0.0, 0.0, 145.0), 0.0);
    let off = mean_abs_diff(&downscale_rgb(mid, 640, 360, 4).0, &downscale_rgb(&straight, 640, 360, 4).0);
    assert!(off > 12.0, "à mi-parcours l'image n'est pas rectiligne (écart {off:.2})");
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
            x3studio::geometry::Keyframe { t: 0.0, yaw: Some(0.0), pitch: Some(30.0), ..Default::default() },
            x3studio::geometry::Keyframe { t: 1.0, yaw: Some(90.0), pitch: Some(30.0), ..Default::default() },
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
    assert!((plan.tiles[0].mats[0] - want).abs().max() < 1e-12);
    assert_eq!(plan.tiles[0].fovs[0], 70.0);
}

/// Réglages de couleur : l'export concorde avec la formule de référence appliquée à l'export sans réglage.
#[test]
fn color_grade_follows_the_reference_formula() {
    let c = static_clip();
    let dir = tempfile::tempdir().unwrap();
    let view = (25.0, -10.0, 0.0, 90.0);
    let frame_of = |name: &str, grade: Grade| {
        let out = dir.path().join(name);
        export(&c, &Calibration::default(), &ExportSettings { grade, ..flat_settings(view) }, &out);
        read_frames(&out, 640, 360, "rgb24", None, Some(8)).swap_remove(4)
    };
    let plain = frame_of("plain.mp4", Grade::default());
    for (name, g, reference) in [
        ("bright", Grade { brightness: 0.15, ..Default::default() }, (0.15, 1.0, 1.0, 1.0, 0.0, 0.0)),
        ("contrast", Grade { contrast: 1.6, gamma: 1.3, ..Default::default() }, (0.0, 1.6, 1.0, 1.3, 0.0, 0.0)),
        ("grey", Grade { saturation: 0.0, ..Default::default() }, (0.0, 1.0, 0.0, 1.0, 0.0, 0.0)),
        ("vivid", Grade { saturation: 1.7, ..Default::default() }, (0.0, 1.0, 1.7, 1.0, 0.0, 0.0)),
        ("warm", Grade { warmth: 0.8, tint: -0.4, ..Default::default() }, (0.0, 1.0, 1.0, 1.0, 0.8, -0.4)),
        (
            "all",
            Grade {
                brightness: -0.1,
                contrast: 1.2,
                saturation: 1.3,
                gamma: 0.8,
                warmth: -0.5,
                tint: 0.3,
                ..Default::default()
            },
            (-0.1, 1.2, 1.3, 0.8, -0.5, 0.3),
        ),
    ] {
        let got = frame_of(&format!("{name}.mp4"), g);
        let err = mean_abs_diff(&got, &grade_rgb(&plain, reference));
        let off = mean_abs_diff(&got, &plain);
        assert!(err < 3.5, "{name} : écart {err:.2} avec la formule de référence");
        assert!(
            off > 3.0 * err,
            "{name} : l'image a peu changé (écart {off:.2} contre {err:.2} : test non discriminant)"
        );
    }
    // aucun réglage = aucune opération : l'image est identique à celle de l'export sans réglage
    let again = frame_of("neutral.mp4", Grade { contrast: 1.0, ..Default::default() });
    assert!(mean_abs_diff(&again, &plain) < 0.5);
    // la capture d'image reçoit le même traitement, en JPEG comme en PNG
    let src = probe_source(&c.path).unwrap();
    for (ext, tol) in [("png", 3.0), ("jpg", 4.0)] {
        let s = ExportSettings {
            grade: Grade { brightness: 0.15, saturation: 1.5, ..Default::default() },
            width: 640,
            height: 360,
            view: json!({"yaw": view.0, "pitch": view.1, "fov": view.3}),
            ..Default::default()
        };
        let out = dir.path().join(format!("snap.{ext}"));
        run_plan(
            build_snapshot_plan(&src, &Calibration::default(), &s, 0.5, &out, None).unwrap(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        let (img, ..) = read_image(&out);
        let err = mean_abs_diff(&img, &grade_rgb(&plain, (0.15, 1.0, 1.5, 1.0, 0.0, 0.0)));
        assert!(err < tol, "capture .{ext} : écart {err:.2} avec la formule de référence");
    }
}

/// Netteté et réduction du bruit passent par les filtres de FFmpeg : leur effet se mesure sur l'image.
#[test]
fn sharpen_adds_contrast_on_edges_and_denoise_removes_noise() {
    let dir = tempfile::tempdir().unwrap();
    let c = static_clip();
    let edges = |grade: Grade| {
        let out = dir.path().join(format!("e{}.mp4", grade.sharpen));
        export(&c, &Calibration::default(), &ExportSettings { grade, ..flat_settings((25.0, -10.0, 0.0, 90.0)) }, &out);
        let f = read_frames(&out, 640, 360, "rgb24", None, Some(8)).swap_remove(4);
        edge_energy(&f, 640, 360)
    };
    let (soft, sharp) = (edges(Grade::default()), edges(Grade { sharpen: 1.0, ..Default::default() }));
    assert!(sharp > 1.08 * soft, "contours : {soft:.2} -> {sharp:.2}");

    let noisy = clip(
        "noisy",
        SynthOptions { seconds: 1.5, fps: 30.0, lens: 480, motion: Motion::Static, noise: 10.0, ..Default::default() },
    );
    let grain = |denoise: f64| {
        let out = dir.path().join(format!("n{denoise}.mp4"));
        let s = ExportSettings {
            grade: Grade { denoise, ..Default::default() },
            crf: 8,
            ..flat_settings((25.0, -10.0, 0.0, 90.0))
        };
        export(&noisy, &Calibration::default(), &s, &out);
        let frames = read_frames(&out, 640, 360, "rgb24", Some(0.3), Some(24));
        temporal_noise(&frames)
    };
    let (before, after) = (grain(0.0), grain(1.0));
    assert!(before > 3.0, "la vidéo de test est bruitée ({before:.2})");
    assert!(after < 0.75 * before, "bruit temporel : {before:.2} -> {after:.2}");
}

/// Perche à selfie : un cône magenta de 10° autour du nadir de la caméra, vu en fisheye depuis le dessus.
#[test]
fn the_selfie_stick_zone_is_rebuilt_by_mirror_or_covered() {
    let c = clip(
        "stick",
        SynthOptions { seconds: 1.0, fps: 30.0, lens: 480, motion: Motion::Static, stick: 10.0, ..Default::default() },
    );
    let dir = tempfile::tempdir().unwrap();
    let view = (0.0, -90.0, 0.0, 120.0);
    let settings = |stick: Stick| {
        fast(ExportSettings {
            projection: "fisheye".into(),
            width: 480,
            height: 480,
            end: 0.6,
            view: json!({"yaw": view.0, "pitch": view.1, "roll": view.2, "fov": view.3}),
            stick,
            ..Default::default()
        })
    };
    let frame_of = |name: &str, stick: Stick| {
        let out = dir.path().join(name);
        export(&c, &Calibration::default(), &settings(stick), &out);
        read_frames(&out, 480, 480, "rgb24", None, Some(8)).swap_remove(4)
    };
    // 480 px pour 120° de champ : 4 px par degré ; le cône de 20° fait 80 px de rayon, la perche (10°) 40 px
    let (rho_px, axis) = (80.0, x3studio::geometry::Vec3::new(0.0, -1.0, 0.0));
    let plain = frame_of("plain.mp4", Stick::default());
    assert!(magenta_count(&plain) > 3_500, "la perche est visible sans traitement ({} pixels)", magenta_count(&plain));

    // miroir : plus aucun pixel de perche, et le centre montre l'anneau réfléchi de la scène
    let mirror = frame_of("mirror.mp4", Stick { mode: StickMode::Mirror, radius: 20.0, ..Default::default() });
    assert!(magenta_count(&mirror) < 30, "la perche a disparu ({} pixels)", magenta_count(&mirror));
    let want = expected_view_stick(
        TEX(),
        "fisheye",
        480,
        480,
        view,
        &StickRef { mode: "mirror", radius: 20.0, softness: 0.3, axis, color: [0.0; 3] },
    );
    let scene = expected_view(TEX(), "fisheye", 480, 480, &Mat3::identity(), view);
    let (inside, outside) =
        (disc_diff(&mirror, &want, 480, 480, 12.0, rho_px), disc_diff(&mirror, &want, 480, 480, 110.0, 235.0));
    let unprocessed = disc_diff(&mirror, &scene, 480, 480, 12.0, rho_px);
    assert!(inside < 14.0, "zone reconstruite : écart {inside:.1} avec la réflexion attendue");
    assert!(
        unprocessed > 1.7 * inside,
        "la zone n'est pas celle de la scène non traitée ({unprocessed:.1} contre {inside:.1})"
    );
    assert!(outside < 10.0, "hors de la zone, la scène est inchangée (écart {outside:.1})");

    // disque de couleur : vert uni au centre, bord adouci, scène inchangée au-delà
    let cover = frame_of(
        "cover.mp4",
        Stick { mode: StickMode::Cover, radius: 20.0, softness: 0.3, color: "#00ff00".into(), ..Default::default() },
    );
    assert!(magenta_count(&cover) < 30, "{} pixels de perche restent", magenta_count(&cover));
    for (r, label) in [(0usize, "centre"), (40, "à mi-chemin")] {
        let px = avg_rgb(&cover, 480, 240 + r, 240, 3);
        assert!(near(px, [0.0, 255.0, 0.0], 30.0), "{label} : {px:?}");
    }
    let want = expected_view_stick(
        TEX(),
        "fisheye",
        480,
        480,
        view,
        &StickRef { mode: "cover", radius: 20.0, softness: 0.3, axis, color: [0.0, 255.0, 0.0] },
    );
    let (disc, beyond) =
        (disc_diff(&cover, &want, 480, 480, 0.0, rho_px * 1.1), disc_diff(&cover, &want, 480, 480, 110.0, 235.0));
    assert!(disc < 14.0 && beyond < 10.0, "disque {disc:.1}, au-delà {beyond:.1}");

    // une zone désactivée ne change rien, et la zone est fixe dans le repère de la caméra : la perche ne suit pas la vue
    let off = frame_of("off.mp4", Stick { mode: StickMode::Off, radius: 40.0, ..Default::default() });
    assert!(mean_abs_diff(&off, &plain) < 1.0);
    let side = ExportSettings {
        view: json!({"yaw": 0, "pitch": 0, "fov": 90}),
        projection: "flat".into(),
        stick: Stick { mode: StickMode::Mirror, radius: 20.0, ..Default::default() },
        ..settings(Stick::default())
    };
    let out = dir.path().join("side.mp4");
    export(&c, &Calibration::default(), &side, &out);
    let horizon = read_frames(&out, 480, 480, "rgb24", None, Some(8)).swap_remove(4);
    let plain_side = {
        let out = dir.path().join("side0.mp4");
        export(&c, &Calibration::default(), &ExportSettings { stick: Stick::default(), ..side.clone() }, &out);
        read_frames(&out, 480, 480, "rgb24", None, Some(8)).swap_remove(4)
    };
    assert!(
        mean_abs_diff(&horizon, &plain_side) < 1.0,
        "regarder l'horizon : la zone du nadir n'est pas dans le champ"
    );
}

/// Export 360° et source déjà assemblée : la perche (bas de l'image équirectangulaire) disparaît aussi.
#[test]
fn the_stick_is_removed_from_360_exports_and_from_stitched_sources() {
    let dir = tempfile::tempdir().unwrap();
    let dual = clip(
        "stick360",
        SynthOptions { seconds: 1.0, fps: 30.0, lens: 480, motion: Motion::Static, stick: 10.0, ..Default::default() },
    );
    let stitched = equirect_clip(
        "stick360-eq",
        960,
        SynthOptions { seconds: 1.0, fps: 30.0, motion: Motion::Static, stick: 10.0, ..Default::default() },
    );
    for (label, src, stick) in [
        ("sans traitement", &dual, Stick::default()),
        ("miroir", &dual, Stick { mode: StickMode::Mirror, radius: 18.0, ..Default::default() }),
        (
            "disque",
            &dual,
            Stick { mode: StickMode::Cover, radius: 18.0, color: "#336699".into(), ..Default::default() },
        ),
    ] {
        let s =
            fast(ExportSettings { mode: "equirect".into(), stitch_width: 960, end: 0.5, stick, ..Default::default() });
        let out = dir.path().join(format!("{label}.mp4"));
        export(src, &Calibration::default(), &s, &out);
        let img = read_frames(&out, 960, 480, "rgb24", None, Some(6)).swap_remove(3);
        // le bas de l'image (les 60 dernières lignes, 22° autour du nadir) : le reste de la scène n'est pas en cause
        let n = magenta_count(&img[960 * 3 * 420..]);
        if label == "sans traitement" {
            assert!(n > 960 * 15, "{label} : la bande de la perche est visible ({n} pixels)");
        } else {
            assert!(n < 120, "{label} : {n} pixels de perche");
        }
    }
    // la source est déjà assemblée : le cône est évalué sur l'image équirectangulaire
    for (label, stick) in [
        ("source assemblée, miroir", Stick { mode: StickMode::Mirror, radius: 18.0, ..Default::default() }),
        ("source assemblée, disque", Stick { mode: StickMode::Cover, radius: 18.0, ..Default::default() }),
    ] {
        let s = fast(ExportSettings {
            projection: "fisheye".into(),
            width: 480,
            height: 480,
            end: 0.5,
            view: json!({"yaw": 0, "pitch": -90, "fov": 120}),
            stick,
            ..Default::default()
        });
        let out = dir.path().join(format!("{label}.mp4"));
        export(&stitched, &Calibration::default(), &s, &out);
        let img = read_frames(&out, 480, 480, "rgb24", None, Some(6)).swap_remove(3);
        assert!(magenta_count(&img) < 60, "{label} : {} pixels de perche", magenta_count(&img));
    }
    let plain = fast(ExportSettings {
        projection: "fisheye".into(),
        width: 480,
        height: 480,
        end: 0.5,
        view: json!({"yaw": 0, "pitch": -90, "fov": 120}),
        ..Default::default()
    });
    let out = dir.path().join("eq-plain.mp4");
    export(&stitched, &Calibration::default(), &plain, &out);
    let img = read_frames(&out, 480, 480, "rgb24", None, Some(6)).swap_remove(3);
    assert!(magenta_count(&img) > 3_500, "la perche de la source assemblée est visible sans traitement");
}

fn tile_view(proj: &str, yaw: f64, pitch: f64, fov: f64) -> TileView {
    TileView { projection: proj.into(), view: View { yaw, pitch, roll: 0.0, fov, distortion: 0.0 }, keyframes: vec![] }
}

/// Plusieurs perspectives : chaque rectangle de la disposition montre la vue que la scène impose à cet endroit.
#[test]
fn several_views_share_one_video() {
    let c = static_clip();
    let dir = tempfile::tempdir().unwrap();
    let main = (25.0, -10.0, 0.0, 80.0);
    let others = vec![
        tile_view("flat", -100.0, 20.0, 100.0),
        tile_view("fisheye", 160.0, 0.0, 180.0),
        tile_view("planet", 0.0, -90.0, 250.0),
    ];
    let spec = |i: usize| -> (&str, (f64, f64, f64, f64)) {
        if i == 0 {
            ("flat", main)
        } else {
            let v = &others[i - 1];
            (v.projection.as_str(), (v.view.yaw, v.view.pitch, 0.0, v.view.fov))
        }
    };
    for (layout, gap) in [
        (MultiLayout::Side, 0),
        (MultiLayout::Stack, 0),
        (MultiLayout::Pip, 0),
        (MultiLayout::Trio, 0),
        (MultiLayout::Quad, 0),
        (MultiLayout::Quad, 8),
    ] {
        let s = fast(ExportSettings {
            width: 640,
            height: 360,
            end: 1.0,
            view: json!({"yaw": main.0, "pitch": main.1, "roll": main.2, "fov": main.3}),
            multi: Multi { layout, gap, views: others.clone() },
            ..Default::default()
        });
        let out = dir.path().join(format!("{layout:?}{gap}.mp4"));
        export(&c, &Calibration::default(), &s, &out);
        let frame = read_frames(&out, 640, 360, "rgb24", None, Some(8)).swap_remove(4);
        for (i, r) in layout_rects(layout, 640, 360, gap).iter().enumerate() {
            let (proj, view) = spec(i);
            let (w, h) = (r.w as usize, r.h as usize);
            // l'incrustation recouvre un coin de la vue principale : on ne compare que le quart supérieur gauche
            let (cw, ch) = if layout == MultiLayout::Pip && i == 0 { (w / 2, h / 2) } else { (w, h) };
            let got = crop_rgb(&frame, 640, (r.x as usize, r.y as usize, cw, ch));
            let err = |v: (f64, f64, f64, f64)| {
                let want = expected_view(TEX(), proj, w, h, &Mat3::identity(), v);
                let want = crop_rgb(&want, w, (0, 0, cw, ch));
                mean_abs_diff(&downscale_rgb(&got, cw, ch, 4).0, &downscale_rgb(&want, cw, ch, 4).0)
            };
            let good = err(view);
            let shifted = err((view.0 + 25.0, view.1, view.2, view.3));
            assert!(good < 10.0, "{layout:?} (écart {gap}) vue {} : écart {good:.2} avec la scène attendue", i + 1);
            assert!(
                shifted > 1.5 * good,
                "{layout:?} vue {} : {good:.2} contre {shifted:.2} décalée (test non discriminant)",
                i + 1
            );
        }
        if gap > 0 {
            // les écarts entre les vues sont noirs
            for (x, y) in [(320, 90), (320, 270), (160, 180), (480, 180)] {
                let px = avg_rgb(&frame, 640, x, y, 1);
                assert!(near(px, [0.0, 0.0, 0.0], 14.0), "écart noir en ({x}, {y}) : {px:?}");
            }
        }
    }
}

/// Le fond des écarts entre les vues reçoit les réglages d'image une seule fois, sur chaque image : les tampons de sortie
/// sont recyclés d'une image à l'autre, et un tampon déjà corrigé ne doit pas l'être une seconde fois.
#[test]
fn gaps_between_views_get_the_color_adjustment_once_on_every_frame() {
    let c = static_clip();
    let dir = tempfile::tempdir().unwrap();
    let s = fast(ExportSettings {
        width: 640,
        height: 360,
        end: 1.0,
        view: json!({"yaw": 25.0, "pitch": -10.0, "fov": 80.0}),
        grade: Grade { brightness: 0.25, ..Default::default() },
        multi: Multi { layout: MultiLayout::Side, gap: 8, views: vec![tile_view("flat", -100.0, 20.0, 100.0)] },
        ..Default::default()
    });
    let out = dir.path().join("ecart.mp4");
    export(&c, &Calibration::default(), &s, &out);
    let frames = read_frames(&out, 640, 360, "rgb24", None, None);
    assert_eq!(frames.len(), 30);
    // le noir (0, 0, 0) après les réglages
    let black = grade_rgb(&[0, 0, 0], (0.25, 1.0, 1.0, 1.0, 0.0, 0.0));
    let want = [f64::from(black[0]), f64::from(black[1]), f64::from(black[2])];
    assert!(want[0] > 55.0 && want[0] < 75.0, "fond de référence {want:?}");
    // l'écart entre les deux vues occupe les colonnes 316 à 323
    for (k, frame) in frames.iter().enumerate() {
        for (x, y) in [(320, 90), (320, 270)] {
            let px = avg_rgb(frame, 640, x, y, 1);
            assert!(near(px, want, 6.0), "image {k}, écart en ({x}, {y}) : {px:?} au lieu de {want:?}");
        }
    }
}

/// Chaque vue a ses propres keyframes ; l'image de sortie suit celles de chaque rectangle.
#[test]
fn each_view_follows_its_own_keyframes() {
    let c = static_clip();
    let dir = tempfile::tempdir().unwrap();
    let kf = |t: f64, yaw: f64, fov: f64| Keyframe {
        t,
        yaw: Some(yaw),
        pitch: Some(0.0),
        roll: Some(0.0),
        fov: Some(fov),
        ..Default::default()
    };
    let s = fast(ExportSettings {
        width: 640,
        height: 360,
        end: 1.0,
        keyframes: vec![kf(0.0, 25.0, 80.0), kf(1.0, -60.0, 80.0)],
        multi: Multi {
            layout: MultiLayout::Side,
            gap: 0,
            views: vec![TileView {
                keyframes: vec![kf(0.0, 0.0, 70.0), kf(1.0, 90.0, 110.0)],
                ..tile_view("flat", 0.0, 0.0, 90.0)
            }],
        },
        ..Default::default()
    });
    let out = dir.path().join("kf.mp4");
    export(&c, &Calibration::default(), &s, &out);
    let frames = read_frames(&out, 640, 360, "rgb24", None, None);
    assert_eq!(frames.len(), 30);
    let smooth = |x: f64| x * x * (3.0 - 2.0 * x);
    for k in [0usize, 15, 29] {
        let u = smooth(k as f64 / 30.0);
        let want = [(25.0 + (-85.0) * u, 80.0), (90.0 * u, 70.0 + 40.0 * u)];
        for (i, (yaw, fov)) in want.iter().enumerate() {
            let (x, w) = (i * 320, 320usize);
            let got = crop_rgb(&frames[k], 640, (x, 0, w, 360));
            let exp = expected_view(TEX(), "flat", w, 360, &Mat3::identity(), (*yaw, 0.0, 0.0, *fov));
            let err = mean_abs_diff(&downscale_rgb(&got, w, 360, 4).0, &downscale_rgb(&exp, w, 360, 4).0);
            assert!(err < 10.0, "image {k}, vue {} : écart {err:.2} (cap {yaw:.1}°, champ {fov:.1}°)", i + 1);
        }
    }
    // les deux vues bougent : l'image du milieu diffère de celle du début dans chaque moitié
    for i in 0..2 {
        let (a, b) =
            (crop_rgb(&frames[0], 640, (i * 320, 0, 320, 360)), crop_rgb(&frames[29], 640, (i * 320, 0, 320, 360)));
        assert!(mean_abs_diff(&a, &b) > 15.0, "vue {} immobile", i + 1);
    }
}

/// Dispositions : tailles trop petites, valeurs inconnues et écart invalide sont refusés avant tout rendu.
#[test]
fn invalid_layouts_are_refused_up_front() {
    let c = static_clip();
    let src = probe_source(&c.path).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let base = ExportSettings {
        width: 128,
        height: 72,
        multi: Multi { layout: MultiLayout::Quad, ..Default::default() },
        ..Default::default()
    };
    let e = build_plan(&src, &Calibration::default(), &base, &dir.path().join("a.mp4"), None).err().unwrap();
    assert!(is_user_error(&e) && e.to_string().contains("trop petite"), "{e}");
    let bad_gap = ExportSettings {
        multi: Multi { layout: MultiLayout::Side, gap: 6, ..Default::default() },
        ..Default::default()
    };
    assert!(build_plan(&src, &Calibration::default(), &bad_gap, &dir.path().join("b.mp4"), None).is_err());
    // l'export 360° ignore la disposition : une seule image équirectangulaire
    let s = ExportSettings {
        mode: "equirect".into(),
        multi: Multi { layout: MultiLayout::Quad, ..Default::default() },
        ..Default::default()
    };
    let plan = build_plan(&src, &Calibration::default(), &s, &dir.path().join("c.mp4"), None).unwrap();
    assert_eq!(plan.tiles.len(), 1);
}

/// Keyframes enregistrés (interpolation linéaire) : vitesse constante ; sans cela, départ et arrivée en douceur.
#[test]
fn linear_keyframes_move_at_a_constant_speed() {
    let c = static_clip();
    let dir = tempfile::tempdir().unwrap();
    let kf = |t: f64, yaw: f64, ease: Option<Ease>| Keyframe {
        t,
        yaw: Some(yaw),
        fov: Some(80.0),
        ease,
        ..Default::default()
    };
    let frames_for = |name: &str, ease: Option<Ease>| {
        let s = fast(ExportSettings {
            width: 640,
            height: 360,
            end: 1.0,
            keyframes: vec![kf(0.0, 0.0, ease), kf(1.0, 80.0, None)],
            ..Default::default()
        });
        let out = dir.path().join(name);
        export(&c, &Calibration::default(), &s, &out);
        read_frames(&out, 640, 360, "rgb24", None, None)
    };
    let (linear, smooth) = (frames_for("lin.mp4", Some(Ease::Linear)), frames_for("smooth.mp4", None));
    let k = 7usize; // t = 0,2333 s
    let t = k as f64 / 30.0;
    let (lin_yaw, smooth_yaw) = (80.0 * t, 80.0 * t * t * (3.0 - 2.0 * t));
    assert!((lin_yaw - smooth_yaw).abs() > 7.0, "le cas discrimine : {lin_yaw:.1}° contre {smooth_yaw:.1}°");
    let err = |frame: &[u8], yaw: f64| {
        let want = expected_view(TEX(), "flat", 640, 360, &Mat3::identity(), (yaw, 0.0, 0.0, 80.0));
        mean_abs_diff(&downscale_rgb(frame, 640, 360, 4).0, &downscale_rgb(&want, 640, 360, 4).0)
    };
    assert!(err(&linear[k], lin_yaw) < 9.0 && err(&linear[k], smooth_yaw) > 2.0 * err(&linear[k], lin_yaw));
    assert!(err(&smooth[k], smooth_yaw) < 9.0 && err(&smooth[k], lin_yaw) > 2.0 * err(&smooth[k], smooth_yaw));
}
