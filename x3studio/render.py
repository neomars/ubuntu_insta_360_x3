"""Moteur de rendu direct « objectifs -> vue » (OpenCV + NumPy).

Pour chaque image de sortie, on calcule la direction de chaque pixel, on la fait
tourner (stabilisation, horizon, keyframes), puis on retrouve dans chacun des deux
fisheye le pixel correspondant (``cv2.remap``) et on fond la couture.

Contrairement à ``v360`` + ``sendcmd`` (qui recalcule toute la table de
correspondance à chaque changement de paramètre, ~0,1 s par commande à 1080p),
les tables sont calculées sur une grille 4 à 8 fois plus grossière puis
interpolées, et on évite l'image équirectangulaire intermédiaire de 5,7K.

Les conventions sont celles de :mod:`x3studio.geometry` / :mod:`x3studio.calibration`
(vérifiées contre ``v360`` dans les tests).
"""
from __future__ import annotations

import math
import queue
import subprocess
import threading
from collections import deque
from concurrent.futures import Future, ThreadPoolExecutor
from typing import Callable

import numpy as np

try:  # OpenCV est optionnel : sans lui, on retombe sur le chemin 100 % FFmpeg
    import cv2
except Exception:  # pragma: no cover
    cv2 = None

from .calibration import Calibration


def available() -> bool:
    return cv2 is not None


# --------------------------------------------------------------------------
# Rayons de sortie
# --------------------------------------------------------------------------
def output_rays(proj: str, gw: int, gh: int, fov_deg: float, out_w: int, out_h: int,
                y0: int = 0, y1: int | None = None) -> np.ndarray:
    """Directions (gh, gw, 3) float32 pour une grille grossière couvrant les lignes y0..y1.

    La grille échantillonne les centres de pixels d'une grille régulière de
    ``gw x gh`` cellules couvrant exactement la bande [y0, y1) de l'image de sortie.
    """
    if y1 is None:
        y1 = out_h
    u = ((np.arange(gw) + 0.5) / gw * 2.0 - 1.0).astype(np.float64)
    ypix = y0 + (np.arange(gh) + 0.5) / gh * (y1 - y0)
    v = (ypix / out_h * 2.0 - 1.0).astype(np.float64)
    U, V = np.meshgrid(u, v)
    aspect = out_h / out_w
    if proj == "flat":
        t = math.tan(math.radians(fov_deg) / 2)
        a = np.stack([U * t, -V * t * aspect, np.ones_like(U)], axis=-1)
        a /= np.linalg.norm(a, axis=-1, keepdims=True)
    elif proj == "fisheye":
        xa = U * math.radians(fov_deg) / 2
        ya = V * aspect * math.radians(fov_deg) / 2
        th = np.hypot(xa, ya)
        sinc = np.where(th > 1e-9, np.sin(th) / np.where(th > 1e-9, th, 1), 1.0)
        a = np.stack([xa * sinc, -ya * sinc, np.cos(th)], axis=-1)
    elif proj == "planet":
        # stéréographique : échelles tan(fov/4) distinctes en h et v (comme v360)
        kx = math.tan(math.radians(fov_deg) / 4)
        ky = math.tan(math.radians(fov_deg * aspect) / 4)
        xs, ys = U * kx, V * ky
        r = np.hypot(xs, ys)
        th = 2 * np.arctan(r)
        s = np.where(r > 1e-9, np.sin(th) / np.where(r > 1e-9, r, 1), 2.0)
        a = np.stack([xs * s, -ys * s, np.cos(th)], axis=-1)
    elif proj == "equirect":
        lon = U * math.pi
        lat = -V * math.pi / 2
        a = np.stack([np.cos(lat) * np.sin(lon), np.sin(lat), np.cos(lat) * np.cos(lon)], axis=-1)
    else:
        raise ValueError(f"projection inconnue : {proj}")
    return a.astype(np.float32)


# --------------------------------------------------------------------------
# Tables de correspondance
# --------------------------------------------------------------------------
def _lens_coords(a: np.ndarray, rot: np.ndarray, fov_deg: float, size: int):
    """Coordonnées pixel (x, y) dans un fisheye carré ``size`` + angle à l'axe (rad)."""
    d = a @ rot.astype(np.float32).T
    dx, dy, dz = d[..., 0], d[..., 1], d[..., 2]
    rho = np.hypot(dx, dy)
    theta = np.arctan2(rho, dz)
    half = math.radians(fov_deg) / 2
    scale = np.where(rho > 1e-7, theta / np.where(rho > 1e-7, rho, 1.0) / half, 1.0 / half)
    scale = np.where(dz < 0, np.where(rho > 1e-7, scale, 0.0), scale)
    u, v = dx * scale, -dy * scale
    x = (u + 1) * 0.5 * size - 0.5
    y = (v + 1) * 0.5 * size - 0.5
    return x.astype(np.float32), y.astype(np.float32), theta.astype(np.float32)


def lens_tables(a_low: np.ndarray, m: np.ndarray, calib: Calibration, size: int):
    """Tables basse résolution : (xA, yA, xB, yB, poids de A)."""
    ra, rb = calib.lens_matrices()
    xa, ya, ta = _lens_coords(a_low, ra @ m, calib.fov, size)
    xb, yb, tb = _lens_coords(a_low, rb @ m, calib.fov, size)
    half_fov = math.radians(calib.fov) / 2 - math.radians(0.3)
    cov_a = (ta <= half_fov).astype(np.float32)
    cov_b = (tb <= half_fov).astype(np.float32)
    b = calib.effective_blend()
    th = np.degrees(ta)
    if b <= 0:
        wa = (th < 90).astype(np.float32)
    else:
        x = np.clip((th - (90 - b / 2)) / b, 0, 1)
        wa = (1 - x * x * (3 - 2 * x)).astype(np.float32)
    wa_c = wa * cov_a
    wb_c = (1 - wa) * cov_b
    s = wa_c + wb_c
    w = np.where(s > 1e-6, wa_c / np.where(s > 1e-6, s, 1), 0.5).astype(np.float32)
    return xa, ya, xb, yb, w


class LensRenderer:
    """Rend une vue à partir d'une image I420 « deux objectifs côte à côte »."""

    def __init__(self, calib: Calibration, lens_size: int, out_w: int, out_h: int, projection: str,
                 grid_div: int = 4, band_rows: int = 540):
        if cv2 is None:
            raise RuntimeError("OpenCV (cv2) est requis pour ce moteur de rendu.")
        if out_w % 2 or out_h % 2:
            raise ValueError("La taille de sortie doit être paire.")
        self.calib, self.S = calib, lens_size
        self.w, self.h, self.proj = out_w, out_h, projection
        self.div = grid_div
        # bandes horizontales (limite la mémoire et permet de paralléliser)
        self.bands = []
        y = 0
        while y < out_h:
            y1 = min(out_h, y + band_rows)
            if out_h - y1 < 2:
                y1 = out_h
            if (y1 - y) % 2:
                y1 -= 1
            self.bands.append((y, y1))
            y = y1
        self.y_off = {}
        self.frame_bytes_in = (2 * lens_size) * lens_size * 3 // 2
        self.frame_bytes_out = out_w * out_h * 3 // 2

    # ------------------------------------------------------------------
    def _band_tables(self, band, m, fov):
        y0, y1 = band
        bh = y1 - y0
        gw = max(2, self.w // self.div)
        gh = max(2, bh // self.div)
        a = output_rays(self.proj, gw, gh, fov, self.w, self.h, y0, y1)
        xa, ya, xb, yb, wt = lens_tables(a, m, self.calib, self.S)
        S = self.S

        def up(arr, w, h):
            return cv2.resize(arr, (w, h), interpolation=cv2.INTER_LINEAR)
        out = {}
        for tag, (cw, ch) in (("y", (self.w, bh)), ("c", (self.w // 2, bh // 2))):
            scale = 1.0 if tag == "y" else 0.5
            ax, ay = up(xa, cw, ch), up(ya, cw, ch)
            bx, by = up(xb, cw, ch), up(yb, cw, ch)
            wfull = up(wt, cw, ch)
            if tag == "c":
                # coordonnées luma -> coordonnées chroma
                ax, ay, bx, by = ax * 0.5 - 0.25, ay * 0.5 - 0.25, bx * 0.5 - 0.25, by * 0.5 - 0.25
            lim = S * scale
            np.clip(ax, 0, lim - 1, out=ax)
            np.clip(ay, 0, lim - 1, out=ay)
            np.clip(bx, 0, lim - 1, out=bx)
            np.clip(by, 0, lim - 1, out=by)
            bx += lim  # l'objectif B est à droite dans l'image côte à côte
            out[tag] = (ax, ay, bx, by, wfull)
        return out

    @staticmethod
    def _blend(plane_src, tables, dst):
        ax, ay, bx, by, w = tables
        ya = cv2.remap(plane_src, ax, ay, cv2.INTER_LINEAR)
        yb = cv2.remap(plane_src, bx, by, cv2.INTER_LINEAR)
        w16 = np.rint(w * 256).astype(np.uint16)
        res = (yb.astype(np.uint16) * (256 - w16) + ya.astype(np.uint16) * w16 + 128) >> 8
        dst[:] = res.astype(np.uint8)

    def render(self, frame: np.ndarray, m: np.ndarray, fov: float) -> np.ndarray:
        """``frame`` : octets I420 d'une image 2S x S ; retourne les octets I420 de sortie."""
        S = self.S
        ysz = 2 * S * S
        csz = (S // 2) * S
        yp = frame[:ysz].reshape(S, 2 * S)
        up_ = frame[ysz:ysz + csz].reshape(S // 2, S)
        vp = frame[ysz + csz:ysz + 2 * csz].reshape(S // 2, S)
        out = np.empty(self.frame_bytes_out, dtype=np.uint8)
        oy = out[:self.w * self.h].reshape(self.h, self.w)
        ou = out[self.w * self.h:self.w * self.h * 5 // 4].reshape(self.h // 2, self.w // 2)
        ov = out[self.w * self.h * 5 // 4:].reshape(self.h // 2, self.w // 2)
        for band in self.bands:
            y0, y1 = band
            t = self._band_tables(band, m, fov)
            self._blend(yp, t["y"], oy[y0:y1])
            self._blend(up_, t["c"], ou[y0 // 2:y1 // 2])
            self._blend(vp, t["c"], ov[y0 // 2:y1 // 2])
        return out


# --------------------------------------------------------------------------
# Exécution : décodeur FFmpeg -> rendu -> encodeur FFmpeg
# --------------------------------------------------------------------------
def render_stream(decoder_cmd: list[str], encoder_cmd: list[str], renderer: LensRenderer,
                  mats: np.ndarray, fovs: np.ndarray,
                  on_progress: Callable[[float], None] | None = None,
                  cancel: threading.Event | None = None, workers: int = 3) -> None:
    from .pipeline import Cancelled, ExportError

    n = len(mats)
    dec = subprocess.Popen(decoder_cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
    enc = subprocess.Popen(encoder_cmd, stdin=subprocess.PIPE, stdout=subprocess.DEVNULL,
                           stderr=subprocess.PIPE, bufsize=0)
    errs: dict[str, deque] = {"dec": deque(maxlen=30), "enc": deque(maxlen=30)}

    def drain(proc, key):
        for line in iter(proc.stderr.readline, b""):
            errs[key].append(line.decode(errors="replace").rstrip())

    for p, k in ((dec, "dec"), (enc, "enc")):
        threading.Thread(target=drain, args=(p, k), daemon=True).start()

    q: queue.Queue = queue.Queue(maxsize=workers + 2)
    stop = threading.Event()
    n_in = renderer.frame_bytes_in

    def reader():
        try:
            for _ in range(n):
                if stop.is_set():
                    break
                buf = bytearray(n_in)
                mv = memoryview(buf)
                got = 0
                while got < n_in:
                    r = dec.stdout.readinto(mv[got:])
                    if not r:
                        break
                    got += r
                if got < n_in:
                    break
                q.put(np.frombuffer(buf, dtype=np.uint8))
        finally:
            q.put(None)

    rt = threading.Thread(target=reader, daemon=True)
    rt.start()
    pool = ThreadPoolExecutor(max_workers=workers)
    pending: deque[Future] = deque()
    done = 0
    failed: BaseException | None = None

    def flush(limit: int) -> None:
        nonlocal done
        while len(pending) > limit:
            res = pending.popleft().result()
            try:
                enc.stdin.write(res)
            except (BrokenPipeError, OSError):
                raise ExportError("L'encodeur s'est arrêté :\n" + "\n".join(errs["enc"]))
            done += 1
            if on_progress:
                on_progress(done / n)

    try:
        i = 0
        while True:
            if cancel is not None and cancel.is_set():
                raise Cancelled()
            frame = q.get()
            if frame is None:
                break
            k = min(i, n - 1)
            pending.append(pool.submit(renderer.render, frame, mats[k], float(fovs[k])))
            i += 1
            flush(workers * 2)
            if i >= n:
                break
        flush(0)
        stop.set()
        enc.stdin.close()
        rc = enc.wait()
        dec.stdout.close()
        dec.wait(timeout=10)
        if i == 0:
            raise ExportError("Aucune image décodée :\n" + "\n".join(errs["dec"]))
        if rc != 0:
            raise ExportError("FFmpeg (encodage) a échoué :\n" + "\n".join(errs["enc"]))
    except BaseException as e:
        failed = e
        stop.set()
        raise
    finally:
        pool.shutdown(wait=False, cancel_futures=True)
        for p in (dec, enc):
            if p.poll() is None:
                p.kill()
        while True:  # débloque le lecteur
            try:
                q.get_nowait()
            except queue.Empty:
                break
        if failed is None and on_progress:
            on_progress(1.0)
