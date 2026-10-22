"""Tests de l'interface dans un vrai navigateur (Chromium sans écran + WebGL logiciel)."""
import base64
import glob
import json
import os
import threading
import time
import urllib.request

import numpy as np
import pytest

pytest.importorskip("cv2")
sync_api = pytest.importorskip("playwright.sync_api")

from x3studio.server import make_server  # noqa: E402

pytestmark = pytest.mark.ui


def _chromium_path():
    env = os.environ.get("X3_CHROMIUM")
    if env:
        return env
    c = sorted(glob.glob("/opt/pw-browsers/chromium-*/chrome-linux/chrome"))
    return c[-1] if c else None


@pytest.fixture(scope="module")
def browser():
    with sync_api.sync_playwright() as pw:
        try:
            b = pw.chromium.launch(executable_path=_chromium_path(), args=[
                "--no-sandbox", "--use-gl=angle", "--use-angle=swiftshader", "--enable-unsafe-swiftshader",
                "--ignore-gpu-blocklist", "--autoplay-policy=no-user-gesture-required"])
        except Exception as e:  # pragma: no cover
            pytest.skip(f"Chromium indisponible : {e}")
        yield b
        b.close()


@pytest.fixture
def ui(browser, app, shake_insv):
    srv = make_server(app)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    page = browser.new_page(viewport={"width": 1300, "height": 820})
    page.errors, page.requests = [], []
    page.on("pageerror", lambda e: page.errors.append(str(e)))
    page.on("request", lambda r: page.requests.append(r.url))
    page.goto(f"http://127.0.0.1:{srv.port}/?key={srv.key}")
    page.wait_for_selector("#btn-open")
    page.srv, page.app = srv, app
    page.path = str(shake_insv[0])
    yield page
    page.close()
    srv.shutdown()
    srv.server_close()


def open_and_wait(page):
    page.click("#btn-open")
    page.wait_for_selector("#fs-list li")
    page.fill("#fs-path", os.path.dirname(page.path))
    page.click("#fs-go")
    page.click(f'#fs-list li:has-text("{os.path.basename(page.path)}")')
    page.wait_for_function("window.__x3.S.videoReady === true", timeout=90000)
    for _ in range(150):
        if page.evaluate("window.__x3.api('GET', '/api/project/' + window.__x3.S.proj.id).then(p => p.analysis.ready)"):
            break
        time.sleep(0.2)
    time.sleep(0.6)


def canvas_png(page):
    import cv2
    data = page.evaluate("document.getElementById('gl').toDataURL('image/png')")
    return cv2.imdecode(np.frombuffer(base64.b64decode(data.split(",")[1]), np.uint8), cv2.IMREAD_COLOR)


def test_open_render_and_stay_local(ui):
    open_and_wait(ui)
    img = canvas_png(ui)
    assert img.std() > 20                                     # une vraie scène, pas un écran uni
    assert ui.inner_text("#file-info").startswith("shake.insv")
    assert ui.errors == []
    hosts = {u.split("/")[2] for u in ui.requests if u.startswith("http")}
    assert hosts == {f"127.0.0.1:{ui.srv.port}"}, hosts       # aucune requête vers l'extérieur


def test_preview_matches_export(ui):
    """Pour un même état (vue, stabilisation), l'aperçu WebGL et l'export serveur donnent la même image."""
    import cv2
    open_and_wait(ui)
    for proj, view, stab, t in [("flat", dict(yaw=30, pitch=10, roll=5, fov=90), "lock", 1.0),
                                ("fisheye", dict(yaw=10, pitch=0, roll=0, fov=200), "smooth", 0.5),
                                ("planet", dict(yaw=0, pitch=-90, roll=0, fov=250), "off", 1.5)]:
        ui.evaluate("""async ([proj, view, stab, t]) => {
            const X = window.__x3, S = X.S;
            S.st.projection = proj; S.st.view = view; S.st.keyframes = []; S.st.stabilization.mode = stab; S.override = null;
            X.viewer.setProjection(proj);
            for (const k of ['projection', 'view', 'keyframes', 'stabilization']) S.pending.add(k);
            await X.flushSave(); await X.fetchStab(); X.seek(t); S.dirty = true; }""", [proj, view, stab, t])
        time.sleep(1.0)
        w, h = ui.evaluate("[document.getElementById('gl').width, document.getElementById('gl').height]")
        w, h = w // 2 * 2, h // 2 * 2
        shot = canvas_png(ui)
        pid = ui.evaluate("window.__x3.S.proj.id")
        tt = ui.evaluate("window.__x3.S.t")
        url = f"http://127.0.0.1:{ui.srv.port}/api/project/{pid}/frame?t={tt}&w={w}&h={h}&token={ui.srv.token}"
        ex = cv2.imdecode(np.frombuffer(urllib.request.urlopen(url).read(), np.uint8), cv2.IMREAD_COLOR)
        a = cv2.resize(shot, (w // 3, h // 3), interpolation=cv2.INTER_AREA).astype(float)
        b = cv2.resize(ex, (w // 3, h // 3), interpolation=cv2.INTER_AREA).astype(float)
        assert np.abs(a - b).mean() / 255 < 0.04, proj
    assert ui.errors == []


def test_keyframes_drag_and_persist(ui):
    open_and_wait(ui)
    box = ui.locator("#gl").bounding_box()
    cx, cy = box["x"] + box["width"] / 2, box["y"] + box["height"] / 2
    ui.mouse.move(cx, cy); ui.mouse.down(); ui.mouse.move(cx - 120, cy + 30, steps=4); ui.mouse.up()
    v0 = ui.evaluate("window.__x3.S.st.view")
    assert v0["yaw"] > 5 and v0["pitch"] > 1                  # glisser vers la gauche/bas : on regarde à droite/haut
    ui.keyboard.press("k")
    ui.evaluate("window.__x3.seek(2.0)")
    time.sleep(0.4)
    ui.mouse.move(cx, cy); ui.mouse.down(); ui.mouse.move(cx + 200, cy, steps=4); ui.mouse.up()
    ui.mouse.wheel(0, -400)
    ui.keyboard.press("k")
    kfs = ui.evaluate("window.__x3.S.st.keyframes")
    assert len(kfs) == 2 and kfs[0]["t"] == 0 and kfs[1]["t"] == pytest.approx(2.0, abs=0.05)
    assert kfs[1]["fov"] < kfs[0]["fov"] and kfs[1]["yaw"] < kfs[0]["yaw"]
    assert ui.locator("#kf-list li").count() == 2 and ui.locator(".kf").count() == 2
    ui.evaluate("window.__x3.seek(1.0)")
    time.sleep(0.4)
    assert "champ" in ui.inner_text("#hud-readout")
    ui.evaluate("window.__x3.flushSave()")
    time.sleep(0.5)
    saved = json.loads((ui.app.projects[ui.evaluate("window.__x3.S.proj.id")].state_file).read_text())
    assert len(saved["keyframes"]) == 2
    assert ui.errors == []


def test_export_from_ui(ui):
    open_and_wait(ui)
    ui.click('.tab[data-tab=export]')
    ui.select_option("#e-size", "1280x720")
    ui.select_option("#e-crf", "28")
    ui.select_option("#e-speed", "2")
    ui.click("#btn-export")
    ui.wait_for_selector('#job-list li:has-text("Export")')
    deadline = time.time() + 120
    while time.time() < deadline:
        states = ui.evaluate("window.__x3.S.jobs.filter(j => j.kind === 'export').map(j => j.state)")
        if states and states[0] in ("done", "error"):
            break
        time.sleep(0.3)
    assert states[0] == "done", ui.evaluate("window.__x3.S.jobs")
    outs = list(ui.app.output_dir.glob("*.mp4"))
    assert len(outs) == 1
    assert ui.errors == []


def test_lens_calibration_changes_preview(ui):
    open_and_wait(ui)
    ui.evaluate("window.__x3.editView(v => ({...v, yaw: 90, pitch: 0, roll: 0, fov: 100}))")   # on regarde la couture
    time.sleep(0.6)
    before = canvas_png(ui).astype(float)
    ui.click('.tab[data-tab=lens]')
    ui.click("summary")
    ui.fill("#c-b_yaw", "8")
    ui.dispatch_event("#c-b_yaw", "input")
    time.sleep(0.6)
    after = canvas_png(ui).astype(float)
    assert np.abs(before - after).mean() > 1.0                # le réglage agit en direct
    assert ui.evaluate("window.__x3.S.st.calibration.b_yaw") == pytest.approx(8)
