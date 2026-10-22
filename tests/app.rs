//! Logique applicative partagée par l'interface web et la ligne de commande : projets, aperçu, analyse, exports.
mod common;

use common::*;
use serde_json::json;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};
use x3studio::app::App;
use x3studio::error::is_user_error;
use x3studio::jobs::JobState;
use x3studio::project::Project;
use x3studio::synth::{Motion, SynthOptions};

struct Fixture {
    app: Arc<App>,
    out: PathBuf,
    dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    require_ffmpeg();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("sorties");
    let app = App::new(Some(dir.path().join("home")), Some(out.clone())).unwrap();
    Fixture { app, out, dir }
}

fn shake() -> PathBuf {
    clip("app-shake", SynthOptions { seconds: 2.0, fps: 30.0, lens: 480, motion: Motion::Shake, ..Default::default() })
        .path
}

fn wait_idle(app: &App, p: &Project) {
    let t0 = Instant::now();
    loop {
        let busy = ["proxy", "analyze", "export"].iter().any(|k| app.jobs.active(&p.id, k).is_some());
        if !busy {
            return;
        }
        assert!(t0.elapsed() < Duration::from_secs(120), "tâches interminables : {:?}", app.jobs.list());
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn tiny_export(preset_only: bool) -> serde_json::Value {
    let _ = preset_only;
    json!({"width": 160, "height": 90, "preset": "ultrafast", "crf": 30})
}

#[test]
fn opening_a_project_builds_the_preview_then_the_analysis() {
    let f = fixture();
    let p = f.app.open_project(&shake(), true).unwrap();
    // l'aperçu est demandé tout de suite ; les deux tâches s'enchaînent
    let first = f.app.jobs.active(&p.id, "proxy").expect("tâche d'aperçu");
    assert_eq!(first.kind, "proxy");
    assert!(first.title.starts_with("Aperçu : "));
    // une seconde demande reprend la tâche en cours au lieu d'en lancer une autre
    let again = f.app.ensure_proxy(&p).unwrap().expect("même tâche");
    assert_eq!(again.id, first.id);
    wait_idle(&f.app, &p);
    let t0 = Instant::now();
    while p.orientation().is_none() && t0.elapsed() < Duration::from_secs(60) {
        wait_idle(&f.app, &p);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(p.has_proxy() && p.orientation().is_some());
    let jobs = f.app.jobs.list();
    assert!(jobs.iter().all(|j| j["state"] == "done"), "{jobs:?}");
    let kinds: HashSet<&str> = jobs.iter().map(|j| j["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, HashSet::from(["proxy", "analyze"]));
    let info = p.analysis_info();
    assert_eq!((info["ready"].clone(), info["stale"].clone()), (json!(true), json!(false)));
    // tout est prêt : plus rien à faire
    assert!(f.app.ensure_proxy(&p).unwrap().is_none());
    // le même fichier rouvert donne le même projet (l'instance déjà connue)
    let again = f.app.open_project(&shake(), false).unwrap();
    assert!(Arc::ptr_eq(&p, &again));
    assert!(Arc::ptr_eq(&p, &f.app.get_project(&p.id).unwrap()));
    assert!(f.app.get_project("0000000000000000").is_none());
    // pas de fichier temporaire d'aperçu oublié
    assert!(!p.proxy_path().with_extension("part").exists());
}

#[test]
fn the_analysis_needs_a_preview_and_goes_stale_with_the_calibration() {
    let f = fixture();
    let p = f.app.open_project(&shake(), false).unwrap();
    let e = f.app.start_analysis(&p).err().unwrap();
    assert!(is_user_error(&e) && e.to_string().contains("pas encore prêt"), "{e}");
    // version bloquante (ligne de commande) : aperçu puis analyse, avec progression
    let seen = std::sync::Mutex::new(Vec::<String>::new());
    f.app
        .prepare_sync(&p, true, &|label, _| seen.lock().unwrap().push(label.to_string()), &AtomicBool::new(false))
        .unwrap();
    let labels: HashSet<String> = seen.into_inner().unwrap().into_iter().collect();
    assert_eq!(labels, HashSet::from(["aperçu".to_string(), "analyse du mouvement".to_string()]));
    assert_eq!(p.analysis_info()["stale"], false);
    // un second appel ne refait rien : l'analyse est à jour
    let mtime = |p: &Project| std::fs::metadata(p.orient_path()).unwrap().modified().unwrap();
    let before = mtime(&p);
    std::thread::sleep(Duration::from_millis(30));
    f.app.prepare_sync(&p, true, &|_, _| panic!("rien à faire"), &AtomicBool::new(false)).unwrap();
    assert_eq!(mtime(&p), before);
    // le champ des objectifs change : l'analyse est périmée, prepare_sync la refait
    p.update_state(&json!({"calibration": {"fov": 190}})).unwrap();
    assert_eq!(p.analysis_info()["stale"], true);
    f.app.prepare_sync(&p, true, &|_, _| {}, &AtomicBool::new(false)).unwrap();
    assert_eq!(p.analysis_info()["stale"], false);
    assert!(mtime(&p) > before);
}

#[test]
fn queued_exports_get_distinct_names() {
    let f = fixture();
    let p = f.app.open_project(&shake(), false).unwrap();
    // six exports demandés presque en même temps : chacun a son propre fichier, avant même le début du premier
    let ids: Vec<(String, String)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..6)
            .map(|_| {
                s.spawn(|| {
                    let j = f.app.start_export(&p, Some(&tiny_export(true)), None).unwrap();
                    (j.id.clone(), j.title.clone())
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let titles: HashSet<&String> = ids.iter().map(|(_, t)| t).collect();
    assert_eq!(titles.len(), 6, "titres (donc noms de fichiers) tous différents : {titles:?}");
    for (id, _) in &ids {
        assert_eq!(f.app.jobs.wait(id, 120.0).unwrap().state(), JobState::Done);
    }
    let stem = shake().file_stem().unwrap().to_str().unwrap().to_string();
    let mut names: Vec<String> =
        std::fs::read_dir(&f.out).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    let mut want: Vec<String> = vec![format!("{stem}_flat.mp4")];
    want.extend((2..=6).map(|n| format!("{stem}_flat_{n}.mp4")));
    want.sort();
    assert_eq!(names, want);
    assert!(names.iter().all(|n| std::fs::metadata(f.out.join(n)).unwrap().len() > 1000), "aucun fichier vide");
}

#[test]
fn a_cancelled_or_rejected_export_leaves_no_placeholder() {
    let f = fixture();
    let p = f.app.open_project(&shake(), false).unwrap();
    // paramètres invalides : refusés tout de suite, sans fichier ni tâche
    for bad in [
        json!({"width": 161}),
        json!({"codec": "av1"}),
        json!({"start": 99}),
        json!({"projection": "cube"}),
        json!({"width": "x"}),
    ] {
        let e = f.app.start_export(&p, Some(&bad), None).err().unwrap();
        assert!(is_user_error(&e), "{bad} : {e}");
    }
    assert!(f.app.jobs.list().is_empty());
    let leftovers = |dir: &Path| dir.read_dir().map(|d| d.count()).unwrap_or(0);
    assert_eq!(leftovers(&f.out), 0, "un export refusé ne laisse pas de fichier vide");
    // un export annulé alors qu'il attend son tour
    let slow = f.app.start_export(&p, Some(&json!({"width": 1920, "height": 1080, "preset": "slow"})), None).unwrap();
    let queued = f.app.start_export(&p, Some(&tiny_export(true)), None).unwrap();
    assert!(f.app.jobs.cancel(&queued.id));
    assert!(f.app.jobs.cancel(&slow.id));
    assert_eq!(f.app.jobs.wait(&queued.id, 60.0).unwrap().state(), JobState::Cancelled);
    assert_eq!(f.app.jobs.wait(&slow.id, 60.0).unwrap().state(), JobState::Cancelled);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        leftovers(&f.out),
        0,
        "ni sortie partielle ni fichier réservé : {:?}",
        f.out.read_dir().unwrap().collect::<Vec<_>>()
    );
    // dossier de sortie impossible (un fichier à la place d'un dossier)
    let blocker = f.dir.path().join("fichier");
    std::fs::write(&blocker, b"x").unwrap();
    assert!(f.app.start_export(&p, Some(&tiny_export(true)), Some(&blocker.join("sous"))).is_err());
}

#[test]
fn snapshots_use_unique_names_or_the_requested_path() {
    let f = fixture();
    let p = f.app.open_project(&shake(), false).unwrap();
    let stem = shake().file_stem().unwrap().to_str().unwrap().to_string();
    let o = tiny_export(true);
    let a = f.app.snapshot(&p, 0.5, "jpg", None, None, Some(&o)).unwrap();
    let b = f.app.snapshot(&p, 0.5, "jpg", None, None, Some(&o)).unwrap();
    let c = f.app.snapshot(&p, 0.5, "png", None, None, Some(&o)).unwrap();
    assert_eq!(a, f.out.join(format!("{stem}_flat.jpg")));
    assert_eq!(b, f.out.join(format!("{stem}_flat_2.jpg")));
    assert_eq!(c, f.out.join(format!("{stem}_flat.png")));
    assert_eq!(&std::fs::read(&a).unwrap()[..3], b"\xff\xd8\xff");
    let explicit = f.dir.path().join("choisi/ici.jpg");
    assert_eq!(f.app.snapshot(&p, 1.0, "jpg", None, Some(&explicit), Some(&o)).unwrap(), explicit);
    assert!(explicit.is_file());
    // une erreur ne laisse pas de fichier réservé
    let before = f.out.read_dir().unwrap().count();
    assert!(f.app.snapshot(&p, 0.5, "jpg", None, None, Some(&json!({"width": 161}))).is_err());
    assert_eq!(f.out.read_dir().unwrap().count(), before);
    // dossier choisi à la demande
    let other = f.dir.path().join("autre dossier");
    let d = f.app.snapshot(&p, 0.0, "jpg", Some(&other), None, Some(&o)).unwrap();
    assert!(d.starts_with(&other) && d.is_file());
}

#[test]
fn the_stabilization_settings_of_the_project_are_used_by_exports() {
    let f = fixture();
    let p = f.app.open_project(&shake(), false).unwrap();
    f.app.prepare_sync(&p, true, &|_, _| {}, &AtomicBool::new(false)).unwrap();
    p.update_state(&json!({"stabilization": {"mode": "lock"}, "view": {"yaw": 20, "fov": 90}})).unwrap();
    let settings = json!({"width": 480, "height": 270, "preset": "ultrafast", "crf": 26});
    let steady = f.app.start_export(&p, Some(&settings), None).unwrap();
    assert_eq!(f.app.jobs.wait(&steady.id, 120.0).unwrap().state(), JobState::Done);
    p.update_state(&json!({"stabilization": {"mode": "off"}})).unwrap();
    let shaky = f.app.start_export(&p, Some(&settings), None).unwrap();
    assert_eq!(f.app.jobs.wait(&shaky.id, 120.0).unwrap().state(), JobState::Done);
    let out = |j: &Arc<x3studio::jobs::Job>| PathBuf::from(&j.inner.lock().unwrap().output);
    let (b, a) = (mean_shift(&out(&steady), 480, 270), mean_shift(&out(&shaky), 480, 270));
    assert!(a > 2.0 && b < 0.5, "sans : {a:.2} px ; verrouillé : {b:.2} px");
}

#[test]
fn info_reports_the_environment() {
    let f = fixture();
    let i = f.app.info();
    assert_eq!(i["version"], x3studio::VERSION);
    assert!(i["ffmpeg"].as_str().unwrap().starts_with("ffmpeg version"), "{}", i["ffmpeg"]);
    assert_eq!(i["output_dir"], f.out.to_str().unwrap());
    assert!(i["cpus"].as_u64().unwrap() >= 1);
    let enc: Vec<&str> = i["encoders"].as_array().unwrap().iter().map(|e| e.as_str().unwrap()).collect();
    assert!(enc.contains(&"libx264"), "{enc:?}");
}

#[test]
fn cancelling_the_preview_removes_the_partial_file() {
    let f = fixture();
    let p = f.app.open_project(&shake(), false).unwrap();
    let cancel = AtomicBool::new(false);
    let e = f
        .app
        .prepare_sync(
            &p,
            true,
            &|label, frac| {
                if label == "aperçu" && frac > 0.05 {
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed)
                }
            },
            &cancel,
        )
        .unwrap_err();
    assert!(x3studio::error::is_cancelled(&e), "{e}");
    assert!(
        !p.proxy_path().exists() && !p.proxy_path().with_extension("part").exists(),
        "ni aperçu ni fichier temporaire"
    );
    assert!(!p.has_proxy() && p.orientation().is_none());
    // la tâche d'aperçu annulée par l'interface : état annulé, rien n'est enchaîné ensuite
    let job = f.app.ensure_proxy(&p).unwrap().expect("tâche d'aperçu");
    assert!(f.app.jobs.cancel(&job.id));
    assert_eq!(f.app.jobs.wait(&job.id, 60.0).unwrap().state(), JobState::Cancelled);
    std::thread::sleep(Duration::from_millis(200));
    assert!(!p.has_proxy() && !p.proxy_path().with_extension("part").exists());
    assert!(f.app.jobs.active(&p.id, "analyze").is_none(), "pas d'analyse sans aperçu");
}
