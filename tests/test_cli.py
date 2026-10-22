import json
import subprocess
import sys
from pathlib import Path

import pytest

from helpers import probe_video

ROOT = Path(__file__).parent.parent


def x3(*args, env=None):
    import os
    e = dict(os.environ)
    e.update(env or {})
    return subprocess.run([sys.executable, "-m", "x3studio", *map(str, args)], capture_output=True, text=True, cwd=ROOT, env=e)


@pytest.fixture
def env(tmp_path):
    return {"X3STUDIO_HOME": str(tmp_path / "home"), "X3STUDIO_OUTPUT": str(tmp_path / "out")}


def test_probe_and_json(static_insv, env):
    r = x3("probe", static_insv, env=env)
    assert r.returncode == 0 and "deux flux fisheye" in r.stdout and "480×480" in r.stdout
    j = json.loads(x3("probe", static_insv, "--json", env=env).stdout)
    assert j["layout"] == "two_streams" and j["frames"] == 45


def test_probe_error_is_friendly(tmp_path, env):
    r = x3("probe", tmp_path / "nope.insv", env=env)
    assert r.returncode == 1 and "Erreur" in r.stderr and "Traceback" not in r.stderr


def test_reframe_and_dry_run(static_insv, tmp_path, env):
    out = tmp_path / "o.mp4"
    r = x3("reframe", static_insv, "-o", out, "--size", "320x180", "--yaw", "30", "--crf", "30", "--preset", "ultrafast", env=env)
    assert r.returncode == 0, r.stderr
    assert probe_video(out)["w"] == 320
    r = x3("reframe", static_insv, "--dry-run", "--engine", "v360", "--size", "320x180", env=env)
    assert r.returncode == 0 and "v360" in r.stdout and not (tmp_path / "out").exists()


def test_stabilised_export_and_snapshot(shake_insv, tmp_path, env):
    out = tmp_path / "s.mp4"
    r = x3("reframe", shake_insv[0], "-o", out, "--size", "320x180", "--stabilize", "lock", "--crf", "30", "--preset", "ultrafast", env=env)
    assert r.returncode == 0, r.stderr
    jpg = tmp_path / "s.jpg"
    r = x3("snapshot", shake_insv[0], "-o", jpg, "--t", "1.0", "--size", "320x180", "--projection", "planet", "--pitch", "-90", env=env)
    assert r.returncode == 0, r.stderr
    assert jpg.read_bytes()[:3] == b"\xff\xd8\xff"


def test_keyframes_file(static_insv, tmp_path, env):
    kf = tmp_path / "kf.json"
    kf.write_text(json.dumps([{"t": 0, "yaw": 0, "fov": 90}, {"t": 1, "yaw": 90, "fov": 60}]))
    out = tmp_path / "k.mp4"
    r = x3("reframe", static_insv, "-o", out, "--keyframes", kf, "--size", "320x180", "--crf", "30", "--preset", "ultrafast", env=env)
    assert r.returncode == 0, r.stderr


def test_demo_generates_playable_insv(tmp_path, env):
    p = tmp_path / "demo.insv"
    r = x3("demo", p, "--seconds", "1", "--lens", "240", "--motion", "static", env=env)
    assert r.returncode == 0 and p.exists()
    assert "two_streams" in x3("probe", p, "--json", env=env).stdout
