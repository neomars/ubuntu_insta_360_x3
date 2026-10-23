//! Ligne de commande : le vrai binaire `x3studio`, lancé comme le ferait un utilisateur.
mod common;

use common::*;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use x3studio::synth::{Motion, SynthOptions};

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        require_ffmpeg();
        Env { dir: tempfile::tempdir().unwrap() }
    }

    fn out(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn x3(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_x3studio"))
            .args(args)
            .env("X3STUDIO_HOME", self.dir.path().join("home"))
            .env("X3STUDIO_OUTPUT", self.dir.path().join("sorties"))
            .stdin(Stdio::null())
            .output()
            .expect("lancement de x3studio")
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn static_clip() -> Clip {
    clip(
        "cli-static",
        SynthOptions { seconds: 1.5, fps: 30.0, lens: 480, motion: Motion::Static, ..Default::default() },
    )
}

fn p(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn version_and_help() {
    let e = Env::new();
    let v = e.x3(&["--version"]);
    assert!(
        v.status.success() && text(&v.stdout).trim() == format!("x3studio {}", x3studio::VERSION),
        "{}",
        text(&v.stdout)
    );
    let h = e.x3(&["--help"]);
    let help = text(&h.stdout);
    assert!(h.status.success());
    for sub in ["gui", "probe", "reframe", "equirect", "snapshot", "demo"] {
        assert!(help.contains(sub), "{sub} absent de l'aide :\n{help}");
    }
    let r = e.x3(&["reframe", "--help"]);
    let help = text(&r.stdout);
    for opt in [
        "--output",
        "--start",
        "--end",
        "--speed",
        "--fps",
        "--codec",
        "--crf",
        "--preset",
        "--no-audio",
        "--stabilize",
        "--strength",
        "--heading",
        "--orient",
        "--calibration",
        "--dry-run",
        "--output-dir",
        "--projection",
        "--size",
        "--yaw",
        "--pitch",
        "--roll",
        "--fov",
        "--keyframes",
    ] {
        assert!(help.contains(opt), "{opt} absent de l'aide de reframe");
    }
}

#[test]
fn probe_text_and_json() {
    let e = Env::new();
    let c = static_clip();
    let r = e.x3(&["probe", p(&c.path)]);
    let out = text(&r.stdout);
    assert!(r.status.success(), "{}", text(&r.stderr));
    assert!(
        out.contains("deux flux fisheye") && out.contains("480×480 par objectif") && out.contains("45 images"),
        "{out}"
    );
    assert!(out.contains("Audio     : oui — métadonnées Insta360 : non"), "{out}");
    let j: Value = serde_json::from_str(&text(&e.x3(&["probe", p(&c.path), "--json"]).stdout)).unwrap();
    assert_eq!(
        (j["layout"].as_str(), j["frames"].as_u64(), j["lens_size"][0].as_u64()),
        (Some("two_streams"), Some(45), Some(480))
    );
}

#[test]
fn errors_are_friendly_and_exit_codes_are_right() {
    let e = Env::new();
    let r = e.x3(&["probe", p(&e.out("absent.insv"))]);
    assert_eq!(r.status.code(), Some(1));
    let err = text(&r.stderr);
    assert!(
        err.starts_with("Erreur : ")
            && err.contains("introuvable")
            && !err.contains("panicked")
            && !err.contains("backtrace"),
        "{err}"
    );
    assert!(r.stdout.is_empty());
    // option invalide : code 2 (usage) et message de clap
    let r = e.x3(&["reframe", p(&static_clip().path), "--codec", "vp9"]);
    assert_eq!(r.status.code(), Some(2));
    assert!(text(&r.stderr).contains("vp9"), "{}", text(&r.stderr));
    // erreur de paramètres d'export
    let c = static_clip();
    let r = e.x3(&["reframe", p(&c.path), "-o", p(&e.out("x.mp4")), "--size", "321x180"]);
    assert_eq!(r.status.code(), Some(1));
    assert!(text(&r.stderr).contains("paires"), "{}", text(&r.stderr));
    assert!(!e.out("x.mp4").exists());
    // `nan` / `inf` sont des nombres pour l'analyseur d'options : refusés avec un message clair
    for opt in [["--yaw", "nan"], ["--fov", "inf"], ["--start", "nan"], ["--speed", "inf"]] {
        let r = e.x3(&["reframe", p(&c.path), "--dry-run", opt[0], opt[1]]);
        assert_eq!(r.status.code(), Some(1), "{opt:?}");
        assert!(text(&r.stderr).contains("Valeur invalide"), "{opt:?} : {}", text(&r.stderr));
    }
    // fichier qui n'est pas une vidéo
    let junk = e.out("faux.insv");
    std::fs::write(&junk, b"pas une video").unwrap();
    let r = e.x3(&["probe", p(&junk)]);
    assert_eq!(r.status.code(), Some(1));
    assert!(text(&r.stderr).starts_with("Erreur : "));
    // calibration illisible
    let cal = e.out("cal.json");
    std::fs::write(&cal, b"{oups").unwrap();
    let r = e.x3(&["reframe", p(&c.path), "--calibration", p(&cal), "--dry-run"]);
    assert_eq!(r.status.code(), Some(1));
    assert!(text(&r.stderr).contains("Calibration illisible"), "{}", text(&r.stderr));
}

#[test]
fn reframe_writes_the_video_and_prints_its_path() {
    let e = Env::new();
    let c = static_clip();
    let out = e.out("sortie é.mp4");
    let r = e.x3(&[
        "reframe",
        p(&c.path),
        "-o",
        p(&out),
        "--size",
        "320x180",
        "--yaw",
        "30",
        "--crf",
        "30",
        "--preset",
        "ultrafast",
        "--end",
        "1",
    ]);
    assert!(r.status.success(), "{}", text(&r.stderr));
    assert_eq!(text(&r.stdout).trim(), p(&out));
    let info = probe_video(&out);
    assert_eq!((info.w, info.h, info.frames), (320, 180, 30));
    let err = text(&r.stderr);
    assert!(err.contains("Rendu →") && err.contains("export : 100 %"), "{err}");
    // nom par défaut : dossier de sortie de l'environnement, sans écraser l'existant
    let args = [
        "reframe",
        p(&c.path),
        "--size",
        "160x90",
        "--preset",
        "ultrafast",
        "--end",
        "0.5",
        "--no-audio",
        "--fps",
        "10",
    ];
    let first = text(&e.x3(&args).stdout);
    let second = text(&e.x3(&args).stdout);
    let stem = c.path.file_stem().unwrap().to_str().unwrap();
    let dir = e.dir.path().join("sorties");
    assert_eq!(first.trim(), p(&dir.join(format!("{stem}_flat.mp4"))));
    assert_eq!(second.trim(), p(&dir.join(format!("{stem}_flat_2.mp4"))));
    assert_eq!(streams(&dir.join(format!("{stem}_flat.mp4"))).len(), 1);
    assert!((probe_video(&dir.join(format!("{stem}_flat.mp4"))).fps - 10.0).abs() < 1e-6);
    // --output-dir
    let r = e.x3(&[
        "reframe",
        p(&c.path),
        "--output-dir",
        p(&e.out("ailleurs")),
        "--size",
        "160x90",
        "--preset",
        "ultrafast",
        "--end",
        "0.3",
    ]);
    assert!(r.status.success(), "{}", text(&r.stderr));
    assert!(e.out("ailleurs").join(format!("{stem}_flat.mp4")).is_file());
}

#[test]
fn dry_run_prints_the_commands_and_writes_nothing() {
    let e = Env::new();
    let c = static_clip();
    let r = e.x3(&["reframe", p(&c.path), "--dry-run", "--size", "320x180", "--start", "0.5"]);
    assert!(r.status.success(), "{}", text(&r.stderr));
    let out = text(&r.stdout);
    assert!(
        out.contains("ffmpeg")
            && out.contains("rawvideo")
            && out.contains("<rendu x3studio>")
            && out.contains("320x180"),
        "{out}"
    );
    assert!(!out.contains("v360"), "plus aucun filtre v360 dans le produit : {out}");
    assert!(!e.dir.path().join("sorties").exists(), "--dry-run ne crée rien");
    assert!(!e
        .dir
        .path()
        .join("home/projects")
        .read_dir()
        .map(|mut d| d.any(|f| f.unwrap().path().join("proxy.webm").exists()))
        .unwrap_or(false));
    // la calibration passe par un fichier JSON
    let cal = e.out("cal.json");
    std::fs::write(&cal, r#"{"fov": 195, "swap_lenses": true}"#).unwrap();
    let r = e.x3(&["equirect", p(&c.path), "--dry-run", "--calibration", p(&cal), "--width", "1024"]);
    assert!(r.status.success(), "{}", text(&r.stderr));
}

#[test]
fn equirect_export_and_snapshot() {
    let e = Env::new();
    let c = static_clip();
    let out = e.out("360.mp4");
    let r = e.x3(&["equirect", p(&c.path), "-o", p(&out), "--width", "1024", "--preset", "ultrafast", "--end", "0.5"]);
    assert!(r.status.success(), "{}", text(&r.stderr));
    let info = probe_video(&out);
    assert_eq!((info.w, info.h), (1024, 512));
    assert!(x3studio::spherical::has_spherical_v1(&out));
    // capture : JPEG par défaut, planète, vue vers le bas
    let jpg = e.out("s.jpg");
    let r = e.x3(&[
        "snapshot",
        p(&c.path),
        "-o",
        p(&jpg),
        "--t",
        "1.0",
        "--size",
        "320x180",
        "--projection",
        "planet",
        "--pitch",
        "-90",
    ]);
    assert!(r.status.success(), "{}", text(&r.stderr));
    assert_eq!(&std::fs::read(&jpg).unwrap()[..3], b"\xff\xd8\xff");
    let png = e.out("s.png");
    assert!(e.x3(&["snapshot", p(&c.path), "-o", p(&png), "--size", "160x90"]).status.success());
    assert_eq!(&std::fs::read(&png).unwrap()[..4], b"\x89PNG");
    // sans -o : <nom>_<projection>.jpg dans le dossier de sortie
    let r = e.x3(&["snapshot", p(&c.path), "--size", "160x90", "--projection", "fisheye"]);
    assert!(text(&r.stdout).trim().ends_with("_fisheye.jpg"), "{}", text(&r.stdout));
}

#[test]
fn keyframes_file_and_stabilization() {
    let e = Env::new();
    let c = static_clip();
    let kf = e.out("kf.json");
    std::fs::write(&kf, r#"[{"t": 0, "yaw": 0, "fov": 90}, {"t": 1, "yaw": 90, "fov": 60}]"#).unwrap();
    let out = e.out("k.mp4");
    let r = e.x3(&[
        "reframe",
        p(&c.path),
        "-o",
        p(&out),
        "--keyframes",
        p(&kf),
        "--size",
        "320x180",
        "--preset",
        "ultrafast",
        "--end",
        "1",
    ]);
    assert!(r.status.success(), "{}", text(&r.stderr));
    let frames = read_frames(&out, 320, 180, "rgb24", None, None);
    assert!(
        near(avg_rgb(&frames[frames.len() - 1], 320, 160, 45, 2), [0.0, 255.0, 0.0], 80.0),
        "la vue finit sur la bande verte (cap 90°)"
    );
    // keyframes invalides
    std::fs::write(&kf, "pas du json").unwrap();
    let r = e.x3(&["reframe", p(&c.path), "--keyframes", p(&kf), "--dry-run"]);
    assert_eq!(r.status.code(), Some(1));
    assert!(text(&r.stderr).contains("Keyframes invalides"), "{}", text(&r.stderr));
    // stabilisation : aperçu + analyse du mouvement avant l'export
    let shake = clip(
        "cli-shake",
        SynthOptions { seconds: 2.0, fps: 30.0, lens: 480, motion: Motion::Shake, ..Default::default() },
    );
    let out = e.out("stab.mp4");
    let r = e.x3(&[
        "reframe",
        p(&shake.path),
        "-o",
        p(&out),
        "--size",
        "320x180",
        "--stabilize",
        "lock",
        "--preset",
        "ultrafast",
        "--crf",
        "28",
    ]);
    assert!(r.status.success(), "{}", text(&r.stderr));
    let err = text(&r.stderr);
    assert!(err.contains("aperçu :") && err.contains("analyse du mouvement : 100 %"), "{err}");
    assert!(mean_shift(&out, 320, 180) < 0.5, "vidéo stabilisée");
    // orientation sans analyse (--orient seul : pas de stabilisation, donc pas d'analyse)
    let r = e.x3(&[
        "reframe",
        p(&shake.path),
        "-o",
        p(&e.out("o.mp4")),
        "--size",
        "160x90",
        "--orient",
        "0,0,-3.5",
        "--preset",
        "ultrafast",
        "--end",
        "0.5",
    ]);
    assert!(r.status.success(), "{}", text(&r.stderr));
    assert!(!text(&r.stderr).contains("analyse"), "{}", text(&r.stderr));
}

#[test]
fn demo_generates_a_playable_insv() {
    let e = Env::new();
    let f = e.out("démo.insv");
    let r = e.x3(&["demo", p(&f), "--seconds", "1", "--lens", "240", "--motion", "static"]);
    assert!(r.status.success(), "{}", text(&r.stderr));
    assert_eq!(text(&r.stdout).trim(), p(&f));
    assert!(text(&r.stderr).contains("Génération"));
    let out = text(&e.x3(&["probe", p(&f), "--json"]).stdout);
    assert!(out.contains("two_streams") && out.contains("\"frames\": 30"), "{out}");
    // paramètres invalides
    assert_eq!(e.x3(&["demo", p(&e.out("x.insv")), "--lens", "15"]).status.code(), Some(1));
    assert_eq!(e.x3(&["demo", p(&e.out("x.insv")), "--seconds", "0"]).status.code(), Some(1));
    assert!(!e.out("x.insv").exists());
}

#[test]
fn ctrl_c_cancels_cleanly_and_leaves_no_ffmpeg() {
    let e = Env::new();
    let c = static_clip();
    let needle = format!("interrompu-{}", std::process::id());
    let out = e.out(&format!("{needle}.mp4"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_x3studio"))
        .args(["reframe", p(&c.path), "-o", p(&out), "--size", "1920x1080", "--preset", "veryslow", "--crf", "10"])
        .env("X3STUDIO_HOME", e.dir.path().join("home"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // on attend que l'encodage ait démarré puis on envoie SIGINT au processus (comme Ctrl+C)
    let t0 = std::time::Instant::now();
    while processes_mentioning(&needle).iter().all(|c| !c.contains("rawvideo")) && t0.elapsed().as_secs() < 30 {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(Command::new("kill").args(["-INT", &child.id().to_string()]).status().unwrap().success());
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(130), "code de sortie d'une interruption");
    assert!(!out.exists(), "sortie partielle supprimée");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let left: Vec<String> = processes_mentioning(&needle).into_iter().filter(|c| c.contains("ffmpeg")).collect();
    assert!(left.is_empty(), "ffmpeg orphelin : {left:?}");
}

#[test]
fn the_gui_server_stops_cleanly_on_sigint_during_an_export() {
    use std::io::{BufRead, BufReader};
    use std::time::{Duration, Instant};
    let e = Env::new();
    let c = static_clip();
    let needle = format!("arret-{}", std::process::id());
    let out_dir = e.out(&needle);
    let mut child = Command::new(env!("CARGO_BIN_EXE_x3studio"))
        .args(["gui", "--no-browser"])
        .env("X3STUDIO_HOME", e.dir.path().join("home"))
        .env("X3STUDIO_OUTPUT", e.dir.path().join("sorties"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // adresse annoncée au démarrage : http://127.0.0.1:PORT/?key=CLE
    let mut lines = BufReader::new(child.stdout.take().unwrap());
    let mut url = String::new();
    let mut line = String::new();
    while url.is_empty() {
        line.clear();
        assert!(lines.read_line(&mut line).unwrap() > 0, "le serveur s'est arrêté avant d'annoncer son adresse");
        if let Some(i) = line.find("http://127.0.0.1:") {
            url = line[i..].trim().to_string();
        }
    }
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let html = agent.get(&url).call().unwrap().into_body().read_to_string().unwrap();
    let token = html
        .split("name=\"x3-token\" content=\"")
        .nth(1)
        .and_then(|r| r.split('"').next())
        .expect("jeton dans la page")
        .to_string();
    let base = url.split("/?").next().unwrap().to_string();
    let get = |path: &str| -> serde_json::Value {
        agent
            .get(&format!("{base}{path}"))
            .header("X-X3-Token", &token)
            .call()
            .unwrap()
            .into_body()
            .read_json()
            .unwrap()
    };
    let post = |path: &str, body: serde_json::Value| -> serde_json::Value {
        agent
            .post(&format!("{base}{path}"))
            .header("X-X3-Token", &token)
            .send_json(&body)
            .unwrap()
            .into_body()
            .read_json()
            .unwrap()
    };
    let project = post("/api/open", serde_json::json!({"path": c.path}));
    let pid = project["id"].as_str().unwrap().to_string();
    let t0 = Instant::now();
    while get("/api/jobs").as_array().unwrap().iter().any(|j| j["state"] != "done" && j["state"] != "error") {
        assert!(t0.elapsed() < Duration::from_secs(120), "aperçu interminable");
        std::thread::sleep(Duration::from_millis(100));
    }
    let job = post(
        &format!("/api/project/{pid}/export"),
        serde_json::json!({"settings": {"width": 1920, "height": 1080, "preset": "veryslow", "crf": 10}, "directory": out_dir}),
    );
    let id = job["id"].as_str().unwrap().to_string();
    // l'export tourne : le processus d'encodage existe
    let t0 = Instant::now();
    while processes_mentioning(&needle).iter().all(|c| !c.contains("rawvideo")) {
        assert!(
            t0.elapsed() < Duration::from_secs(60),
            "l'export ne démarre pas : {}",
            get(&format!("/api/jobs/{id}"))
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(Command::new("kill").args(["-INT", &child.id().to_string()]).status().unwrap().success());
    let t0 = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(t0.elapsed() < Duration::from_secs(30), "le serveur ne s'arrête pas");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "arrêt propre : {status:?}");
    // aucun ffmpeg orphelin, aucune sortie partielle ni fichier réservé
    let left: Vec<String> = processes_mentioning(&needle).into_iter().filter(|c| c.contains("ffmpeg")).collect();
    assert!(left.is_empty(), "ffmpeg orphelin : {left:?}");
    let files: Vec<_> =
        std::fs::read_dir(&out_dir).map(|d| d.flatten().map(|f| f.path()).collect()).unwrap_or_default();
    assert!(files.is_empty(), "sortie partielle : {files:?}");
}

#[test]
fn the_gui_reports_startup_problems_without_panicking() {
    let e = Env::new();
    // port déjà pris
    let taken = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = taken.local_addr().unwrap().port().to_string();
    let r = e.x3(&["gui", "--no-browser", "--port", &port]);
    assert_eq!(r.status.code(), Some(1));
    let err = text(&r.stderr);
    assert!(err.starts_with("Erreur : ") && !err.contains("panicked"), "{err}");
    // vidéo à ouvrir introuvable
    let r = e.x3(&["gui", "--no-browser", p(&e.out("absent.insv"))]);
    assert_eq!(r.status.code(), Some(1));
    assert!(text(&r.stderr).contains("introuvable"), "{}", text(&r.stderr));
    // `x3studio fichier` (sans sous-commande) ouvre aussi l'interface : même erreur
    let r = e.x3(&[p(&e.out("absent.insv"))]);
    assert_eq!(r.status.code(), Some(1));
    // port invalide : erreur d'usage
    assert_eq!(e.x3(&["gui", "--port", "70000"]).status.code(), Some(2));
}

#[test]
fn image_adjustments_are_options_checked_and_visible_in_the_commands() {
    let e = Env::new();
    let c = static_clip();
    // netteté et bruit changent les filtres FFmpeg ; la couleur est appliquée par le moteur
    let r = e.x3(&["reframe", p(&c.path), "--dry-run", "--sharpen", "0.5", "--denoise", "0.5"]);
    assert!(r.status.success(), "{}", text(&r.stderr));
    let out = text(&r.stdout);
    assert!(out.contains("hqdn3d=4.000:3.000:6.000:4.500") && out.contains("unsharp=5:5:0.750:5:5:0"), "{out}");
    let r = e.x3(&["reframe", p(&c.path), "--dry-run"]);
    let out = text(&r.stdout);
    assert!(!out.contains("hqdn3d") && !out.contains("unsharp"), "sans réglage, aucun filtre en plus : {out}");
    // une valeur négative s'écrit sans « = » ; hors bornes ou non numérique : erreur claire
    let r = e.x3(&[
        "reframe",
        p(&c.path),
        "--dry-run",
        "--brightness",
        "-0.2",
        "--contrast",
        "1.3",
        "--saturation",
        "0.5",
        "--gamma",
        "1.2",
        "--warmth",
        "-0.4",
        "--tint",
        "0.1",
    ]);
    assert!(r.status.success(), "{}", text(&r.stderr));
    for (opt, value, word) in [
        ("--brightness", "0.9", "luminosité"),
        ("--contrast", "5", "contraste"),
        ("--saturation", "-1", "saturation"),
        ("--gamma", "0", "tons moyens"),
        ("--warmth", "2", "chaleur"),
        ("--tint", "-2", "teinte"),
        ("--sharpen", "1.5", "netteté"),
        ("--denoise", "-0.5", "bruit"),
    ] {
        let r = e.x3(&["reframe", p(&c.path), "--dry-run", opt, value]);
        assert_eq!(r.status.code(), Some(1), "{opt} {value}");
        assert!(text(&r.stderr).contains(word), "{opt} {value} : {}", text(&r.stderr));
    }
    let r = e.x3(&["reframe", p(&c.path), "--dry-run", "--gamma", "nan"]);
    assert_eq!(r.status.code(), Some(1));
    assert!(text(&r.stderr).contains("Valeur invalide"), "{}", text(&r.stderr));
    // l'export réel applique la correction (image plus claire qu'avec les réglages neutres)
    let mean = |args: &[&str], name: &str| {
        let out = e.out(name);
        let mut a = vec!["snapshot", p(&c.path), "-o", p(&out), "--size", "160x90", "--t", "0.5"];
        a.extend_from_slice(args);
        let r = e.x3(&a);
        assert!(r.status.success(), "{}", text(&r.stderr));
        let (img, ..) = read_image(&out);
        img.iter().map(|&v| f64::from(v)).sum::<f64>() / img.len() as f64
    };
    let (plain, bright) = (mean(&[], "plain.jpg"), mean(&["--brightness", "0.2"], "bright.jpg"));
    assert!(bright > plain + 20.0, "{plain:.1} -> {bright:.1}");
}
