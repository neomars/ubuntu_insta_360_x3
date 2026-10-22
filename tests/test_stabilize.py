import numpy as np
import pytest

pytest.importorskip("cv2")

from helpers import angle_between, mean_shift
from x3studio import stabilize as st
from x3studio.calibration import Calibration
from x3studio.media import probe_source
from x3studio.pipeline import ExportSettings, build_plan, build_proxy_cmd, run_ffmpeg, run_plan


@pytest.fixture(scope="module")
def analysed(shake_insv, tmp_path_factory):
    path, truth = shake_insv
    d = tmp_path_factory.mktemp("stab")
    src = probe_source(path)
    run_ffmpeg(build_proxy_cmd(src, d / "proxy.webm", lens_px=480))
    orient = st.analyze_video(src, Calibration(), d / "proxy.webm")
    return src, orient, truth


def test_orientation_matches_ground_truth(analysed):
    """L'orientation estimée suit la vraie rotation de la caméra à mieux que 0,2°."""
    _, o, truth = analysed
    n = min(o.n, len(truth))
    rel_true = np.einsum("ji,njk->nik", truth[0], truth[:n])        # C0ᵀ · Ct
    err = angle_between(o.matrices()[:n], rel_true)
    assert err.max() < 0.2, err.max()
    assert np.median(o.inliers[1:]) > 100


def _residual(truth, T):
    world = np.einsum("nij,njk->nik", truth[: len(T)], T[: len(truth)])   # direction « monde » d'un pixel de sortie
    return angle_between(np.broadcast_to(world[0], world.shape), world)


def test_modes_remove_shake(analysed):
    _, o, truth = analysed
    shake = angle_between(np.broadcast_to(truth[0], truth.shape), truth)
    assert np.sqrt((shake ** 2).mean()) > 2.0              # la vidéo de test tremble réellement
    for mode, kw, limit in [("lock", {}, 0.2), ("smooth", {"strength": 1.0}, 0.5), ("horizon", {"strength": 1.0}, 0.3)]:
        T = st.stabilization_matrices(o, mode, n_frames=o.n, **kw)
        assert T.shape == (o.n, 3, 3)
        assert np.sqrt((_residual(truth, T) ** 2).mean()) < limit, mode


def test_smoothing_strength_is_monotonic(analysed):
    _, o, truth = analysed
    res = [np.sqrt((_residual(truth, st.stabilization_matrices(o, "smooth", s, n_frames=o.n)) ** 2).mean()) for s in (0.1, 0.3, 1.0)]
    assert res[0] > res[1] > res[2]


def test_off_returns_user_orientation_only(analysed):
    _, o, _ = analysed
    from x3studio.geometry import ypr_to_matrix
    T = st.stabilization_matrices(o, "off", user_orientation={"yaw": 30, "roll": 5}, n_frames=4)
    assert np.allclose(T, ypr_to_matrix(30, 0, 5).T)


def test_horizon_mode_levels_a_tilted_camera():
    """Caméra penchée de 20° sans mouvement : le mode horizon + correction manuelle redresse l'image."""
    from x3studio.geometry import ypr_to_matrix, matrix_to_ypr
    n = 30
    tilt = ypr_to_matrix(0, 0, 20)
    # Q = orientation de la caméra dans le monde : constante, ici la caméra est la référence (identité)
    o = st.Orientation(np.tile([1.0, 0, 0, 0], (n, 1)), 30.0, np.full(n, 100))
    T = st.stabilization_matrices(o, "horizon", 0.5, {"roll": 20}, n_frames=n)
    # avec O = roulis 20°, l'horizon de sortie est tourné de -20° par rapport à la caméra
    assert matrix_to_ypr(T[0])[2] == pytest.approx(-20, abs=1e-6)


def test_end_to_end_export_is_steady(analysed, shake_insv, tmp_path):
    src, o, _ = analysed
    shaky = tmp_path / "shaky.mp4"
    steady = tmp_path / "steady.mp4"
    kw = dict(mode="reframe", width=480, height=270, view={"yaw": 20, "fov": 90}, crf=26, preset="ultrafast", engine="cv")
    run_plan(build_plan(src, Calibration(), ExportSettings(**kw), shaky))
    T = st.stabilization_matrices(o, "lock", n_frames=o.n)
    run_plan(build_plan(src, Calibration(), ExportSettings(**kw), steady, stab=T))
    a, b = mean_shift(shaky, 480, 270), mean_shift(steady, 480, 270)
    assert a > 2.0 and b < 0.5 and b < a / 8, (a, b)


def test_v360_engine_with_per_frame_commands_matches(analysed, tmp_path):
    """Le chemin 100 % FFmpeg (sendcmd + reset_rot) donne aussi une vidéo stable."""
    src, o, _ = analysed
    T = st.stabilization_matrices(o, "lock", n_frames=o.n)
    out = tmp_path / "v.mp4"
    kw = dict(mode="reframe", width=480, height=270, view={"yaw": 20, "fov": 90}, crf=26, preset="ultrafast", engine="v360", quality="fast")
    run_plan(build_plan(src, Calibration(), ExportSettings(**kw), out, stab=T))
    assert mean_shift(out, 480, 270) < 0.5


def test_orientation_save_load(analysed, tmp_path):
    _, o, _ = analysed
    o.key = "abc"
    o.save(tmp_path / "o.npz")
    o2 = st.Orientation.load(tmp_path / "o.npz")
    assert np.allclose(o.quats, o2.quats) and o2.key == "abc" and o2.fps == o.fps
