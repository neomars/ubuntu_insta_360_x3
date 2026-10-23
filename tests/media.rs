//! Fichiers réels : détection du format, fichiers abîmés, métadonnées sphériques sur un vrai MP4.
mod common;

use common::*;
use x3studio::error::is_user_error;
use x3studio::media::{looks_like_dual_fisheye, probe_source, Layout, INSTA360_TRAILER_MAGIC};
use x3studio::spherical::{has_spherical_v1, inject_spherical_v1};
use x3studio::synth::{Motion, SynthOptions};

fn x3_clip() -> Clip {
    clip("media-x3", SynthOptions { seconds: 1.0, fps: 30.0, lens: 96, motion: Motion::Static, ..Default::default() })
}

#[test]
fn the_insta360_trailer_is_detected_without_breaking_the_file() {
    let c = x3_clip();
    let dir = tempfile::tempdir().unwrap();
    let plain = probe_source(&c.path).unwrap();
    assert!(!plain.has_trailer);
    // les .insv de la caméra se terminent par 32 octets de signature : ffprobe et ffmpeg lisent quand même le fichier
    let with = dir.path().join("VID_20240101_120000_00_001.insv");
    let mut bytes = std::fs::read(&c.path).unwrap();
    bytes.extend_from_slice(INSTA360_TRAILER_MAGIC);
    std::fs::write(&with, &bytes).unwrap();
    let s = probe_source(&with).unwrap();
    assert!(s.has_trailer, "signature de fin de fichier");
    assert_eq!((s.layout, s.lens_size, s.frames(), s.camera.as_str()), (Layout::TwoStreams, (96, 96), 30, "Insta360"));
    assert_eq!(s.size_bytes, bytes.len() as u64);
    assert_eq!(
        read_frames(&with, 96, 96, "gray", None, None).len(),
        30,
        "le décodage du premier flux fonctionne malgré la signature"
    );
    assert_eq!(probe_video(&with).frames, 30);
}

#[test]
fn the_extension_does_not_decide_the_layout() {
    let c = x3_clip();
    let dir = tempfile::tempdir().unwrap();
    let mp4 = dir.path().join("deux flux.mp4");
    std::fs::copy(&c.path, &mp4).unwrap();
    let s = probe_source(&mp4).unwrap();
    assert_eq!((s.layout, s.camera.as_str()), (Layout::TwoStreams, ""), "deux flux vidéo carrés = deux objectifs");
    // chemin relatif et lien symbolique
    let link = dir.path().join("lien.insv");
    std::os::unix::fs::symlink(&mp4, &link).unwrap();
    let s2 = probe_source(&link).unwrap();
    assert_eq!(s2.path, std::fs::canonicalize(&mp4).unwrap().to_str().unwrap(), "chemin canonique");
}

#[test]
fn a_two_to_one_file_is_side_by_side_fisheye_or_a_panorama_according_to_its_frames() {
    let c = x3_clip();
    // le .lrv de la caméra : reconnu à son nom
    let lrv = derive(&c, "lrv", "LRV_20261021_004354_11_001.lrv", &HSTACK);
    let s = probe_source(&lrv).unwrap();
    assert_eq!((s.layout, s.lens_size, s.camera.as_str()), (Layout::SbsFisheye, (96, 96), "Insta360"));
    assert!(s.notes.iter().any(|n| n.contains("basse résolution")), "{:?}", s.notes);
    // le même flux sans nom ni signature : reconnu à ses images (deux cercles sur fond noir)
    let plain = derive(&c, "lrv", "copie.mp4", &HSTACK);
    let s = probe_source(&plain).unwrap();
    assert_eq!((s.layout, s.lens_size, s.camera.as_str()), (Layout::SbsFisheye, (96, 96), ""));
    assert!(looks_like_dual_fisheye(&plain, s.duration));
    // un panorama équirectangulaire 2:1 qui remplit le cadre reste un panorama
    let pano = equirect_clip(
        "media-pano",
        512,
        SynthOptions { seconds: 1.0, fps: 30.0, motion: Motion::Static, with_audio: false, ..Default::default() },
    );
    let s = probe_source(&pano.path).unwrap();
    assert_eq!((s.layout, s.lens_size), (Layout::Equirect, (512, 256)));
    assert!(!looks_like_dual_fisheye(&pano.path, s.duration));
    // l'aperçu et l'export de la copie sans marque se comportent comme ceux d'un fichier de la caméra
    assert_eq!(probe_video(&plain).frames, 30);
}

#[test]
fn damaged_and_foreign_files_are_media_errors() {
    require_ffmpeg();
    let c = x3_clip();
    let dir = tempfile::tempdir().unwrap();
    let bytes = std::fs::read(&c.path).unwrap();
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("vide.insv", vec![]),
        ("texte.mp4", b"ceci n'est pas une video du tout".to_vec()),
        ("tronque.insv", bytes[..bytes.len() / 3].to_vec()),
        ("entete.mp4", bytes[..40].to_vec()),
        ("binaire.mkv", (0..4096u32).map(|i| (i * 31 % 251) as u8).collect()),
    ];
    for (name, data) in cases {
        let f = dir.path().join(name);
        std::fs::write(&f, data).unwrap();
        let e = probe_source(&f).err().unwrap_or_else(|| panic!("{name} ne devrait pas être accepté"));
        assert!(is_user_error(&e) && !e.to_string().is_empty(), "{name} : {e}");
    }
    assert!(is_user_error(&probe_source(dir.path()).err().unwrap()), "un dossier");
    assert!(is_user_error(&probe_source(&dir.path().join("absent.insv")).err().unwrap()));
    // une image fixe n'est pas une vidéo 360° exploitable ici : message clair, pas de panique
    let img = dir.path().join("photo.jpg");
    let still = derive(&c, "still", "still.jpg", &["-frames:v", "1", "-map", "0:v:0"]);
    std::fs::copy(still, &img).unwrap();
    let r = probe_source(&img);
    assert!(r.is_err() || r.unwrap().duration >= 0.0);
}

#[test]
fn spherical_metadata_on_a_real_mp4() {
    let c = equirect_clip(
        "media-eq",
        512,
        SynthOptions { seconds: 1.0, fps: 30.0, motion: Motion::Static, with_audio: false, ..Default::default() },
    );
    let dir = tempfile::tempdir().unwrap();
    // sans faststart (moov à la fin, comme le produit l'écrit en mode 360°)
    let f = dir.path().join("360.mp4");
    let st = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(&c.path)
        .args(["-c", "copy", "-movflags", "0"])
        .arg(&f)
        .status()
        .unwrap();
    assert!(st.success());
    let before = read_frames(&f, 512, 256, "gray", None, Some(3));
    let size0 = std::fs::metadata(&f).unwrap().len();
    assert!(!has_spherical_v1(&f));
    assert!(inject_spherical_v1(&f).unwrap());
    assert!(has_spherical_v1(&f));
    let size1 = std::fs::metadata(&f).unwrap().len();
    assert!(size1 > size0 && size1 - size0 < 1024, "une seule petite boîte ajoutée : +{} octets", size1 - size0);
    // le fichier est toujours parfaitement lisible, images identiques
    assert_eq!(read_frames(&f, 512, 256, "gray", None, Some(3)), before);
    let info = probe_video(&f);
    assert_eq!((info.w, info.h, info.frames), (512, 256, 30));
    // idempotent
    assert!(inject_spherical_v1(&f).unwrap());
    assert_eq!(std::fs::metadata(&f).unwrap().len(), size1);
    // ffprobe voit la balise (side data / tags selon les versions) ou au moins la boîte `uuid` dans la piste vidéo
    let out = std::process::Command::new("ffprobe").args(["-v", "trace", "-i"]).arg(&f).output().unwrap();
    let log = String::from_utf8_lossy(&out.stderr);
    assert!(
        log.contains("uuid") || log.contains("spherical") || log.contains("Spherical"),
        "boîte non vue par ffprobe"
    );
    // faststart (moov au début) : le fichier n'est pas touché
    let fs_file = dir.path().join("faststart.mp4");
    assert!(std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(&c.path)
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(&fs_file)
        .status()
        .unwrap()
        .success());
    let before = std::fs::read(&fs_file).unwrap();
    assert!(!inject_spherical_v1(&fs_file).unwrap());
    assert_eq!(std::fs::read(&fs_file).unwrap(), before);
    // fichier illisible : Ok(false) ou erreur d'E/S, jamais de panique ni de modification
    let junk = dir.path().join("junk.mp4");
    std::fs::write(&junk, b"0123456789abcdef0123456789").unwrap();
    assert!(!inject_spherical_v1(&junk).unwrap());
    assert_eq!(std::fs::read(&junk).unwrap(), b"0123456789abcdef0123456789");
    assert!(inject_spherical_v1(&dir.path().join("absent.mp4")).is_err());
}
