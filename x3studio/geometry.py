"""Géométrie sphérique : rotations, quaternions, interpolation de keyframes.

Conventions (identiques à celles du filtre ``v360`` de FFmpeg, vérifiées
empiriquement avec une image dont la couleur code la direction) :

* repère main droite : x vers la droite, y vers le haut, z vers l'avant ;
* une rotation ``M(yaw, pitch, roll)`` (ordre ``ypr``) transforme la direction
  d'un pixel de la *sortie* en la direction échantillonnée dans l'*entrée* :
  ``d_entrée = M · d_sortie`` ;
* ``yaw > 0`` regarde à droite, ``pitch > 0`` regarde vers le haut.

``M = Ry(yaw) · Rp(pitch) · Rr(roll)`` avec les matrices définies ci-dessous.
"""
from __future__ import annotations

import math
from typing import Iterable, Sequence

import numpy as np


# --------------------------------------------------------------------------
# Matrices élémentaires
# --------------------------------------------------------------------------
def rot_yaw(a: float) -> np.ndarray:
    c, s = math.cos(a), math.sin(a)
    return np.array([[c, 0, s], [0, 1, 0], [-s, 0, c]], dtype=float)


def rot_pitch(a: float) -> np.ndarray:
    c, s = math.cos(a), math.sin(a)
    return np.array([[1, 0, 0], [0, c, s], [0, -s, c]], dtype=float)


def rot_roll(a: float) -> np.ndarray:
    c, s = math.cos(a), math.sin(a)
    return np.array([[c, s, 0], [-s, c, 0], [0, 0, 1]], dtype=float)


def ypr_to_matrix(yaw: float, pitch: float, roll: float) -> np.ndarray:
    """Angles en degrés -> matrice 3x3 (convention v360, ordre ``ypr``)."""
    return rot_yaw(math.radians(yaw)) @ rot_pitch(math.radians(pitch)) @ rot_roll(math.radians(roll))


def matrix_to_ypr(m: np.ndarray) -> tuple[float, float, float]:
    """Inverse de :func:`ypr_to_matrix` (angles en degrés)."""
    sp = float(np.clip(m[1, 2], -1.0, 1.0))
    pitch = math.asin(sp)
    if abs(sp) > 0.999999:  # blocage de cardan : on fixe roll = 0
        yaw = math.atan2(-m[2, 0], m[0, 0])
        roll = 0.0
    else:
        yaw = math.atan2(m[0, 2], m[2, 2])
        roll = math.atan2(-m[1, 0], m[1, 1])
    return math.degrees(yaw), math.degrees(pitch), math.degrees(roll)


def matrices_to_ypr(ms: np.ndarray) -> np.ndarray:
    """Version vectorisée : (N,3,3) -> (N,3) en degrés."""
    ms = np.asarray(ms, dtype=float)
    sp = np.clip(ms[:, 1, 2], -1.0, 1.0)
    pitch = np.arcsin(sp)
    lock = np.abs(sp) > 0.999999
    yaw = np.where(lock, np.arctan2(-ms[:, 2, 0], ms[:, 0, 0]), np.arctan2(ms[:, 0, 2], ms[:, 2, 2]))
    roll = np.where(lock, 0.0, np.arctan2(-ms[:, 1, 0], ms[:, 1, 1]))
    return np.degrees(np.stack([yaw, pitch, roll], axis=1))


def wrap180(a):
    """Ramène un angle (ou tableau) en degrés dans [-180, 180)."""
    return (np.asarray(a) + 180.0) % 360.0 - 180.0


# --------------------------------------------------------------------------
# Quaternions (w, x, y, z)
# --------------------------------------------------------------------------
def quat_from_matrix(m: np.ndarray) -> np.ndarray:
    m = np.asarray(m, dtype=float)
    t = m[0, 0] + m[1, 1] + m[2, 2]
    if t > 0:
        s = math.sqrt(t + 1.0) * 2
        q = np.array([0.25 * s, (m[2, 1] - m[1, 2]) / s, (m[0, 2] - m[2, 0]) / s, (m[1, 0] - m[0, 1]) / s])
    elif m[0, 0] > m[1, 1] and m[0, 0] > m[2, 2]:
        s = math.sqrt(1.0 + m[0, 0] - m[1, 1] - m[2, 2]) * 2
        q = np.array([(m[2, 1] - m[1, 2]) / s, 0.25 * s, (m[0, 1] + m[1, 0]) / s, (m[0, 2] + m[2, 0]) / s])
    elif m[1, 1] > m[2, 2]:
        s = math.sqrt(1.0 + m[1, 1] - m[0, 0] - m[2, 2]) * 2
        q = np.array([(m[0, 2] - m[2, 0]) / s, (m[0, 1] + m[1, 0]) / s, 0.25 * s, (m[1, 2] + m[2, 1]) / s])
    else:
        s = math.sqrt(1.0 + m[2, 2] - m[0, 0] - m[1, 1]) * 2
        q = np.array([(m[1, 0] - m[0, 1]) / s, (m[0, 2] + m[2, 0]) / s, (m[1, 2] + m[2, 1]) / s, 0.25 * s])
    return q / np.linalg.norm(q)


def matrix_from_quat(q: np.ndarray) -> np.ndarray:
    w, x, y, z = np.asarray(q, dtype=float) / np.linalg.norm(q)
    return np.array([
        [1 - 2 * (y * y + z * z), 2 * (x * y - z * w), 2 * (x * z + y * w)],
        [2 * (x * y + z * w), 1 - 2 * (x * x + z * z), 2 * (y * z - x * w)],
        [2 * (x * z - y * w), 2 * (y * z + x * w), 1 - 2 * (x * x + y * y)],
    ])


def quats_to_matrices(qs: np.ndarray) -> np.ndarray:
    """(N,4) -> (N,3,3)."""
    qs = np.asarray(qs, dtype=float)
    qs = qs / np.linalg.norm(qs, axis=1, keepdims=True)
    w, x, y, z = qs.T
    out = np.empty((len(qs), 3, 3))
    out[:, 0, 0] = 1 - 2 * (y * y + z * z)
    out[:, 0, 1] = 2 * (x * y - z * w)
    out[:, 0, 2] = 2 * (x * z + y * w)
    out[:, 1, 0] = 2 * (x * y + z * w)
    out[:, 1, 1] = 1 - 2 * (x * x + z * z)
    out[:, 1, 2] = 2 * (y * z - x * w)
    out[:, 2, 0] = 2 * (x * z - y * w)
    out[:, 2, 1] = 2 * (y * z + x * w)
    out[:, 2, 2] = 1 - 2 * (x * x + y * y)
    return out


def quat_mul(a: np.ndarray, b: np.ndarray) -> np.ndarray:
    aw, ax, ay, az = a
    bw, bx, by, bz = b
    return np.array([
        aw * bw - ax * bx - ay * by - az * bz,
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
    ])


def quat_conj(q: np.ndarray) -> np.ndarray:
    return np.array([q[0], -q[1], -q[2], -q[3]])


def rotvec_to_matrix(v: Sequence[float]) -> np.ndarray:
    """Formule de Rodrigues : vecteur de rotation (radians) -> matrice."""
    v = np.asarray(v, dtype=float)
    th = float(np.linalg.norm(v))
    if th < 1e-12:
        return np.eye(3)
    k = v / th
    kx = np.array([[0, -k[2], k[1]], [k[2], 0, -k[0]], [-k[1], k[0], 0]])
    return np.eye(3) + math.sin(th) * kx + (1 - math.cos(th)) * (kx @ kx)


def matrix_to_rotvec(m: np.ndarray) -> np.ndarray:
    q = quat_from_matrix(m)
    if q[0] < 0:
        q = -q
    s = np.linalg.norm(q[1:])
    if s < 1e-12:
        return np.zeros(3)
    ang = 2 * math.atan2(s, q[0])
    return q[1:] / s * ang


def weighted_quat_mean(qs: np.ndarray, w: np.ndarray) -> np.ndarray:
    """Moyenne pondérée de quaternions (Markley et al.)."""
    ref = qs[len(qs) // 2]
    sign = np.where((qs @ ref) < 0, -1.0, 1.0)
    qs = qs * sign[:, None]
    m = (qs * w[:, None]).T @ qs
    vals, vecs = np.linalg.eigh(m)
    q = vecs[:, -1]
    if q @ ref < 0:
        q = -q
    return q / np.linalg.norm(q)


# --------------------------------------------------------------------------
# Keyframes de vue (yaw / pitch / roll / fov) -- miroir de web/app.js
# --------------------------------------------------------------------------
VIEW_FIELDS = ("yaw", "pitch", "roll", "fov")


def _smoothstep(x: float) -> float:
    x = min(1.0, max(0.0, x))
    return x * x * (3 - 2 * x)


def interpolate_view(keyframes: Sequence[dict], t: float, default: dict | None = None) -> dict:
    """Vue (yaw, pitch, roll, fov) à l'instant ``t`` (secondes, temps source).

    Interpolation douce (smoothstep) entre keyframes ; le yaw suit le chemin
    angulaire le plus court. Hors de l'intervalle, la vue des extrémités est
    maintenue. Sans keyframe : ``default``.
    """
    base = {"yaw": 0.0, "pitch": 0.0, "roll": 0.0, "fov": 90.0}
    if default:
        base.update({k: float(default[k]) for k in VIEW_FIELDS if k in default})
    if not keyframes:
        return base
    kfs = sorted(keyframes, key=lambda k: k["t"])
    if t <= kfs[0]["t"] or len(kfs) == 1:
        return {k: float(kfs[0].get(k, base[k])) for k in VIEW_FIELDS}
    if t >= kfs[-1]["t"]:
        return {k: float(kfs[-1].get(k, base[k])) for k in VIEW_FIELDS}
    for a, b in zip(kfs, kfs[1:]):
        if a["t"] <= t <= b["t"]:
            span = b["t"] - a["t"]
            u = _smoothstep((t - a["t"]) / span) if span > 0 else 1.0
            out = {}
            for k in VIEW_FIELDS:
                va, vb = float(a.get(k, base[k])), float(b.get(k, base[k]))
                if k == "yaw":
                    d = (vb - va + 180.0) % 360.0 - 180.0
                    out[k] = va + d * u
                else:
                    out[k] = va + (vb - va) * u
            return out
    return base


def view_matrix(view: dict) -> np.ndarray:
    return ypr_to_matrix(view["yaw"], view["pitch"], view["roll"])


def flat_vfov(h_fov: float, width: int, height: int) -> float:
    """Champ vertical d'une projection rectiligne de champ horizontal donné."""
    return math.degrees(2 * math.atan(math.tan(math.radians(h_fov) / 2) * height / width))


def frame_times(start: float, duration: float, fps: float) -> np.ndarray:
    n = max(1, int(round(duration * fps)))
    return start + np.arange(n) / fps


def as_list(x: Iterable) -> list:
    return list(x)
