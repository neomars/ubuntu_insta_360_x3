"""Vecteurs de référence pour `src/stabilize.rs` (lissage de quaternions, matrices de stabilisation).

    python3 tools/golden/gen_stabilize.py > tests/golden/stabilize.json

La version Python (`x3studio/stabilize.py`) sert de spécification ; ce script sera supprimé avec elle.
"""
import json
import math
import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from x3studio import stabilize as st  # noqa: E402
from x3studio.geometry import quat_from_matrix, ypr_to_matrix  # noqa: E402

N, FPS = 48, 30.0
rng = np.random.default_rng(11)
t = np.arange(N) / FPS


def shake(amp):
    out = np.zeros(N)
    for f in (0.8, 2.1, 3.7):
        out += rng.uniform(0.4, 1.0) * np.sin(2 * math.pi * f * t + rng.uniform(0, 2 * math.pi))
    return amp * out / 2.0


# caméra qui tourne lentement (cap de 0° à 170°, ce qui traverse la coupure ±180° en mode horizon) et tremble
yaw = np.linspace(-40, 130, N) + shake(3.0)
pitch = 12 + shake(2.0)
roll = -8 + shake(3.0)
mats = np.array([ypr_to_matrix(a, b, c) for a, b, c in zip(yaw, pitch, roll)])
quats = np.array([quat_from_matrix(m) for m in mats])
s = np.sign(np.einsum("ij,ij->i", quats[1:], quats[:-1]))
s[s == 0] = 1
quats *= np.concatenate([[1.0], np.cumprod(s)])[:, None]
orient = st.Orientation(quats, FPS, np.full(N, 100))

CASES = [
    ("off", dict(mode="off", strength=1.0, user_orientation={"yaw": 30, "roll": 5})),
    ("smooth_0.5", dict(mode="smooth", strength=0.5)),
    ("smooth_user", dict(mode="smooth", strength=0.2, user_orientation={"yaw": -20, "pitch": 10, "roll": 3})),
    ("lock_ref0", dict(mode="lock", ref_frame=0)),
    ("lock_ref20_user", dict(mode="lock", ref_frame=20, user_orientation={"yaw": 15})),
    ("lock_ref_beyond", dict(mode="lock", ref_frame=500)),
    ("horizon", dict(mode="horizon", strength=0.4)),
    ("horizon_heading", dict(mode="horizon", strength=0.4, heading_strength=0.1, user_orientation={"roll": 6})),
]
cases = []
for name, kw in CASES:
    T = st.stabilization_matrices(orient, n_frames=N, **kw)
    kw = dict(kw)
    cases.append({
        "name": name,
        "mode": kw.pop("mode"),
        "strength": kw.pop("strength", 1.0),
        "ref_frame": kw.pop("ref_frame", 0),
        "heading_strength": kw.pop("heading_strength", None),
        "user": {"yaw": 0.0, "pitch": 0.0, "roll": 0.0, **kw.pop("user_orientation", {})},
        "matrices": [[round(float(v), 12) for v in m.ravel()] for m in T],
    })

smoothed = st.smooth_quats(quats, 0.5 * FPS)
out = {
    "fps": FPS,
    "quats": [[round(float(v), 12) for v in q] for q in quats],
    "smooth": {"sigma_frames": 0.5 * FPS, "quats": [[round(float(v), 12) for v in q] for q in smoothed]},
    "cases": cases,
}
json.dump(out, sys.stdout, separators=(",", ":"))
