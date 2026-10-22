import numpy as np
import pytest

from helpers import mean_shift, probe_video, read_frames, read_image
from x3studio import stabilize
from x3studio.calibration import Calibration
from x3studio.geometry import ypr_to_matrix
from x3studio.media import probe_source
from x3studio.pipeline import ExportError, ExportSettings, build_plan, build_snapshot_plan, run_plan
from x3studio.synth import make_texture

CAL = Calibration()


def snapshot(src, settings, t, out, stab=None):
    run_plan(build_snapshot_plan(src, CAL, settings, t, out, stab=stab))
    return read_image(out)


def small(a, k=2):
    h, w = a.shape[:2]
    return a[: h // k * k, : w // k * k].reshape(h // k, k, w // k, k, -1).mean(axis=(1, 3))


@pytest.mark.parametrize("engine,quality", [("v360", "high"), ("v360", "fast")])
def test_stitch_reconstructs_the_scene(static_insv, texture, tmp_path, engine, quality):
    """Une caméra fixe : l'équirectangulaire assemblée doit retrouver la texture d'origine."""
    src = probe_source(static_insv)
    s = ExportSettings(mode="equirect", engine=engine, quality=quality, stitch_width=1024, interp="cubic")
    img = snapshot(src, s, 0.5, tmp_path / "s.ppm")
    ref = small(texture, 1)
    err = np.abs(small(img, 4) - small(ref, 4)).mean()
    assert err < 0.03, err
    # contre-épreuve : une image décalée de 180° doit être nettement pire
    assert np.abs(small(np.roll(img, 512, axis=1), 4) - small(ref, 4)).mean() > 4 * err


def test_cv_engine_stitch_matches_texture(static_insv, texture):
    from x3studio.render import LensRenderer
    import subprocess
    src = probe_source(static_insv)
    S = src.lens_size[0]
    raw = subprocess.run(
        ["ffmpeg", "-v", "error", "-ss", "0.5", "-i", str(static_insv), "-filter_complex",
         "[0:v:0]format=yuv420p[a];[0:v:1]format=yuv420p[b];[a][b]hstack=inputs=2[v]", "-map", "[v]", "-frames:v", "1",
         "-f", "rawvideo", "-pix_fmt", "yuv420p", "-"], capture_output=True, check=True).stdout
    out = LensRenderer(CAL, S, 1024, 512, "equirect", grid_div=8).render(np.frombuffer(raw, np.uint8), np.eye(3), 0)
    yuv = tmp = None
    import tempfile, pathlib
    with tempfile.TemporaryDirectory() as d:
        f = pathlib.Path(d) / "o.yuv"
        f.write_bytes(out.tobytes())
        subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "yuv420p", "-s", "1024x512",
                        "-i", str(f), str(pathlib.Path(d) / "o.ppm")], check=True)
        img = read_image(pathlib.Path(d) / "o.ppm")
    assert np.abs(small(img, 4) - small(texture, 4)).mean() < 0.03


VIEWS = [
    ("flat", dict(yaw=30, pitch=10, roll=5, fov=90), 640, 360),
    ("flat", dict(yaw=170, pitch=-20, roll=0, fov=120), 640, 360),
    ("fisheye", dict(yaw=-40, pitch=20, roll=10, fov=180), 480, 480),
    ("fisheye", dict(yaw=0, pitch=0, roll=0, fov=220), 640, 360),
    ("planet", dict(yaw=0, pitch=-90, roll=0, fov=250), 480, 480),
    ("planet", dict(yaw=20, pitch=-60, roll=0, fov=200), 640, 360),
]


@pytest.mark.parametrize("proj,view,w,h", VIEWS)
def test_cv_and_v360_engines_agree(static_insv, tmp_path, proj, view, w, h):
    """Le moteur OpenCV reproduit le rendu v360 (référence FFmpeg) pour toutes les projections."""
    from x3studio.render import LensRenderer
    import subprocess
    src = probe_source(static_insv)
    s = ExportSettings(mode="reframe", projection=proj, width=w, height=h, view=view, engine="v360", interp="cubic")
    ref = snapshot(src, s, 0.5, tmp_path / "ref.ppm")
    raw = subprocess.run(
        ["ffmpeg", "-v", "error", "-ss", "0.5", "-i", str(static_insv), "-filter_complex",
         "[0:v:0]format=yuv420p[a];[0:v:1]format=yuv420p[b];[a][b]hstack=inputs=2[v]", "-map", "[v]", "-frames:v", "1",
         "-f", "rawvideo", "-pix_fmt", "yuv420p", "-"], capture_output=True, check=True).stdout
    out = LensRenderer(CAL, 480, w, h, proj, grid_div=4).render(
        np.frombuffer(raw, np.uint8), ypr_to_matrix(view["yaw"], view["pitch"], view["roll"]), view["fov"])
    (tmp_path / "o.yuv").write_bytes(out.tobytes())
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "yuv420p", "-s", f"{w}x{h}",
                    "-i", str(tmp_path / "o.yuv"), str(tmp_path / "o.ppm")], check=True)
    cv = read_image(tmp_path / "o.ppm")
    assert np.abs(small(cv, 2) - small(ref, 2)).mean() < 0.025


def test_reframe_export_with_keyframes(static_insv, tmp_path):
    src = probe_source(static_insv)
    kfs = [{"t": 0, "yaw": -60, "pitch": 0, "roll": 0, "fov": 90}, {"t": 1.4, "yaw": 60, "pitch": 10, "roll": 0, "fov": 70}]
    s = ExportSettings(mode="reframe", width=640, height=360, keyframes=kfs, crf=23, preset="veryfast", engine="cv")
    out = tmp_path / "o.mp4"
    plan = build_plan(src, CAL, s, out)
    assert plan.engine == "cv"
    progress = []
    run_plan(plan, progress.append)
    assert progress[-1] == 1.0
    info = probe_video(out)
    assert (info["w"], info["h"], info["frames"]) == (640, 360, 45) and info["codec"] == "h264"
    frames = read_frames(out, 640, 360).astype(float) / 255
    # la dernière image doit ressembler à la vue du dernier keyframe, la première à celle du premier
    last = snapshot(src, ExportSettings(mode="reframe", width=640, height=360, view=kfs[1]), 1.45, tmp_path / "l.ppm")
    first = snapshot(src, ExportSettings(mode="reframe", width=640, height=360, view=kfs[0]), 0.0, tmp_path / "f.ppm")
    assert np.abs(small(frames[-1], 2) - small(last, 2)).mean() < 0.05
    assert np.abs(small(frames[0], 2) - small(first, 2)).mean() < 0.05
    assert np.abs(small(frames[0], 2) - small(last, 2)).mean() > 0.1     # le cadrage a bien bougé


def test_trim_speed_fps_and_audio(static_insv, tmp_path):
    src = probe_source(static_insv)
    s = ExportSettings(mode="reframe", width=320, height=180, start=0.3, end=1.5, speed=2.0, fps=15, crf=30, preset="ultrafast")
    out = tmp_path / "o.mp4"
    run_plan(build_plan(src, CAL, s, out))
    info = probe_video(out)
    assert info["fps"] == pytest.approx(15)
    assert info["duration"] == pytest.approx(0.6, abs=0.12)
    import subprocess
    streams = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "stream=codec_type", "-of", "csv=p=0", str(out)],
                             capture_output=True, text=True).stdout.split()
    assert "audio" in streams
    s.audio = False
    run_plan(build_plan(src, CAL, s, out))
    assert "audio" not in subprocess.run(["ffprobe", "-v", "error", "-show_entries", "stream=codec_type", "-of", "csv=p=0", str(out)],
                                         capture_output=True, text=True).stdout


def test_equirect_export_has_spherical_metadata(static_insv, tmp_path):
    from x3studio.spherical import has_spherical_v1
    src = probe_source(static_insv)
    out = tmp_path / "o.mp4"
    run_plan(build_plan(src, CAL, ExportSettings(mode="equirect", stitch_width=960, crf=30, preset="ultrafast"), out))
    info = probe_video(out)
    assert (info["w"], info["h"]) == (960, 480)
    assert has_spherical_v1(out)
    # le fichier reste lisible après l'injection
    assert read_frames(out, 960, 480, 1).shape[0] == 1


def test_equirect_export_with_rotation_uses_cv_engine(static_insv, tmp_path):
    src = probe_source(static_insv)
    s = ExportSettings(mode="equirect", stitch_width=960, crf=30, preset="ultrafast", orientation={"yaw": 90})
    plan = build_plan(src, CAL, s, tmp_path / "o.mp4", stab=stabilize.stabilization_matrices(None, "off", user_orientation={"yaw": 90}, n_frames=60))
    assert plan.engine == "cv"
    run_plan(plan)
    got = read_frames(tmp_path / "o.mp4", 960, 480, 1)[0].astype(float) / 255
    ref = snapshot(src, ExportSettings(mode="equirect", stitch_width=960, orientation={"yaw": 90}, engine="v360"), 0.0, tmp_path / "r.ppm",
                   stab=stabilize.stabilization_matrices(None, "off", user_orientation={"yaw": 90}, n_frames=60))
    assert np.abs(small(got, 4) - small(ref, 4)).mean() < 0.04


def test_invalid_settings(static_insv, tmp_path):
    src = probe_source(static_insv)
    with pytest.raises(ExportError):
        build_plan(src, CAL, ExportSettings(mode="reframe", width=641, height=360), tmp_path / "o.mp4")
    with pytest.raises(ExportError):
        build_plan(src, CAL, ExportSettings(mode="reframe", projection="cube"), tmp_path / "o.mp4")
    with pytest.raises(ExportError):
        build_plan(src, CAL, ExportSettings(mode="wat"), tmp_path / "o.mp4")
    with pytest.raises(ExportError):
        build_plan(src, CAL, ExportSettings(start=5.0, end=5.01), tmp_path / "o.mp4")


def test_cancel_removes_partial_output(static_insv, tmp_path):
    import threading
    from x3studio.pipeline import Cancelled
    src = probe_source(static_insv)
    ev = threading.Event()
    ev.set()
    out = tmp_path / "o.mp4"
    plan = build_plan(src, CAL, ExportSettings(mode="reframe", width=320, height=180, engine="cv"), out)
    with pytest.raises(Cancelled):
        run_plan(plan, cancel=ev)
    assert not out.exists()


def test_equirect_source_can_be_reframed(static_insv, tmp_path):
    """Une vidéo 360° déjà assemblée (équirectangulaire) peut être recadrée."""
    pano = tmp_path / "pano.mp4"
    run_plan(build_plan(probe_source(static_insv), CAL, ExportSettings(mode="equirect", stitch_width=960, crf=26, preset="ultrafast", spherical_metadata=False), pano))
    src = probe_source(pano)
    assert src.layout == "equirect"
    view = dict(yaw=40, pitch=5, roll=0, fov=90)
    out = tmp_path / "r.mp4"
    run_plan(build_plan(src, CAL, ExportSettings(mode="reframe", width=320, height=180, view=view, crf=26, preset="ultrafast"), out))
    got = read_frames(out, 320, 180, 1)[0].astype(float) / 255
    ref = snapshot(probe_source(static_insv), ExportSettings(mode="reframe", width=320, height=180, view=view), 0.0, tmp_path / "ref.ppm")
    assert np.abs(small(got, 2) - small(ref, 2)).mean() < 0.06
