#!/usr/bin/env python3
"""Valeurs de référence du moteur de rendu (Python/OpenCV) -> tests/golden/render.json.

Usage : python3 tools/golden/gen_render.py   (depuis la racine du dépôt, avec x3studio/ Python présent)
"""
import json
import sys
from pathlib import Path

import numpy as np

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))
from x3studio.calibration import Calibration  # noqa: E402
from x3studio.geometry import ypr_to_matrix  # noqa: E402
from x3studio.render import LensRenderer, output_rays  # noqa: E402

S = 48  # taille d'un objectif


def source_frame() -> np.ndarray:
    """Image I420 « deux objectifs côte à côte », lisse et déterministe."""
    w, h = 2 * S, S
    y, x = np.mgrid[0:h, 0:w].astype(np.float64)
    Y = 128 + 50 * np.sin(0.11 * x + 0.5) + 40 * np.cos(0.17 * y) + 20 * np.sin(0.05 * (x + y))
    yc, xc = np.mgrid[0:h // 2, 0:w // 2].astype(np.float64)
    U = 128 + 40 * np.sin(0.2 * xc) + 30 * np.cos(0.13 * yc)
    V = 128 + 35 * np.cos(0.15 * xc + 1.0) + 25 * np.sin(0.21 * yc)
    to8 = lambda a: np.clip(np.rint(a), 0, 255).astype(np.uint8)
    return np.concatenate([to8(Y).ravel(), to8(U).ravel(), to8(V).ravel()])


def swap_halves(frame: np.ndarray) -> np.ndarray:
    """Équivalent Python de `swap_lenses` (fait par le décodeur dans la version Python) : moitiés échangées."""
    w, h = 2 * S, S
    y = frame[: w * h].reshape(h, w)
    u = frame[w * h: w * h * 5 // 4].reshape(h // 2, w // 2)
    v = frame[w * h * 5 // 4:].reshape(h // 2, w // 2)
    sw = lambda a: np.concatenate([a[:, a.shape[1] // 2:], a[:, : a.shape[1] // 2]], axis=1)
    return np.concatenate([sw(y).ravel(), sw(u).ravel(), sw(v).ravel()])


RENDERS = [
    dict(name="flat", proj="flat", w=32, h=18, ypr=[20, 10, 5], fov=90, calib={}),
    dict(name="flat_seam", proj="flat", w=32, h=18, ypr=[90, 0, 0], fov=60, calib={}),
    dict(name="fisheye_square", proj="fisheye", w=24, h=24, ypr=[-40, 20, 10], fov=180, calib={}),
    dict(name="fisheye_wide", proj="fisheye", w=32, h=18, ypr=[0, 0, 0], fov=220, calib={}),
    dict(name="planet_down", proj="planet", w=24, h=24, ypr=[0, -90, 0], fov=250, calib={}),
    dict(name="planet_wide", proj="planet", w=32, h=18, ypr=[20, -60, 0], fov=200, calib={}),
    dict(name="equirect", proj="equirect", w=48, h=24, ypr=[0, 0, 0], fov=0, calib={}),
    dict(name="flat_swap", proj="flat", w=32, h=18, ypr=[20, 0, 0], fov=90, calib={"swap_lenses": True}),
    dict(name="flat_tuned", proj="flat", w=32, h=18, ypr=[100, 5, 0], fov=100,
         calib={"fov": 195.0, "blend": 20.0, "b_yaw": 3.0, "a_pitch": -2.0}),
]

RAYS = [
    dict(proj="flat", gw=6, gh=4, fov=90, w=48, h=27, y0=0, y1=27),
    dict(proj="fisheye", gw=5, gh=5, fov=200, w=30, h=30, y0=0, y1=30),
    dict(proj="planet", gw=7, gh=3, fov=250, w=64, h=32, y0=8, y1=24),
    dict(proj="equirect", gw=8, gh=4, fov=0, w=64, h=32, y0=0, y1=32),
]


def main() -> None:
    frame = source_frame()
    out = {"lens": S, "frame": frame.tobytes().hex(), "renders": [], "rays": []}
    for c in RENDERS:
        calib = Calibration.from_dict(c["calib"])
        src = frame
        if calib.swap_lenses:  # le moteur Python ne permute pas lui-même : on échange les moitiés de l'image
            src, calib.swap_lenses = swap_halves(frame), False
        r = LensRenderer(calib, S, c["w"], c["h"], c["proj"], grid_div=4)
        o = r.render(src, ypr_to_matrix(*c["ypr"]), float(c["fov"]))
        out["renders"].append({**c, "expected": o.tobytes().hex()})
    for c in RAYS:
        a = output_rays(c["proj"], c["gw"], c["gh"], float(c["fov"]), c["w"], c["h"], c["y0"], c["y1"])
        out["rays"].append({**c, "expected": np.round(a.reshape(-1, 3), 6).tolist()})
    path = ROOT / "tests" / "golden" / "render.json"
    path.write_text(json.dumps(out, separators=(",", ":")))
    print(f"{path} : {path.stat().st_size / 1024:.1f} Ko")


main()
