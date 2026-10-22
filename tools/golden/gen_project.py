"""Vecteurs de référence pour `src/project.rs` : état par défaut et fusion profonde des réglages.

    python3 tools/golden/gen_project.py > tests/golden/project.json

La version Python (`x3studio/project.py`) sert de spécification ; ce script sera supprimé avec elle.
"""
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from x3studio.pipeline import DEFAULT_FOV  # noqa: E402
from x3studio.project import DEFAULT_STATE, _merge  # noqa: E402

PATCHES = [
    {},
    {"view": {"fov": 60}},
    {"trim": {"start": 1.5}, "projection": "fisheye"},
    {"calibration": {"fov": 195.5, "swap_lenses": True}},
    {"stabilization": {"mode": "horizon", "heading_strength": 0.5}},
    {"orientation": {"roll": -3.5}},
    {"keyframes": [{"t": 2.0, "yaw": 10.0}, {"t": 1.0, "fov": 50.0}]},
    {"export": {"codec": "h265", "crf": 18}},
    {"view": 5},                       # une valeur scalaire remplace l'objet
    {"unknown": {"a": 1}},             # clé inconnue : copiée telle quelle par la fusion (filtrée avant, dans update_state)
    {"view": {"yaw": None}},
]
cases = []
base = json.loads(json.dumps(DEFAULT_STATE))
for patch in PATCHES:
    cases.append({"base": base, "patch": patch, "merged": _merge(base, patch)})
# fusions successives : `export` est remplacé (pas fusionné), les listes aussi
chain = _merge(base, {"export": {"codec": "h265", "crf": 18}, "keyframes": [{"t": 1.0}]})
cases.append({"base": chain, "patch": {"export": {"preset": "slow"}, "keyframes": [{"t": 3.0, "yaw": 1.0}]},
              "merged": _merge(chain, {"export": {"preset": "slow"}, "keyframes": [{"t": 3.0, "yaw": 1.0}]})})
cases.append({"base": {"a": {"b": {"c": 1, "d": 2}}, "l": [1, 2]}, "patch": {"a": {"b": {"c": 9}}, "l": [3]},
              "merged": _merge({"a": {"b": {"c": 1, "d": 2}}, "l": [1, 2]}, {"a": {"b": {"c": 9}}, "l": [3]})})
json.dump({"default_state": DEFAULT_STATE, "default_fov": DEFAULT_FOV, "cases": cases}, sys.stdout, separators=(",", ":"))
