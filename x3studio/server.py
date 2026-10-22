"""Serveur HTTP local (127.0.0.1 uniquement) + API JSON de l'interface web.

Sécurité : le serveur n'écoute que sur la boucle locale, vérifie l'en-tête ``Host``
(contre le « DNS rebinding »), refuse les requêtes ``Origin`` étrangères et exige un
jeton aléatoire (généré à chaque lancement) pour toute l'API et les médias. La page
d'accueil n'est servie qu'avec la clé de lancement présente dans l'URL affichée au
démarrage (puis un cookie de session), de sorte qu'un autre programme ne puisse pas
simplement interroger le port.
"""
from __future__ import annotations

import hmac
import json
import mimetypes
import os
import re
import secrets
import socket
import subprocess
import sys
import threading
import time
import webbrowser
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, unquote, urlparse

import numpy as np

from .app import App
from .media import VIDEO_EXTS, MediaError
from .pipeline import ExportError

WEB_DIR = Path(__file__).parent / "web"
MAX_BODY = 4 * 1024 * 1024


class ApiError(Exception):
    def __init__(self, status: int, message: str):
        super().__init__(message)
        self.status, self.message = status, message


class X3Server(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, addr, app: App, token: str, key: str | None = None):
        super().__init__(addr, Handler)
        self.app, self.token = app, token
        self.key = key or secrets.token_urlsafe(16)      # clé de lancement (dans l'URL ouverte au démarrage)
        self.port = self.server_address[1]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server: X3Server

    def log_message(self, fmt, *args):  # silencieux
        if os.environ.get("X3STUDIO_DEBUG"):
            sys.stderr.write("%s - %s\n" % (self.address_string(), fmt % args))

    # ------------------------------------------------------------------
    # sécurité
    # ------------------------------------------------------------------
    def _host_ok(self) -> bool:
        host = self.headers.get("Host", "")
        return host in (f"127.0.0.1:{self.server.port}", f"localhost:{self.server.port}")

    def _origin_ok(self) -> bool:
        origin = self.headers.get("Origin")
        if not origin:
            return True
        return origin in (f"http://127.0.0.1:{self.server.port}", f"http://localhost:{self.server.port}")

    def _token_ok(self, query: dict) -> bool:
        tok = self.headers.get("X-X3-Token") or (query.get("token") or [""])[0]
        return hmac.compare_digest(tok.encode(), self.server.token.encode())

    # ------------------------------------------------------------------
    # réponses
    # ------------------------------------------------------------------
    def _send(self, status: int, body: bytes, ctype: str = "application/json", headers: dict | None = None):
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        for k, v in (headers or {}).items():
            self.send_header(k, str(v))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def _json(self, obj, status: int = 200):
        self._send(status, json.dumps(obj).encode())

    def _error(self, status: int, message: str):
        self._json({"error": message}, status)

    def _body(self) -> dict:
        n = int(self.headers.get("Content-Length") or 0)
        if n > MAX_BODY:
            raise ApiError(413, "Requête trop volumineuse")
        if n == 0:
            return {}
        try:
            data = json.loads(self.rfile.read(n))
        except ValueError:
            raise ApiError(400, "JSON invalide")
        if not isinstance(data, dict):
            raise ApiError(400, "Objet JSON attendu")
        return data

    # ------------------------------------------------------------------
    # routage
    # ------------------------------------------------------------------
    def do_HEAD(self):
        self._dispatch()

    def do_GET(self):
        self._dispatch()

    def do_POST(self):
        self._dispatch()

    def do_PUT(self):
        self._dispatch()

    def _dispatch(self):
        try:
            url = urlparse(self.path)
            query = parse_qs(url.query)
            path = unquote(url.path)
            if not self._host_ok() or not self._origin_ok():
                return self._error(403, "Hôte ou origine refusés")
            if path.startswith(("/api/", "/media/")):
                if not self._token_ok(query):
                    return self._error(401, "Jeton invalide")
                if self.command in ("POST", "PUT") and not self.headers.get("X-X3-Token"):
                    return self._error(401, "Jeton manquant")
                if path.startswith("/media/"):
                    return self._media(path)
                return self._api(path, query)
            if self.command not in ("GET", "HEAD"):
                return self._error(405, "Méthode non autorisée")
            return self._static(path, query)
        except ApiError as e:
            self._error(e.status, e.message)
        except (MediaError, ExportError) as e:
            self._error(400, str(e))
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as e:  # noqa: BLE001
            if os.environ.get("X3STUDIO_DEBUG"):
                import traceback
                traceback.print_exc()
            try:
                self._error(500, f"Erreur interne : {e.__class__.__name__}: {e}")
            except OSError:
                pass

    # ------------------------------------------------------------------
    # fichiers statiques
    # ------------------------------------------------------------------
    def _static(self, path: str, query: dict | None = None):
        if path in ("/", "/index.html"):
            key = ((query or {}).get("key") or [""])[0]
            cookies = self.headers.get("Cookie", "")
            has_cookie = f"x3session={self.server.token}" in [c.strip() for c in cookies.split(";")]
            if not (key and hmac.compare_digest(key.encode(), self.server.key.encode())) and not has_cookie:
                return self._send(403, "Ouvrez l'adresse complète affichée dans le terminal au lancement de x3studio "
                                  "(elle contient la clé de session).".encode(), "text/plain; charset=utf-8")
            html = (WEB_DIR / "index.html").read_text(encoding="utf-8")
            html = html.replace("__X3_TOKEN__", self.server.token)
            return self._send(200, html.encode(), "text/html; charset=utf-8", {
                "Set-Cookie": f"x3session={self.server.token}; HttpOnly; SameSite=Strict; Path=/"})
        rel = path.lstrip("/")
        f = (WEB_DIR / rel).resolve()
        if WEB_DIR.resolve() not in f.parents or not f.is_file():
            return self._error(404, "Introuvable")
        ctype = mimetypes.guess_type(f.name)[0] or "application/octet-stream"
        if f.suffix == ".js":
            ctype = "text/javascript; charset=utf-8"
        elif f.suffix == ".css":
            ctype = "text/css; charset=utf-8"
        self._send(200, f.read_bytes(), ctype)

    def _media(self, path: str):
        m = re.fullmatch(r"/media/([0-9a-f]{16})/proxy\.webm", path)
        if not m:
            return self._error(404, "Introuvable")
        p = self.server.app.get_project(m.group(1))
        if not p or not p.has_proxy():
            return self._error(404, "Aperçu indisponible")
        f = p.proxy_path
        size = f.stat().st_size
        start, end = 0, size - 1
        status = 200
        rng = self.headers.get("Range")
        if rng:
            mm = re.fullmatch(r"bytes=(\d*)-(\d*)", rng.strip())
            if mm:
                a, b = mm.groups()
                if a == "" and b != "":
                    start = max(0, size - int(b))
                elif a != "":
                    start = int(a)
                    if b != "":
                        end = min(int(b), size - 1)
                if start > end or start >= size:
                    return self._send(416, b"", headers={"Content-Range": f"bytes */{size}"})
                status = 206
        length = end - start + 1
        self.send_response(status)
        self.send_header("Content-Type", "video/webm")
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Length", str(length))
        self.send_header("Cache-Control", "no-store")
        if status == 206:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.end_headers()
        if self.command == "HEAD":
            return
        with open(f, "rb") as fh:
            fh.seek(start)
            left = length
            while left > 0:
                chunk = fh.read(min(1 << 20, left))
                if not chunk:
                    break
                self.wfile.write(chunk)
                left -= len(chunk)

    # ------------------------------------------------------------------
    # API
    # ------------------------------------------------------------------
    def _project(self, pid: str):
        p = self.server.app.get_project(pid)
        if not p:
            raise ApiError(404, "Projet inconnu")
        return p

    def _api(self, path: str, query: dict):
        app = self.server.app
        m = self.command
        if path == "/api/info" and m == "GET":
            return self._json(app.info())
        if path == "/api/fs" and m == "GET":
            return self._json(self._list_dir((query.get("path") or [""])[0]))
        if path == "/api/open" and m == "POST":
            data = self._body()
            p = app.open_project(str(data.get("path", "")))
            return self._json(p.to_json())
        if path == "/api/jobs" and m == "GET":
            return self._json(app.jobs.list())
        mm = re.fullmatch(r"/api/jobs/(j\d+)(/cancel)?", path)
        if mm:
            job = app.jobs.get(mm.group(1))
            if not job:
                raise ApiError(404, "Tâche inconnue")
            if mm.group(2) and m == "POST":
                return self._json({"cancelled": app.jobs.cancel(job.id)})
            return self._json(job.to_dict())
        if path == "/api/reveal" and m == "POST":
            data = self._body()
            target = Path(str(data.get("path", ""))).expanduser()
            folder = target if target.is_dir() else target.parent
            if not folder.is_dir():
                raise ApiError(404, "Dossier introuvable")
            try:
                subprocess.Popen(["xdg-open", str(folder)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            except FileNotFoundError:
                raise ApiError(501, "xdg-open indisponible")
            return self._json({"ok": True})
        mm = re.fullmatch(r"/api/project/([0-9a-f]{16})(?:/(\w+))?", path)
        if mm:
            return self._project_api(self._project(mm.group(1)), mm.group(2) or "", query)
        raise ApiError(404, "Route inconnue")

    def _project_api(self, p, action: str, query: dict):
        app, m = self.server.app, self.command
        if action == "" and m == "GET":
            return self._json(p.to_json())
        if action == "state" and m == "PUT":
            p.update_state(self._body())
            return self._json(p.to_json())
        if action == "analyze" and m == "POST":
            return self._json(app.start_analysis(p).to_dict())
        if action == "prepare" and m == "POST":
            job = app.ensure_proxy(p)
            return self._json(job.to_dict() if job else {"ok": True})
        if action == "stab" and m == "GET":
            return self._stab(p, query)
        if action == "export" and m == "POST":
            data = self._body()
            job = app.start_export(p, data.get("settings"), data.get("directory") or None)
            return self._json(job.to_dict())
        if action == "snapshot" and m == "POST":
            data = self._body()
            fmt = "png" if data.get("format") == "png" else "jpg"
            out = app.snapshot(p, float(data.get("t", 0)), fmt, data.get("directory") or None,
                               overrides=data.get("settings"))
            return self._json({"path": str(out)})
        if action == "frame" and m == "GET":
            return self._frame(p, query)
        raise ApiError(404, "Action inconnue")

    def _stab(self, p, query: dict):
        """Matrices de stabilisation (N x 9 float32, ligne par ligne) pour l'aperçu."""
        def f(name, default):
            try:
                return float((query.get(name) or [default])[0])
            except ValueError:
                return float(default)
        over = {
            "mode": (query.get("mode") or [p.state["stabilization"]["mode"]])[0],
            "strength": f("strength", p.state["stabilization"]["strength"]),
            "heading_strength": f("heading", p.state["stabilization"]["heading_strength"]),
            "orientation": {"yaw": f("yaw", 0), "pitch": f("pitch", 0), "roll": f("roll", 0)},
        }
        if over["mode"] not in ("off", "smooth", "lock", "horizon"):
            raise ApiError(400, "Mode inconnu")
        ref = int(f("ref", 0))
        T = None
        from . import stabilize
        o = p.orientation()
        if over["mode"] == "off" or o is None:
            T = stabilize.stabilization_matrices(None, "off", user_orientation=over["orientation"], n_frames=1)
        else:
            T = stabilize.stabilization_matrices(o, over["mode"], over["strength"], over["orientation"], ref,
                                                 over["heading_strength"], n_frames=o.n)
        arr = np.ascontiguousarray(T.reshape(len(T), 9), dtype="<f4")
        self._send(200, arr.tobytes(), "application/octet-stream",
                   {"X-Frames": len(arr), "X-Fps": p.src.fps})

    def _frame(self, p, query: dict):
        t = float((query.get("t") or ["0"])[0])
        over: dict = {"mode": "reframe"}
        for key, name in (("w", "width"), ("h", "height")):
            if key in query:
                over[name] = max(2, min(4096, int(float(query[key][0])))) // 2 * 2
        tmp = p.dir / f"frame_{secrets.token_hex(4)}.jpg"
        try:
            self.server.app.snapshot(p, t, "jpg", out_path=str(tmp), overrides=over)
            data = tmp.read_bytes()
        finally:
            tmp.unlink(missing_ok=True)
        self._send(200, data, "image/jpeg")

    # ------------------------------------------------------------------
    def _list_dir(self, path: str) -> dict:
        base = Path(path).expanduser() if path else Path(self.server.app.output_dir).parent
        if not base.is_dir():
            base = Path.home()
        base = base.resolve()
        entries = []
        try:
            for e in sorted(base.iterdir(), key=lambda x: (not x.is_dir(), x.name.lower())):
                if e.name.startswith("."):
                    continue
                try:
                    if e.is_dir():
                        entries.append({"name": e.name, "path": str(e), "dir": True})
                    elif e.suffix.lower() in VIDEO_EXTS:
                        st = e.stat()
                        entries.append({"name": e.name, "path": str(e), "dir": False,
                                        "size": st.st_size, "mtime": st.st_mtime})
                except OSError:
                    continue
        except PermissionError:
            raise ApiError(403, "Accès refusé à ce dossier")
        return {"path": str(base), "parent": str(base.parent) if base.parent != base else None,
                "home": str(Path.home()), "entries": entries}


def make_server(app: App | None = None, port: int = 0) -> X3Server:
    app = app or App()
    return X3Server(("127.0.0.1", port), app, secrets.token_urlsafe(24))


def serve(port: int = 0, open_browser: bool = True, path: str | None = None, app: App | None = None) -> None:
    srv = make_server(app, port)
    url = f"http://127.0.0.1:{srv.port}/?key={srv.key}"
    print(f"X3 Studio Local — interface : {url}")
    print("Les vidéos sont traitées localement ; le serveur n'écoute que sur 127.0.0.1. Ctrl+C pour quitter.")
    if path:
        srv.app.open_project(path)
    if open_browser:
        threading.Timer(0.5, lambda: webbrowser.open(url)).start()
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        srv.server_close()
