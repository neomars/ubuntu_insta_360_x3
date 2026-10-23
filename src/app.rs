//! Logique applicative partagée par l'interface web et la ligne de commande.
//! Spec : `x3studio/app.py`.

use crate::error::{export_err, Result};
use crate::jobs::{Job, JobManager, JobState};
use crate::pipeline::{self, ExportSettings};
use crate::project::{self, Project, StabOverride};
use crate::stabilize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// Largeur d'un objectif dans le proxy d'aperçu (px).
const PROXY_LENS_PX: u32 = 960;

/// Taille d'un objectif dans l'aperçu : 960 px au plus, sans agrandir une source plus petite (le `.lrv` de l'X3
/// n'a que 512 px par objectif).
fn proxy_lens_px(src: &crate::media::Source) -> u32 {
    PROXY_LENS_PX.min(src.lens_size.1 & !1).max(16)
}

pub struct App {
    pub home: PathBuf,
    pub output_dir: PathBuf,
    pub jobs: JobManager,
    pub projects: Mutex<HashMap<String, Arc<Project>>>,
    encoders: Vec<String>,
}

/// Nom de fichier sûr : suites de caractères hors `[A-Za-z0-9._-]` remplacées par `_`.
fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_us = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
            last_us = false;
        } else if !last_us {
            out.push('_');
            last_us = true;
        }
    }
    let t = out.trim_matches('_');
    if t.is_empty() {
        "video".into()
    } else {
        t.to_string()
    }
}

fn detect_encoders() -> Vec<String> {
    let Ok(out) = Command::new("ffmpeg").args(["-hide_banner", "-encoders"]).output() else {
        return vec![];
    };
    let text = String::from_utf8_lossy(&out.stdout);
    ["libx264", "libx265"]
        .iter()
        .filter(|name| text.lines().any(|l| l.split_whitespace().nth(1) == Some(**name)))
        .map(|s| s.to_string())
        .collect()
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

impl App {
    /// `home` / `output_dir` : défauts de `project::home_dir()` / `project::default_output_dir()`.
    pub fn new(home: Option<PathBuf>, output_dir: Option<PathBuf>) -> Result<Arc<App>> {
        let home = home.unwrap_or_else(project::home_dir);
        std::fs::create_dir_all(&home)?;
        Ok(Arc::new(App {
            home,
            output_dir: output_dir.unwrap_or_else(project::default_output_dir),
            jobs: JobManager::new(),
            projects: Mutex::new(HashMap::new()),
            encoders: detect_encoders(),
        }))
    }

    // -- projets -----------------------------------------------------------
    /// Ouvre un projet (déjà connu : le même) ; `prepare` lance l'aperçu puis l'analyse si nécessaire.
    pub fn open_project(self: &Arc<Self>, path: &Path, prepare: bool) -> Result<Arc<Project>> {
        let opened = Project::open(path, &self.home)?;
        let p = {
            let mut map = self.projects.lock().unwrap_or_else(|e| e.into_inner());
            map.entry(opened.id.clone()).or_insert(opened).clone()
        };
        if prepare {
            self.ensure_proxy(&p)?;
        }
        Ok(p)
    }

    pub fn get_project(&self, id: &str) -> Option<Arc<Project>> {
        self.projects.lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned()
    }

    // -- aperçu + analyse ----------------------------------------------------
    /// Lance (si besoin) l'aperçu WebM puis l'analyse du mouvement (tâches `proxy` puis `analyze`).
    pub fn ensure_proxy(self: &Arc<Self>, p: &Arc<Project>) -> Result<Option<Arc<Job>>> {
        if p.has_proxy() {
            if p.orientation().is_none() && self.jobs.active(&p.id, "analyze").is_none() {
                return Ok(Some(self.start_analysis(p)?));
            }
            return Ok(None);
        }
        if let Some(j) = self.jobs.active(&p.id, "proxy") {
            return Ok(Some(j));
        }
        let (app, pc) = (self.clone(), p.clone());
        let title = format!("Aperçu : {}", file_name(Path::new(&p.src.path)));
        let job = self.jobs.submit(
            "proxy",
            &title,
            &p.id,
            true,
            Box::new(move |job| {
                job.set_message("Préparation de l'aperçu");
                let part = pc.proxy_path().with_extension("part");
                let cmd = pipeline::build_proxy_cmd(&pc.src, &part, proxy_lens_px(&pc.src));
                let res = pipeline::run_ffmpeg(&cmd, pc.src.duration, &|f| job.set_progress(f), &job.cancel);
                match res {
                    Ok(()) => std::fs::rename(&part, pc.proxy_path())?,
                    Err(e) => {
                        let _ = std::fs::remove_file(&part);
                        return Err(e);
                    }
                }
                if pc.orientation().is_none() {
                    // l'analyse est rapide : on l'enchaîne tout de suite (erreur sans importance ici)
                    let _ = app.start_analysis(&pc);
                }
                Ok(pc.proxy_path().to_string_lossy().into_owned())
            }),
            None,
        );
        Ok(Some(job))
    }

    pub fn start_analysis(self: &Arc<Self>, p: &Arc<Project>) -> Result<Arc<Job>> {
        if let Some(j) = self.jobs.active(&p.id, "analyze") {
            return Ok(j);
        }
        if !p.has_proxy() {
            return Err(export_err("L'aperçu n'est pas encore prêt."));
        }
        let pc = p.clone();
        let title = format!("Analyse du mouvement : {}", file_name(Path::new(&p.src.path)));
        Ok(self.jobs.submit(
            "analyze",
            &title,
            &p.id,
            true,
            Box::new(move |job| {
                job.set_message("Analyse du mouvement");
                let o = stabilize::analyze_video(
                    &pc.src,
                    &pc.calibration(),
                    &pc.proxy_path(),
                    &|f| job.set_progress(f),
                    &job.cancel,
                )?;
                o.save(&pc.orient_path())?;
                Ok(pc.orient_path().to_string_lossy().into_owned())
            }),
            None,
        ))
    }

    /// Version bloquante (ligne de commande) : aperçu puis, si `analysis`, analyse du mouvement.
    pub fn prepare_sync(
        &self,
        p: &Arc<Project>,
        analysis: bool,
        on_progress: &dyn Fn(&str, f64),
        cancel: &AtomicBool,
    ) -> Result<()> {
        if !p.has_proxy() {
            let part = p.proxy_path().with_extension("part");
            let cmd = pipeline::build_proxy_cmd(&p.src, &part, proxy_lens_px(&p.src));
            let res = pipeline::run_ffmpeg(&cmd, p.src.duration, &|f| on_progress("aperçu", f), cancel);
            match res {
                Ok(()) => std::fs::rename(&part, p.proxy_path())?,
                Err(e) => {
                    let _ = std::fs::remove_file(&part);
                    return Err(e);
                }
            }
        }
        if analysis {
            let calib = p.calibration();
            let current = p.orientation().is_some_and(|o| {
                o.key == stabilize::calibration_key(&calib) || p.src.layout == crate::media::Layout::Equirect
            });
            if !current {
                let o = stabilize::analyze_video(
                    &p.src,
                    &calib,
                    &p.proxy_path(),
                    &|f| on_progress("analyse du mouvement", f),
                    cancel,
                )?;
                o.save(&p.orient_path())?;
            }
        }
        Ok(())
    }

    // -- exports -------------------------------------------------------------
    /// Nom de sortie candidat n°`n` : `<dossier>/<nom>_<360|projection>[_N].<ext>` (`n` = 1 : sans suffixe).
    fn output_candidate(&self, p: &Project, s: &ExportSettings, dir: Option<&Path>, ext: &str, n: usize) -> PathBuf {
        let d = dir.map(Path::to_path_buf).unwrap_or_else(|| self.output_dir.clone());
        let stem =
            slug(&Path::new(&p.src.path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
        let tag = if s.mode == "equirect" { "360" } else { s.projection.as_str() };
        let ext = ext.trim_start_matches('.');
        if n <= 1 {
            d.join(format!("{stem}_{tag}.{ext}"))
        } else {
            d.join(format!("{stem}_{tag}_{n}.{ext}"))
        }
    }

    /// Nom de sortie unique `<dossier>/<nom>_<360|projection>[_N].<ext>` (ne crée rien).
    pub fn output_path(&self, p: &Project, s: &ExportSettings, dir: Option<&Path>, ext: &str) -> PathBuf {
        (1..).map(|n| self.output_candidate(p, s, dir, ext, n)).find(|c| !c.exists()).expect("suite infinie")
    }

    /// Comme [`App::output_path`] mais crée le fichier (vide, création exclusive) : deux exports en file ou lancés
    /// en même temps ne visent jamais le même nom. Crée aussi le dossier de sortie.
    fn reserve_output(&self, p: &Project, s: &ExportSettings, dir: Option<&Path>, ext: &str) -> Result<PathBuf> {
        for n in 1.. {
            let candidate = self.output_candidate(p, s, dir, ext, n);
            if let Some(parent) = candidate.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&candidate) {
                Ok(_) => return Ok(candidate),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        }
        unreachable!("suite infinie")
    }

    /// Construit le plan tout de suite (erreurs de paramètres remontées à l'appelant) puis soumet l'export.
    /// Le nom de sortie est réservé dès maintenant ; le fichier est supprimé si l'export n'aboutit pas.
    pub fn start_export(
        self: &Arc<Self>,
        p: &Arc<Project>,
        overrides: Option<&serde_json::Value>,
        dir: Option<&Path>,
    ) -> Result<Arc<Job>> {
        let s = p.export_settings(overrides)?;
        let stab = p.stab_matrices(&StabOverride::default());
        let out = self.reserve_output(p, &s, dir, "mp4")?;
        let plan = match pipeline::build_plan(&p.src, &p.calibration(), &s, &out, stab.as_deref()) {
            Ok(plan) => plan,
            Err(e) => {
                let _ = std::fs::remove_file(&out);
                return Err(e);
            }
        };
        let kind = if s.mode == "equirect" { "360°".to_string() } else { s.projection.clone() };
        let title = format!("Export {kind} : {}", file_name(&out));
        Ok(self.jobs.submit(
            "export",
            &title,
            &p.id,
            true,
            Box::new(move |job| {
                let out = pipeline::run_plan(plan, &|f| job.set_progress(f), &job.cancel)?;
                Ok(out.to_string_lossy().into_owned())
            }),
            Some(Box::new(move |job| {
                if job.state() != JobState::Done {
                    let _ = std::fs::remove_file(&out); // annulée en file d'attente, ou en erreur
                }
            })),
        ))
    }

    /// Capture bloquante ; `overrides` peut fixer `width` / `height` / `mode`.
    pub fn snapshot(
        &self,
        p: &Arc<Project>,
        t: f64,
        fmt: &str,
        dir: Option<&Path>,
        out_path: Option<&Path>,
        overrides: Option<&serde_json::Value>,
    ) -> Result<PathBuf> {
        let s = p.export_settings(overrides)?;
        let reserved = match out_path {
            Some(_) => None,
            None => Some(self.reserve_output(p, &s, dir, if fmt == "png" { "png" } else { "jpg" })?),
        };
        let out = reserved.clone().or_else(|| out_path.map(Path::to_path_buf)).expect("sortie choisie");
        if let Some(parent) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let stab = p.stab_matrices(&StabOverride::default());
        let result = pipeline::build_snapshot_plan(&p.src, &p.calibration(), &s, t, &out, stab.as_deref())
            .and_then(|plan| pipeline::run_plan(plan, &|_| {}, &AtomicBool::new(false)));
        if result.is_err() {
            if let Some(r) = reserved {
                let _ = std::fs::remove_file(r);
            }
        }
        result
    }

    /// `{version, ffmpeg, output_dir, home, encoders, cpus}`.
    pub fn info(&self) -> serde_json::Value {
        let ffmpeg = Command::new("ffmpeg")
            .arg("-version")
            .output()
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).lines().next().map(str::to_string))
            .unwrap_or_else(|| "introuvable".into());
        serde_json::json!({
            "version": crate::VERSION,
            "ffmpeg": ffmpeg,
            "output_dir": self.output_dir.to_string_lossy(),
            "home": std::env::var("HOME").unwrap_or_default(),
            "encoders": self.encoders,
            "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{proxy_lens_px, slug};
    use crate::media::{Layout, Source};

    fn source_of(lens: u32) -> Source {
        Source {
            path: "/x/a.insv".into(),
            layout: Layout::SbsFisheye,
            lens_files: vec![],
            lens_streams: vec![],
            lens_size: (lens, lens),
            fps: 30.0,
            duration: 1.0,
            codec: "h264".into(),
            has_audio: false,
            audio_file: None,
            has_trailer: false,
            size_bytes: 0,
            camera: String::new(),
            notes: vec![],
        }
    }

    #[test]
    fn the_preview_is_never_larger_than_the_source() {
        assert_eq!(proxy_lens_px(&source_of(2880)), 960, "un .insv de l'X3 est réduit à 960 px par objectif");
        assert_eq!(proxy_lens_px(&source_of(512)), 512, "le .lrv n'est pas agrandi");
        assert_eq!(proxy_lens_px(&source_of(481)), 480, "taille paire");
        assert_eq!(proxy_lens_px(&source_of(8)), 16, "plancher");
    }

    #[test]
    fn slug_cleans_names() {
        assert_eq!(slug("VID_20240101_120000_00_001"), "VID_20240101_120000_00_001");
        assert_eq!(slug("Mon clip été (1)"), "Mon_clip_t_1");
        assert_eq!(slug("   "), "video");
        assert_eq!(slug("a/b\\c"), "a_b_c");
    }
}
