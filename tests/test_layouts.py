"""Les différentes organisations de fichiers Insta360 donnent la même image que le cas X3 (deux flux)."""
import subprocess

import numpy as np
import pytest

from helpers import read_frames, read_image
from x3studio.calibration import Calibration
from x3studio.media import probe_source
from x3studio.pipeline import ExportSettings, build_plan, build_snapshot_plan, run_plan


def ff(*args):
    subprocess.run(["ffmpeg", "-v", "error", "-y", *map(str, args)], check=True)


@pytest.fixture(scope="module")
def variants(static_insv, tmp_path_factory):
    d = tmp_path_factory.mktemp("layouts")
    sbs = d / "sbs.insv"
    ff("-i", static_insv, "-filter_complex", "[0:v:0][0:v:1]hstack=inputs=2[v]", "-map", "[v]", "-map", "0:a",
       "-c:v", "libx264", "-crf", "16", "-c:a", "aac", "-f", "mp4", sbs)
    a, b = d / "VID_20240101_120000_00_001.insv", d / "VID_20240101_120000_10_001.insv"
    ff("-i", static_insv, "-map", "0:v:0", "-map", "0:a", "-c", "copy", "-f", "mp4", a)
    ff("-i", static_insv, "-map", "0:v:1", "-c", "copy", "-f", "mp4", b)
    return {"two_streams": static_insv, "sbs_fisheye": sbs, "two_files": a}


def small(a, k=3):
    h, w = a.shape[:2]
    return a[: h // k * k, : w // k * k].reshape(h // k, k, w // k, k, -1).mean(axis=(1, 3))


@pytest.mark.parametrize("layout", ["sbs_fisheye", "two_files"])
@pytest.mark.parametrize("engine", ["cv", "v360"])
def test_layout_export_matches_two_streams(variants, tmp_path, layout, engine):
    ref_src = probe_source(variants["two_streams"])
    src = probe_source(variants[layout])
    assert src.layout == layout and src.lens_size == (480, 480)
    view = {"yaw": 100, "pitch": 5, "roll": 0, "fov": 100}     # on regarde la zone de la couture
    s = ExportSettings(mode="reframe", width=320, height=180, view=view, engine=engine, crf=20, preset="ultrafast")
    out, ref = tmp_path / "o.mp4", tmp_path / "r.mp4"
    run_plan(build_plan(src, Calibration(), s, out))
    run_plan(build_plan(ref_src, Calibration(), s, ref))
    a = read_frames(out, 320, 180, 1)[0].astype(float) / 255
    b = read_frames(ref, 320, 180, 1)[0].astype(float) / 255
    assert np.abs(small(a) - small(b)).mean() < 0.03


def test_swap_lenses_exchanges_front_and_back(variants, tmp_path):
    src = probe_source(variants["two_streams"])
    base = ExportSettings(mode="reframe", width=320, height=180, view={"fov": 90}, engine="cv", crf=20, preset="ultrafast")
    front, back = tmp_path / "f.mp4", tmp_path / "b.mp4"
    run_plan(build_plan(src, Calibration(), base, front))
    run_plan(build_plan(src, Calibration(swap_lenses=True), base, back))
    f = read_frames(front, 320, 180, 1)[0].astype(float)
    b = read_frames(back, 320, 180, 1)[0].astype(float)
    assert np.abs(f - b).mean() > 20          # avant et arrière sont bien échangés


def test_equirect_source_gets_analysed(variants, tmp_path, app):
    """L'analyse du mouvement accepte aussi une vidéo déjà assemblée."""
    pytest.importorskip("cv2")
    pano = tmp_path / "pano.mp4"
    run_plan(build_plan(probe_source(variants["two_streams"]), Calibration(),
                        ExportSettings(mode="equirect", stitch_width=480, crf=28, preset="ultrafast", spherical_metadata=False), pano))
    p = app.open_project(pano, prepare=False)
    assert p.src.layout == "equirect"
    app.prepare_sync(p, analysis=True)
    assert p.orientation() is not None and p.orientation().n >= 40


def test_broken_input_reports_decoder_error(tmp_path):
    pytest.importorskip("cv2")
    from x3studio.pipeline import ExportError
    from x3studio.render import LensRenderer, render_stream
    r = LensRenderer(Calibration(), 480, 320, 180, "flat")
    dec = ["ffmpeg", "-v", "error", "-i", str(tmp_path / "nope.mp4"), "-f", "rawvideo", "-pix_fmt", "yuv420p", "-"]
    enc = ["ffmpeg", "-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "yuv420p", "-s", "320x180", "-framerate", "30",
           "-i", "-", str(tmp_path / "o.mp4")]
    with pytest.raises(ExportError):
        render_stream(dec, enc, r, np.tile(np.eye(3), (10, 1, 1)), np.full(10, 90.0))
