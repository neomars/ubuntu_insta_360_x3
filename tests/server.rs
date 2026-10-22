//! Serveur local : sécurité, API JSON, flux complet ouverture → aperçu → analyse → réglages → export.
//! Le serveur est lancé sur un port libre de 127.0.0.1 et interrogé par un vrai client HTTP.
mod common;

use common::*;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use x3studio::app::App;
use x3studio::server::start_test_server;
use x3studio::synth::{Motion, SynthOptions};

struct Srv {
    base: String,
    token: String,
    key: String,
    port: u16,
    app: Arc<App>,
    out: PathBuf,
    agent: ureq::Agent,
    _home: tempfile::TempDir,
    _rt: tokio::runtime::Runtime,
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("JSON attendu ({e}) : {:?}", String::from_utf8_lossy(&self.body)))
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn server() -> Srv {
    require_ffmpeg();
    let home = tempfile::tempdir().unwrap();
    let out = home.path().join("sorties");
    let app = App::new(Some(home.path().join("home")), Some(out.clone())).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (addr, state, _handle) = rt.block_on(start_test_server(app.clone())).unwrap();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(120)))
        .build()
        .into();
    Srv {
        base: format!("http://127.0.0.1:{}", addr.port()),
        token: state.token,
        key: state.key,
        port: addr.port(),
        app,
        out,
        agent,
        _home: home,
        _rt: rt,
    }
}

impl Srv {
    /// Requête avec jeton d'API (sauf `token == false`) ; `headers` complète ou remplace les en-têtes.
    fn request(&self, method: &str, path: &str, body: Option<&Value>, token: bool, headers: &[(&str, &str)]) -> Resp {
        let mut b = ureq::http::Request::builder().method(method).uri(format!("{}{path}", self.base));
        if token {
            b = b.header("X-X3-Token", &self.token);
        }
        if body.is_some() {
            b = b.header("Content-Type", "application/json");
        }
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let resp = match body {
            Some(v) => self.agent.run(b.body(serde_json::to_vec(v).unwrap()).unwrap()),
            None => self.agent.run(b.body(()).unwrap()),
        }
        .unwrap_or_else(|e| panic!("{method} {path} : {e}"));
        let status = resp.status().as_u16();
        let headers =
            resp.headers().iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string())).collect();
        let body = resp.into_body().read_to_vec().unwrap_or_default();
        Resp { status, headers, body }
    }

    fn call(&self, method: &str, path: &str, body: Option<Value>) -> Resp {
        self.request(method, path, body.as_ref(), true, &[])
    }

    fn get(&self, path: &str) -> Resp {
        self.call("GET", path, None)
    }

    fn status(&self, method: &str, path: &str, body: Option<Value>) -> u16 {
        self.call(method, path, body).status
    }

    /// Requête HTTP brute (en-têtes totalement maîtrisés : `Host`, `Origin`...).
    fn raw(&self, request_head: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(request_head.replace('\n', "\r\n").as_bytes()).unwrap();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf);
        let text = String::from_utf8_lossy(&buf).into_owned();
        let status = text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        (status, text)
    }

    /// Attend la fin de toutes les tâches.
    fn wait_jobs(&self) -> Vec<Value> {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(180) {
            let jobs = self.get("/api/jobs").json();
            let jobs = jobs.as_array().unwrap().clone();
            if !jobs.is_empty()
                && jobs.iter().all(|j| ["done", "error", "cancelled"].contains(&j["state"].as_str().unwrap()))
            {
                return jobs;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("les tâches ne se terminent pas : {}", self.get("/api/jobs").text());
    }
}

fn shake_path() -> PathBuf {
    clip(
        "server-shake",
        SynthOptions { seconds: 2.5, fps: 30.0, lens: 480, motion: Motion::Shake, ..Default::default() },
    )
    .path
}

fn static_path() -> PathBuf {
    clip(
        "server-static",
        SynthOptions { seconds: 1.0, fps: 30.0, lens: 240, motion: Motion::Static, ..Default::default() },
    )
    .path
}

#[test]
fn security_rules() {
    let s = server();
    // jeton
    assert_eq!(s.request("GET", "/api/info", None, false, &[]).status, 401);
    assert_eq!(s.request("GET", "/api/info", None, false, &[("X-X3-Token", "mauvais")]).status, 401);
    assert_eq!(s.request("GET", "/api/info", None, false, &[("X-X3-Token", &"x".repeat(1000))]).status, 401);
    assert_eq!(s.request("POST", "/api/open", Some(&json!({"path": "/etc/passwd"})), false, &[]).status, 401);
    assert_eq!(s.request("GET", "/media/0123456789abcdef/proxy.webm", None, false, &[]).status, 401);
    // le jeton passe aussi par l'URL (pour <video src>), jamais pour une écriture
    assert_eq!(s.request("GET", &format!("/api/info?token={}", s.token), None, false, &[]).status, 200);
    assert_eq!(
        s.request("POST", &format!("/api/open?token={}", s.token), Some(&json!({"path": "x"})), false, &[]).status,
        401
    );
    assert_eq!(
        s.request(
            "PUT",
            &format!("/api/project/0123456789abcdef/state?token={}", s.token),
            Some(&json!({})),
            false,
            &[]
        )
        .status,
        401
    );
    // « DNS rebinding » : l'en-tête Host doit être l'adresse locale
    assert_eq!(s.request("GET", "/api/info", None, true, &[("Host", "evil.example")]).status, 403);
    assert_eq!(s.request("GET", "/api/info", None, true, &[("Host", &format!("evil.example:{}", s.port))]).status, 403);
    assert_eq!(s.request("GET", "/api/info", None, true, &[("Host", &format!("localhost:{}", s.port))]).status, 200);
    assert_eq!(s.request("GET", "/api/info", None, true, &[("Host", &format!("LOCALHOST:{}", s.port))]).status, 200);
    let (code, _) = s.raw(&format!("GET /api/info HTTP/1.1\nX-X3-Token: {}\nConnection: close\n\n", s.token));
    assert_eq!(code, 403, "pas d'en-tête Host du tout");
    // pages tierces : l'en-tête Origin doit être le serveur lui-même
    assert_eq!(s.request("GET", "/api/info", None, true, &[("Origin", "https://evil.example")]).status, 403);
    assert_eq!(s.request("GET", "/api/info", None, true, &[("Origin", "null")]).status, 403);
    assert_eq!(
        s.request("GET", "/api/info", None, true, &[("Origin", &format!("http://127.0.0.1:{}", s.port))]).status,
        200
    );
    assert_eq!(
        s.request("POST", "/api/open", Some(&json!({})), true, &[("Origin", "http://evil.example")]).status,
        403
    );
    // chemins étranges
    for p in
        ["/../../etc/passwd", "/%2e%2e/%2e%2e/etc/passwd", "/api/../../etc/passwd", "//etc/passwd", "/app.js/../../x"]
    {
        let st = s.request("GET", p, None, false, &[]).status;
        assert!([400, 401, 403, 404].contains(&st), "{p} → {st}");
    }
    // méthodes
    assert_eq!(s.request("DELETE", "/", None, false, &[]).status, 405);
    assert_eq!(s.request("POST", "/app.js", Some(&json!({})), false, &[]).status, 405);
    assert_eq!(s.status("DELETE", "/api/info", None), 405);
    // le serveur n'écoute que sur la boucle locale
    assert!(s.base.starts_with("http://127.0.0.1:"));
    // en-têtes de réponse
    let r = s.get("/api/info");
    assert_eq!((r.header("cache-control"), r.header("x-content-type-options")), (Some("no-store"), Some("nosniff")));
}

#[test]
fn index_and_static_files() {
    let s = server();
    let anon = |p: &str, h: &[(&str, &str)]| s.request("GET", p, None, false, h);
    assert_eq!(anon("/", &[]).status, 403, "sans la clé de lancement");
    assert_eq!(anon("/?key=devinette", &[]).status, 403);
    assert_eq!(anon(&format!("/?key={}", &s.key[..s.key.len() - 1]), &[]).status, 403);
    assert_eq!(anon("/?key=", &[]).status, 403);
    let page = anon(&format!("/?key={}", s.key), &[]);
    assert_eq!(page.status, 200);
    let html = page.text();
    assert!(html.contains(&s.token) && !html.contains("__X3_TOKEN__"));
    assert!(page.header("content-type").unwrap().starts_with("text/html"));
    let cookie = page.header("set-cookie").unwrap();
    assert!(
        cookie.contains("HttpOnly")
            && cookie.contains("SameSite=Strict")
            && cookie.starts_with(&format!("x3session={}", s.token))
    );
    // le cookie de session permet de recharger la page sans la clé
    assert!(anon("/", &[("Cookie", &format!("x3session={}", s.token))]).text().contains(&s.token));
    assert!(anon("/", &[("Cookie", &format!("a=b; x3session={}; c=d", s.token))]).status == 200);
    assert_eq!(anon("/", &[("Cookie", "x3session=faux")]).status, 403);
    assert_eq!(anon("/index.html", &[("Cookie", &format!("x3session={}", s.token))]).status, 200);
    // fichiers de l'interface
    let js = anon("/app.js", &[]);
    assert!(js.text().contains("createElement") && js.header("content-type").unwrap().starts_with("text/javascript"));
    for (p, ct) in [("/viewer.js", "text/javascript"), ("/geometry.js", "text/javascript"), ("/style.css", "text/css")]
    {
        let r = anon(p, &[]);
        assert!(r.status == 200 && r.header("content-type").unwrap().starts_with(ct) && !r.body.is_empty(), "{p}");
    }
    assert_eq!(anon("/nope.js", &[]).status, 404);
    assert_eq!(anon("/web/app.js", &[]).status, 404);
    assert_eq!(s.request("HEAD", "/app.js", None, false, &[]).status, 200);
    // le jeton d'API n'apparaît que dans la page servie avec la clé
    assert!(!anon("/app.js", &[]).text().contains(&s.token));
}

#[test]
fn file_listing() {
    let s = server();
    let dir = tempfile::tempdir().unwrap();
    for name in ["b.mp4", "A.INSV", "notes.txt", ".cache.mp4", "c.MOV"] {
        std::fs::write(dir.path().join(name), b"x").unwrap();
    }
    for name in ["zeta", "Alpha", ".caché"] {
        std::fs::create_dir(dir.path().join(name)).unwrap();
    }
    let r = s.get(&format!("/api/fs?path={}", dir.path().display())).json();
    let names: Vec<(&str, bool)> = r["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["name"].as_str().unwrap(), e["dir"].as_bool().unwrap()))
        .collect();
    assert_eq!(
        names,
        vec![("Alpha", true), ("zeta", true), ("A.INSV", false), ("b.mp4", false), ("c.MOV", false)],
        "dossiers d'abord, vidéos seulement, rien de caché"
    );
    assert_eq!(r["path"], dir.path().canonicalize().unwrap().to_str().unwrap());
    assert!(r["parent"].is_string());
    let file = r["entries"][2].clone();
    assert_eq!(file["size"], 1);
    assert!(file["mtime"].as_f64().unwrap() > 1e9);
    // chemin accentué et encodé dans l'URL
    let acc = dir.path().join("été 2024");
    std::fs::create_dir(&acc).unwrap();
    std::fs::write(acc.join("clip.mp4"), b"x").unwrap();
    let enc = acc.to_str().unwrap().replace(' ', "%20").replace('é', "%C3%A9");
    assert_eq!(s.get(&format!("/api/fs?path={enc}")).json()["entries"][0]["name"], "clip.mp4");
    // chemin inexistant ou fichier : on retombe sur le dossier personnel, sans erreur
    for p in ["/nulle/part", &dir.path().join("b.mp4").display().to_string(), ""] {
        let r = s.get(&format!("/api/fs?path={p}"));
        assert_eq!(r.status, 200, "{p}");
        assert!(r.json()["entries"].is_array());
    }
    // la racine n'a pas de parent
    assert!(s.get("/api/fs?path=/").json()["parent"].is_null());
}

#[test]
fn request_validation() {
    let s = server();
    let bad = |method: &str, path: &str, body: &[u8]| {
        let b = ureq::http::Request::builder()
            .method(method)
            .uri(format!("{}{path}", s.base))
            .header("X-X3-Token", &s.token)
            .header("Content-Type", "application/json")
            .body(body.to_vec())
            .unwrap();
        let r = s.agent.run(b).unwrap();
        let st = r.status().as_u16();
        (st, String::from_utf8_lossy(&r.into_body().read_to_vec().unwrap()).into_owned())
    };
    let (st, body) = bad("POST", "/api/open", b"{pas du json");
    assert_eq!(st, 400);
    assert!(body.contains("JSON invalide"), "{body}");
    let (st, body) = bad("POST", "/api/open", b"[1,2]");
    assert_eq!(st, 400);
    assert!(body.contains("Objet JSON"), "{body}");
    assert_eq!(bad("POST", "/api/open", &vec![b'a'; 5 * 1024 * 1024]).0, 413, "corps trop gros");
    // erreurs de projets et de tâches
    assert_eq!(s.status("POST", "/api/open", Some(json!({"path": "/nulle/part.insv"}))), 400);
    assert_eq!(s.status("POST", "/api/open", Some(json!({}))), 400);
    assert_eq!(s.status("POST", "/api/open", Some(json!({"path": 42}))), 400);
    let e = s.call("POST", "/api/open", Some(json!({"path": "/nulle/part.insv"}))).json();
    assert!(e["error"].as_str().unwrap().contains("introuvable"), "{e}");
    for id in ["0123456789abcdef", "nope", "0123456789ABCDEF", "../../etc"] {
        for (m, sub) in [
            ("GET", ""),
            ("PUT", "/state"),
            ("POST", "/export"),
            ("GET", "/stab"),
            ("POST", "/snapshot"),
            ("GET", "/frame"),
            ("POST", "/analyze"),
        ] {
            let st = s.status(m, &format!("/api/project/{id}{sub}"), (m != "GET").then(|| json!({})));
            assert_eq!(st, 404, "{m} {id}{sub}");
        }
    }
    assert_eq!(s.status("GET", "/media/0123456789abcdef/proxy.webm", None), 404);
    assert_eq!(s.status("GET", "/api/jobs/j9999", None), 404);
    assert_eq!(s.status("GET", "/api/jobs/jabc", None), 404);
    assert_eq!(s.status("POST", "/api/jobs/j9999/cancel", Some(json!({}))), 404);
    assert_eq!(s.status("GET", "/api/open", None), 405);
    assert_eq!(s.status("POST", "/api/reveal", Some(json!({"path": "/nulle/part/ailleurs"}))), 404);
    assert_eq!(s.get("/api/jobs").json(), json!([]));
    let info = s.get("/api/info").json();
    assert_eq!(info["output_dir"], s.out.to_str().unwrap());
    for k in ["version", "ffmpeg", "opencv", "home", "encoders", "cpus"] {
        assert!(info.get(k).is_some(), "{k}");
    }
    assert_eq!(info["version"], x3studio::VERSION);
    assert!(info["encoders"].as_array().unwrap().iter().any(|e| e == "libx264"));
}

#[test]
fn open_edit_and_export_flow() {
    let s = server();
    let path = shake_path();
    let p = s.call("POST", "/api/open", Some(json!({"path": path}))).json();
    let pid = p["id"].as_str().unwrap().to_string();
    assert_eq!(pid.len(), 16);
    assert_eq!(p["source"]["layout"], "two_streams");
    assert_eq!(p["source"]["frames"], 75);
    assert_eq!(p["proxy"]["ready"], false);
    assert_eq!(p["name"], path.file_name().unwrap().to_str().unwrap());
    assert_eq!(p["state"]["projection"], "flat");
    assert_eq!(p["default_fov"], json!({"flat": 90.0, "fisheye": 180.0, "planet": 250.0, "equirect": 0.0}));
    let jobs = s.wait_jobs();
    let kinds: Vec<&str> = jobs.iter().map(|j| j["kind"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"proxy") && kinds.contains(&"analyze"), "{kinds:?}");
    assert!(jobs.iter().all(|j| j["state"] == "done"), "{jobs:?}");
    let p = s.get(&format!("/api/project/{pid}")).json();
    assert_eq!(
        (p["proxy"]["ready"].clone(), p["analysis"]["ready"].clone(), p["analysis"]["stale"].clone()),
        (json!(true), json!(true), json!(false))
    );
    let frames = p["analysis"]["frames"].as_u64().unwrap() as usize;
    assert!((74..=76).contains(&frames));
    assert!(p["analysis"]["median_inliers"].as_u64().unwrap() > 100 && p["analysis"]["weak_frames"] == 0);

    // aperçu : lecture partielle (<video> demande des plages)
    let url = format!("{}{}?token={}", s.base, p["proxy"]["url"].as_str().unwrap(), s.token);
    let part = s.agent.get(&url).header("Range", "bytes=10-109").call().unwrap();
    assert_eq!(part.status().as_u16(), 206);
    let cr = part.headers().get("content-range").unwrap().to_str().unwrap().to_string();
    let total: u64 = cr.rsplit('/').next().unwrap().parse().unwrap();
    assert!(cr.starts_with("bytes 10-109/") && total > 1000, "{cr}");
    assert_eq!(part.into_body().read_to_vec().unwrap().len(), 100);

    // matrices de stabilisation : N x 9 flottants f32 + en-têtes
    let lock = s.get(&format!("/api/project/{pid}/stab?mode=lock"));
    assert_eq!(lock.body.len(), 36 * frames);
    assert_eq!(lock.header("x-frames"), Some(frames.to_string().as_str()));
    assert!((lock.header("x-fps").unwrap().parse::<f64>().unwrap() - 30.0).abs() < 1e-9);
    let f: Vec<f32> = lock.body.chunks(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
    assert!(f.iter().all(|v| v.is_finite() && v.abs() <= 1.0001));
    assert!(
        (f[0] - 1.0).abs() < 1e-6 && (f[4] - 1.0).abs() < 1e-6 && (f[8] - 1.0).abs() < 1e-6,
        "verrouillé sur la première image : identité"
    );
    let off = s.get(&format!("/api/project/{pid}/stab?mode=off&yaw=30"));
    assert_eq!(off.body.len(), 36);
    let o: Vec<f32> = off.body.chunks(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
    let want = x3studio::geometry::ypr_to_matrix(30.0, 0.0, 0.0).transpose();
    for (i, w) in want.transpose().as_slice().iter().enumerate() {
        assert!((f64::from(o[i]) - w).abs() < 1e-6, "{o:?}");
    }
    assert_eq!(off.header("x-frames"), Some("1"));
    assert_eq!(s.status("GET", &format!("/api/project/{pid}/stab?mode=bogus"), None), 400);
    assert_eq!(s.get(&format!("/api/project/{pid}/stab?mode=smooth&strength=0.5&heading=2")).body.len(), 36 * frames);
    assert_eq!(
        s.get(&format!("/api/project/{pid}/stab?mode=horizon&strength=abc&ref=-4")).body.len(),
        36 * frames,
        "valeurs invalides ignorées"
    );

    // réglages : fusion partielle, persistance, clés inconnues ignorées
    let kfs = json!([{"t": 2, "yaw": 80, "pitch": 5, "roll": 0, "fov": 70}, {"t": 0, "yaw": 0, "pitch": 0, "roll": 0, "fov": 90}]);
    let r = s
        .call(
            "PUT",
            &format!("/api/project/{pid}/state"),
            Some(json!({"keyframes": kfs, "stabilization": {"mode": "lock"}, "trim": {"start": 0.2, "end": 2.0}})),
        )
        .json();
    assert_eq!(r["state"]["stabilization"], json!({"mode": "lock", "strength": 1.0, "heading_strength": 1.5}));
    assert_eq!(r["state"]["keyframes"][0]["t"], 0.0, "triés par instant");
    let saved: Value = serde_json::from_str(
        &std::fs::read_to_string(s.app.get_project(&pid).unwrap().dir.join("state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["keyframes"][1]["yaw"], 80.0);
    assert_eq!(s.status("PUT", &format!("/api/project/{pid}/state"), Some(json!({"malveillant": 1}))), 200);
    let e = s.call("PUT", &format!("/api/project/{pid}/state"), Some(json!({"stabilization": {"mode": "zoom"}})));
    assert_eq!(e.status, 400);
    assert!(e.json()["error"].as_str().unwrap().contains("Réglages invalides"));

    // export
    let j = s
        .call(
            "POST",
            &format!("/api/project/{pid}/export"),
            Some(json!({"settings": {"width": 320, "height": 180, "crf": 30, "preset": "ultrafast"}})),
        )
        .json();
    assert_eq!((j["kind"].as_str(), j["state"].as_str()), (Some("export"), Some("queued")));
    assert!(j["title"].as_str().unwrap().starts_with("Export flat : "));
    let done = s.wait_jobs();
    let ex = done.iter().find(|x| x["id"] == j["id"]).unwrap();
    assert_eq!(ex["state"], "done", "{ex}");
    let stem = path.file_stem().unwrap().to_str().unwrap().to_string();
    let out = s.out.join(format!("{stem}_flat.mp4"));
    assert_eq!(ex["output"], out.to_str().unwrap());
    let info = probe_video(&out);
    assert_eq!((info.w, info.h), (320, 180));
    assert!((info.duration - 1.8).abs() < 0.15, "plage 0,2 → 2 s : {} s", info.duration);
    // un second export ne remplace pas le premier
    s.call("POST", &format!("/api/project/{pid}/export"), Some(json!({"settings": {"width": 160, "height": 90, "preset": "ultrafast"}, "directory": s.out.to_str().unwrap()})));
    s.wait_jobs();
    assert!(s.out.join(format!("{stem}_flat_2.mp4")).is_file());
    // paramètres invalides : erreur immédiate, pas de tâche
    let before = s.get("/api/jobs").json().as_array().unwrap().len();
    assert_eq!(
        s.status(
            "POST",
            &format!("/api/project/{pid}/export"),
            Some(json!({"settings": {"width": 321, "height": 180}}))
        ),
        400
    );
    assert_eq!(
        s.status("POST", &format!("/api/project/{pid}/export"), Some(json!({"settings": {"codec": "av1"}}))),
        400
    );
    assert_eq!(s.get("/api/jobs").json().as_array().unwrap().len(), before);

    // image de la vue (aperçu image par image) et capture
    let img =
        s.request("GET", &format!("/api/project/{pid}/frame?t=1.0&w=320&h=180&token={}", s.token), None, false, &[]);
    assert_eq!(&img.body[..3], b"\xff\xd8\xff");
    assert_eq!(img.header("content-type"), Some("image/jpeg"));
    let (_, w, h) = read_image_bytes(&img.body);
    assert_eq!((w, h), (320, 180));
    assert_eq!(
        s.status("GET", &format!("/api/project/{pid}/frame?t=1.0&w=321&h=0"), None),
        200,
        "tailles bornées et arrondies"
    );
    let snap = s.call("POST", &format!("/api/project/{pid}/snapshot"), Some(json!({"t": 1.0, "format": "png"}))).json();
    assert!(
        snap["path"].as_str().unwrap().ends_with(".png") && PathBuf::from(snap["path"].as_str().unwrap()).is_file()
    );
    let snap = s
        .call(
            "POST",
            &format!("/api/project/{pid}/snapshot"),
            Some(json!({"t": 0.5, "settings": {"width": 160, "height": 90}})),
        )
        .json();
    assert!(snap["path"].as_str().unwrap().ends_with(".jpg"));

    // le projet est retrouvé avec ses réglages à la réouverture
    let p2 = s.call("POST", "/api/open", Some(json!({"path": path}))).json();
    assert_eq!(p2["id"], pid);
    assert_eq!(p2["state"]["keyframes"][1]["yaw"], 80.0);
    assert_eq!(p2["proxy"]["ready"], true);
    // analyse à la demande
    let a = s.call("POST", &format!("/api/project/{pid}/analyze"), Some(json!({}))).json();
    assert_eq!(a["kind"], "analyze");
    s.wait_jobs();
    let prep = s.call("POST", &format!("/api/project/{pid}/prepare"), Some(json!({}))).json();
    assert_eq!(prep, json!({"ok": true}), "aperçu et analyse déjà prêts");
}

/// Dimensions d'une image encodée en mémoire (via un fichier temporaire).
fn read_image_bytes(bytes: &[u8]) -> (Vec<u8>, usize, usize) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("i.jpg");
    std::fs::write(&p, bytes).unwrap();
    read_image(&p)
}

#[test]
fn preview_ranges_and_missing_proxy() {
    let s = server();
    let path = static_path();
    let p = s.call("POST", "/api/open", Some(json!({"path": path}))).json();
    let (pid, url) = (p["id"].as_str().unwrap().to_string(), p["proxy"]["url"].as_str().unwrap().to_string());
    // pas encore d'aperçu (ou en cours de création) : 404 net ; puis tout est prêt
    s.wait_jobs();
    let size = std::fs::metadata(s.app.get_project(&pid).unwrap().proxy_path()).unwrap().len();
    let get = |range: Option<&str>| {
        let mut r = s.agent.get(&format!("{}{url}", s.base)).header("X-X3-Token", &s.token);
        if let Some(v) = range {
            r = r.header("Range", v);
        }
        let resp = r.call().unwrap();
        (resp.status().as_u16(), resp.headers().clone(), resp.into_body().read_to_vec().unwrap())
    };
    let (st, h, body) = get(None);
    assert_eq!((st, body.len() as u64), (200, size));
    assert_eq!(h.get("accept-ranges").unwrap(), "bytes");
    assert_eq!(h.get("content-type").unwrap(), "video/webm");
    assert_eq!(h.get("content-length").unwrap().to_str().unwrap(), size.to_string());
    assert_eq!(&body[..4], &[0x1a, 0x45, 0xdf, 0xa3], "en-tête EBML d'un WebM");
    let (st, h, part) = get(Some("bytes=0-0"));
    assert_eq!((st, part.len()), (206, 1));
    assert_eq!(h.get("content-range").unwrap().to_str().unwrap(), format!("bytes 0-0/{size}"));
    let (st, _, tail) = get(Some("bytes=-500"));
    assert_eq!((st, tail.len()), (206, 500));
    assert_eq!(tail, body[body.len() - 500..]);
    let (st, h, open) = get(Some(&format!("bytes={}-", size - 10)));
    assert_eq!((st, open.len()), (206, 10));
    assert_eq!(h.get("content-range").unwrap().to_str().unwrap(), format!("bytes {}-{}/{size}", size - 10, size - 1));
    let (st, h, _) = get(Some(&format!("bytes={size}-")));
    assert_eq!(st, 416);
    assert_eq!(h.get("content-range").unwrap().to_str().unwrap(), format!("bytes */{size}"));
    assert_eq!(get(Some("bytes=5-1")).0, 416);
    assert_eq!(get(Some("bytes=abc")).0, 416);
    assert_eq!(get(Some("octets=0-1")).0, 200, "unité inconnue : ignorée");
    let (_, _, clipped) = get(Some(&format!("bytes=10-{}", size * 10)));
    assert_eq!(clipped.len() as u64, size - 10, "la fin est bornée à la taille");
    // HEAD : en-têtes seulement
    let head = s.request("HEAD", &url, None, true, &[]);
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    // projet sans aperçu
    let other = tempfile::tempdir().unwrap();
    let app = App::new(Some(other.path().join("h")), Some(other.path().join("o"))).unwrap();
    let p = app.open_project(&shake_path(), false).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (addr, state, _h) = rt.block_on(start_test_server(app)).unwrap();
    let r = ureq::Agent::config_builder().http_status_as_error(false).build().new_agent();
    let resp = r
        .get(&format!("http://127.0.0.1:{}/media/{}/proxy.webm", addr.port(), p.id))
        .header("X-X3-Token", &state.token)
        .call()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

#[test]
fn jobs_can_be_cancelled_and_listed() {
    let s = server();
    let path = shake_path();
    let pid = s.call("POST", "/api/open", Some(json!({"path": path}))).json()["id"].as_str().unwrap().to_string();
    s.wait_jobs();
    let big = s
        .call(
            "POST",
            &format!("/api/project/{pid}/export"),
            Some(json!({"settings": {"width": 1920, "height": 1080, "crf": 30}})),
        )
        .json();
    let queued: Vec<Value> = (0..2)
        .map(|_| {
            s.call(
                "POST",
                &format!("/api/project/{pid}/export"),
                Some(json!({"settings": {"width": 320, "height": 180, "preset": "ultrafast"}})),
            )
            .json()
        })
        .collect();
    // le premier est lourd, les suivants attendent derrière (une seule tâche lourde à la fois)
    let states: Vec<String> = queued
        .iter()
        .map(|j| {
            s.get(&format!("/api/jobs/{}", j["id"].as_str().unwrap())).json()["state"].as_str().unwrap().to_string()
        })
        .collect();
    assert!(states.iter().all(|st| st == "queued" || st == "running" || st == "done"), "{states:?}");
    let cancelled =
        s.call("POST", &format!("/api/jobs/{}/cancel", big["id"].as_str().unwrap()), Some(json!({}))).json();
    assert_eq!(cancelled["cancelled"], true);
    // annuler la dernière, encore en file
    let last = queued[1]["id"].as_str().unwrap();
    s.call("POST", &format!("/api/jobs/{last}/cancel"), Some(json!({})));
    let jobs = s.wait_jobs();
    let state_of = |id: &Value| jobs.iter().find(|j| j["id"] == *id).unwrap()["state"].as_str().unwrap().to_string();
    assert_eq!(state_of(&big["id"]), "cancelled");
    assert!(["done", "cancelled"].contains(&state_of(&queued[1]["id"]).as_str()));
    assert_eq!(state_of(&queued[0]["id"]), "done");
    let stem = path.file_stem().unwrap().to_str().unwrap().to_string();
    assert!(!s.out.join(format!("{stem}_flat.mp4")).exists(), "la sortie annulée est supprimée");
    assert!(s.out.join(format!("{stem}_flat_2.mp4")).is_file());
    // une tâche terminée ne peut plus être annulée
    assert_eq!(
        s.call("POST", &format!("/api/jobs/{}/cancel", big["id"].as_str().unwrap()), Some(json!({}))).json()
            ["cancelled"],
        false
    );
    // plus récentes d'abord
    let list = s.get("/api/jobs").json();
    let ids: Vec<u64> =
        list.as_array().unwrap().iter().map(|j| j["id"].as_str().unwrap()[1..].parse().unwrap()).collect();
    assert!(ids.windows(2).all(|w| w[0] > w[1]), "{ids:?}");
    assert_eq!(s.get(&format!("/api/jobs/{}", big["id"].as_str().unwrap())).json()["state"], "cancelled");
}

#[test]
fn bad_files_are_reported_not_crashed() {
    let s = server();
    let dir = tempfile::tempdir().unwrap();
    let junk = dir.path().join("faux.insv");
    std::fs::write(&junk, b"pas une video").unwrap();
    let r = s.call("POST", "/api/open", Some(json!({"path": junk})));
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(!r.json()["error"].as_str().unwrap().is_empty());
    let empty = dir.path().join("vide.mp4");
    std::fs::write(&empty, b"").unwrap();
    assert_eq!(s.status("POST", "/api/open", Some(json!({"path": empty}))), 400);
    assert_eq!(s.status("POST", "/api/open", Some(json!({"path": dir.path()}))), 400, "un dossier n'est pas une vidéo");
    // le serveur répond toujours
    assert_eq!(s.status("GET", "/api/info", None), 200);
}
