"""Analyse des fichiers Insta360 (.insv) et d'autres vidéos 360° via ffprobe."""
from __future__ import annotations

import json
import re
import subprocess
from dataclasses import asdict, dataclass, field
from pathlib import Path

INSTA360_TRAILER_MAGIC = bytes.fromhex("8db42d694ccc418790edff439fe026bf")

# Nom de fichier des caméras Insta360 : VID_20240101_120000_00_001.insv
_INSV_RE = re.compile(r"^(?P<prefix>(?:PRO_)?VID_\d{8}_\d{6})_(?P<lens>00|10)_(?P<num>\d+)\.insv$", re.I)

VIDEO_EXTS = {".insv", ".mp4", ".mov", ".mkv", ".webm", ".m4v"}


class MediaError(RuntimeError):
    pass


@dataclass
class Source:
    path: str
    layout: str                   # two_streams | two_files | sbs_fisheye | equirect
    lens_files: list[str]         # un fichier par objectif (ou le même deux fois)
    lens_streams: list[int]       # rang du flux vidéo dans chaque fichier (v:N)
    lens_size: tuple[int, int]    # (largeur, hauteur) d'une image d'objectif / de l'équirect
    fps: float
    duration: float
    codec: str
    has_audio: bool
    audio_file: str | None
    has_trailer: bool             # métadonnées Insta360 (gyro, etc.) présentes en fin de fichier
    size_bytes: int
    camera: str = ""
    notes: list[str] = field(default_factory=list)

    @property
    def frames(self) -> int:
        return int(round(self.duration * self.fps))

    def to_dict(self) -> dict:
        d = asdict(self)
        d["frames"] = self.frames
        return d

    @classmethod
    def from_dict(cls, d: dict) -> "Source":
        d = dict(d)
        d.pop("frames", None)
        d["lens_size"] = tuple(d["lens_size"])
        return cls(**d)


def ffprobe(path: str | Path) -> dict:
    cmd = ["ffprobe", "-v", "error", "-print_format", "json", "-show_format", "-show_streams", str(path)]
    try:
        out = subprocess.run(cmd, capture_output=True, text=True)
    except FileNotFoundError as e:  # pragma: no cover
        raise MediaError("ffprobe introuvable : installez ffmpeg (sudo apt install ffmpeg)") from e
    if out.returncode != 0 or not out.stdout.strip():
        # .insv est un conteneur MP4/QuickTime ; on force le démuxeur si la détection échoue
        out = subprocess.run(cmd[:1] + ["-f", "mov"] + cmd[1:], capture_output=True, text=True)
        if out.returncode != 0:
            raise MediaError(f"ffprobe a échoué sur {path}: {out.stderr.strip()[:300]}")
    return json.loads(out.stdout)


def _fps(stream: dict) -> float:
    for key in ("avg_frame_rate", "r_frame_rate"):
        v = stream.get(key, "0/0")
        try:
            n, d = v.split("/")
            if float(d) != 0 and float(n) != 0:
                return float(n) / float(d)
        except ValueError:
            continue
    return 30.0


def _video_streams(info: dict) -> list[dict]:
    """Flux vidéo « réels » (on ignore les vignettes jointes)."""
    vs = [s for s in info.get("streams", []) if s.get("codec_type") == "video"]
    real = []
    for rank, s in enumerate(vs):
        if s.get("disposition", {}).get("attached_pic"):
            continue
        s = dict(s)
        s["_rank"] = rank
        real.append(s)
    return real


def has_insta360_trailer(path: str | Path) -> bool:
    try:
        with open(path, "rb") as f:
            f.seek(-32, 2)
            return f.read(32) == INSTA360_TRAILER_MAGIC
    except OSError:
        return False


def find_lens_pair(path: Path) -> Path | None:
    """Pour les anciens modèles : ``..._00_...insv`` + ``..._10_...insv``."""
    m = _INSV_RE.match(path.name)
    if not m or m.group("lens") != "00":
        return None
    other = path.with_name(f"{m.group('prefix')}_10_{m.group('num')}{path.suffix}")
    return other if other.exists() else None


def probe_source(path: str | Path) -> Source:
    path = Path(path).expanduser().resolve()
    if not path.is_file():
        raise MediaError(f"Fichier introuvable : {path}")
    info = ffprobe(path)
    vids = _video_streams(info)
    if not vids:
        raise MediaError("Aucun flux vidéo dans ce fichier.")
    audio = [s for s in info["streams"] if s.get("codec_type") == "audio"]
    duration = float(info.get("format", {}).get("duration") or vids[0].get("duration") or 0)
    fps = _fps(vids[0])
    notes: list[str] = []
    camera = "Insta360" if path.suffix.lower() == ".insv" else ""

    # --- deux flux carrés dans un même fichier (X3, X2, ONE R, ...)
    squares = [s for s in vids if s["width"] == s["height"]]
    if len(squares) >= 2 and squares[0]["width"] == squares[1]["width"]:
        a, b = squares[0], squares[1]
        layout, files = "two_streams", [str(path), str(path)]
        streams = [a["_rank"], b["_rank"]]
        size = (a["width"], a["height"])
        codec = a.get("codec_name", "")
    else:
        v = vids[0]
        pair = find_lens_pair(path)
        if pair is not None and v["width"] == v["height"]:
            # deux fichiers, un objectif chacun
            info_b = ffprobe(pair)
            vb = _video_streams(info_b)
            if not vb:
                raise MediaError(f"Aucun flux vidéo dans {pair}")
            layout, files = "two_files", [str(path), str(pair)]
            streams = [v["_rank"], vb[0]["_rank"]]
            size = (v["width"], v["height"])
            codec = v.get("codec_name", "")
        elif v["width"] == 2 * v["height"] and v["width"] % 2 == 0 and path.suffix.lower() == ".insv":
            layout, files, streams = "sbs_fisheye", [str(path), str(path)], [v["_rank"], v["_rank"]]
            size = (v["height"], v["height"])
            codec = v.get("codec_name", "")
            notes.append("Deux fisheye côte à côte dans un seul flux.")
        elif v["width"] == 2 * v["height"]:
            layout, files, streams = "equirect", [str(path)], [v["_rank"]]
            size = (v["width"], v["height"])
            codec = v.get("codec_name", "")
            notes.append("Vidéo déjà assemblée (équirectangulaire) : seul le recadrage est disponible.")
        else:
            raise MediaError(
                f"Format non reconnu ({v['width']}x{v['height']}). Attendu : deux flux carrés (.insv), "
                "deux fisheye côte à côte, ou une vidéo équirectangulaire 2:1."
            )
    if layout == "two_files":
        notes.append("Deux fichiers d'objectif appariés (_00_ / _10_).")
    audio_file = str(path) if audio else None
    return Source(
        path=str(path), layout=layout, lens_files=files, lens_streams=streams,
        lens_size=(int(size[0]), int(size[1])), fps=fps, duration=duration, codec=codec,
        has_audio=bool(audio), audio_file=audio_file, has_trailer=has_insta360_trailer(path),
        size_bytes=path.stat().st_size, camera=camera, notes=notes,
    )
