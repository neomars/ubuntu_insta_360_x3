import json
import threading
import time
import urllib.error
import urllib.request

import pytest

from x3studio.server import make_server


@pytest.fixture
def server(app):
    srv = make_server(app)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    yield srv
    srv.shutdown()
    srv.server_close()


class Client:
    def __init__(self, srv):
        self.srv = srv
        self.base = f"http://127.0.0.1:{srv.port}"

    def call(self, method, path, body=None, token=True, headers=None, raw=False):
        h = {"Content-Type": "application/json"}
        if token:
            h["X-X3-Token"] = self.srv.token
        h.update(headers or {})
        req = urllib.request.Request(self.base + path, method=method, headers=h,
                                     data=None if body is None else json.dumps(body).encode())
        with urllib.request.urlopen(req) as r:
            data = r.read()
            return data if raw else json.loads(data)

    def status(self, *a, **k):
        try:
            self.call(*a, **k)
            return 200
        except urllib.error.HTTPError as e:
            return e.code

    def wait_jobs(self, timeout=120):
        t0 = time.time()
        while time.time() - t0 < timeout:
            jobs = self.call("GET", "/api/jobs")
            if jobs and all(j["state"] in ("done", "error", "cancelled") for j in jobs):
                return jobs
            time.sleep(0.2)
        raise TimeoutError


@pytest.fixture
def client(server):
    return Client(server)


def test_security(client, server):
    assert client.status("GET", "/api/info", token=False) == 401
    assert client.status("GET", "/api/info", headers={"X-X3-Token": "mauvais"}) == 401
    assert client.status("GET", "/api/info", headers={"Host": "evil.example"}) == 403            # DNS rebinding
    assert client.status("GET", "/api/info", headers={"Origin": "https://evil.example"}) == 403   # page tierce
    assert client.status("POST", "/api/open", {"path": "/etc/passwd"}, token=False) == 401
    assert client.status("GET", "/media/0123456789abcdef/proxy.webm", token=False) == 401
    assert client.status("GET", "/../../etc/passwd", token=False) in (403, 404)
    assert client.status("GET", "/%2e%2e/%2e%2e/etc/passwd", token=False) in (403, 404)
    # le jeton passe aussi par l'URL (pour <video src>), mais jamais pour les écritures
    assert client.status("GET", f"/api/info?token={server.token}", token=False) == 200
    assert client.status("POST", f"/api/open?token={server.token}", {"path": "x"}, token=False) == 401
    assert server.server_address[0] == "127.0.0.1"


def test_index_and_static(client, server):
    assert client.status("GET", "/", token=False) == 403                        # sans la clé de lancement
    assert client.status("GET", "/?key=devinette", token=False) == 403
    html = client.call("GET", f"/?key={server.key}", token=False, raw=True).decode()
    assert server.token in html and "__X3_TOKEN__" not in html
    # cookie de session : permet de recharger la page sans la clé
    req = urllib.request.Request(client.base + "/", headers={"Cookie": f"x3session={server.token}"})
    assert server.token in urllib.request.urlopen(req).read().decode()
    with urllib.request.urlopen(client.base + f"/?key={server.key}") as r:
        assert "HttpOnly" in r.headers["Set-Cookie"] and "SameSite=Strict" in r.headers["Set-Cookie"]
    assert b"createElement" in client.call("GET", "/app.js", token=False, raw=True)
    assert client.status("GET", "/nope.js", token=False) == 404


def test_fs_listing(client, tmp_path, static_insv):
    r = client.call("GET", f"/api/fs?path={static_insv.parent}")
    assert any(e["name"] == "static.insv" and not e["dir"] for e in r["entries"])
    assert not any(e["name"].startswith(".") for e in r["entries"])


def test_open_edit_export_flow(client, app, shake_insv, tmp_path):
    path, _ = shake_insv
    p = client.call("POST", "/api/open", {"path": str(path)})
    pid = p["id"]
    assert p["source"]["layout"] == "two_streams" and not p["proxy"]["ready"]
    assert client.status("POST", "/api/open", {"path": str(tmp_path / "absent.insv")}) == 400
    jobs = client.wait_jobs()
    assert {j["kind"] for j in jobs} >= {"proxy", "analyze"} and all(j["state"] == "done" for j in jobs)
    p = client.call("GET", f"/api/project/{pid}")
    assert p["proxy"]["ready"] and p["analysis"]["ready"] and not p["analysis"]["stale"]

    # lecture partielle de l'aperçu
    req = urllib.request.Request(f"{client.base}{p['proxy']['url']}?token={client.srv.token}", headers={"Range": "bytes=10-109"})
    with urllib.request.urlopen(req) as r:
        assert r.status == 206 and len(r.read()) == 100 and r.headers["Content-Range"].startswith("bytes 10-109/")

    # matrices de stabilisation
    raw = client.call("GET", f"/api/project/{pid}/stab?mode=lock", raw=True)
    assert len(raw) == 36 * p["analysis"]["frames"]
    assert len(client.call("GET", f"/api/project/{pid}/stab?mode=off", raw=True)) == 36
    assert client.status("GET", f"/api/project/{pid}/stab?mode=bogus") == 400

    # sauvegarde des réglages (persistés sur disque)
    kfs = [{"t": 0, "yaw": 0, "pitch": 0, "roll": 0, "fov": 90}, {"t": 2, "yaw": 80, "pitch": 5, "roll": 0, "fov": 70}]
    st = client.call("PUT", f"/api/project/{pid}/state", {"keyframes": kfs, "stabilization": {"mode": "lock"},
                                                          "trim": {"start": 0.2, "end": 2.0}})["state"]
    assert st["stabilization"]["mode"] == "lock" and st["stabilization"]["strength"] == 1.0   # fusion partielle
    assert json.loads((app.projects[pid].dir / "state.json").read_text())["keyframes"][1]["yaw"] == 80
    assert client.status("PUT", f"/api/project/{pid}/state", {"malveillant": 1}) == 200      # clés inconnues ignorées

    j = client.call("POST", f"/api/project/{pid}/export", {"settings": {"width": 320, "height": 180, "crf": 30, "preset": "ultrafast"}})
    done = client.wait_jobs()
    ex = next(x for x in done if x["id"] == j["id"])
    assert ex["state"] == "done", ex
    out = tmp_path / "out"
    assert (out / "shake_flat.mp4").exists()
    assert client.status("POST", f"/api/project/{pid}/export", {"settings": {"width": 321, "height": 180}}) == 400

    img = client.call("GET", f"/api/project/{pid}/frame?t=1.0&w=320&h=180&token={client.srv.token}", token=False, raw=True)
    assert img[:3] == b"\xff\xd8\xff"
    snap = client.call("POST", f"/api/project/{pid}/snapshot", {"t": 1.0, "format": "png"})
    assert snap["path"].endswith(".png")

    # le projet est retrouvé avec ses réglages à la réouverture
    p2 = client.call("POST", "/api/open", {"path": str(path)})
    assert p2["id"] == pid and p2["state"]["keyframes"][1]["yaw"] == 80 and p2["proxy"]["ready"]


def test_job_cancel(client, shake_insv):
    path, _ = shake_insv
    pid = client.call("POST", "/api/open", {"path": str(path)})["id"]
    client.wait_jobs()
    j = client.call("POST", f"/api/project/{pid}/export", {"settings": {"width": 1920, "height": 1080, "crf": 30}})
    for other in range(2):  # en file d'attente derrière
        client.call("POST", f"/api/project/{pid}/export", {"settings": {"width": 320, "height": 180}})
    assert client.call("POST", f"/api/jobs/{j['id']}/cancel", {})["cancelled"] in (True, False)
    jobs = client.wait_jobs()
    assert all(x["state"] in ("done", "cancelled") for x in jobs)
    assert client.status("GET", "/api/jobs/j9999") == 404
