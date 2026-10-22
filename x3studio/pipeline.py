"""Construction et exécution des commandes FFmpeg (tout est traité localement).

Deux moteurs de rendu, qui donnent le même résultat :

* ``cv``   : décodage FFmpeg -> :mod:`x3studio.render` (OpenCV, objectifs lus
  directement, rotation différente à chaque image) -> encodage FFmpeg. Rapide
  dès que la vue bouge (stabilisation, keyframes) ;
* ``v360`` : un seul graphe de filtres FFmpeg (assemblage ``v360`` avec fondu de la
  couture, puis rotation/projection). Simple et sans OpenCV, mais ~0,1 s par
  commande et par image dès que la rotation varie.
"""
from __future__ import annotations

import os
import re
import shutil
import subprocess
import tempfile
import threading
from collections import deque
from dataclasses import asdict, dataclass, field, fields
from pathlib import Path
from typing import Callable, Optional

import numpy as np

from .calibration import Calibration, write_pgm
from .geometry import flat_vfov, interpolate_view, matrices_to_ypr, view_matrix, ypr_to_matrix
from .media import Source

PROJECTIONS = {"flat": "flat", "fisheye": "fisheye", "planet": "sg"}
MAX_FOV = {"flat": 170.0, "fisheye": 360.0, "planet": 350.0, "equirect": 0.0}
DEFAULT_FOV = {"flat": 90.0, "fisheye": 180.0, "planet": 250.0, "equirect": 0.0}
LENS_LAYOUTS = ("two_streams", "two_files", "sbs_fisheye")


class ExportError(RuntimeError):
    pass


class Cancelled(Exception):
    pass


# --------------------------------------------------------------------------
# Paramètres d'export
# --------------------------------------------------------------------------
@dataclass
class ExportSettings:
    mode: str = "reframe"            # reframe | equirect
    projection: str = "flat"         # flat | fisheye | planet (mode reframe)
    width: int = 1920
    height: int = 1080
    fps: float = 0.0                 # 0 = identique à la source
    start: float = 0.0
    end: float = 0.0                 # 0 = jusqu'à la fin
    speed: float = 1.0
    codec: str = "h264"              # h264 | h265
    crf: int = 20
    preset: str = "medium"
    quality: str = "high"            # high (fondu de couture) | fast (dfisheye brut) -- moteur v360
    interp: str = "line"             # line | cubic | lanc -- moteur v360
    stitch_width: int = 0            # 0 = automatique (2 x taille d'objectif)
    engine: str = "auto"             # auto | cv | v360
    audio: bool = True
    keyframes: list = field(default_factory=list)   # [{t, yaw, pitch, roll, fov}]
    view: dict = field(default_factory=dict)        # vue fixe si pas de keyframe
    orientation: dict = field(default_factory=dict)  # {yaw, pitch, roll} (recentrage / horizon)
    spherical_metadata: bool = True  # balises 360° dans le MP4 (mode equirect)

    @classmethod
    def from_dict(cls, d: dict | None) -> "ExportSettings":
        d = d or {}
        names = {f.name for f in fields(cls)}
        return cls(**{k: v for k, v in d.items() if k in names})

    def to_dict(self) -> dict:
        return asdict(self)


@dataclass
class Plan:
    cmd: list[str]
    out_path: str
    out_duration: float                 # durée de la sortie (s) pour la progression
    workdir: str
    spherical: bool = False
    kind: str = "video"                 # video | image
    engine: str = "v360"
    decoder_cmd: list[str] | None = None   # moteur cv
    renderer: object | None = None
    mats: np.ndarray | None = None
    fovs: np.ndarray | None = None

    def cleanup(self) -> None:
        shutil.rmtree(self.workdir, ignore_errors=True)

    def shell(self) -> str:
        import shlex
        parts = []
        if self.decoder_cmd:
            parts.append(" ".join(shlex.quote(c) for c in self.decoder_cmd) + "  |  <rendu x3studio>  |  ")
        parts.append(" ".join(shlex.quote(c) for c in self.cmd))
        return "".join(parts)


# --------------------------------------------------------------------------
# Utilitaires
# --------------------------------------------------------------------------
def ffq(path: str | Path) -> str:
    """Échappe un chemin pour l'utiliser comme valeur d'option de filtre."""
    s = str(path).replace("\\", "\\\\").replace("'", "'\\\\\\''").replace(":", "\\\\:")
    return f"'{s}'"


def _fmt(x: float) -> str:
    s = f"{float(x):.4f}".rstrip("0").rstrip(".")
    return s if s not in ("", "-0") else "0"


def write_sendcmd(path: Path, times: np.ndarray, target: str, params: dict[str, np.ndarray]) -> None:
    """Un enregistrement par image : ``t target cmd val, target cmd val;``."""
    lines = []
    keys = list(params)
    for i, t in enumerate(times):
        cmds = ", ".join(f"{target} {k} {_fmt(params[k][i])}" for k in keys)
        # légèrement avant l'instant exact : évite qu'un arrondi retarde la commande d'une image
        lines.append(f"{max(0.0, float(t) - 0.0005):.5f} {cmds};")
    path.write_text("\n".join(lines) + "\n")


def atempo_chain(speed: float) -> str:
    parts = []
    s = speed
    while s > 2.0:
        parts.append("atempo=2.0")
        s /= 2.0
    while s < 0.5:
        parts.append("atempo=0.5")
        s /= 0.5
    parts.append(f"atempo={s:.5f}")
    return ",".join(parts)


def encoder_args(s: ExportSettings) -> list[str]:
    if s.codec == "h265":
        return ["-c:v", "libx265", "-preset", s.preset, "-crf", str(s.crf), "-tag:v", "hvc1",
                "-pix_fmt", "yuv420p", "-x265-params", "log-level=error"]
    return ["-c:v", "libx264", "-preset", s.preset, "-crf", str(s.crf), "-pix_fmt", "yuv420p"]


def _clip_range(src: Source, s: ExportSettings) -> tuple[float, float]:
    start = max(0.0, s.start)
    if start >= src.duration - 0.05:
        raise ExportError("Le début de la plage dépasse la durée de la vidéo.")
    end = s.end if s.end and s.end > start else src.duration
    end = min(end, src.duration)
    if end - start < 0.05:
        raise ExportError("Plage de lecture vide.")
    return start, end


def _process_fps(src: Source, s: ExportSettings) -> tuple[float, float]:
    """(fps de traitement, fps de sortie)."""
    ofps = s.fps if s.fps and s.fps > 0 else src.fps
    pfps = src.fps
    if s.speed >= 1.0 and ofps * s.speed < src.fps - 0.01:
        pfps = ofps * s.speed
    return pfps, ofps


def stitch_size(src: Source, s: ExportSettings) -> tuple[int, int]:
    if s.stitch_width:
        w = int(s.stitch_width)
    elif src.layout == "equirect":
        w = src.lens_size[0]
    else:
        w = 2 * src.lens_size[0]
    w = max(256, min(w, 8192)) // 2 * 2
    return w, w // 2


def view_params(src: Source, s: ExportSettings, times_abs: np.ndarray,
                stab: Optional[np.ndarray]) -> tuple[np.ndarray, np.ndarray]:
    """Matrice de vue (sortie -> sphère de la caméra) et champ de vision, par image.

    ``stab`` : (N,3,3) une matrice par image *source* (orientation utilisateur
    incluse), voir :mod:`x3studio.stabilize` ; sinon seule l'orientation
    utilisateur s'applique.
    """
    n = len(times_abs)
    orient = {"yaw": 0.0, "pitch": 0.0, "roll": 0.0, **(s.orientation or {})}
    o_inv = ypr_to_matrix(orient["yaw"], orient["pitch"], orient["roll"]).T
    proj = s.projection if s.mode == "reframe" else "equirect"
    base_view = {"yaw": 0.0, "pitch": 0.0, "roll": 0.0, "fov": DEFAULT_FOV.get(proj, 90.0), **(s.view or {})}
    if stab is not None:
        idx = np.clip(np.rint(times_abs * src.fps).astype(int), 0, len(stab) - 1)
        base = stab[idx]
    else:
        base = np.broadcast_to(o_inv, (n, 3, 3))
    mats = np.empty((n, 3, 3))
    fovs = np.zeros(n)
    for i, t in enumerate(times_abs):
        if s.mode == "reframe":
            v = interpolate_view(s.keyframes, float(t), base_view)
            mats[i] = base[i] @ view_matrix(v)
            fovs[i] = min(max(v["fov"], 5.0), MAX_FOV[proj])
        else:
            mats[i] = base[i]
    return mats, fovs


# --------------------------------------------------------------------------
# Moteur v360
# --------------------------------------------------------------------------
def _lens_labels(src: Source, calib: Calibration) -> tuple[str, str]:
    """Étiquettes FFmpeg des deux flux (A = avant, B = arrière)."""
    if src.layout == "two_files":
        a, b = f"[0:v:{src.lens_streams[0]}]", f"[1:v:{src.lens_streams[1]}]"
    else:
        a, b = f"[0:v:{src.lens_streams[0]}]", f"[0:v:{src.lens_streams[1]}]"
    if calib.swap_lenses:
        a, b = b, a
    return a, b


def _input_args(src: Source, start: float, dur: float) -> list[str]:
    args: list[str] = []
    files = src.lens_files if src.layout == "two_files" else src.lens_files[:1]
    for f in files:
        args += ["-ss", f"{start:.4f}", "-t", f"{dur:.4f}", "-i", f]
    return args


def _lens_inputs(src: Source, calib: Calibration, fps_f: str) -> tuple[list[str], str, str]:
    """Filtres préparatoires et étiquettes des objectifs A/B (gère le mode côte à côte)."""
    if src.layout == "sbs_fisheye":
        pre = [f"[0:v:{src.lens_streams[0]}]{fps_f}format=yuv420p,split[sbs0][sbs1]",
               "[sbs0]crop=iw/2:ih:0:0[lA]", "[sbs1]crop=iw/2:ih:iw/2:0[lB]"]
        a, b = "[lA]", "[lB]"
        if calib.swap_lenses:
            a, b = b, a
        return pre, a, b
    a, b = _lens_labels(src, calib)
    return [], a, b


def stitch_filters(src: Source, calib: Calibration, s: ExportSettings, pfps: float, workdir: Path,
                   mask_input_index: int) -> tuple[list[str], str, list[str]]:
    """Retourne (filtres, étiquette de sortie équirectangulaire, entrées additionnelles)."""
    ws, hs = stitch_size(src, s)
    fps_f = f"fps={pfps:.5f}," if abs(pfps - src.fps) > 0.01 else ""

    if src.layout == "equirect":
        return [f"[0:v:{src.lens_streams[0]}]{fps_f}scale={ws}:{hs}:flags=bicubic,format=yuv420p[st]"], "[st]", []

    fov = calib.fov
    pre, a_lab, b_lab = _lens_inputs(src, calib, fps_f)
    if src.layout == "sbs_fisheye":
        fps_f = ""

    if s.quality == "fast":
        # dfisheye : moitié gauche = objectif arrière (centré à 180°), moitié droite = avant
        f = pre + [
            f"{a_lab}{fps_f}format=yuv420p[fa]",
            f"{b_lab}{fps_f}format=yuv420p[fb]",
            f"[fb][fa]hstack=inputs=2,v360=input=dfisheye:output=e:ih_fov={fov}:iv_fov={fov}"
            f":w={ws}:h={hs}:interp={s.interp}[st]",
        ]
        return f, "[st]", []

    ra, rb = calib.lens_matrices()
    ya, pa, rra = matrices_to_ypr(np.array([ra]))[0]
    yb, pb, rrb = matrices_to_ypr(np.array([rb]))[0]
    mask_path = workdir / "seam_mask.pgm"
    write_pgm(mask_path, calib.front_weight(ws, hs))
    extra = ["-loop", "1", "-framerate", f"{pfps:.5f}", "-t", "86400", "-i", str(mask_path)]

    def v360(yaw, pitch, roll):
        return (f"v360=input=fisheye:output=e:ih_fov={fov}:iv_fov={fov}:w={ws}:h={hs}"
                f":yaw={_fmt(yaw)}:pitch={_fmt(pitch)}:roll={_fmt(roll)}:interp={s.interp}")

    f = pre + [
        f"{a_lab}{fps_f}format=yuv420p,{v360(ya, pa, rra)}[ea]",
        f"{b_lab}{fps_f}format=yuv420p,{v360(yb, pb, rrb)}[eb]",
        f"[{mask_input_index}:v]format=gray[m]",
        "[ea][m]alphamerge[eaa]",
        "[eb][eaa]overlay=format=yuv420:shortest=1[st]",
    ]
    return f, "[st]", extra


def _view_filter(src: Source, s: ExportSettings, mats: np.ndarray, fovs: np.ndarray,
                 times_rel: np.ndarray, workdir: Path) -> Optional[str]:
    n = len(mats)
    ypr = matrices_to_ypr(mats)
    variable = bool(np.ptp(ypr, axis=0).max() > 1e-3 or (n > 1 and np.ptp(fovs) > 1e-3))
    if not variable and np.abs(ypr).max() < 1e-3 and s.mode == "equirect":
        return None
    ow, oh = (s.width, s.height) if s.mode == "reframe" else stitch_size(src, s)
    if s.mode == "equirect":
        vf = f"v360@view=input=e:output=e:w={ow}:h={oh}:interp={s.interp}:reset_rot=1"
        params = {"yaw": ypr[:, 0], "pitch": ypr[:, 1], "roll": ypr[:, 2]}
    else:
        out_name = PROJECTIONS[s.projection]
        vfov = np.array([flat_vfov(f, ow, oh) if s.projection == "flat" else f * oh / ow for f in fovs])
        vf = (f"v360@view=input=e:output={out_name}:w={ow}:h={oh}:interp={s.interp}:reset_rot=1"
              f":h_fov={_fmt(fovs[0])}:v_fov={_fmt(vfov[0])}")
        params = {"yaw": ypr[:, 0], "pitch": ypr[:, 1], "roll": ypr[:, 2], "h_fov": fovs, "v_fov": vfov}
        if np.ptp(fovs) < 1e-3:
            params = {k: params[k] for k in ("yaw", "pitch", "roll")}
    vf += f":yaw={_fmt(ypr[0, 0])}:pitch={_fmt(ypr[0, 1])}:roll={_fmt(ypr[0, 2])}"
    if variable:
        cmdfile = workdir / "view.cmd"
        write_sendcmd(cmdfile, times_rel, "v360@view", params)
        vf = f"sendcmd=f={ffq(cmdfile)}," + vf
    return vf


# --------------------------------------------------------------------------
# Plans
# --------------------------------------------------------------------------
def choose_engine(src: Source, s: ExportSettings, mats: np.ndarray, fovs: np.ndarray) -> str:
    from . import render
    if s.engine == "v360" or src.layout not in LENS_LAYOUTS:
        return "v360"
    if s.engine == "cv":
        if not render.available():
            raise ExportError("Le moteur 'cv' nécessite OpenCV (pip install opencv-python-headless).")
        return "cv"
    if not render.available():
        return "v360"
    ypr = matrices_to_ypr(mats)
    constant = np.ptp(ypr, axis=0).max() < 1e-3 and (len(fovs) < 2 or np.ptp(fovs) < 1e-3)
    if s.mode == "equirect" and constant and np.abs(ypr).max() < 1e-3:
        return "v360"      # simple assemblage statique : le graphe FFmpeg suffit
    return "cv"


def _audio_and_encode(cmd: list[str], src: Source, s: ExportSettings, start: float, dur: float,
                      audio_input_index: int, speed: float, out_dur: float, out_path, spherical: bool):
    use_audio = s.audio and src.has_audio
    if use_audio:
        cmd += ["-map", f"{audio_input_index}:a:0?"]
        if abs(speed - 1.0) > 1e-3:
            cmd += ["-af", atempo_chain(speed)]
        cmd += ["-c:a", "aac", "-b:a", "192k"]
    else:
        cmd += ["-an"]
    cmd += encoder_args(s)
    cmd += ["-t", f"{out_dur:.4f}"]
    if not spherical:
        cmd += ["-movflags", "+faststart"]
    cmd += [str(out_path)]
    return cmd


def build_plan(src: Source, calib: Calibration, s: ExportSettings, out_path: str | Path,
               stab: Optional[np.ndarray] = None, workroot: str | Path | None = None) -> Plan:
    """Construit le plan d'un export vidéo (commande(s) FFmpeg + rendu)."""
    if s.mode not in ("reframe", "equirect"):
        raise ExportError(f"Mode inconnu : {s.mode}")
    if s.mode == "reframe" and s.projection not in PROJECTIONS:
        raise ExportError(f"Projection inconnue : {s.projection}")
    if s.mode == "reframe" and (s.width % 2 or s.height % 2):
        raise ExportError("La largeur et la hauteur de sortie doivent être paires.")
    start, end = _clip_range(src, s)
    dur = end - start
    pfps, ofps = _process_fps(src, s)
    speed = max(0.05, float(s.speed or 1.0))
    n_proc = max(1, int(round(dur * pfps)))
    times_rel = np.arange(n_proc) / pfps
    mats, fovs = view_params(src, s, start + times_rel, stab)
    engine = choose_engine(src, s, mats, fovs)
    out_dur = dur / speed
    spherical = s.mode == "equirect" and s.spherical_metadata
    workdir = Path(tempfile.mkdtemp(prefix="x3job_", dir=workroot))

    post = []
    if abs(speed - 1.0) > 1e-3:
        post.append(f"setpts=PTS/{speed:.5f}")
    if abs(ofps - pfps * speed) > 0.01:
        post.append(f"fps={ofps:.5f}")
    post.append("format=yuv420p")

    if engine == "cv":
        from .render import LensRenderer
        ow, oh = (s.width, s.height) if s.mode == "reframe" else stitch_size(src, s)
        proj = s.projection if s.mode == "reframe" else "equirect"
        renderer = LensRenderer(calib, src.lens_size[0], ow, oh, proj,
                                grid_div=4 if ow * oh <= 4_200_000 else 8)
        dec = ["ffmpeg", "-hide_banner", "-nostdin", "-v", "error"]
        dec += _input_args(src, start, dur)
        fps_f = f"fps={pfps:.5f}," if abs(pfps - src.fps) > 0.01 else ""
        pre, a_lab, b_lab = _lens_inputs(src, calib, fps_f)
        if src.layout == "sbs_fisheye" and not calib.swap_lenses:
            g = f"[0:v:{src.lens_streams[0]}]{fps_f}format=yuv420p[v]"
        elif src.layout == "sbs_fisheye":
            g = ";".join(pre) + f";{a_lab}{b_lab}hstack=inputs=2[v]"
        else:
            g = (f"{a_lab}{fps_f}format=yuv420p[fa];{b_lab}{fps_f}format=yuv420p[fb];"
                 "[fa][fb]hstack=inputs=2[v]")
        dec += ["-filter_complex", g, "-map", "[v]", "-an", "-f", "rawvideo", "-pix_fmt", "yuv420p", "-"]
        enc = ["ffmpeg", "-hide_banner", "-nostdin", "-y", "-v", "error", "-f", "rawvideo", "-pix_fmt", "yuv420p",
               "-s", f"{ow}x{oh}", "-framerate", f"{pfps:.5f}", "-i", "-"]
        if s.audio and src.has_audio:
            enc += ["-ss", f"{start:.4f}", "-t", f"{dur:.4f}", "-i", src.audio_file or src.lens_files[0]]
        enc += ["-filter_complex", f"[0:v]{','.join(post)}[vout]", "-map", "[vout]"]
        enc = _audio_and_encode(enc, src, s, start, dur, 1, speed, out_dur, out_path, spherical)
        return Plan(cmd=enc, out_path=str(out_path), out_duration=out_dur, workdir=str(workdir),
                    spherical=spherical, engine="cv", decoder_cmd=dec, renderer=renderer,
                    mats=mats, fovs=fovs)

    filters, label, extra = stitch_filters(src, calib, s, pfps, workdir, 1 if src.layout != "two_files" else 2)
    view = _view_filter(src, s, mats, fovs, times_rel, workdir)
    chain = list(filters)
    if view:
        chain.append(f"{label}{view}[vw]")
        label = "[vw]"
    chain.append(f"{label}{','.join(post)}[vout]")
    cmd = ["ffmpeg", "-hide_banner", "-nostdin", "-y", "-v", "error", "-progress", "pipe:1", "-nostats"]
    cmd += _input_args(src, start, dur) + extra
    cmd += ["-filter_complex", ";".join(chain), "-map", "[vout]"]
    cmd = _audio_and_encode(cmd, src, s, start, dur, 0, speed, out_dur, out_path, spherical)
    return Plan(cmd=cmd, out_path=str(out_path), out_duration=out_dur, workdir=str(workdir),
                spherical=spherical, engine="v360")


def build_snapshot_plan(src: Source, calib: Calibration, s: ExportSettings, t: float,
                        out_path: str | Path, stab: Optional[np.ndarray] = None,
                        workroot: str | Path | None = None) -> Plan:
    """Image fixe (JPEG/PNG) à l'instant ``t`` avec la chaîne v360."""
    t = float(min(max(0.0, t), max(0.0, src.duration - 1.0 / max(src.fps, 1))))
    s2 = ExportSettings.from_dict({**s.to_dict(), "start": t, "end": min(src.duration, t + 1.0), "speed": 1.0})
    workdir = Path(tempfile.mkdtemp(prefix="x3snap_", dir=workroot))
    times_abs = np.array([t])
    mats, fovs = view_params(src, s2, times_abs, stab)
    filters, label, extra = stitch_filters(src, calib, s2, src.fps, workdir, 1 if src.layout != "two_files" else 2)
    view = _view_filter(src, s2, mats, fovs, np.array([0.0]), workdir)
    chain = list(filters)
    if view:
        chain.append(f"{label}{view}[vw]")
        label = "[vw]"
    is_jpg = str(out_path).lower().endswith((".jpg", ".jpeg"))
    chain.append(f"{label}format={'yuvj420p' if is_jpg else 'rgb24'}[vout]")
    cmd = ["ffmpeg", "-hide_banner", "-nostdin", "-y", "-v", "error"]
    cmd += _input_args(src, t, s2.end - t) + extra
    cmd += ["-filter_complex", ";".join(chain), "-map", "[vout]", "-an", "-frames:v", "1"]
    if is_jpg:
        cmd += ["-q:v", "2"]
    cmd += [str(out_path)]
    return Plan(cmd=cmd, out_path=str(out_path), out_duration=1.0, workdir=str(workdir), kind="image")


# --------------------------------------------------------------------------
# Aperçu (proxy basse résolution pour le lecteur web) et analyse
# --------------------------------------------------------------------------
def build_proxy_cmd(src: Source, out_path: str | Path, lens_px: int = 960) -> list[str]:
    """Vidéo WebM légère : deux fisheye côte à côte (ou équirect 2:1)."""
    cmd = ["ffmpeg", "-hide_banner", "-nostdin", "-y", "-v", "error", "-progress", "pipe:1", "-nostats"]
    sc = f"scale={lens_px}:{lens_px}:flags=bilinear"
    if src.layout == "two_files":
        cmd += ["-i", src.lens_files[0], "-i", src.lens_files[1]]
        g = (f"[0:v:{src.lens_streams[0]}]{sc}[a];[1:v:{src.lens_streams[1]}]{sc}[b];"
             "[a][b]hstack=inputs=2,format=yuv420p[v]")
    elif src.layout == "two_streams":
        cmd += ["-i", src.lens_files[0]]
        g = (f"[0:v:{src.lens_streams[0]}]{sc}[a];[0:v:{src.lens_streams[1]}]{sc}[b];"
             "[a][b]hstack=inputs=2,format=yuv420p[v]")
    else:  # sbs_fisheye / equirect : déjà une image 2:1
        cmd += ["-i", src.lens_files[0]]
        g = f"[0:v:{src.lens_streams[0]}]scale={2 * lens_px}:{lens_px}:flags=bilinear,format=yuv420p[v]"
    cmd += ["-filter_complex", g, "-map", "[v]"]
    if src.has_audio:
        cmd += ["-map", "0:a:0?", "-c:a", "libopus", "-b:a", "96k"]
    else:
        cmd += ["-an"]
    cmd += ["-c:v", "libvpx-vp9", "-b:v", "0", "-crf", "36", "-deadline", "realtime", "-cpu-used", "8",
            "-row-mt", "1", "-tile-columns", "2", "-g", "15", "-pix_fmt", "yuv420p", "-f", "webm", str(out_path)]
    return cmd


def build_analysis_cmd(src: Source, calib: Calibration, proxy: str | Path, width: int = 960) -> list[str]:
    """Images équirectangulaires grises (rawvideo sur stdout) pour l'analyse du mouvement.

    Part du proxy (décodage très rapide) : deux fisheye côte à côte -> équirect.
    """
    h = width // 2
    cmd = ["ffmpeg", "-hide_banner", "-nostdin", "-v", "error", "-i", str(proxy)]
    if src.layout == "equirect":
        g = f"[0:v:0]scale={width}:{h}:flags=bilinear,format=gray[g]"
    else:
        fov = calib.fov
        v = (f"v360=input=dfisheye:output=e:ih_fov={fov}:iv_fov={fov}:w={width}:h={h}:interp=line,format=gray[g]")
        # dfisheye : gauche = arrière, droite = avant ; le proxy est ordonné (A, B)
        if calib.swap_lenses:
            g = f"[0:v:0]{v}"          # (B, A) déjà dans l'ordre gauche = arrière
        else:
            g = ("[0:v:0]split[p0][p1];[p0]crop=iw/2:ih:0:0[la];[p1]crop=iw/2:ih:iw/2:0[lb];"
                 f"[lb][la]hstack=inputs=2,{v}")
    cmd += ["-filter_complex", g, "-map", "[g]", "-f", "rawvideo", "-pix_fmt", "gray", "-"]
    return cmd


# --------------------------------------------------------------------------
# Exécution
# --------------------------------------------------------------------------
def run_ffmpeg(cmd: list[str], total_seconds: float = 0.0,
               on_progress: Callable[[float, dict], None] | None = None,
               cancel: threading.Event | None = None) -> None:
    """Exécute FFmpeg, remonte la progression (0..1) et gère l'annulation."""
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
    errlines: deque[str] = deque(maxlen=40)

    def drain():
        for line in proc.stderr:
            errlines.append(line.rstrip())

    t = threading.Thread(target=drain, daemon=True)
    t.start()
    info: dict = {}
    try:
        for line in proc.stdout:
            if cancel is not None and cancel.is_set():
                proc.terminate()
                raise Cancelled()
            k, _, v = line.strip().partition("=")
            info[k] = v
            if k == "progress" and on_progress:
                us = info.get("out_time_us") or info.get("out_time_ms") or "0"
                try:
                    sec = max(0.0, int(us) / 1e6)
                except ValueError:
                    sec = 0.0
                frac = min(1.0, sec / total_seconds) if total_seconds > 0 else 0.0
                on_progress(frac, dict(info))
        rc = proc.wait()
    except BaseException:
        proc.kill()
        raise
    finally:
        t.join(timeout=2)
    if cancel is not None and cancel.is_set():
        raise Cancelled()
    if rc != 0:
        raise ExportError("FFmpeg a échoué :\n" + "\n".join(errlines))
    if on_progress:
        on_progress(1.0, info)


def run_plan(plan: Plan, on_progress: Callable[[float], None] | None = None,
             cancel: threading.Event | None = None) -> str:
    try:
        if plan.engine == "cv":
            from .render import render_stream
            render_stream(plan.decoder_cmd, plan.cmd, plan.renderer, plan.mats, plan.fovs,
                          on_progress=on_progress, cancel=cancel)
        else:
            run_ffmpeg(plan.cmd, plan.out_duration, (lambda f, _i: on_progress(f)) if on_progress else None, cancel)
        if plan.spherical:
            from .spherical import inject_spherical_v1
            inject_spherical_v1(plan.out_path)
        return plan.out_path
    except BaseException:
        if os.path.exists(plan.out_path):
            try:
                os.remove(plan.out_path)
            except OSError:
                pass
        raise
    finally:
        plan.cleanup()


def detect_encoders() -> set[str]:
    try:
        out = subprocess.run(["ffmpeg", "-hide_banner", "-encoders"], capture_output=True, text=True).stdout
    except FileNotFoundError:
        return set()
    return set(re.findall(r"^\s*V\S*\s+(\S+)", out, flags=re.M))
