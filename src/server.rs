//! Serveur HTTP local (127.0.0.1 uniquement) + API JSON de l'interface web (axum).
//! Spec : `x3studio/server.py` + `web/app.js` (le client). L'interface (`web/*`) est embarquée dans
//! le binaire (`include_str!`).
//!
//! Sécurité : écoute sur 127.0.0.1 seulement ; contrôle de l'en-tête `Host` (contre le « DNS
//! rebinding ») ; refus des `Origin` étrangères ; jeton aléatoire par lancement pour `/api/*` et
//! `/media/*` (en-tête `X-X3-Token`, ou `?token=` pour les lectures ; les écritures exigent l'en-tête) ;
//! la page d'accueil n'est servie qu'avec la clé de lancement présente dans l'URL affichée au
//! démarrage (puis un cookie de session `HttpOnly; SameSite=Strict`), de sorte qu'un autre programme
//! ne puisse pas simplement interroger le port.

use crate::app::App;
use crate::error::{is_user_error, Result};
use crate::geometry::Ypr;
use crate::media::VIDEO_EXTS;
use crate::project::Project;
use crate::stabilize::{stabilization_matrices, StabMode};
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path as UrlPath, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

const MAX_BODY: usize = 4 * 1024 * 1024;

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const VIEWER_JS: &str = include_str!("../web/viewer.js");
const GEOMETRY_JS: &str = include_str!("../web/geometry.js");
const STYLE_CSS: &str = include_str!("../web/style.css");

#[derive(Clone)]
pub struct ServerState {
    pub app: Arc<App>,
    /// Jeton d'API (généré à chaque lancement).
    pub token: String,
    /// Clé de lancement (dans l'URL ouverte au démarrage).
    pub key: String,
    pub port: u16,
}

// ---------------------------------------------------------------------------
// Erreurs d'API
// ---------------------------------------------------------------------------
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        ApiError { status, message: message.into() }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        if is_user_error(&e) {
            ApiError::new(StatusCode::BAD_REQUEST, e.to_string())
        } else {
            tracing::warn!("erreur interne : {e:#}");
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("Erreur interne : {e}"))
        }
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

/// Exécute un calcul bloquant (sonde ffprobe, rendu, matrices...) hors de la boucle asynchrone.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> ApiResult<T> + Send + 'static) -> ApiResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("Erreur interne : {e}")))?
}

fn parse_body(bytes: &Bytes) -> ApiResult<Value> {
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    match serde_json::from_slice::<Value>(bytes) {
        Ok(v @ Value::Object(_)) => Ok(v),
        Ok(_) => Err(ApiError::new(StatusCode::BAD_REQUEST, "Objet JSON attendu")),
        Err(_) => Err(ApiError::new(StatusCode::BAD_REQUEST, "JSON invalide")),
    }
}

fn project_of(state: &ServerState, id: &str) -> ApiResult<Arc<Project>> {
    let valid = id.len() == 16 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    valid
        .then(|| state.app.get_project(id))
        .flatten()
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Projet inconnu"))
}

// ---------------------------------------------------------------------------
// Sécurité (middleware)
// ---------------------------------------------------------------------------
/// Comparaison en temps constant.
fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn query_of(req: &Request) -> HashMap<String, String> {
    req.uri().query().map(form_urlencoded_pairs).unwrap_or_default()
}

/// Décodage `application/x-www-form-urlencoded` minimal (`%XX`, `+`).
fn form_urlencoded_pairs(q: &str) -> HashMap<String, String> {
    fn dec(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'+' => out.push(b' '),
                b'%' if i + 2 < b.len() => {
                    let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                    match hex {
                        Some(v) => {
                            out.push(v);
                            i += 2;
                        }
                        None => out.push(b'%'),
                    }
                }
                c => out.push(c),
            }
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (dec(k), dec(v)),
            None => (dec(p), String::new()),
        })
        .collect()
}

fn host_ok(headers: &HeaderMap, port: u16) -> bool {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
}

fn origin_ok(headers: &HeaderMap, port: u16) -> bool {
    match headers.get(header::ORIGIN) {
        None => true,
        Some(v) => {
            let o = v.to_str().unwrap_or("").to_ascii_lowercase();
            o == format!("http://127.0.0.1:{port}") || o == format!("http://localhost:{port}")
        }
    }
}

async fn guard(State(st): State<ServerState>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    let deny = |status: StatusCode, msg: &str| ApiError::new(status, msg).into_response();

    let mut resp = if !host_ok(req.headers(), st.port) || !origin_ok(req.headers(), st.port) {
        deny(StatusCode::FORBIDDEN, "Hôte ou origine refusés")
    } else if path.starts_with("/api/") || path.starts_with("/media/") {
        let header_token = req.headers().get("x-x3-token").and_then(|v| v.to_str().ok()).map(str::to_string);
        let query_token = query_of(&req).get("token").cloned();
        let given = header_token.clone().or(query_token).unwrap_or_default();
        if !ct_eq(&given, &st.token) {
            deny(StatusCode::UNAUTHORIZED, "Jeton invalide")
        } else if matches!(method, Method::POST | Method::PUT) && header_token.is_none() {
            deny(StatusCode::UNAUTHORIZED, "Jeton manquant")
        } else {
            next.run(req).await
        }
    } else if !matches!(method, Method::GET | Method::HEAD) {
        deny(StatusCode::METHOD_NOT_ALLOWED, "Méthode non autorisée")
    } else {
        next.run(req).await
    };
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    resp
}

// ---------------------------------------------------------------------------
// Pages et fichiers statiques
// ---------------------------------------------------------------------------
fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

async fn index(State(st): State<ServerState>, headers: HeaderMap, req: Request) -> Response {
    let key = query_of(&req).get("key").cloned().unwrap_or_default();
    let cookie_ok = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.split(';').any(|p| ct_eq(p.trim(), &format!("x3session={}", st.token))));
    if !(!key.is_empty() && ct_eq(&key, &st.key)) && !cookie_ok {
        return (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "Ouvrez l'adresse complète affichée dans le terminal au lancement de x3studio (elle contient la clé de session).",
        )
            .into_response();
    }
    let html = INDEX_HTML.replace("__X3_TOKEN__", &st.token);
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (header::SET_COOKIE, format!("x3session={}; HttpOnly; SameSite=Strict; Path=/", st.token)),
        ],
        html,
    )
        .into_response()
}

async fn app_js() -> Response {
    asset("text/javascript; charset=utf-8", APP_JS)
}
async fn viewer_js() -> Response {
    asset("text/javascript; charset=utf-8", VIEWER_JS)
}
async fn geometry_js() -> Response {
    asset("text/javascript; charset=utf-8", GEOMETRY_JS)
}
async fn style_css() -> Response {
    asset("text/css; charset=utf-8", STYLE_CSS)
}
async fn not_found() -> Response {
    ApiError::new(StatusCode::NOT_FOUND, "Introuvable").into_response()
}

// ---------------------------------------------------------------------------
// Médias (aperçu WebM, requêtes Range)
// ---------------------------------------------------------------------------
/// `(début, fin incluse)` d'un en-tête `Range: bytes=...` ; `None` = invalide / non satisfaisable.
fn parse_range(value: &str, size: u64) -> Option<(u64, u64)> {
    let spec = value.trim().strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    if size == 0 {
        return None;
    }
    let (start, end) = match (a.is_empty(), b.is_empty()) {
        (true, true) => return None,
        (true, false) => (size.saturating_sub(b.parse::<u64>().ok()?), size - 1),
        (false, true) => (a.parse().ok()?, size - 1),
        (false, false) => (a.parse().ok()?, b.parse::<u64>().ok()?.min(size - 1)),
    };
    (start <= end && start < size).then_some((start, end))
}

async fn media(State(st): State<ServerState>, UrlPath(id): UrlPath<String>, headers: HeaderMap) -> ApiResult<Response> {
    let p = project_of(&st, &id).map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "Aperçu indisponible"))?;
    let pc = p.clone();
    if !blocking(move || Ok(pc.has_proxy())).await? {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Aperçu indisponible"));
    }
    let file_path = p.proxy_path();
    let size = tokio::fs::metadata(&file_path)
        .await
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "Aperçu indisponible"))?
        .len();
    let mut status = StatusCode::OK;
    let (mut start, mut end) = (0, size.saturating_sub(1));
    if let Some(r) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        if r.trim().starts_with("bytes=") {
            match parse_range(r, size) {
                Some((s, e)) => {
                    (start, end, status) = (s, e, StatusCode::PARTIAL_CONTENT);
                }
                None => {
                    return Ok((
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        [(header::CONTENT_RANGE, format!("bytes */{size}"))],
                    )
                        .into_response());
                }
            }
        }
    }
    let length = if size == 0 { 0 } else { end - start + 1 };
    let mut file = tokio::fs::File::open(&file_path)
        .await
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "Aperçu indisponible"))?;
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let body = Body::from_stream(tokio_util::io::ReaderStream::with_capacity(file.take(length), 1 << 20));
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("video/webm"));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    if status == StatusCode::PARTIAL_CONTENT {
        h.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{size}")).expect("en-tête valide"),
        );
    }
    Ok(resp)
}

// ---------------------------------------------------------------------------
// API
// ---------------------------------------------------------------------------
async fn api_info(State(st): State<ServerState>) -> ApiResult<Json<Value>> {
    Ok(Json(blocking(move || Ok(st.app.info())).await?))
}

fn list_dir(app: &App, path: &str) -> ApiResult<Value> {
    let mut base = if path.is_empty() {
        app.output_dir.parent().map(Path::to_path_buf).unwrap_or_default()
    } else {
        expand_home(path)
    };
    if !base.is_dir() {
        base = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()));
    }
    let base = base.canonicalize().unwrap_or(base);
    let rd = std::fs::read_dir(&base).map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => ApiError::new(StatusCode::FORBIDDEN, "Accès refusé à ce dossier"),
        _ => ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    })?;
    let mut entries: Vec<(bool, String, Value)> = vec![];
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let p = e.path();
        let Ok(md) = std::fs::metadata(&p) else { continue };
        if md.is_dir() {
            entries.push((false, name.to_lowercase(), json!({"name": name, "path": p.to_string_lossy(), "dir": true})));
        } else {
            let ext = p.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
            if VIDEO_EXTS.contains(&ext.as_str()) {
                let mtime = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                entries.push((
                    true,
                    name.to_lowercase(),
                    json!({"name": name, "path": p.to_string_lossy(), "dir": false, "size": md.len(), "mtime": mtime}),
                ));
            }
        }
    }
    entries.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let parent = base.parent().filter(|p| *p != base).map(|p| p.to_string_lossy().into_owned());
    Ok(json!({
        "path": base.to_string_lossy(),
        "parent": parent,
        "home": std::env::var("HOME").unwrap_or_default(),
        "entries": entries.into_iter().map(|e| e.2).collect::<Vec<_>>(),
    }))
}

fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~") {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(p),
    }
}

async fn api_fs(State(st): State<ServerState>, Query(q): Query<HashMap<String, String>>) -> ApiResult<Json<Value>> {
    let path = q.get("path").cloned().unwrap_or_default();
    Ok(Json(blocking(move || list_dir(&st.app, &path)).await?))
}

async fn api_open(State(st): State<ServerState>, body: Bytes) -> ApiResult<Json<Value>> {
    let data = parse_body(&body)?;
    let path = data.get("path").and_then(Value::as_str).unwrap_or("").to_string();
    Ok(Json(
        blocking(move || {
            let p = st.app.open_project(&expand_home(&path), true)?;
            Ok(p.to_json())
        })
        .await?,
    ))
}

async fn api_jobs(State(st): State<ServerState>) -> Json<Value> {
    Json(Value::Array(st.app.jobs.list()))
}

fn valid_job_id(id: &str) -> bool {
    id.len() > 1 && id.starts_with('j') && id[1..].bytes().all(|b| b.is_ascii_digit())
}

async fn api_job(State(st): State<ServerState>, UrlPath(id): UrlPath<String>) -> ApiResult<Json<Value>> {
    let job = valid_job_id(&id).then(|| st.app.jobs.get(&id)).flatten();
    job.map(|j| Json(j.to_json())).ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Tâche inconnue"))
}

async fn api_job_cancel(State(st): State<ServerState>, UrlPath(id): UrlPath<String>) -> ApiResult<Json<Value>> {
    if !valid_job_id(&id) || st.app.jobs.get(&id).is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Tâche inconnue"));
    }
    Ok(Json(json!({ "cancelled": st.app.jobs.cancel(&id) })))
}

async fn api_reveal(body: Bytes) -> ApiResult<Json<Value>> {
    let data = parse_body(&body)?;
    let target = expand_home(data.get("path").and_then(Value::as_str).unwrap_or(""));
    let folder = if target.is_dir() { target } else { target.parent().map(Path::to_path_buf).unwrap_or_default() };
    if !folder.is_dir() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Dossier introuvable"));
    }
    match std::process::Command::new("xdg-open")
        .arg(&folder)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            Ok(Json(json!({ "ok": true })))
        }
        Err(_) => Err(ApiError::new(StatusCode::NOT_IMPLEMENTED, "xdg-open indisponible")),
    }
}

async fn api_project(State(st): State<ServerState>, UrlPath(id): UrlPath<String>) -> ApiResult<Json<Value>> {
    let p = project_of(&st, &id)?;
    Ok(Json(blocking(move || Ok(p.to_json())).await?))
}

async fn api_state(State(st): State<ServerState>, UrlPath(id): UrlPath<String>, body: Bytes) -> ApiResult<Json<Value>> {
    let p = project_of(&st, &id)?;
    let patch = parse_body(&body)?;
    Ok(Json(
        blocking(move || {
            p.update_state(&patch)?;
            Ok(p.to_json())
        })
        .await?,
    ))
}

async fn api_analyze(State(st): State<ServerState>, UrlPath(id): UrlPath<String>) -> ApiResult<Json<Value>> {
    let p = project_of(&st, &id)?;
    Ok(Json(blocking(move || Ok(st.app.start_analysis(&p)?.to_json())).await?))
}

async fn api_prepare(State(st): State<ServerState>, UrlPath(id): UrlPath<String>) -> ApiResult<Json<Value>> {
    let p = project_of(&st, &id)?;
    Ok(Json(
        blocking(move || {
            Ok(match st.app.ensure_proxy(&p)? {
                Some(j) => j.to_json(),
                None => json!({ "ok": true }),
            })
        })
        .await?,
    ))
}

fn num(q: &HashMap<String, String>, name: &str, default: f64) -> f64 {
    q.get(name).and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite()).unwrap_or(default)
}

/// Matrices de stabilisation pour l'aperçu : N x 9 flottants f32 little-endian, ligne par ligne.
async fn api_stab(
    State(st): State<ServerState>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let p = project_of(&st, &id)?;
    blocking(move || {
        let state = p.state();
        let mode: StabMode = match q.get("mode") {
            Some(m) => m.parse().map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "Mode inconnu"))?,
            None => state.stabilization.mode,
        };
        let strength = num(&q, "strength", state.stabilization.strength);
        let heading = num(&q, "heading", state.stabilization.heading_strength);
        let orient = Ypr { yaw: num(&q, "yaw", 0.0), pitch: num(&q, "pitch", 0.0), roll: num(&q, "roll", 0.0) };
        let reference = num(&q, "ref", 0.0).max(0.0) as usize;
        let analysis = p.orientation();
        let mats = match (&analysis, mode) {
            (Some(o), m) if m != StabMode::Off => {
                stabilization_matrices(Some(o), mode, strength, &orient, reference, Some(heading), o.n())
            }
            _ => stabilization_matrices(None, StabMode::Off, strength, &orient, 0, None, 1),
        };
        let mut bytes = Vec::with_capacity(mats.len() * 36);
        for m in &mats {
            for r in 0..3 {
                for c in 0..3 {
                    bytes.extend_from_slice(&(m[(r, c)] as f32).to_le_bytes());
                }
            }
        }
        let mut resp = Response::new(Body::from(bytes));
        let h = resp.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
        h.insert("x-frames", HeaderValue::from(mats.len()));
        h.insert("x-fps", HeaderValue::from_str(&p.src.fps.to_string()).expect("fps valide"));
        Ok(resp)
    })
    .await
}

fn directory_of(data: &Value) -> Option<PathBuf> {
    data.get("directory").and_then(Value::as_str).filter(|d| !d.is_empty()).map(expand_home)
}

async fn api_export(
    State(st): State<ServerState>,
    UrlPath(id): UrlPath<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let p = project_of(&st, &id)?;
    let data = parse_body(&body)?;
    Ok(Json(
        blocking(move || {
            let dir = directory_of(&data);
            Ok(st.app.start_export(&p, data.get("settings"), dir.as_deref())?.to_json())
        })
        .await?,
    ))
}

async fn api_snapshot(
    State(st): State<ServerState>,
    UrlPath(id): UrlPath<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let p = project_of(&st, &id)?;
    let data = parse_body(&body)?;
    Ok(Json(
        blocking(move || {
            let fmt = if data.get("format").and_then(Value::as_str) == Some("png") { "png" } else { "jpg" };
            let t = data.get("t").and_then(Value::as_f64).unwrap_or(0.0);
            let dir = directory_of(&data);
            let out = st.app.snapshot(&p, t, fmt, dir.as_deref(), None, data.get("settings"))?;
            Ok(json!({ "path": out.to_string_lossy() }))
        })
        .await?,
    ))
}

async fn api_frame(
    State(st): State<ServerState>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let p = project_of(&st, &id)?;
    blocking(move || {
        let t = num(&q, "t", 0.0);
        let mut over = json!({ "mode": "reframe" });
        for (key, name) in [("w", "width"), ("h", "height")] {
            if q.contains_key(key) {
                over[name] = json!((num(&q, key, 0.0).clamp(2.0, 4096.0) as u32) / 2 * 2);
            }
        }
        let mut rnd = [0u8; 4];
        getrandom::fill(&mut rnd).map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let tmp = p.dir.join(format!("frame_{}.jpg", rnd.iter().map(|b| format!("{b:02x}")).collect::<String>()));
        let res = st.app.snapshot(&p, t, "jpg", None, Some(&tmp), Some(&over));
        let data = res.and_then(|_| Ok(std::fs::read(&tmp)?));
        let _ = std::fs::remove_file(&tmp);
        Ok(([(header::CONTENT_TYPE, "image/jpeg")], data?).into_response())
    })
    .await
}

// ---------------------------------------------------------------------------
// Routeur et lancement
// ---------------------------------------------------------------------------
/// Routeur complet (API, médias, page d'accueil, statiques) ; `state.port` sert au contrôle `Host`/`Origin`.
pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/app.js", get(app_js))
        .route("/viewer.js", get(viewer_js))
        .route("/geometry.js", get(geometry_js))
        .route("/style.css", get(style_css))
        .route("/api/info", get(api_info))
        .route("/api/fs", get(api_fs))
        .route("/api/open", post(api_open))
        .route("/api/jobs", get(api_jobs))
        .route("/api/jobs/{id}", get(api_job))
        .route("/api/jobs/{id}/cancel", post(api_job_cancel))
        .route("/api/reveal", post(api_reveal))
        .route("/api/project/{id}", get(api_project))
        .route("/api/project/{id}/state", put(api_state))
        .route("/api/project/{id}/analyze", post(api_analyze))
        .route("/api/project/{id}/prepare", post(api_prepare))
        .route("/api/project/{id}/stab", get(api_stab))
        .route("/api/project/{id}/export", post(api_export))
        .route("/api/project/{id}/snapshot", post(api_snapshot))
        .route("/api/project/{id}/frame", get(api_frame))
        .route("/media/{id}/proxy.webm", get(media))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

fn random_hex(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("générateur aléatoire indisponible : {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Serveur de test : écoute sur un port libre de 127.0.0.1, retourne `(adresse, état, tâche)`.
pub async fn start_test_server(app: Arc<App>) -> Result<(SocketAddr, ServerState, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = listener.local_addr()?;
    let state = ServerState { app, token: random_hex(24)?, key: random_hex(16)?, port: addr.port() };
    let router = router(state.clone());
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok((addr, state, handle))
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

/// Lance le serveur (bloquant jusqu'à Ctrl+C / SIGTERM), affiche l'URL avec la clé, ouvre le navigateur
/// (`xdg-open`, sans erreur fatale s'il échoue) et le projet `path` si donné. `port` 0 = automatique.
pub fn serve(app: Arc<App>, port: u16, open_browser: bool, path: Option<PathBuf>) -> Result<()> {
    if let Some(p) = &path {
        app.open_project(p, true)?;
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        let port = listener.local_addr()?.port();
        let state = ServerState { app: app.clone(), token: random_hex(24)?, key: random_hex(16)?, port };
        let url = format!("http://127.0.0.1:{port}/?key={}", state.key);
        println!("X3 Studio Local — interface : {url}");
        println!("Les vidéos sont traitées localement ; le serveur n'écoute que sur 127.0.0.1. Ctrl+C pour quitter.");
        if open_browser {
            let u = url.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(500));
                match std::process::Command::new("xdg-open")
                    .arg(&u)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                {
                    Ok(mut c) => {
                        let _ = c.wait();
                    }
                    Err(_) => eprintln!("(navigateur non ouvert automatiquement : ouvrez l'adresse ci-dessus)"),
                }
            });
        }
        axum::serve(listener, router(state)).with_graceful_shutdown(shutdown_signal()).await?;
        // Arrêt : on annule les tâches en cours pour ne laisser aucun ffmpeg orphelin.
        for j in app.jobs.list() {
            if let Some(id) = j.get("id").and_then(Value::as_str) {
                app.jobs.cancel(id);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parsing() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=10-5000", 1000), Some((10, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=5-1", 1000), None);
        assert_eq!(parse_range("bytes=-", 1000), None);
        assert_eq!(parse_range("items=0-1", 1000), None);
        assert_eq!(parse_range("bytes=0-1", 0), None);
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "abcd"));
    }

    #[test]
    fn query_decoding() {
        let q = form_urlencoded_pairs("path=%2Fhome%2Fx%20y&token=abc&flag&plus=a+b");
        assert_eq!(q["path"], "/home/x y");
        assert_eq!(q["token"], "abc");
        assert_eq!(q["flag"], "");
        assert_eq!(q["plus"], "a b");
        assert_eq!(form_urlencoded_pairs("a=%zz")["a"], "%zz");
        assert_eq!(form_urlencoded_pairs("a=%2")["a"], "%2");
    }

    #[test]
    fn job_ids() {
        assert!(valid_job_id("j12") && !valid_job_id("j") && !valid_job_id("12") && !valid_job_id("jx1"));
    }
}
