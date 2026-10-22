"""Stabilisation 360° et mise à niveau de l'horizon, calculées localement.

1. *Estimation* (:func:`analyze_video`) : sur un proxy équirectangulaire basse
   résolution, on suit des points sur cinq vues rectilignes (avant, droite, arrière,
   gauche, haut), on les convertit en directions 3D et on en déduit la rotation de
   la caméra entre deux images (Kabsch + rejet des aberrants). L'accumulation donne
   l'orientation ``Q_t`` (caméra -> monde de la première image).
2. *Cible* (:func:`stabilization_matrices`) : orientation voulue ``Z_t`` selon le mode
   (``smooth`` lissage, ``lock`` direction verrouillée, ``horizon`` horizon à plat
   avec cap lissé) ; la matrice retournée est ``T_t = Q_t⁻¹ · Z_t`` (sortie -> caméra),
   utilisée telle quelle par le moteur de rendu et par l'aperçu.

Sans OpenCV l'analyse est indisponible (le reste du logiciel fonctionne).
"""
from __future__ import annotations

import math
import subprocess
import threading
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

import numpy as np

try:
    import cv2
except Exception:  # pragma: no cover
    cv2 = None

from .calibration import Calibration
from .geometry import (matrices_to_ypr, quat_from_matrix, quats_to_matrices, rot_yaw, rotvec_to_matrix,
                       ypr_to_matrix)
from .media import Source

MODES = ("off", "smooth", "lock", "horizon")


def available() -> bool:
    return cv2 is not None


@dataclass
class Orientation:
    quats: np.ndarray        # (N,4) w,x,y,z : caméra(t) -> monde (image 0)
    fps: float
    inliers: np.ndarray      # (N,) nombre de points valides par image
    key: str = ""            # empreinte de la calibration utilisée à l'analyse

    def save(self, path: str | Path) -> None:
        np.savez_compressed(path, quats=self.quats, fps=self.fps, inliers=self.inliers, key=self.key)

    @classmethod
    def load(cls, path: str | Path) -> "Orientation":
        d = np.load(path, allow_pickle=False)
        return cls(d["quats"], float(d["fps"]), d["inliers"], str(d["key"]))

    @property
    def n(self) -> int:
        return len(self.quats)

    def matrices(self) -> np.ndarray:
        return quats_to_matrices(self.quats)


def calibration_key(c: Calibration) -> str:
    return "|".join(f"{v:.2f}" if isinstance(v, float) else str(v) for k, v in sorted(c.to_dict().items())
                    if k not in ("blend",))


# --------------------------------------------------------------------------
# Estimation de la rotation entre deux images
# --------------------------------------------------------------------------
class RotationEstimator:
    FACES = ((0, 0), (90, 0), (180, 0), (-90, 0), (0, 90))   # (yaw, pitch) : avant, droite, arrière, gauche, haut

    def __init__(self, width: int, height: int, face: int = 224, fov: float = 100.0, min_inliers: int = 14):
        if cv2 is None:
            raise RuntimeError("OpenCV est requis pour la stabilisation (pip install opencv-python-headless).")
        self.W, self.H, self.n, self.t = width, height, face, math.tan(math.radians(fov) / 2)
        self.min_inliers = min_inliers
        self.rots = [ypr_to_matrix(y, p, 0) for y, p in self.FACES]
        u = (np.arange(face) + 0.5) / face * 2 - 1
        U, V = np.meshgrid(u, u)
        r = np.stack([U * self.t, -V * self.t, np.ones_like(U)], -1)
        r /= np.linalg.norm(r, axis=-1, keepdims=True)
        self.maps = []
        for rot in self.rots:
            d = r @ rot.T
            lon = np.arctan2(d[..., 0], d[..., 2])
            lat = np.arcsin(np.clip(d[..., 1], -1, 1))
            mx = ((lon / (2 * np.pi) + 0.5) * width - 0.5).astype(np.float32)
            my = ((0.5 - lat / np.pi) * height - 0.5).astype(np.float32)
            self.maps.append((mx, my))

    def faces(self, gray: np.ndarray) -> list[np.ndarray]:
        return [cv2.remap(gray, mx, my, cv2.INTER_LINEAR, borderMode=cv2.BORDER_WRAP) for mx, my in self.maps]

    def _dirs(self, pts: np.ndarray, rot: np.ndarray) -> np.ndarray:
        u = (pts[:, 0] + 0.5) / self.n * 2 - 1
        v = (pts[:, 1] + 0.5) / self.n * 2 - 1
        r = np.stack([u * self.t, -v * self.t, np.ones_like(u)], -1)
        r /= np.linalg.norm(r, axis=-1, keepdims=True)
        return r @ rot.T

    def relative_rotation(self, fa: list[np.ndarray], fb: list[np.ndarray]) -> tuple[np.ndarray, int]:
        """Rotation R telle que ``b = R a`` pour des points fixes du monde (a: image 1, b: image 2)."""
        A, B = [], []
        lk = dict(winSize=(21, 21), maxLevel=3,
                  criteria=(cv2.TERM_CRITERIA_EPS | cv2.TERM_CRITERIA_COUNT, 20, 0.01))
        for k, (a, b) in enumerate(zip(fa, fb)):
            p0 = cv2.goodFeaturesToTrack(a, maxCorners=120, qualityLevel=0.01, minDistance=7, blockSize=7)
            if p0 is None or len(p0) < 6:
                continue
            p1, st, _ = cv2.calcOpticalFlowPyrLK(a, b, p0, None, **lk)
            p0r, st2, _ = cv2.calcOpticalFlowPyrLK(b, a, p1, None, **lk)
            ok = (st.ravel() == 1) & (st2.ravel() == 1) & (np.linalg.norm((p0r - p0).reshape(-1, 2), axis=1) < 0.6)
            if ok.sum() < 4:
                continue
            q0, q1 = p0.reshape(-1, 2)[ok], p1.reshape(-1, 2)[ok]
            da, db = self._dirs(q0, self.rots[k]), self._dirs(q1, self.rots[k])
            keep = da[:, 1] > -0.7          # ignore le nadir (souvent la personne / la perche)
            A.append(da[keep])
            B.append(db[keep])
        if not A:
            return np.eye(3), 0
        A, B = np.concatenate(A), np.concatenate(B)
        if len(A) < self.min_inliers:
            return np.eye(3), len(A)
        mask = np.ones(len(A), bool)
        R = np.eye(3)
        for _ in range(4):
            H = A[mask].T @ B[mask]
            U, _, Vt = np.linalg.svd(H)
            D = np.diag([1, 1, np.linalg.det(Vt.T @ U.T)])
            R = Vt.T @ D @ U.T
            ang = np.degrees(np.arccos(np.clip(np.einsum("ij,ij->i", B, A @ R.T), -1, 1)))
            thr = max(0.12, 2.5 * float(np.median(ang[mask])))
            new = ang < thr
            if new.sum() < self.min_inliers or (new == mask).all():
                mask = new if new.sum() >= self.min_inliers else mask
                break
            mask = new
        return R, int(mask.sum())


def analyze_frames(frames, width: int, height: int, fps: float, n_hint: int = 0,
                   on_progress: Callable[[float], None] | None = None,
                   cancel: threading.Event | None = None) -> Orientation:
    """``frames`` : itérable d'images grises (height, width) uint8."""
    from .pipeline import Cancelled
    est = RotationEstimator(width, height)
    qs = [np.array([1.0, 0, 0, 0])]
    inl = [0]
    q = np.eye(3)
    prev = None
    for i, g in enumerate(frames):
        if cancel is not None and cancel.is_set():
            raise Cancelled()
        cur = est.faces(g)
        if prev is not None:
            R, n = est.relative_rotation(prev, cur)
            q = q @ R.T                       # Q_{t+1} = Q_t · R⁻¹
            qs.append(quat_from_matrix(q))
            inl.append(n)
        prev = cur
        if on_progress and n_hint and i % 5 == 0:
            on_progress(min(1.0, i / n_hint))
    quats = np.array(qs)
    # continuité du signe des quaternions
    s = np.sign(np.einsum("ij,ij->i", quats[1:], quats[:-1]))
    s[s == 0] = 1
    quats *= np.concatenate([[1.0], np.cumprod(s)])[:, None]
    if on_progress:
        on_progress(1.0)
    return Orientation(quats, fps, np.array(inl))


def analyze_video(src: Source, calib: Calibration, proxy: str | Path,
                  on_progress: Callable[[float], None] | None = None,
                  cancel: threading.Event | None = None, width: int = 960) -> Orientation:
    from .pipeline import ExportError, build_analysis_cmd
    height = width // 2
    cmd = build_analysis_cmd(src, calib, proxy, width)
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
    errs: list[str] = []
    threading.Thread(target=lambda: errs.extend(l.decode(errors="replace") for l in proc.stderr), daemon=True).start()
    size = width * height

    def gen():
        while True:
            buf = proc.stdout.read(size)
            while buf is not None and len(buf) < size:
                more = proc.stdout.read(size - len(buf))
                if not more:
                    break
                buf += more
            if not buf or len(buf) < size:
                return
            yield np.frombuffer(buf, dtype=np.uint8).reshape(height, width)

    try:
        o = analyze_frames(gen(), width, height, src.fps, n_hint=src.frames, on_progress=on_progress, cancel=cancel)
    finally:
        proc.kill()
    if o.n < 2:
        raise ExportError("Analyse impossible : aucune image décodée.\n" + "".join(errs[-10:]))
    o.key = calibration_key(calib)
    return o


# --------------------------------------------------------------------------
# Lissage et cibles
# --------------------------------------------------------------------------
def _gauss_kernel(sigma_frames: float) -> np.ndarray:
    r = max(1, int(math.ceil(3 * sigma_frames)))
    x = np.arange(-r, r + 1)
    k = np.exp(-0.5 * (x / sigma_frames) ** 2)
    return k / k.sum()


def _smooth_signal(x: np.ndarray, sigma_frames: float) -> np.ndarray:
    """Lissage gaussien le long de l'axe 0 avec normalisation aux bords."""
    if sigma_frames < 1e-3:
        return x.copy()
    k = _gauss_kernel(sigma_frames)
    r = len(k) // 2
    n = len(x)

    def conv(v):
        return np.convolve(v, k, mode="full")[r:r + n]

    ones = conv(np.ones(n))
    x2 = x.reshape(n, -1)
    out = np.stack([conv(x2[:, c]) / ones for c in range(x2.shape[1])], axis=1)
    return out.reshape(x.shape)


def smooth_quats(qs: np.ndarray, sigma_frames: float) -> np.ndarray:
    """Moyenne gaussienne de quaternions (méthode des valeurs propres de Markley)."""
    if sigma_frames < 1e-3:
        return qs.copy()
    outer = np.einsum("ni,nj->nij", qs, qs).reshape(len(qs), 16)
    avg = _smooth_signal(outer, sigma_frames).reshape(len(qs), 4, 4)
    vals, vecs = np.linalg.eigh(avg)
    q = vecs[:, :, -1]
    sign = np.where(np.einsum("ij,ij->i", q, qs) < 0, -1.0, 1.0)
    q = q * sign[:, None]
    return q / np.linalg.norm(q, axis=1, keepdims=True)


def orientation_matrix(orient: dict | None) -> np.ndarray:
    o = {"yaw": 0.0, "pitch": 0.0, "roll": 0.0, **(orient or {})}
    return ypr_to_matrix(o["yaw"], o["pitch"], o["roll"])


def stabilization_matrices(orient: Orientation | None, mode: str = "off", strength: float = 1.0,
                           user_orientation: dict | None = None, ref_frame: int = 0,
                           heading_strength: float | None = None, n_frames: int = 1) -> np.ndarray:
    """Matrices ``T_t`` (N,3,3) : direction de sortie -> direction dans l'image équirectangulaire.

    ``strength`` : écart-type du lissage en secondes (mode ``smooth``) ;
    ``heading_strength`` : idem pour le cap (mode ``horizon``, 0 = le cap suit la caméra).
    """
    O = orientation_matrix(user_orientation)
    o_inv = O.T
    if orient is None or mode == "off":
        return np.broadcast_to(o_inv, (max(1, n_frames), 3, 3)).copy()
    Q = orient.matrices()                                  # caméra -> monde
    Qi = np.transpose(Q, (0, 2, 1))
    fps = orient.fps
    if mode == "smooth":
        qs = smooth_quats(orient.quats, strength * fps)
        Qs = quats_to_matrices(qs)
        return Qi @ Qs @ o_inv
    if mode == "lock":
        ref = Q[min(max(ref_frame, 0), len(Q) - 1)]
        return Qi @ (ref @ o_inv)
    if mode == "horizon":
        P = O @ Q                                          # orientation de la caméra dans le monde à plat
        yaw = np.unwrap(np.radians(matrices_to_ypr(P)[:, 0]))
        hs = strength if heading_strength is None else heading_strength
        ys = _smooth_signal(yaw, hs * fps)
        Ps = np.array([rot_yaw(a) for a in ys])
        return Qi @ (o_inv @ Ps)
    raise ValueError(f"mode de stabilisation inconnu : {mode}")
