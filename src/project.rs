//! Projets : un fichier source + ses réglages (sauvegardés localement) + ses caches.
//! Tout est dans `~/.cache/x3studio` (ou `$X3STUDIO_HOME`) : proxy d'aperçu, orientation de la
//! caméra, réglages d'édition. Rien ne quitte la machine. Spec : `x3studio/project.py`.
//!
//! Le JSON de l'état (`state.json`) et de `to_json()` est celui attendu par `web/app.js`.

use crate::calibration::Calibration;
use crate::error::{export_err, Result};
use crate::geometry::{Keyframe, Mat3, View, Ypr};
use crate::media::{probe_source, Layout, Source};
use crate::pipeline::{default_fov, ExportSettings};
use crate::stabilize::{self, calibration_key, Orientation, StabMode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Trim {
    pub start: f64,
    /// 0 = jusqu'à la fin.
    pub end: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StabParams {
    pub mode: StabMode,
    pub strength: f64,
    pub heading_strength: f64,
}

impl Default for StabParams {
    fn default() -> Self {
        StabParams { mode: StabMode::Off, strength: 1.0, heading_strength: 1.5 }
    }
}

/// Réglages d'édition d'un projet (forme JSON = `DEFAULT_STATE` de project.py).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub calibration: Calibration,
    pub trim: Trim,
    pub projection: String,
    pub view: View,
    pub keyframes: Vec<Keyframe>,
    pub stabilization: StabParams,
    pub orientation: Ypr,
    /// Réglages d'export mémorisés (libres).
    pub export: Value,
}

impl Default for State {
    fn default() -> Self {
        State {
            calibration: Calibration::default(),
            trim: Trim::default(),
            projection: "flat".into(),
            view: View::default(),
            keyframes: vec![],
            stabilization: StabParams::default(),
            orientation: Ypr::default(),
            export: json!({}),
        }
    }
}

/// Clés de premier niveau acceptées par `update_state`.
const STATE_KEYS: [&str; 8] =
    ["calibration", "trim", "projection", "view", "keyframes", "stabilization", "orientation", "export"];

/// Surcharges des réglages de stabilisation (aperçu, CLI) : `None` = valeur de l'état du projet.
#[derive(Debug, Clone, Default)]
pub struct StabOverride {
    pub mode: Option<StabMode>,
    pub strength: Option<f64>,
    pub heading_strength: Option<f64>,
    pub orientation: Option<Ypr>,
    /// Début de la plage (s) : l'image de référence du mode « verrouillé ». `None` = début de la plage du projet.
    pub start: Option<f64>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn user_home() -> PathBuf {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

/// Dossier des caches et réglages : `$X3STUDIO_HOME`, sinon `$XDG_CACHE_HOME/x3studio`, sinon `~/.cache/x3studio`.
pub fn home_dir() -> PathBuf {
    home_dir_from(std::env::var("X3STUDIO_HOME").ok(), std::env::var("XDG_CACHE_HOME").ok(), user_home())
}

fn home_dir_from(x3_home: Option<String>, xdg_cache: Option<String>, home: PathBuf) -> PathBuf {
    if let Some(h) = x3_home.filter(|h| !h.is_empty()) {
        return PathBuf::from(h);
    }
    let base = xdg_cache.filter(|c| !c.is_empty()).map(PathBuf::from).unwrap_or_else(|| home.join(".cache"));
    base.join("x3studio")
}

/// Dossier de sortie : `$X3STUDIO_OUTPUT`, sinon `xdg-user-dir VIDEOS`/X3Studio, sinon `~/Videos/X3Studio`.
pub fn default_output_dir() -> PathBuf {
    match std::env::var("X3STUDIO_OUTPUT").ok().filter(|o| !o.is_empty()) {
        Some(o) => PathBuf::from(o),
        None => output_dir_from(xdg_videos_dir(), user_home()),
    }
}

fn output_dir_from(xdg_videos: Option<String>, home: PathBuf) -> PathBuf {
    match xdg_videos.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        Some(v) if Path::new(&v) != home => Path::new(&v).join("X3Studio"),
        _ => home.join("Videos").join("X3Studio"),
    }
}

/// Résultat de `xdg-user-dir VIDEOS` (attente de 2 s au plus) ; `None` si la commande est absente ou échoue.
fn xdg_videos_dir() -> Option<String> {
    let mut child = Command::new("xdg-user-dir")
        .arg("VIDEOS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let out = child.wait_with_output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Identifiant stable (16 hex) d'après le chemin, la taille et la date de modification.
pub fn project_id(path: &Path) -> Result<String> {
    let p = std::fs::canonicalize(path)?;
    let md = std::fs::metadata(&p)?;
    let mtime = md.modified()?.duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let digest = Sha256::digest(format!("{}|{}|{}", p.display(), md.len(), mtime).as_bytes());
    Ok(digest.iter().take(8).map(|b| format!("{b:02x}")).collect())
}

/// Fusion profonde d'un patch JSON dans l'état (objets fusionnés, sauf `export` ; listes remplacées).
pub fn merge_state(base: &Value, patch: &Value) -> Value {
    let mut out = base.clone();
    let (Some(out_map), Some(patch_map)) = (out.as_object_mut(), patch.as_object()) else {
        return out;
    };
    for (k, v) in patch_map {
        let both_objects = v.is_object() && out_map.get(k).is_some_and(Value::is_object);
        let merged = if both_objects && k != "export" { merge_state(&out_map[k], v) } else { v.clone() };
        out_map.insert(k.clone(), merged);
    }
    out
}

pub struct Project {
    pub id: String,
    pub src: Source,
    pub dir: PathBuf,
    state: Mutex<State>,
    /// Dernière orientation lue : (date de modification, taille, contenu).
    orient_cache: Mutex<Option<(SystemTime, u64, Arc<Orientation>)>>,
}

impl Project {
    /// Ouvre (sonde le fichier, crée le dossier du projet, relit `state.json`).
    pub fn open(path: &Path, home: &Path) -> Result<Arc<Project>> {
        let src = probe_source(path)?;
        let id = project_id(Path::new(&src.path))?;
        let dir = home.join("projects").join(&id);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("source.json"), serde_json::to_string_pretty(&src.to_json())?)?;
        let mut state = State::default();
        if let Ok(text) = std::fs::read_to_string(dir.join("state.json")) {
            let merged = serde_json::from_str::<Value>(&text)
                .map(|file| merge_state(&serde_json::to_value(&state).unwrap_or(Value::Null), &file));
            match merged.and_then(serde_json::from_value::<State>) {
                Ok(s) => state = s,
                Err(e) => tracing::warn!("réglages illisibles, valeurs par défaut utilisées ({e})"),
            }
        }
        if src.layout == Layout::Equirect {
            state.calibration = Calibration::default();
        }
        Ok(Arc::new(Project { id, src, dir, state: Mutex::new(state), orient_cache: Mutex::new(None) }))
    }

    pub fn state(&self) -> State {
        lock(&self.state).clone()
    }

    /// Fusionne un patch (clés inconnues ignorées ; keyframes triés par `t`) et sauvegarde.
    /// Erreur « utilisateur » (HTTP 400) si le résultat n'est pas un jeu de réglages valide ; l'état reste alors inchangé.
    pub fn update_state(&self, patch: &Value) -> Result<State> {
        let mut guard = lock(&self.state);
        let mut known = Map::new();
        if let Some(obj) = patch.as_object() {
            for (k, v) in obj.iter().filter(|(k, _)| STATE_KEYS.contains(&k.as_str())) {
                known.insert(k.clone(), v.clone());
            }
        }
        let current = serde_json::to_value(&*guard)?;
        let merged = merge_state(&current, &Value::Object(known));
        let mut next: State =
            serde_json::from_value(merged).map_err(|e| export_err(format!("Réglages invalides : {e}")))?;
        next.keyframes.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
        let file = self.state_file();
        let tmp = file.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&next)?)?;
        std::fs::rename(&tmp, &file)?;
        *guard = next.clone();
        Ok(next)
    }

    pub fn state_file(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    pub fn proxy_path(&self) -> PathBuf {
        self.dir.join("proxy.webm")
    }

    pub fn orient_path(&self) -> PathBuf {
        self.dir.join("orientation.bin")
    }

    pub fn calibration(&self) -> Calibration {
        self.state().calibration
    }

    pub fn has_proxy(&self) -> bool {
        std::fs::metadata(self.proxy_path()).is_ok_and(|m| m.is_file() && m.len() > 1000)
    }

    /// Orientation analysée (relue si le fichier a changé ; `None` si absente ou illisible).
    pub fn orientation(&self) -> Option<Arc<Orientation>> {
        let md = std::fs::metadata(self.orient_path()).ok()?;
        let stamp = (md.modified().ok()?, md.len());
        let mut cache = lock(&self.orient_cache);
        if let Some((t, len, o)) = cache.as_ref() {
            if (*t, *len) == stamp {
                return Some(o.clone());
            }
        }
        match Orientation::load(&self.orient_path()) {
            Ok(o) => {
                let o = Arc::new(o);
                *cache = Some((stamp.0, stamp.1, o.clone()));
                Some(o)
            }
            Err(_) => {
                *cache = None;
                None
            }
        }
    }

    /// `{available, ready, frames, stale, median_inliers, weak_frames}`.
    pub fn analysis_info(&self) -> Value {
        let mut info = json!({ "available": true, "ready": false });
        if let Some(o) = self.orientation() {
            let inl: Vec<u32> = if o.n() > 1 { o.inliers[1..].to_vec() } else { o.inliers.clone() };
            let mut sorted = inl.clone();
            sorted.sort_unstable();
            let median = match sorted.len() {
                0 => 0,
                n if n % 2 == 1 => u64::from(sorted[n / 2]),
                n => (u64::from(sorted[n / 2 - 1]) + u64::from(sorted[n / 2])) / 2,
            };
            info["ready"] = json!(true);
            info["frames"] = json!(o.n());
            info["stale"] = json!(o.key != calibration_key(&self.calibration()) && self.src.layout != Layout::Equirect);
            info["median_inliers"] = json!(median);
            info["weak_frames"] = json!(inl.iter().filter(|&&n| n < 20).count());
        }
        info
    }

    /// `T_t` pour les réglages courants (ou surchargés) ; `None` si rien à calculer (mode off sans orientation).
    pub fn stab_matrices(&self, over: &StabOverride) -> Option<Vec<Mat3>> {
        let state = self.state();
        let mode = over.mode.unwrap_or(state.stabilization.mode);
        let strength = over.strength.unwrap_or(state.stabilization.strength);
        let heading = over.heading_strength.unwrap_or(state.stabilization.heading_strength);
        let user = over.orientation.unwrap_or(state.orientation);
        let n_frames = self.src.frames() + 1;
        let orient = self.orientation();
        match orient {
            Some(o) if mode != StabMode::Off => {
                let start = over.start.unwrap_or(state.trim.start);
                let reference = (start * self.src.fps).round_ties_even().max(0.0) as usize;
                Some(stabilize::stabilization_matrices(
                    Some(&o),
                    mode,
                    strength,
                    &user,
                    reference,
                    Some(heading),
                    n_frames,
                ))
            }
            _ if user.is_zero() => None,
            _ => Some(stabilize::stabilization_matrices(None, StabMode::Off, strength, &user, 0, None, n_frames)),
        }
    }

    /// Réglages d'export : état du projet (+ `export` mémorisé) surchargés par `overrides`. Les valeurs
    /// `null` (champ vide dans l'interface) sont ignorées : la valeur par défaut s'applique.
    pub fn export_settings(&self, overrides: Option<&Value>) -> Result<ExportSettings> {
        let s = self.state();
        let mut d = Map::new();
        d.insert("projection".into(), json!(s.projection));
        d.insert("keyframes".into(), serde_json::to_value(&s.keyframes)?);
        d.insert("view".into(), serde_json::to_value(s.view)?);
        d.insert("orientation".into(), serde_json::to_value(s.orientation)?);
        d.insert("start".into(), json!(s.trim.start));
        d.insert("end".into(), json!(s.trim.end));
        for extra in [Some(&s.export), overrides].into_iter().flatten() {
            if let Some(obj) = extra.as_object() {
                for (k, v) in obj {
                    d.insert(k.clone(), v.clone());
                }
            }
        }
        d.retain(|_, v| !v.is_null());
        serde_json::from_value(Value::Object(d)).map_err(|e| export_err(format!("Réglages d'export invalides : {e}")))
    }

    /// JSON de l'API : `{id, name, source, state, proxy: {ready, url}, analysis, default_fov}`.
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": Path::new(&self.src.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            "source": self.src.to_json(),
            "state": self.state(),
            "proxy": { "ready": self.has_proxy(), "url": format!("/media/{}/proxy.webm", self.id) },
            "analysis": self.analysis_info(),
            "default_fov": {
                "flat": default_fov("flat"),
                "fisheye": default_fov("fisheye"),
                "planet": default_fov("planet"),
                "equirect": default_fov("equirect"),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{make_synthetic_insv, Motion, SynthOptions};
    use std::sync::OnceLock;

    /// Petit `.insv` synthétique partagé par les tests (généré une fois).
    fn sample() -> &'static Path {
        static SAMPLE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
        &SAMPLE
            .get_or_init(|| {
                let dir = tempfile::tempdir().unwrap();
                let p = dir.path().join("clip été.insv");
                let opt =
                    SynthOptions { seconds: 2.0, fps: 20.0, lens: 64, motion: Motion::Static, ..Default::default() };
                make_synthetic_insv(&p, &opt, None).unwrap();
                (dir, p)
            })
            .1
    }

    fn open(home: &Path) -> Arc<Project> {
        Project::open(sample(), home).unwrap()
    }

    fn golden() -> Value {
        serde_json::from_str(include_str!("../tests/golden/project.json")).unwrap()
    }

    #[test]
    fn default_state_has_the_python_shape() {
        let g = golden();
        assert_eq!(serde_json::to_value(State::default()).unwrap(), g["default_state"]);
        let back: State = serde_json::from_value(g["default_state"].clone()).unwrap();
        assert_eq!(back, State::default());
    }

    #[test]
    fn merge_matches_the_python_reference() {
        for (i, c) in golden()["cases"].as_array().unwrap().iter().enumerate() {
            assert_eq!(merge_state(&c["base"], &c["patch"]), c["merged"], "cas {i} : {}", c["patch"]);
        }
        assert_eq!(merge_state(&json!({"a": 1}), &Value::Null), json!({"a": 1}));
        assert_eq!(merge_state(&json!({"a": 1}), &json!([1])), json!({"a": 1}));
    }

    #[test]
    fn directories_follow_the_environment_rules() {
        let home = PathBuf::from("/home/jeanne");
        assert_eq!(
            home_dir_from(Some("/data/x3".into()), Some("/cache".into()), home.clone()),
            PathBuf::from("/data/x3")
        );
        assert_eq!(home_dir_from(None, Some("/cache".into()), home.clone()), PathBuf::from("/cache/x3studio"));
        assert_eq!(
            home_dir_from(Some(String::new()), Some(String::new()), home.clone()),
            PathBuf::from("/home/jeanne/.cache/x3studio")
        );
        assert_eq!(
            output_dir_from(Some("/home/jeanne/Vidéos\n".into()), home.clone()),
            PathBuf::from("/home/jeanne/Vidéos/X3Studio")
        );
        assert_eq!(
            output_dir_from(Some("/home/jeanne".into()), home.clone()),
            PathBuf::from("/home/jeanne/Videos/X3Studio")
        );
        assert_eq!(output_dir_from(Some("  ".into()), home.clone()), PathBuf::from("/home/jeanne/Videos/X3Studio"));
        assert_eq!(output_dir_from(None, home), PathBuf::from("/home/jeanne/Videos/X3Studio"));
    }

    #[test]
    fn project_ids_are_stable_hex_and_follow_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a b.insv");
        std::fs::write(&f, b"0123456789").unwrap();
        let id = project_id(&f).unwrap();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
        assert_eq!(project_id(&f).unwrap(), id);
        std::fs::write(&f, b"0123456789!").unwrap();
        assert_ne!(project_id(&f).unwrap(), id, "la taille change l'identifiant");
        assert!(project_id(&dir.path().join("absent")).is_err());
    }

    #[test]
    fn opening_creates_the_project_folder_and_persists_state() {
        let home = tempfile::tempdir().unwrap();
        let p = open(home.path());
        assert!(p.dir.starts_with(home.path().join("projects")) && p.dir.ends_with(&p.id));
        assert!(p.dir.join("source.json").is_file());
        assert_eq!(p.state(), State::default());
        let s = p
            .update_state(&json!({
                "view": {"fov": 60},
                "trim": {"start": 0.5},
                "projection": "planet",
                "stabilization": {"mode": "smooth"},
                "inconnu": {"x": 1},
            }))
            .unwrap();
        assert_eq!((s.view.fov, s.view.yaw, s.trim.start, s.trim.end), (60.0, 0.0, 0.5, 0.0), "fusion profonde");
        assert_eq!(
            (s.projection.as_str(), s.stabilization.mode, s.stabilization.strength),
            ("planet", StabMode::Smooth, 1.0)
        );
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(p.state_file()).unwrap()).unwrap();
        assert!(saved.get("inconnu").is_none());
        assert_eq!(saved["view"]["fov"], 60.0);
        assert!(!p.dir.join("state.tmp").exists());
        // une nouvelle ouverture relit les réglages
        let again = open(home.path());
        assert_eq!(again.id, p.id);
        assert_eq!(again.state(), s);
        // fichier d'état corrompu : valeurs par défaut, pas d'erreur
        std::fs::write(p.state_file(), "{pas du json").unwrap();
        assert_eq!(open(home.path()).state(), State::default());
    }

    #[test]
    fn keyframes_are_sorted_sanitised_and_validated() {
        let home = tempfile::tempdir().unwrap();
        let p = open(home.path());
        let s = p
            .update_state(&json!({"keyframes": [
                {"t": 2.0, "yaw": 10, "ease": "linear"},
                {"t": 0.5, "fov": 50.0, "pitch": -5},
            ]}))
            .unwrap();
        assert_eq!(s.keyframes.len(), 2);
        assert_eq!((s.keyframes[0].t, s.keyframes[0].fov, s.keyframes[0].yaw), (0.5, Some(50.0), None));
        assert_eq!((s.keyframes[1].t, s.keyframes[1].yaw), (2.0, Some(10.0)));
        let json = serde_json::to_value(&s.keyframes).unwrap();
        assert!(json[1].get("ease").is_none() && json[1].get("pitch").is_none());
        // un patch invalide est refusé, l'état et le fichier restent intacts
        let before = std::fs::read_to_string(p.state_file()).unwrap();
        for bad in [
            json!({"keyframes": [{"yaw": 1}]}),
            json!({"keyframes": "oui"}),
            json!({"view": {"fov": "large"}}),
            json!({"stabilization": {"mode": "zoom"}}),
            json!({"projection": 3}),
        ] {
            let e = p.update_state(&bad).unwrap_err();
            assert!(crate::error::is_user_error(&e), "{bad} → {e}");
        }
        assert_eq!(p.state(), s);
        assert_eq!(std::fs::read_to_string(p.state_file()).unwrap(), before);
        // `export` est remplacé, pas fusionné
        p.update_state(&json!({"export": {"codec": "h265", "crf": 18}})).unwrap();
        let s = p.update_state(&json!({"export": {"preset": "slow"}})).unwrap();
        assert_eq!(s.export, json!({"preset": "slow"}));
    }

    #[test]
    fn export_settings_layers_state_then_memo_then_overrides() {
        let home = tempfile::tempdir().unwrap();
        let p = open(home.path());
        p.update_state(&json!({
            "projection": "fisheye", "trim": {"start": 0.25, "end": 1.5}, "view": {"yaw": 30},
            "orientation": {"roll": 4},
            "keyframes": [{"t": 0.0, "yaw": 1}],
            "export": {"crf": 18, "width": 640, "inconnu": true},
        }))
        .unwrap();
        let s = p.export_settings(Some(&json!({"width": 320, "height": 180, "fps": null, "mode": "reframe"}))).unwrap();
        assert_eq!((s.projection.as_str(), s.start, s.end), ("fisheye", 0.25, 1.5));
        assert_eq!((s.crf, s.width, s.height, s.fps), (18, 320, 180, 0.0), "null = valeur par défaut");
        assert_eq!(s.orientation.roll, 4.0);
        assert_eq!(s.keyframes.len(), 1);
        assert_eq!(s.view["yaw"], 30.0);
        let e = p.export_settings(Some(&json!({"width": "large"}))).unwrap_err();
        assert!(crate::error::is_user_error(&e), "{e}");
        assert_eq!(p.export_settings(None).unwrap().width, 640);
    }

    #[test]
    fn proxy_and_analysis_files_are_tracked() {
        let home = tempfile::tempdir().unwrap();
        let p = open(home.path());
        assert!(!p.has_proxy() && p.orientation().is_none());
        assert_eq!(p.analysis_info(), json!({"available": true, "ready": false}));
        std::fs::write(p.proxy_path(), vec![0u8; 1000]).unwrap();
        assert!(!p.has_proxy(), "trop petit pour être un aperçu");
        std::fs::write(p.proxy_path(), vec![0u8; 1001]).unwrap();
        assert!(p.has_proxy());
        // orientation : 5 images dont l'estimation était faible pour deux d'entre elles
        let mut o = Orientation {
            quats: vec![[1.0, 0.0, 0.0, 0.0]; 5],
            fps: 20.0,
            inliers: vec![0, 120, 10, 100, 15],
            key: calibration_key(&p.calibration()),
        };
        o.save(&p.orient_path()).unwrap();
        let info = p.analysis_info();
        assert_eq!(
            info,
            json!({"available": true, "ready": true, "frames": 5, "stale": false, "median_inliers": 57, "weak_frames": 2})
        );
        // la calibration change : l'analyse est périmée
        p.update_state(&json!({"calibration": {"fov": 190}})).unwrap();
        assert_eq!(p.analysis_info()["stale"], true);
        // le nouveau fichier est relu (même taille, date différente ou contenu différent)
        o.quats[4] = [0.0, 1.0, 0.0, 0.0];
        o.key = "autre".into();
        std::thread::sleep(Duration::from_millis(20));
        o.save(&p.orient_path()).unwrap();
        assert_eq!(p.orientation().unwrap().quats[4], [0.0, 1.0, 0.0, 0.0]);
        // fichier corrompu : pas d'orientation
        std::fs::write(p.orient_path(), b"nimporte quoi").unwrap();
        assert!(p.orientation().is_none());
        assert_eq!(p.analysis_info()["ready"], false);
    }

    #[test]
    fn stabilization_matrices_follow_the_project_settings() {
        let home = tempfile::tempdir().unwrap();
        let p = open(home.path());
        let n = p.src.frames() + 1;
        assert_eq!(n, 41);
        assert!(p.stab_matrices(&StabOverride::default()).is_none(), "rien à calculer");
        // orientation utilisateur seule : n copies de O⁻¹
        let user = Ypr { yaw: 30.0, pitch: 0.0, roll: 5.0 };
        let t = p.stab_matrices(&StabOverride { orientation: Some(user), ..Default::default() }).unwrap();
        assert_eq!(t.len(), n);
        assert!((t[7] - user.matrix().transpose()).abs().max() < 1e-12);
        // avec analyse : le mode « verrouillé » ramène chaque image sur celle du début de la plage
        let quats: Vec<_> = (0..n)
            .map(|i| crate::geometry::quat_from_matrix(&crate::geometry::ypr_to_matrix(i as f64 * 0.5, 0.0, 0.0)))
            .collect();
        let o = Orientation { quats, fps: 20.0, inliers: vec![100; n], key: String::new() };
        o.save(&p.orient_path()).unwrap();
        p.update_state(&json!({"trim": {"start": 0.5}})).unwrap();
        let lock = p.stab_matrices(&StabOverride { mode: Some(StabMode::Lock), ..Default::default() }).unwrap();
        assert_eq!(lock.len(), n);
        let reference = 10; // 0,5 s à 20 i/s
        let want = o.matrices();
        assert!((lock[3] - want[3].transpose() * want[reference]).abs().max() < 1e-12);
        assert!((lock[reference] - Mat3::identity()).abs().max() < 1e-12);
        // la surcharge `start` (ligne de commande) remplace la plage du projet : référence = image 20 (1 s à 20 i/s)
        let lock_at_1s = p
            .stab_matrices(&StabOverride { mode: Some(StabMode::Lock), start: Some(1.0), ..Default::default() })
            .unwrap();
        assert!((lock_at_1s[3] - want[3].transpose() * want[20]).abs().max() < 1e-12);
        assert!((lock_at_1s[20] - Mat3::identity()).abs().max() < 1e-12);
        // mode du projet (smooth) et surcharge « off »
        p.update_state(&json!({"stabilization": {"mode": "smooth", "strength": 0.5}})).unwrap();
        assert_eq!(p.stab_matrices(&StabOverride::default()).unwrap().len(), n);
        assert!(p.stab_matrices(&StabOverride { mode: Some(StabMode::Off), ..Default::default() }).is_none());
    }

    #[test]
    fn to_json_matches_what_the_web_client_reads() {
        let home = tempfile::tempdir().unwrap();
        let p = open(home.path());
        let v = p.to_json();
        assert_eq!(v["id"], p.id.as_str());
        assert_eq!(v["name"], "clip été.insv");
        assert_eq!(v["source"]["layout"], "two_streams");
        assert_eq!(v["source"]["frames"], 40);
        assert_eq!(v["state"], serde_json::to_value(State::default()).unwrap());
        assert_eq!(v["proxy"], json!({"ready": false, "url": format!("/media/{}/proxy.webm", p.id)}));
        assert_eq!(v["analysis"]["ready"], false);
        assert_eq!(v["default_fov"], golden()["default_fov"]);
    }

    #[test]
    fn non_video_files_are_rejected_as_media_errors() {
        let home = tempfile::tempdir().unwrap();
        let f = home.path().join("notes.insv");
        std::fs::write(&f, b"ceci n'est pas une video").unwrap();
        let e = Project::open(&f, home.path()).err().unwrap();
        assert!(crate::error::is_user_error(&e), "{e}");
        assert!(Project::open(&home.path().join("absent.insv"), home.path()).is_err());
    }
}
