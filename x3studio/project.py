"""Projets : un fichier source + ses réglages (sauvegardés localement) + ses caches.

Tout est stocké dans ``~/.cache/x3studio`` (ou ``$X3STUDIO_HOME``) : proxy d'aperçu,
orientation de la caméra estimée et réglages d'édition. Rien ne quitte la machine.
"""
from __future__ import annotations

import copy
import hashlib
import json
import os
import threading
from pathlib import Path

import numpy as np

from .calibration import Calibration
from .media import Source, probe_source
from .pipeline import DEFAULT_FOV, ExportSettings
from . import stabilize

DEFAULT_STATE = {
    "calibration": Calibration().to_dict(),
    "trim": {"start": 0.0, "end": 0.0},
    "projection": "flat",
    "view": {"yaw": 0.0, "pitch": 0.0, "roll": 0.0, "fov": 90.0},
    "keyframes": [],
    "stabilization": {"mode": "off", "strength": 1.0, "heading_strength": 1.5},
    "orientation": {"yaw": 0.0, "pitch": 0.0, "roll": 0.0},
    "export": {},
}


def home_dir() -> Path:
    env = os.environ.get("X3STUDIO_HOME")
    if env:
        return Path(env)
    base = os.environ.get("XDG_CACHE_HOME") or str(Path.home() / ".cache")
    return Path(base) / "x3studio"


def default_output_dir() -> Path:
    env = os.environ.get("X3STUDIO_OUTPUT")
    if env:
        return Path(env)
    try:
        import subprocess
        out = subprocess.run(["xdg-user-dir", "VIDEOS"], capture_output=True, text=True, timeout=2).stdout.strip()
        if out and Path(out) != Path.home():
            return Path(out) / "X3Studio"
    except Exception:
        pass
    return Path.home() / "Videos" / "X3Studio"


def project_id(path: str | Path) -> str:
    p = Path(path).resolve()
    st = p.stat()
    return hashlib.sha1(f"{p}|{st.st_size}|{st.st_mtime_ns}".encode()).hexdigest()[:16]


def _merge(base: dict, patch: dict) -> dict:
    out = copy.deepcopy(base)
    for k, v in (patch or {}).items():
        if isinstance(v, dict) and isinstance(out.get(k), dict) and k not in ("export",):
            out[k] = _merge(out[k], v)
        else:
            out[k] = copy.deepcopy(v)
    return out


class Project:
    def __init__(self, src: Source, root: Path):
        self.src = src
        self.id = root.name
        self.dir = root
        self.lock = threading.RLock()
        self.state = copy.deepcopy(DEFAULT_STATE)
        self._orient: stabilize.Orientation | None = None
        self._orient_mtime = 0.0
        self._load()

    # -- ouverture -------------------------------------------------------
    @classmethod
    def open(cls, path: str | Path, home: Path | None = None) -> "Project":
        src = probe_source(path)
        pid = project_id(src.path)
        root = (home or home_dir()) / "projects" / pid
        root.mkdir(parents=True, exist_ok=True)
        (root / "source.json").write_text(json.dumps(src.to_dict(), indent=1))
        return cls(src, root)

    # -- persistance -----------------------------------------------------
    @property
    def state_file(self) -> Path:
        return self.dir / "state.json"

    def _load(self) -> None:
        if self.state_file.exists():
            try:
                self.state = _merge(DEFAULT_STATE, json.loads(self.state_file.read_text()))
            except (ValueError, OSError):
                pass
        if self.src.layout == "equirect":
            self.state["calibration"] = Calibration().to_dict()

    def update_state(self, patch: dict) -> dict:
        with self.lock:
            patch = {k: v for k, v in (patch or {}).items() if k in DEFAULT_STATE}
            self.state = _merge(self.state, patch)
            if "keyframes" in patch:
                self.state["keyframes"] = sorted(
                    ({k: float(v) for k, v in kf.items() if k in ("t", "yaw", "pitch", "roll", "fov")}
                     for kf in self.state["keyframes"]), key=lambda k: k["t"])
            tmp = self.state_file.with_suffix(".tmp")
            tmp.write_text(json.dumps(self.state, indent=1))
            tmp.replace(self.state_file)
            return copy.deepcopy(self.state)

    # -- fichiers --------------------------------------------------------
    @property
    def proxy_path(self) -> Path:
        return self.dir / "proxy.webm"

    @property
    def orient_path(self) -> Path:
        return self.dir / "orientation.npz"

    @property
    def calibration(self) -> Calibration:
        return Calibration.from_dict(self.state["calibration"])

    def has_proxy(self) -> bool:
        return self.proxy_path.exists() and self.proxy_path.stat().st_size > 1000

    def orientation(self) -> stabilize.Orientation | None:
        if not self.orient_path.exists():
            return None
        m = self.orient_path.stat().st_mtime
        if self._orient is None or m != self._orient_mtime:
            try:
                self._orient = stabilize.Orientation.load(self.orient_path)
                self._orient_mtime = m
            except Exception:
                return None
        return self._orient

    def analysis_info(self) -> dict:
        o = self.orientation()
        info = {"available": stabilize.available(), "ready": o is not None}
        if o is not None:
            key = stabilize.calibration_key(self.calibration)
            inl = o.inliers[1:] if o.n > 1 else o.inliers
            info.update(frames=int(o.n), stale=(o.key != key and self.src.layout != "equirect"),
                        median_inliers=int(np.median(inl)) if len(inl) else 0,
                        weak_frames=int((inl < 20).sum()) if len(inl) else 0)
        return info

    # -- calcul des matrices de stabilisation ------------------------------
    def stab_matrices(self, **over) -> np.ndarray | None:
        """T_t (N,3,3) pour les réglages courants (ou surchargés) ; None si rien à calculer."""
        st = {**self.state["stabilization"], **over}
        mode = st.get("mode", "off")
        orient = self.orientation()
        ref = int(round(self.state["trim"]["start"] * self.src.fps))
        user_o = over.get("orientation", self.state["orientation"])
        if mode == "off" or orient is None:
            return None if not any(abs(float(user_o.get(k, 0))) > 1e-9 for k in ("yaw", "pitch", "roll")) \
                else stabilize.stabilization_matrices(None, "off", user_orientation=user_o, n_frames=self.src.frames + 1)
        return stabilize.stabilization_matrices(
            orient, mode, strength=float(st.get("strength", 1.0)), user_orientation=user_o, ref_frame=ref,
            heading_strength=float(st.get("heading_strength", 1.5)), n_frames=self.src.frames + 1)

    def export_settings(self, overrides: dict | None = None) -> ExportSettings:
        s = self.state
        d = {
            "projection": s["projection"], "keyframes": s["keyframes"], "view": s["view"],
            "orientation": s["orientation"], "start": s["trim"]["start"], "end": s["trim"]["end"],
        }
        d.update(s.get("export") or {})
        d.update(overrides or {})
        return ExportSettings.from_dict(d)

    def to_json(self) -> dict:
        return {
            "id": self.id,
            "name": Path(self.src.path).name,
            "source": self.src.to_dict(),
            "state": self.state,
            "proxy": {"ready": self.has_proxy(), "url": f"/media/{self.id}/proxy.webm"},
            "analysis": self.analysis_info(),
            "default_fov": DEFAULT_FOV,
        }
