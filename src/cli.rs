//! Ligne de commande : `x3studio` (interface web, sonde, exports par lot, vidéo de démonstration).
//! Spec : `x3studio/cli.py` — mêmes sous-commandes et options (`gui`, `probe`, `reframe`, `equirect`,
//! `snapshot`, `demo`), sans `--engine` / `--quality` (un seul moteur). Sans argument ou avec un nom
//! de fichier : interface web.

use crate::app::App;
use crate::calibration::Calibration;
use crate::error::{export_err, is_cancelled, is_user_error, Result};
use crate::geometry::{Keyframe, Mat3, Ypr};
use crate::media::{probe_source, Layout};
use crate::pipeline::{self, default_fov, ExportSettings};
use crate::project::{Project, StabOverride};
use crate::stabilize::StabMode;
use crate::synth::{make_synthetic_insv, Motion, SynthOptions};
use clap::{Args, Parser, Subcommand};
use serde_json::json;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Parser, Debug)]
#[command(
    name = "x3studio",
    version,
    about = "Traitement local des vidéos Insta360 X3.",
    long_about = "Traitement local des vidéos Insta360 X3 : assemblage des deux objectifs, recadrage 360° avec \
                  keyframes, stabilisation, export. Rien ne quitte votre machine.\n\n\
                  Sans argument, ou avec un nom de fichier, ouvre l'interface web locale.",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Ouvre l'interface web locale (défaut)
    Gui(GuiArgs),
    /// Analyse un fichier
    Probe(ProbeArgs),
    /// Exporte une vidéo plate (recadrage d'une caméra virtuelle)
    Reframe(ReframeArgs),
    /// Exporte la vidéo 360° assemblée (équirectangulaire)
    Equirect(EquirectArgs),
    /// Enregistre une image (JPEG ou PNG) de la vue
    Snapshot(SnapshotArgs),
    /// Génère un .insv synthétique pour essayer le logiciel sans caméra
    Demo(DemoArgs),
}

#[derive(Args, Debug)]
struct GuiArgs {
    /// Vidéo à ouvrir directement
    input: Option<PathBuf>,
    /// Port d'écoute sur 127.0.0.1 (0 = automatique)
    #[arg(long, default_value_t = 0)]
    port: u16,
    /// N'ouvre pas le navigateur
    #[arg(long)]
    no_browser: bool,
}

#[derive(Args, Debug)]
struct ProbeArgs {
    input: PathBuf,
    /// Affiche le résultat en JSON
    #[arg(long)]
    json: bool,
}

/// Options communes aux exports.
#[derive(Args, Debug)]
struct ExportArgs {
    /// Fichier .insv (ou vidéo 360° équirectangulaire)
    input: PathBuf,
    /// Fichier de sortie
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Début (s)
    #[arg(long, default_value_t = 0.0)]
    start: f64,
    /// Fin (s, 0 = jusqu'à la fin)
    #[arg(long, default_value_t = 0.0)]
    end: f64,
    /// Vitesse (0,25 à 8)
    #[arg(long, default_value_t = 1.0)]
    speed: f64,
    /// Images/s de sortie (0 = source)
    #[arg(long, default_value_t = 0.0)]
    fps: f64,
    /// Codec vidéo
    #[arg(long, default_value = "h264", value_parser = ["h264", "h265"])]
    codec: String,
    /// Qualité x264/x265 (plus bas = meilleur)
    #[arg(long, default_value_t = 20)]
    crf: u32,
    /// Préréglage x264/x265
    #[arg(long, default_value = "medium")]
    preset: String,
    /// Sans piste audio
    #[arg(long)]
    no_audio: bool,
    /// Stabilisation
    #[arg(long, default_value = "off", value_parser = ["off", "smooth", "lock", "horizon"])]
    stabilize: String,
    /// Lissage de la stabilisation (s)
    #[arg(long, default_value_t = 1.0)]
    strength: f64,
    /// Lissage du cap en mode horizon (s)
    #[arg(long, default_value_t = 1.5)]
    heading: f64,
    /// Orientation : cap,inclinaison,roulis (°)
    #[arg(long, value_parser = parse_triplet)]
    orient: Option<Ypr>,
    /// JSON de calibration des objectifs
    #[arg(long)]
    calibration: Option<PathBuf>,
    /// Affiche les commandes sans exécuter
    #[arg(long)]
    dry_run: bool,
    /// Dossier de sortie par défaut
    #[arg(long)]
    output_dir: Option<PathBuf>,
}

/// Options de la vue de la caméra virtuelle.
#[derive(Args, Debug)]
struct ViewArgs {
    /// Projection de sortie
    #[arg(long, default_value = "flat", value_parser = ["flat", "fisheye", "planet"])]
    projection: String,
    /// Taille de sortie LARGEURxHAUTEUR
    #[arg(long, default_value = "1920x1080", value_parser = parse_size)]
    size: (u32, u32),
    /// Cap (°)
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    yaw: f64,
    /// Inclinaison (°)
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    pitch: f64,
    /// Roulis (°)
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    roll: f64,
    /// Champ horizontal (°) ; défaut selon la projection
    #[arg(long, default_value_t = 0.0)]
    fov: f64,
}

#[derive(Args, Debug)]
struct ReframeArgs {
    #[command(flatten)]
    export: ExportArgs,
    #[command(flatten)]
    view: ViewArgs,
    /// JSON : [{t, yaw, pitch, roll, fov}, ...]
    #[arg(long)]
    keyframes: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct EquirectArgs {
    #[command(flatten)]
    export: ExportArgs,
    /// Largeur (0 = 2 × taille d'un objectif)
    #[arg(long, default_value_t = 0)]
    width: u32,
}

#[derive(Args, Debug)]
struct SnapshotArgs {
    #[command(flatten)]
    export: ExportArgs,
    #[command(flatten)]
    view: ViewArgs,
    /// Instant (s)
    #[arg(long, default_value_t = 0.0)]
    t: f64,
}

#[derive(Args, Debug)]
struct DemoArgs {
    /// Fichier .insv à créer
    output: PathBuf,
    /// Durée (s)
    #[arg(long, default_value_t = 4.0)]
    seconds: f64,
    /// Taille d'un objectif (px)
    #[arg(long, default_value_t = 960)]
    lens: u32,
    /// Mouvement de la caméra
    #[arg(long, default_value = "shake", value_parser = ["shake", "pan", "static"])]
    motion: String,
}

fn parse_size(txt: &str) -> std::result::Result<(u32, u32), String> {
    let bad = || "format attendu : LARGEURxHAUTEUR (ex. 1920x1080)".to_string();
    let (w, h) = txt.to_lowercase().split_once('x').map(|(w, h)| (w.to_string(), h.to_string())).ok_or_else(bad)?;
    Ok((w.trim().parse().map_err(|_| bad())?, h.trim().parse().map_err(|_| bad())?))
}

fn parse_triplet(txt: &str) -> std::result::Result<Ypr, String> {
    let bad = || "format attendu : cap,inclinaison,roulis (ex. 0,0,-3.5)".to_string();
    let v: Vec<f64> =
        txt.split(',').map(|p| p.trim().parse::<f64>().map_err(|_| bad())).collect::<std::result::Result<_, _>>()?;
    match v[..] {
        [yaw, pitch, roll] if v.iter().all(|x| x.is_finite()) => Ok(Ypr { yaw, pitch, roll }),
        _ => Err(bad()),
    }
}

/// Affichage de la progression : une ligne qui se met à jour dans un terminal, un pas de 10 % sinon.
struct Progress {
    tty: bool,
    last: Mutex<HashMap<String, i32>>,
}

impl Progress {
    fn new() -> Progress {
        Progress { tty: std::io::stderr().is_terminal(), last: Mutex::new(HashMap::new()) }
    }

    fn report(&self, label: &str, fraction: f64) {
        let pct = (fraction.clamp(0.0, 1.0) * 100.0) as i32;
        let step = if self.tty { pct } else { pct / 10 * 10 };
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if last.insert(label.to_string(), step) == Some(step) {
            return;
        }
        let mut err = std::io::stderr().lock();
        let _ = if self.tty {
            write!(err, "\r{label} : {step:3} %{}", if pct >= 100 { "\n" } else { "" })
        } else {
            writeln!(err, "{label} : {step} %")
        };
        let _ = err.flush();
    }
}

/// Ctrl+C pendant un traitement : on demande l'annulation (sortie partielle supprimée, aucun ffmpeg orphelin).
fn install_ctrl_c(flag: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
            return;
        };
        rt.block_on(async {
            if tokio::signal::ctrl_c().await.is_ok() {
                flag.store(true, Ordering::Relaxed);
            }
        });
    });
}

fn prepare(args: &ExportArgs, need_stab: bool, cancel: &AtomicBool) -> Result<(Arc<App>, Arc<Project>)> {
    let app = App::new(None, args.output_dir.clone())?;
    let p = app.open_project(&args.input, false)?;
    if let Some(file) = &args.calibration {
        let c = Calibration::load(file)
            .map_err(|e| export_err(format!("Calibration illisible ({}) : {e}", file.display())))?;
        p.update_state(&json!({ "calibration": c }))?;
    }
    if need_stab && !args.dry_run {
        let progress = Progress::new();
        app.prepare_sync(&p, true, &|label, f| progress.report(label, f), cancel)?;
    }
    Ok((app, p))
}

fn stab_matrices(p: &Project, args: &ExportArgs) -> Result<Option<Vec<Mat3>>> {
    let mode: StabMode = args.stabilize.parse()?;
    if mode == StabMode::Off && args.orient.is_none() {
        return Ok(None);
    }
    Ok(p.stab_matrices(&StabOverride {
        mode: Some(mode),
        strength: Some(args.strength),
        heading_strength: Some(args.heading),
        orientation: Some(args.orient.unwrap_or_default()),
    }))
}

fn settings(args: &ExportArgs) -> ExportSettings {
    ExportSettings {
        start: args.start,
        end: args.end,
        speed: args.speed,
        fps: args.fps,
        codec: args.codec.clone(),
        crf: args.crf,
        preset: args.preset.clone(),
        audio: !args.no_audio,
        ..Default::default()
    }
}

fn view_settings(base: ExportSettings, v: &ViewArgs) -> ExportSettings {
    let fov = if v.fov != 0.0 { v.fov } else { default_fov(&v.projection) };
    ExportSettings {
        mode: "reframe".into(),
        projection: v.projection.clone(),
        width: v.size.0,
        height: v.size.1,
        view: json!({ "yaw": v.yaw, "pitch": v.pitch, "roll": v.roll, "fov": fov }),
        ..base
    }
}

fn run_export(
    app: &Arc<App>,
    p: &Arc<Project>,
    s: &ExportSettings,
    args: &ExportArgs,
    snapshot_t: Option<f64>,
    cancel: &AtomicBool,
) -> Result<()> {
    let stab = stab_matrices(p, args)?;
    let ext = if snapshot_t.is_some() { "jpg" } else { "mp4" };
    let out = args.output.clone().unwrap_or_else(|| app.output_path(p, s, args.output_dir.as_deref(), ext));
    if !args.dry_run {
        if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
    }
    let plan = match snapshot_t {
        Some(t) => pipeline::build_snapshot_plan(&p.src, &p.calibration(), s, t, &out, stab.as_deref())?,
        None => pipeline::build_plan(&p.src, &p.calibration(), s, &out, stab.as_deref())?,
    };
    if args.dry_run {
        println!("{}", plan.shell());
        return Ok(());
    }
    eprintln!("Rendu → {}", out.display());
    let progress = Progress::new();
    let show = snapshot_t.is_none();
    pipeline::run_plan(
        plan,
        &|f| {
            if show {
                progress.report("export", f)
            }
        },
        cancel,
    )?;
    println!("{}", out.display());
    Ok(())
}

fn cmd_gui(a: GuiArgs) -> Result<()> {
    let app = App::new(None, None)?;
    crate::server::serve(app, a.port, !a.no_browser, a.input)
}

fn cmd_probe(a: ProbeArgs) -> Result<()> {
    let src = probe_source(&a.input)?;
    if a.json {
        println!("{}", serde_json::to_string_pretty(&src.to_json())?);
        return Ok(());
    }
    let layout = match src.layout {
        Layout::TwoStreams => "deux flux fisheye dans un fichier",
        Layout::TwoFiles => "deux fichiers fisheye (_00_/_10_)",
        Layout::SbsFisheye => "deux fisheye côte à côte",
        Layout::Equirect => "équirectangulaire (déjà assemblée)",
    };
    let yes = |b: bool| if b { "oui" } else { "non" };
    println!("Fichier   : {}", src.path);
    println!("Format    : {layout}");
    println!(
        "Image     : {}×{}{}",
        src.lens_size.0,
        src.lens_size.1,
        if src.layout == Layout::Equirect { "" } else { " par objectif" }
    );
    println!(
        "Cadence   : {:.3} i/s — durée {:.2} s ({} images) — codec {}",
        src.fps,
        src.duration,
        src.frames(),
        src.codec
    );
    println!("Audio     : {} — métadonnées Insta360 : {}", yes(src.has_audio), yes(src.has_trailer));
    for n in &src.notes {
        println!("Note      : {n}");
    }
    Ok(())
}

fn cmd_reframe(a: ReframeArgs, cancel: &AtomicBool) -> Result<()> {
    let keyframes: Vec<Keyframe> = match &a.keyframes {
        Some(f) => {
            let text = std::fs::read_to_string(f)
                .map_err(|e| export_err(format!("Keyframes illisibles ({}) : {e}", f.display())))?;
            serde_json::from_str(&text)
                .map_err(|e| export_err(format!("Keyframes invalides ({}) : {e}", f.display())))?
        }
        None => vec![],
    };
    let (app, p) = prepare(&a.export, a.export.stabilize != "off", cancel)?;
    let s = ExportSettings { keyframes, ..view_settings(settings(&a.export), &a.view) };
    run_export(&app, &p, &s, &a.export, None, cancel)
}

fn cmd_equirect(a: EquirectArgs, cancel: &AtomicBool) -> Result<()> {
    let (app, p) = prepare(&a.export, a.export.stabilize != "off", cancel)?;
    let s = ExportSettings { mode: "equirect".into(), stitch_width: a.width, ..settings(&a.export) };
    run_export(&app, &p, &s, &a.export, None, cancel)
}

fn cmd_snapshot(a: SnapshotArgs, cancel: &AtomicBool) -> Result<()> {
    let (app, p) = prepare(&a.export, a.export.stabilize != "off", cancel)?;
    let s = view_settings(settings(&a.export), &a.view);
    run_export(&app, &p, &s, &a.export, Some(a.t), cancel)
}

fn cmd_demo(a: DemoArgs) -> Result<()> {
    let motion: Motion = a.motion.parse()?;
    eprintln!("Génération de {} ({:.0} s, mouvement « {} »)…", a.output.display(), a.seconds, a.motion);
    let opt = SynthOptions { seconds: a.seconds, lens: a.lens, motion, ..Default::default() };
    make_synthetic_insv(&a.output, &opt, None)?;
    println!("{}", a.output.display());
    Ok(())
}

const SUBCOMMANDS: [&str; 6] = ["gui", "probe", "reframe", "equirect", "snapshot", "demo"];

/// `x3studio` seul ou `x3studio fichier.insv` ouvre l'interface web.
fn with_default_gui(mut args: Vec<OsString>) -> Vec<OsString> {
    let first = args.get(1).and_then(|a| a.to_str()).map(str::to_string);
    let explicit = matches!(first.as_deref(), Some(a) if SUBCOMMANDS.contains(&a) || matches!(a, "-h" | "--help" | "-V" | "--version"));
    if !explicit {
        args.insert(1.min(args.len()), OsString::from("gui"));
    }
    args
}

/// Point d'entrée : retourne le code de sortie (0 ok, 1 erreur, 2 usage, 130 interruption).
pub fn run(args: Vec<OsString>) -> i32 {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .with_writer(std::io::stderr)
        .try_init();
    let cli = match Cli::try_parse_from(with_default_gui(args)) {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() { 2 } else { 0 };
        }
    };
    let cancel = Arc::new(AtomicBool::new(false));
    if !matches!(cli.cmd, Cmd::Gui(_) | Cmd::Probe(_) | Cmd::Demo(_)) {
        install_ctrl_c(cancel.clone());
    }
    let result = match cli.cmd {
        Cmd::Gui(a) => cmd_gui(a),
        Cmd::Probe(a) => cmd_probe(a),
        Cmd::Reframe(a) => cmd_reframe(a, &cancel),
        Cmd::Equirect(a) => cmd_equirect(a, &cancel),
        Cmd::Snapshot(a) => cmd_snapshot(a, &cancel),
        Cmd::Demo(a) => cmd_demo(a),
    };
    match result {
        Ok(()) => 0,
        Err(e) if is_cancelled(&e) => {
            eprintln!("Interrompu.");
            130
        }
        Err(e) if is_user_error(&e) => {
            eprintln!("Erreur : {e}");
            1
        }
        Err(e) => {
            eprintln!("Erreur : {e:#}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> std::result::Result<Cli, clap::Error> {
        Cli::try_parse_from(with_default_gui(args.iter().map(OsString::from).collect()))
    }

    #[test]
    fn size_and_triplet_parsing() {
        assert_eq!(parse_size("1920x1080").unwrap(), (1920, 1080));
        assert_eq!(parse_size("640X360").unwrap(), (640, 360));
        for bad in ["1920", "axb", "1920x", "x10", "-1x5", "1920*1080"] {
            assert!(parse_size(bad).unwrap_err().contains("LARGEURxHAUTEUR"), "{bad}");
        }
        assert_eq!(parse_triplet("10,-5, 2.5").unwrap(), Ypr { yaw: 10.0, pitch: -5.0, roll: 2.5 });
        for bad in ["1,2", "1,2,3,4", "a,b,c", "", "1,2,nan", "1,2,inf"] {
            assert!(parse_triplet(bad).unwrap_err().contains("cap,inclinaison,roulis"), "{bad}");
        }
    }

    #[test]
    fn file_name_or_nothing_opens_the_gui() {
        let gui = |a: &[&str]| match parse(a).unwrap().cmd {
            Cmd::Gui(g) => (g.input, g.port, g.no_browser),
            other => panic!("{other:?}"),
        };
        assert_eq!(gui(&["x3studio"]), (None, 0, false));
        assert_eq!(gui(&["x3studio", "clip.insv"]), (Some(PathBuf::from("clip.insv")), 0, false));
        assert_eq!(gui(&["x3studio", "gui", "--port", "8123", "--no-browser"]), (None, 8123, true));
        assert_eq!(
            gui(&["x3studio", "/chemin avec espaces/été.insv"]).0,
            Some(PathBuf::from("/chemin avec espaces/été.insv"))
        );
        assert!(matches!(parse(&["x3studio", "probe", "a.insv"]).unwrap().cmd, Cmd::Probe(_)));
        // --help et --version ne sont pas pris pour des fichiers
        assert!(!parse(&["x3studio", "--version"]).unwrap_err().use_stderr());
        assert!(!parse(&["x3studio", "--help"]).unwrap_err().use_stderr());
    }

    #[test]
    fn export_options_and_defaults() {
        let Cmd::Reframe(a) = parse(&[
            "x3studio",
            "reframe",
            "v.insv",
            "-o",
            "o.mp4",
            "--size",
            "1280x720",
            "--yaw",
            "-20",
            "--pitch",
            "5",
            "--fov",
            "100",
            "--stabilize",
            "horizon",
            "--orient",
            "0,0,-3.5",
            "--crf",
            "18",
            "--no-audio",
            "--projection",
            "planet",
            "--dry-run",
        ])
        .unwrap()
        .cmd
        else {
            panic!()
        };
        assert_eq!(
            (a.export.stabilize.as_str(), a.export.crf, a.export.no_audio, a.export.dry_run),
            ("horizon", 18, true, true)
        );
        assert_eq!(a.export.orient, Some(Ypr { yaw: 0.0, pitch: 0.0, roll: -3.5 }));
        let s = view_settings(settings(&a.export), &a.view);
        assert_eq!(
            (s.mode.as_str(), s.projection.as_str(), s.width, s.height, s.audio, s.crf),
            ("reframe", "planet", 1280, 720, false, 18)
        );
        assert_eq!((s.view["yaw"].as_f64(), s.view["fov"].as_f64()), (Some(-20.0), Some(100.0)));
        // champ par défaut selon la projection
        let Cmd::Snapshot(b) =
            parse(&["x3studio", "snapshot", "v.insv", "--projection", "fisheye", "--t", "2.5"]).unwrap().cmd
        else {
            panic!()
        };
        assert_eq!(b.t, 2.5);
        assert_eq!(view_settings(settings(&b.export), &b.view).view["fov"].as_f64(), Some(180.0));
        let d = settings(&b.export);
        assert_eq!(
            (d.start, d.end, d.speed, d.fps, d.codec.as_str(), d.preset.as_str(), d.audio),
            (0.0, 0.0, 1.0, 0.0, "h264", "medium", true)
        );
        // 360°
        let Cmd::Equirect(e) =
            parse(&["x3studio", "equirect", "v.insv", "--width", "4096", "--codec", "h265"]).unwrap().cmd
        else {
            panic!()
        };
        assert_eq!((e.width, e.export.codec.as_str()), (4096, "h265"));
        // démo
        let Cmd::Demo(d) = parse(&["x3studio", "demo", "x.insv", "--motion", "pan", "--seconds", "2"]).unwrap().cmd
        else {
            panic!()
        };
        assert_eq!((d.motion.as_str(), d.seconds, d.lens), ("pan", 2.0, 960));
    }

    #[test]
    fn invalid_options_are_usage_errors() {
        for bad in [
            &["x3studio", "reframe", "v.insv", "--codec", "vp9"][..],
            &["x3studio", "reframe", "v.insv", "--stabilize", "zoom"],
            &["x3studio", "reframe", "v.insv", "--projection", "cube"],
            &["x3studio", "reframe", "v.insv", "--size", "grand"],
            &["x3studio", "reframe", "v.insv", "--orient", "1,2"],
            &["x3studio", "reframe"],
            &["x3studio", "demo"],
            &["x3studio", "demo", "x.insv", "--motion", "danse"],
            &["x3studio", "probe", "a", "--inconnue"],
        ] {
            assert!(parse(bad).unwrap_err().use_stderr(), "{bad:?}");
        }
    }

    #[test]
    fn progress_prints_each_step_once() {
        let p = Progress { tty: false, last: Mutex::new(HashMap::new()) };
        p.report("export", 0.0);
        p.report("export", 0.04);
        p.report("export", 0.5);
        p.report("export", 1.0);
        let last = p.last.lock().unwrap();
        assert_eq!(last["export"], 100);
    }

    #[test]
    fn stab_matrices_need_a_stabilization_or_an_orientation() {
        // sans option de stabilisation ni orientation : aucune matrice, même si le projet en possède une
        let parse_export = |extra: &[&str]| {
            let mut v = vec!["x3studio", "equirect", "v.insv"];
            v.extend(extra);
            match parse(&v).unwrap().cmd {
                Cmd::Equirect(e) => e.export,
                _ => panic!(),
            }
        };
        assert_eq!(parse_export(&[]).stabilize, "off");
        assert!(parse_export(&[]).orient.is_none());
        assert_eq!(parse_export(&["--orient", "10,0,0"]).orient.unwrap().yaw, 10.0);
    }
}
