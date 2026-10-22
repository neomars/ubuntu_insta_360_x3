"""Utilitaires partagés par les tests."""
from __future__ import annotations

import re
import subprocess
from pathlib import Path

import numpy as np


def read_frames(path, w: int, h: int, n: int | None = None, pix_fmt: str = "rgb24", ss: float | None = None) -> np.ndarray:
    ch = 3 if pix_fmt == "rgb24" else 1
    cmd = ["ffmpeg", "-v", "error"] + (["-ss", str(ss)] if ss is not None else []) + ["-i", str(path)]
    cmd += (["-frames:v", str(n)] if n else []) + ["-f", "rawvideo", "-pix_fmt", pix_fmt, "-"]
    out = subprocess.run(cmd, capture_output=True, check=True).stdout
    a = np.frombuffer(out, dtype=np.uint8)
    return a.reshape(-1, h, w, ch) if ch == 3 else a.reshape(-1, h, w)


def read_image(path) -> np.ndarray:
    """Image -> float (h,w,3) 0..1 (PPM/PNG/JPEG via FFmpeg)."""
    p = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height",
                        "-of", "csv=p=0:s=x", str(path)], capture_output=True, text=True, check=True).stdout.strip()
    w, h = (int(v) for v in p.split("x"))
    return read_frames(path, w, h, 1)[0].astype(float) / 255


def probe_video(path) -> dict:
    out = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v:0", "-count_frames", "-show_entries",
                          "stream=width,height,nb_read_frames,r_frame_rate,codec_name:format=duration",
                          "-of", "default=nw=1", str(path)], capture_output=True, text=True, check=True).stdout
    d = dict(l.split("=", 1) for l in out.strip().splitlines())
    n, dn = d["r_frame_rate"].split("/")
    return {"w": int(d["width"]), "h": int(d["height"]), "frames": int(d["nb_read_frames"]),
            "fps": float(n) / float(dn), "duration": float(d["duration"]), "codec": d["codec_name"]}


def angle_between(r1: np.ndarray, r2: np.ndarray) -> np.ndarray:
    """Angle (°) de la rotation r1ᵀ r2, pour des tableaux de matrices."""
    m = np.einsum("...ji,...jk->...ik", r1, r2)
    return np.degrees(np.arccos(np.clip((np.trace(m, axis1=-2, axis2=-1) - 1) / 2, -1, 1)))


def mean_shift(path, w: int, h: int) -> float:
    """Décalage moyen (px) entre images consécutives (corrélation de phase) : mesure du tremblement."""
    import cv2
    fr = read_frames(path, w, h, pix_fmt="gray").astype(np.float32)
    win = cv2.createHanningWindow((w, h), cv2.CV_32F)
    return float(np.mean([np.hypot(*cv2.phaseCorrelate(fr[i], fr[i + 1], win)[0]) for i in range(len(fr) - 1)]))
