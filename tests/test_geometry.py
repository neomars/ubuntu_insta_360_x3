import subprocess

import numpy as np
import pytest

from x3studio import geometry as g


def test_ypr_roundtrip():
    rng = np.random.default_rng(0)
    for _ in range(200):
        y, p, r = rng.uniform(-179, 179), rng.uniform(-88, 88), rng.uniform(-179, 179)
        m = g.ypr_to_matrix(y, p, r)
        assert np.allclose(m @ m.T, np.eye(3), atol=1e-12)
        y2, p2, r2 = g.matrix_to_ypr(m)
        assert np.allclose(g.ypr_to_matrix(y2, p2, r2), m, atol=1e-9)
        assert (y2, p2, r2) == pytest.approx((y, p, r), abs=1e-6)


def test_ypr_gimbal_lock_still_represents_rotation():
    m = g.ypr_to_matrix(30, 90, 20)
    assert np.allclose(g.ypr_to_matrix(*g.matrix_to_ypr(m)), m, atol=1e-6)


def test_vectorised_matches_scalar():
    rng = np.random.default_rng(1)
    ms = np.array([g.ypr_to_matrix(*rng.uniform(-80, 80, 3)) for _ in range(20)])
    assert np.allclose(g.matrices_to_ypr(ms), [g.matrix_to_ypr(m) for m in ms])


def test_convention_matches_ffmpeg_v360(tmp_path):
    """La rotation (yaw, pitch, roll) est bien celle du filtre v360 : d_entrée = M · d_sortie."""
    w, h = 512, 256
    lon = (np.arange(w) + 0.5) / w * 2 * np.pi - np.pi
    lat = np.pi / 2 - (np.arange(h) + 0.5) / h * np.pi
    lo, la = np.meshgrid(lon, lat)
    d = np.stack([np.cos(la) * np.sin(lo), np.sin(la), np.cos(la) * np.cos(lo)], -1)
    ppm = tmp_path / "dir.ppm"
    ppm.write_bytes(b"P6\n%d %d\n65535\n" % (w, h) + ((d * 0.49 + 0.5) * 65535).astype(">u2").tobytes())
    for angles in [(30, 0, 0), (0, 25, 0), (0, 0, 20), (40, -20, 10)]:
        vf = "v360=e:e:yaw=%g:pitch=%g:roll=%g:interp=linear" % angles
        out = subprocess.run(["ffmpeg", "-v", "error", "-i", str(ppm), "-vf", vf, "-f", "rawvideo",
                              "-pix_fmt", "rgb48le", "-"], capture_output=True, check=True).stdout
        a = (np.frombuffer(out, "<u2").reshape(h, w, 3).astype(float) / 65535 - 0.5) / 0.49
        sel = np.abs(d[..., 1]) < 0.7
        u, _, vt = np.linalg.svd(a[sel].T @ d[sel])
        m = u @ np.diag([1, 1, np.linalg.det(u @ vt)]) @ vt
        assert np.allclose(m, g.ypr_to_matrix(*angles), atol=0.01), angles


def test_interpolate_view():
    kfs = [{"t": 1, "yaw": 170, "pitch": 0, "roll": 0, "fov": 90}, {"t": 3, "yaw": -170, "pitch": 20, "roll": 0, "fov": 50}]
    assert g.interpolate_view(kfs, 0)["yaw"] == 170          # maintenu avant le premier
    assert g.interpolate_view(kfs, 9)["fov"] == 50           # et après le dernier
    mid = g.interpolate_view(kfs, 2)
    assert abs(abs(mid["yaw"]) - 180) < 1e-6                 # chemin le plus court (via 180°)
    assert mid["pitch"] == pytest.approx(10) and mid["fov"] == pytest.approx(70)
    assert g.interpolate_view([], 5, {"fov": 77})["fov"] == 77


def test_quaternion_roundtrip_and_mean():
    rng = np.random.default_rng(2)
    for _ in range(50):
        m = g.ypr_to_matrix(*rng.uniform(-170, 170, 3))
        assert np.allclose(g.matrix_from_quat(g.quat_from_matrix(m)), m, atol=1e-9)
        assert np.allclose(g.rotvec_to_matrix(g.matrix_to_rotvec(m)), m, atol=1e-9)
    q = g.quat_from_matrix(g.ypr_to_matrix(10, 5, 3))
    assert abs(g.weighted_quat_mean(np.array([q, -q, q]), np.ones(3)) @ q) == pytest.approx(1.0)


def test_flat_vfov():
    assert g.flat_vfov(90, 1920, 1080) == pytest.approx(58.716, abs=1e-2)
    assert g.flat_vfov(90, 1000, 1000) == pytest.approx(90)
