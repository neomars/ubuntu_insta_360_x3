"""Modèle d'objectifs (deux fisheye) et masque de fusion de la couture.

Chaque objectif est modélisé comme un fisheye équidistant : un rayon
``θ`` (angle à l'axe optique) arrive à la distance ``θ / (fov/2)`` du centre,
le bord de l'image carrée correspondant à ``fov/2`` (c'est exactement le
modèle ``fisheye`` de ``v360``).

Les corrections ``a_*`` / ``b_*`` sont de petites rotations (degrés) qui
permettent d'ajuster l'alignement de chaque objectif ; l'objectif B regarde
vers l'arrière (rotation de 180° en lacet appliquée en plus).
"""
from __future__ import annotations

import math
from dataclasses import asdict, dataclass, field, fields
from pathlib import Path

import numpy as np

from .geometry import rot_yaw, ypr_to_matrix


@dataclass
class Calibration:
    fov: float = 200.0          # champ de vision de chaque objectif (degrés)
    a_yaw: float = 0.0          # objectif A (flux 0, avant)
    a_pitch: float = 0.0
    a_roll: float = 0.0
    b_yaw: float = 0.0          # objectif B (flux 1, arrière)
    b_pitch: float = 0.0
    b_roll: float = 0.0
    blend: float = 10.0         # largeur de fondu de la couture (degrés)
    swap_lenses: bool = False   # inverse le rôle des deux flux vidéo

    # ------------------------------------------------------------------
    @classmethod
    def from_dict(cls, d: dict | None) -> "Calibration":
        d = d or {}
        names = {f.name for f in fields(cls)}
        return cls(**{k: v for k, v in d.items() if k in names})

    def to_dict(self) -> dict:
        return asdict(self)

    @classmethod
    def load(cls, path: str | Path) -> "Calibration":
        import json
        return cls.from_dict(json.loads(Path(path).read_text()))

    # ------------------------------------------------------------------
    def effective_blend(self) -> float:
        """Le fondu ne peut pas dépasser la zone de recouvrement des objectifs."""
        return float(max(0.0, min(self.blend, self.fov - 180.0)))

    def lens_matrices(self) -> tuple[np.ndarray, np.ndarray]:
        """Matrices (sphère -> objectif) pour A et B.

        ``d_objectif = M · d_sphère``, ``d_sphère`` étant la direction dans
        l'image équirectangulaire produite (axe de l'objectif A = z, au centre).
        """
        ra = ypr_to_matrix(self.a_yaw, self.a_pitch, self.a_roll)
        rb = ypr_to_matrix(self.b_yaw, self.b_pitch, self.b_roll) @ rot_yaw(math.pi)
        return ra, rb

    # ------------------------------------------------------------------
    def front_weight(self, width: int, height: int) -> np.ndarray:
        """Poids de l'objectif A dans l'image équirectangulaire (0..1)."""
        lon = (np.arange(width) + 0.5) / width * 2 * np.pi - np.pi
        lat = np.pi / 2 - (np.arange(height) + 0.5) / height * np.pi
        lo, la = np.meshgrid(lon, lat)
        d = np.stack([np.cos(la) * np.sin(lo), np.sin(la), np.cos(la) * np.cos(lo)], axis=-1)
        ra, _ = self.lens_matrices()
        dz = d @ ra[2]                       # composante z dans le repère de l'objectif A
        theta = np.degrees(np.arccos(np.clip(dz, -1, 1)))
        half = self.effective_blend() / 2
        if half <= 0:
            return (theta < 90).astype(np.float32)
        x = np.clip((theta - (90 - half)) / (2 * half), 0, 1)
        return (1 - x * x * (3 - 2 * x)).astype(np.float32)


def write_pgm(path: str | Path, arr01: np.ndarray) -> None:
    """Écrit un tableau flottant 0..1 en PGM 8 bits (lisible par FFmpeg)."""
    a = np.clip(np.rint(arr01 * 255), 0, 255).astype(np.uint8)
    h, w = a.shape
    with open(path, "wb") as f:
        f.write(b"P5\n%d %d\n255\n" % (w, h))
        f.write(a.tobytes())
