//! Gestionnaire de tâches en arrière-plan (aperçu, analyse, exports). Spec : `x3studio/jobs.py`.
//! Les tâches « lourdes » s'exécutent une par une (file), les « légères » par deux.

use crate::error::{is_cancelled, is_user_error, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Error,
    Cancelled,
}

impl JobState {
    pub fn is_finished(self) -> bool {
        matches!(self, JobState::Done | JobState::Error | JobState::Cancelled)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Error => "error",
            JobState::Cancelled => "cancelled",
        }
    }
}

/// État mutable d'une tâche.
#[derive(Debug, Clone)]
pub struct JobInner {
    pub state: JobState,
    pub progress: f64,
    pub message: String,
    pub output: String,
    pub error: String,
    pub created: f64,
    pub started: f64,
    pub finished: f64,
}

/// Fonction exécutée par une tâche : sa valeur `Ok` (chemin du résultat) devient `output`.
pub type JobFn = Box<dyn FnOnce(&Arc<Job>) -> Result<String> + Send>;
/// Appelée à la fin de la tâche dans tous les cas (nettoyage).
pub type DoneFn = Box<dyn FnOnce(&Arc<Job>) + Send>;

#[derive(Debug)]
pub struct Job {
    pub id: String,
    /// `proxy` | `analyze` | `export` | `snapshot`.
    pub kind: String,
    pub title: String,
    pub project_id: String,
    pub cancel: AtomicBool,
    pub inner: Mutex<JobInner>,
    /// Réveille [`JobManager::wait`] quand la tâche se termine.
    finished_cv: Condvar,
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Job {
    fn new(id: String, kind: &str, title: &str, project_id: &str) -> Job {
        Job {
            id,
            kind: kind.to_string(),
            title: title.to_string(),
            project_id: project_id.to_string(),
            cancel: AtomicBool::new(false),
            inner: Mutex::new(JobInner {
                state: JobState::Queued,
                progress: 0.0,
                message: String::new(),
                output: String::new(),
                error: String::new(),
                created: now(),
                started: 0.0,
                finished: 0.0,
            }),
            finished_cv: Condvar::new(),
        }
    }

    pub fn set_progress(&self, p: f64) {
        lock(&self.inner).progress = if p.is_finite() { p.clamp(0.0, 1.0) } else { 0.0 };
    }

    pub fn set_message(&self, m: &str) {
        lock(&self.inner).message = m.to_string();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn state(&self) -> JobState {
        lock(&self.inner).state
    }

    /// JSON de l'API : `{id, kind, title, project_id, state, progress, message, output, error, eta, elapsed}`.
    pub fn to_json(&self) -> Value {
        let j = lock(&self.inner).clone();
        let t = now();
        let eta = (j.state == JobState::Running && j.progress > 0.02)
            .then(|| (t - j.started) / j.progress * (1.0 - j.progress));
        let elapsed = if j.started > 0.0 { (if j.finished > 0.0 { j.finished } else { t }) - j.started } else { 0.0 };
        json!({
            "id": self.id,
            "kind": self.kind,
            "title": self.title,
            "project_id": self.project_id,
            "state": j.state.as_str(),
            "progress": (j.progress * 1e4).round() / 1e4,
            "message": j.message,
            "output": j.output,
            "error": j.error,
            "eta": eta,
            "elapsed": elapsed,
        })
    }

    /// Passe à un état final et réveille ceux qui attendent.
    fn finish(&self, state: JobState, error: Option<String>, output: Option<String>) {
        {
            let mut j = lock(&self.inner);
            j.state = state;
            j.finished = now();
            if let Some(e) = error {
                j.error = e;
            }
            if let Some(o) = output {
                j.output = o;
                j.progress = 1.0;
            }
        }
        self.finished_cv.notify_all();
    }
}

// ---------------------------------------------------------------------------
// Files d'exécution
// ---------------------------------------------------------------------------
type Task = Box<dyn FnOnce() + Send>;

struct PoolState {
    tasks: VecDeque<Task>,
    closed: bool,
    spawned: usize,
}

/// Quelques threads qui exécutent les tâches dans l'ordre d'arrivée (créés à la demande).
struct Pool {
    name: &'static str,
    max_workers: usize,
    shared: Arc<(Mutex<PoolState>, Condvar)>,
}

impl Pool {
    fn new(name: &'static str, max_workers: usize) -> Pool {
        Pool {
            name,
            max_workers,
            shared: Arc::new((
                Mutex::new(PoolState { tasks: VecDeque::new(), closed: false, spawned: 0 }),
                Condvar::new(),
            )),
        }
    }

    fn submit(&self, task: Task) {
        let spawn_id = {
            let mut st = lock(&self.shared.0);
            if st.closed {
                return;
            }
            st.tasks.push_back(task);
            if st.spawned < self.max_workers {
                st.spawned += 1;
                Some(st.spawned)
            } else {
                None
            }
        };
        self.shared.1.notify_one();
        if let Some(i) = spawn_id {
            let shared = self.shared.clone();
            let spawned = std::thread::Builder::new().name(format!("{}-{i}", self.name)).spawn(move || worker(&shared));
            if spawned.is_err() {
                // plus de thread disponible : la tâche reste en file et sera prise par un autre worker, s'il y en a
                lock(&self.shared.0).spawned -= 1;
            }
        }
    }

    /// Arrête les workers (la tâche en cours se termine ; celles en attente sont abandonnées).
    fn close(&self) {
        {
            let mut st = lock(&self.shared.0);
            st.closed = true;
            st.tasks.clear();
        }
        self.shared.1.notify_all();
    }
}

fn worker(shared: &(Mutex<PoolState>, Condvar)) {
    loop {
        let task = {
            let mut st = lock(&shared.0);
            loop {
                if st.closed {
                    return;
                }
                if let Some(t) = st.tasks.pop_front() {
                    break t;
                }
                st = shared.1.wait(st).unwrap_or_else(|e| e.into_inner());
            }
        };
        task();
    }
}

struct Inner {
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    next_id: AtomicU64,
    /// Tâches soumises et pas encore entièrement terminées (`on_done` compris).
    busy: Arc<AtomicUsize>,
    heavy: Pool,
    light: Pool,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.heavy.close();
        self.light.close();
    }
}

/// Au-delà de ce nombre de tâches, les plus anciennes déjà terminées sont oubliées.
const MAX_KEPT_JOBS: usize = 300;

#[derive(Clone)]
pub struct JobManager {
    inner: Arc<Inner>,
}

impl JobManager {
    pub fn new() -> Self {
        JobManager {
            inner: Arc::new(Inner {
                jobs: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                busy: Arc::new(AtomicUsize::new(0)),
                heavy: Pool::new("x3-heavy", 1),
                light: Pool::new("x3-light", 2),
            }),
        }
    }

    /// Soumet `f` ; son résultat `Ok(sortie)` devient `output` ; `Err(Cancelled)` → état annulé ;
    /// toute autre erreur → état erreur (message affiché tel quel pour `MediaError`/`ExportError`).
    /// `on_done` est appelé à la fin dans tous les cas (nettoyage).
    pub fn submit(
        &self,
        kind: &str,
        title: &str,
        project_id: &str,
        heavy: bool,
        f: JobFn,
        on_done: Option<DoneFn>,
    ) -> Arc<Job> {
        let id = format!("j{}", self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let job = Arc::new(Job::new(id.clone(), kind, title, project_id));
        {
            let mut jobs = lock(&self.inner.jobs);
            jobs.insert(id, job.clone());
            if jobs.len() > MAX_KEPT_JOBS {
                let mut done: Vec<(f64, String)> = jobs
                    .values()
                    .filter_map(|j| {
                        let i = lock(&j.inner);
                        i.state.is_finished().then(|| (i.finished, j.id.clone()))
                    })
                    .collect();
                done.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
                let excess = jobs.len() - MAX_KEPT_JOBS;
                for (_, id) in done.into_iter().take(excess) {
                    jobs.remove(&id);
                }
            }
        }
        let j = job.clone();
        let busy = self.inner.busy.clone();
        busy.fetch_add(1, Ordering::SeqCst);
        let task: Task = Box::new(move || {
            run_job(&j, f, on_done);
            busy.fetch_sub(1, Ordering::SeqCst);
        });
        if heavy { &self.inner.heavy } else { &self.inner.light }.submit(task);
        job
    }

    pub fn get(&self, id: &str) -> Option<Arc<Job>> {
        lock(&self.inner.jobs).get(id).cloned()
    }

    /// Annule (retourne vrai si la tâche n'était pas terminée).
    pub fn cancel(&self, id: &str) -> bool {
        let Some(job) = self.get(id) else { return false };
        {
            let mut j = lock(&job.inner);
            if j.state.is_finished() {
                return false;
            }
            job.cancel.store(true, Ordering::Relaxed);
            if j.state == JobState::Queued {
                // en file : annulée tout de suite ; le worker ne la lancera pas
                j.state = JobState::Cancelled;
                j.finished = now();
            }
        }
        job.finished_cv.notify_all();
        true
    }

    /// Arrêt du programme : annule toutes les tâches non terminées et attend (au plus `timeout`) qu'elles aient
    /// réellement fini (processus ffmpeg tués, sorties partielles supprimées). Vrai si tout est arrêté.
    pub fn shutdown(&self, timeout: Duration) -> bool {
        let ids: Vec<String> = lock(&self.inner.jobs).keys().cloned().collect();
        for id in &ids {
            self.cancel(id);
        }
        let deadline = Instant::now() + timeout;
        while self.inner.busy.load(Ordering::SeqCst) > 0 {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    /// Les 50 plus récentes d'abord.
    pub fn list(&self) -> Vec<Value> {
        let mut jobs: Vec<(f64, u64, Arc<Job>)> = lock(&self.inner.jobs)
            .values()
            .map(|j| (lock(&j.inner).created, j.id[1..].parse().unwrap_or(0), j.clone()))
            .collect();
        jobs.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(b.1.cmp(&a.1)));
        jobs.into_iter().take(50).map(|(_, _, j)| j.to_json()).collect()
    }

    /// Tâche active (en attente ou en cours) d'un projet pour un type donné.
    pub fn active(&self, project_id: &str, kind: &str) -> Option<Arc<Job>> {
        lock(&self.inner.jobs)
            .values()
            .find(|j| j.project_id == project_id && j.kind == kind && !j.state().is_finished())
            .cloned()
    }

    /// Attend la fin (tests) ; erreur si délai dépassé.
    pub fn wait(&self, id: &str, timeout_s: f64) -> Result<Arc<Job>> {
        let job = self.get(id).ok_or_else(|| anyhow::anyhow!("tâche inconnue : {id}"))?;
        let deadline = Instant::now() + Duration::from_secs_f64(timeout_s.max(0.0));
        let mut guard = lock(&job.inner);
        while !guard.state.is_finished() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                anyhow::bail!("délai dépassé en attendant la tâche {id}");
            }
            guard = job.finished_cv.wait_timeout(guard, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        drop(guard);
        Ok(job)
    }
}

impl Default for JobManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Exécution d'une tâche par un worker : une erreur ou une panique de `f` ne tue jamais le worker.
fn run_job(job: &Arc<Job>, f: JobFn, on_done: Option<DoneFn>) {
    let start = {
        let mut j = lock(&job.inner);
        let queued = j.state == JobState::Queued;
        if queued {
            j.state = JobState::Running;
            j.started = now();
        }
        queued
    };
    if start {
        match catch_unwind(AssertUnwindSafe(|| f(job))) {
            Ok(Ok(out)) => job.finish(JobState::Done, None, Some(out)),
            Ok(Err(e)) if is_cancelled(&e) => job.finish(JobState::Cancelled, None, None),
            Ok(Err(e)) => {
                let msg = if is_user_error(&e) { e.to_string() } else { format!("{e:#}") };
                job.finish(JobState::Error, Some(if msg.is_empty() { "erreur".into() } else { msg }), None);
            }
            Err(panic) => {
                let what = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                tracing::error!("panique dans la tâche {} : {what}", job.id);
                job.finish(JobState::Error, Some(format!("Erreur interne : {what}")), None);
            }
        }
    }
    if let Some(done) = on_done {
        let _ = catch_unwind(AssertUnwindSafe(|| done(job)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{export_err, Cancelled};
    use std::sync::mpsc;

    fn ok(out: &'static str) -> JobFn {
        Box::new(move |_| Ok(out.to_string()))
    }

    #[test]
    fn a_successful_job_reports_its_output_and_json_shape() {
        let m = JobManager::new();
        let j = m.submit(
            "export",
            "Export : démo",
            "p1",
            true,
            Box::new(|job| {
                job.set_message("en cours");
                job.set_progress(0.5);
                Ok("/tmp/sortie.mp4".into())
            }),
            None,
        );
        assert_eq!(j.id, "j1");
        let j = m.wait("j1", 10.0).unwrap();
        let v = j.to_json();
        assert_eq!(v["state"], "done");
        assert_eq!(v["progress"], 1.0);
        assert_eq!(v["output"], "/tmp/sortie.mp4");
        assert_eq!(v["message"], "en cours");
        assert_eq!(
            (v["kind"].as_str(), v["title"].as_str(), v["project_id"].as_str()),
            (Some("export"), Some("Export : démo"), Some("p1"))
        );
        assert!(v["eta"].is_null() && v["error"] == "" && v["elapsed"].as_f64().unwrap() >= 0.0);
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        for k in
            ["id", "kind", "title", "project_id", "state", "progress", "message", "output", "error", "eta", "elapsed"]
        {
            assert!(keys.contains(&k), "clé manquante : {k}");
        }
        assert_eq!(m.submit("proxy", "x", "", true, ok("a"), None).id, "j2");
    }

    #[test]
    fn errors_and_panics_are_reported_and_the_worker_survives() {
        let m = JobManager::new();
        let a = m.submit("export", "a", "p", true, Box::new(|_| Err(export_err("Plage de lecture vide."))), None);
        let b = m.submit(
            "export",
            "b",
            "p",
            true,
            Box::new(|_| Err(anyhow::anyhow!("disque plein").context("écriture"))),
            None,
        );
        let c = m.submit("export", "c", "p", true, Box::new(|_| panic!("boum")), None);
        let d = m.submit("export", "d", "p", true, ok("fin"), None);
        let (a, b, c, d) = (
            m.wait(&a.id, 10.0).unwrap(),
            m.wait(&b.id, 10.0).unwrap(),
            m.wait(&c.id, 10.0).unwrap(),
            m.wait(&d.id, 10.0).unwrap(),
        );
        assert_eq!((a.state(), lock(&a.inner).error.clone()), (JobState::Error, "Plage de lecture vide.".to_string()));
        assert_eq!(lock(&b.inner).error, "écriture: disque plein");
        assert_eq!(c.state(), JobState::Error);
        assert!(lock(&c.inner).error.contains("boum"));
        assert_eq!(d.state(), JobState::Done, "le worker survit à une panique");
    }

    #[test]
    fn cancelling_a_running_job_and_a_queued_one() {
        let m = JobManager::new();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel::<String>();
        let tx = done_tx.clone();
        let running = m.submit(
            "export",
            "long",
            "p",
            true,
            Box::new(move |job| {
                started_tx.send(()).unwrap();
                while !job.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(Cancelled.into())
            }),
            Some(Box::new(move |j| tx.send(format!("{}:{}", j.id, j.state().as_str())).unwrap())),
        );
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let tx = done_tx;
        let queued = m.submit(
            "export",
            "attend",
            "p",
            true,
            ok("jamais"),
            Some(Box::new(move |j| tx.send(format!("{}:{}", j.id, j.state().as_str())).unwrap())),
        );
        assert_eq!(queued.state(), JobState::Queued, "un seul travail lourd à la fois");
        assert!(m.active("p", "export").is_some());
        // annulation de la tâche en file : immédiate
        assert!(m.cancel(&queued.id));
        assert_eq!(queued.state(), JobState::Cancelled);
        assert!(m.cancel(&running.id));
        assert_eq!(m.wait(&running.id, 10.0).unwrap().state(), JobState::Cancelled);
        assert!(!m.cancel(&running.id), "déjà terminée");
        assert!(!m.cancel("j99"));
        let mut calls = vec![
            done_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            done_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        ];
        calls.sort();
        assert_eq!(calls, vec!["j1:cancelled", "j2:cancelled"], "on_done est appelé dans tous les cas");
        assert!(m.active("p", "export").is_none());
    }

    #[test]
    fn light_jobs_run_two_at_a_time() {
        let m = JobManager::new();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let ids: Vec<String> = (0..2)
            .map(|_| {
                let b = barrier.clone();
                m.submit(
                    "snapshot",
                    "s",
                    "p",
                    false,
                    Box::new(move |_| {
                        b.wait();
                        Ok("ok".into())
                    }),
                    None,
                )
                .id
                .clone()
            })
            .collect();
        for id in ids {
            assert_eq!(m.wait(&id, 10.0).unwrap().state(), JobState::Done, "les deux tâches se croisent à la barrière");
        }
    }

    #[test]
    fn eta_list_order_and_wait_timeout() {
        let m = JobManager::new();
        let (tx, rx) = mpsc::channel::<()>();
        let slow = m.submit(
            "proxy",
            "lent",
            "p",
            true,
            Box::new(move |job| {
                job.set_progress(0.5);
                rx.recv_timeout(Duration::from_secs(20)).ok();
                Ok(String::new())
            }),
            None,
        );
        std::thread::sleep(Duration::from_millis(150));
        let v = slow.to_json();
        assert_eq!(v["state"], "running");
        assert!(v["eta"].as_f64().unwrap() > 0.0 && v["elapsed"].as_f64().unwrap() > 0.1, "{v}");
        let e = m.wait(&slow.id, 0.05).unwrap_err();
        assert!(e.to_string().contains("délai"));
        assert!(m.wait("j404", 1.0).is_err());
        tx.send(()).unwrap();
        m.wait(&slow.id, 10.0).unwrap();
        for i in 0..60 {
            m.submit("proxy", &format!("t{i}"), "q", false, ok(""), None);
        }
        let list = m.list();
        assert_eq!(list.len(), 50);
        assert_eq!(list[0]["id"], "j61", "la plus récente d'abord");
        assert_eq!(m.get("j1").map(|j| j.id.clone()), Some("j1".to_string()));
        // la progression est bornée
        slow.set_progress(7.0);
        assert_eq!(lock(&slow.inner).progress, 1.0);
        slow.set_progress(f64::NAN);
        assert_eq!(lock(&slow.inner).progress, 0.0);
    }

    #[test]
    fn shutdown_cancels_everything_and_waits_for_the_cleanup() {
        let m = JobManager::new();
        let (started_tx, started_rx) = mpsc::channel();
        let cleaned = Arc::new(AtomicBool::new(false));
        let flag = cleaned.clone();
        let running = m.submit(
            "export",
            "long",
            "p",
            true,
            Box::new(move |job| {
                started_tx.send(()).unwrap();
                while !job.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                std::thread::sleep(Duration::from_millis(150)); // nettoyage lent (tuer ffmpeg, supprimer la sortie)
                Err(Cancelled.into())
            }),
            Some(Box::new(move |_| {
                std::thread::sleep(Duration::from_millis(50));
                flag.store(true, Ordering::SeqCst);
            })),
        );
        let queued = m.submit("export", "attend", "p", true, ok("jamais"), None);
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(m.shutdown(Duration::from_secs(10)));
        assert!(cleaned.load(Ordering::SeqCst), "on_done a fini avant le retour de shutdown");
        assert_eq!((running.state(), queued.state()), (JobState::Cancelled, JobState::Cancelled));
        // rien à faire : retour immédiat
        let t0 = Instant::now();
        assert!(m.shutdown(Duration::from_secs(10)) && t0.elapsed() < Duration::from_secs(1));
        // une tâche qui ignore l'annulation : le délai est respecté
        let stubborn = JobManager::new();
        let (tx, rx) = mpsc::channel::<()>();
        stubborn.submit(
            "export",
            "têtue",
            "p",
            true,
            Box::new(move |_| Ok(rx.recv_timeout(Duration::from_secs(3)).map(|_| String::new()).unwrap_or_default())),
            None,
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(!stubborn.shutdown(Duration::from_millis(200)));
        drop(tx);
    }

    #[test]
    fn dropping_the_manager_stops_its_workers() {
        let m = JobManager::new();
        let shared = m.inner.heavy.shared.clone();
        m.submit("proxy", "x", "", true, ok(""), None);
        m.wait("j1", 10.0).unwrap();
        drop(m);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&shared) > 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(Arc::strong_count(&shared), 1, "le thread de travail s'est arrêté");
    }
}
