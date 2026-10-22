"""Génération d'une vidéo « type Insta360 X3 » synthétique.

Utile pour essayer le logiciel sans caméra et pour les tests automatiques :
une scène sphérique texturée est filmée par une caméra virtuelle qui tremble
(mouvement connu), et les deux objectifs fisheye sont écrits comme deux flux
vidéo dans un fichier ``.insv`` (conteneur MP4), comme le fait l'X3.
"""
from __future__ import annotations

import math
import subprocess
import tempfile
from pathlib import Path

import numpy as np

from .geometry import matrices_to_ypr, rot_yaw, ypr_to_matrix
from .pipeline import write_sendcmd


def make_texture(width: int = 2048, seed: int = 1) -> np.ndarray:
    """Texture équirectangulaire riche en coins (suivi de points) et repères colorés."""
    rng = np.random.default_rng(seed)
    h = width // 2
    y = np.linspace(0, 1, h)[:, None]
    img = np.zeros((h, width, 3), dtype=np.float32)
    sky = np.array([0.35, 0.55, 0.9]); ground = np.array([0.35, 0.28, 0.2])
    img[:] = np.where(y[..., None] < 0.5, sky * (1 - y[..., None] * 0.6), ground * (0.6 + y[..., None] * 0.4))
    # bruit multi-échelle
    for scale, amp in ((64, 0.25), (24, 0.2), (8, 0.15)):
        small = rng.random((h // scale + 1, width // scale + 1, 3)).astype(np.float32)
        up = np.kron(small, np.ones((scale, scale, 1), dtype=np.float32))[:h, :width]
        img += (up - 0.5) * amp
    # formes
    for _ in range(500):
        cx, cy = rng.integers(0, width), rng.integers(int(h * 0.12), int(h * 0.88))
        sw, sh = rng.integers(8, 70), rng.integers(8, 70)
        col = rng.random(3).astype(np.float32)
        x0, x1 = max(0, cx - sw), min(width, cx + sw)
        y0, y1 = max(0, cy - sh), min(h, cy + sh)
        if rng.random() < 0.5:
            img[y0:y1, x0:x1] = col
        else:
            yy, xx = np.ogrid[y0:y1, x0:x1]
            m = ((xx - cx) / sw) ** 2 + ((yy - cy) / sh) ** 2 < 1
            img[y0:y1, x0:x1][m] = col
    # repères : bandes verticales aux 4 points cardinaux + horizon
    for lon, col in ((0, (1, 0, 0)), (90, (0, 1, 0)), (180, (0, 0.2, 1)), (-90, (1, 1, 0))):
        cx = int(((lon + 180) % 360) / 360 * width)
        img[:, max(0, cx - 6):cx + 6] = np.array(col, dtype=np.float32)
    img[h // 2 - 2:h // 2 + 2, :] = 1.0
    return np.clip(img, 0, 1)


def write_ppm(path: Path, img01: np.ndarray) -> None:
    a = np.clip(np.rint(img01 * 255), 0, 255).astype(np.uint8)
    h, w, _ = a.shape
    with open(path, "wb") as f:
        f.write(b"P6\n%d %d\n255\n" % (w, h))
        f.write(a.tobytes())


def camera_motion(n: int, fps: float, motion: str = "shake", seed: int = 3):
    """Orientations caméra -> monde (matrices 3x3) pour chaque image."""
    rng = np.random.default_rng(seed)
    t = np.arange(n) / fps
    if motion == "static":
        yaw = pitch = roll = np.zeros(n)
    else:
        def shake(amp):
            out = np.zeros(n)
            for f in (0.9, 2.3, 4.1):
                out += rng.uniform(0.4, 1.0) * np.sin(2 * math.pi * f * t + rng.uniform(0, 2 * math.pi))
            return amp * out / 2.0
        pan = 12.0 * t if motion == "pan" else 0.0
        yaw = pan + shake(3.0)
        pitch = shake(2.5)
        roll = shake(3.5)
    mats = np.array([ypr_to_matrix(a, b, c) for a, b, c in zip(yaw, pitch, roll)])
    return t, mats


def make_synthetic_insv(path: str | Path, seconds: float = 3.0, fps: float = 30.0, lens: int = 960,
                        fov: float = 200.0, motion: str = "shake", seed: int = 3, texture: np.ndarray | None = None,
                        codec: str = "libx264", with_audio: bool = True) -> np.ndarray:
    """Écrit le .insv synthétique ; retourne les orientations vraies (N,3,3)."""
    path = Path(path)
    n = int(round(seconds * fps))
    t, mats = camera_motion(n, fps, motion, seed)
    tex = make_texture(seed=seed) if texture is None else texture
    flip = rot_yaw(math.pi)
    with tempfile.TemporaryDirectory(prefix="x3synth_") as d:
        d = Path(d)
        write_ppm(d / "tex.ppm", tex)
        ypr_a = matrices_to_ypr(mats)
        ypr_b = matrices_to_ypr(np.array([m @ flip for m in mats]))
        chains = []
        for tag, ypr in (("a", ypr_a), ("b", ypr_b)):
            opts = (f"input=e:output=fisheye:w={lens}:h={lens}:h_fov={fov}:v_fov={fov}:interp=cubic:reset_rot=1"
                    f":yaw={ypr[0, 0]:.4f}:pitch={ypr[0, 1]:.4f}:roll={ypr[0, 2]:.4f}")
            pre = ""
            if np.ptp(ypr, axis=0).max() > 1e-6:   # mouvement : une commande par image
                write_sendcmd(d / f"{tag}.cmd", t, f"v360@{tag}", {"yaw": ypr[:, 0], "pitch": ypr[:, 1], "roll": ypr[:, 2]})
                pre = f"sendcmd=f={d}/{tag}.cmd,"
            chains.append(f"[s{1 if tag == 'a' else 2}]{pre}v360@{tag}={opts},format=yuv420p[{tag}]")
        g = "[0:v]split[s1][s2];" + ";".join(chains)
        cmd = ["ffmpeg", "-hide_banner", "-nostdin", "-y", "-v", "error",
               "-loop", "1", "-framerate", str(fps), "-i", str(d / "tex.ppm")]
        if with_audio:
            cmd += ["-f", "lavfi", "-i", f"sine=frequency=440:sample_rate=48000:duration={seconds}"]
        cmd += ["-filter_complex", g, "-map", "[a]", "-map", "[b]"]
        if with_audio:
            cmd += ["-map", "1:a", "-c:a", "aac", "-b:a", "96k"]
        cmd += ["-c:v", codec, "-crf", "18", "-preset", "veryfast", "-pix_fmt", "yuv420p",
                "-frames:v", str(n), "-r", str(fps), "-f", "mp4", str(path)]
        subprocess.run(cmd, check=True)
    return mats
