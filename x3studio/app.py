"""Logique applicative partagée par l'interface web et la ligne de commande."""
from __future__ import annotations

import os
import re
import subprocess
import threading
import time
from pathlib import Path

from . import stabilize
from .jobs import Job, JobManager
from .pipeline import (ExportError, ExportSettings, build_plan, build_proxy_cmd, build_snapshot_plan,
                       detect_encoders, run_ffmpeg, run_plan)
from .project import Project, default_output_dir, home_dir, project_id
from .media import probe_source


def _slug(s: str) -> str:
    return re.sub(r"[^A-Za-z0-9._-]+", "_", s).strip("_") or "video"


class App:
    def __init__(self, home: Path | None = None, output_dir: Path | None = None):
        self.home = Path(home) if home else home_dir()
        self.home.mkdir(parents=True, exist_ok=True)
        self.output_dir = Path(output_dir) if output_dir else default_output_dir()
        self.jobs = JobManager()
        self.projects: dict[str, Project] = {}
        self.encoders = detect_encoders()
        self._lock = threading.Lock()

    # -- projets -----------------------------------------------------------
    def open_project(self, path: str | Path, prepare: bool = True) -> Project:
        p = Project.open(path, self.home)
        with self._lock:
            known = self.projects.get(p.id)
            if known:
                p = known
            else:
                self.projects[p.id] = p
        if prepare:
            self.ensure_proxy(p)
        return p

    def get_project(self, pid: str) -> Project | None:
        return self.projects.get(pid)

    # -- aperçu + analyse ---------------------------------------------------
    def ensure_proxy(self, p: Project) -> Job | None:
        if p.has_proxy():
            if stabilize.available() and p.orientation() is None and not self.jobs.active(p.id, "analyze"):
                return self.start_analysis(p)
            return None
        act = self.jobs.active(p.id, "proxy")
        if act:
            return act

        def run(job: Job) -> str:
            part = p.proxy_path.with_suffix(".part")
            job.message = "Préparation de l'aperçu"
            try:
                run_ffmpeg(build_proxy_cmd(p.src, part), p.src.duration,
                           lambda f, _i: setattr(job, "progress", f), job.cancel)
                os.replace(part, p.proxy_path)
            finally:
                if part.exists():
                    part.unlink()
            if stabilize.available() and p.orientation() is None:
                self.start_analysis(p)
            return str(p.proxy_path)

        return self.jobs.submit("proxy", f"Aperçu : {Path(p.src.path).name}", run, p.id)

    def start_analysis(self, p: Project, force: bool = False) -> Job:
        if not stabilize.available():
            raise ExportError("OpenCV n'est pas installé : analyse du mouvement indisponible.")
        act = self.jobs.active(p.id, "analyze")
        if act:
            return act
        if not p.has_proxy():
            raise ExportError("L'aperçu n'est pas encore prêt.")

        def run(job: Job) -> str:
            job.message = "Analyse du mouvement"
            o = stabilize.analyze_video(p.src, p.calibration, p.proxy_path,
                                        lambda f: setattr(job, "progress", f), job.cancel)
            o.save(p.orient_path)
            return str(p.orient_path)

        return self.jobs.submit("analyze", f"Analyse du mouvement : {Path(p.src.path).name}", run, p.id)

    def prepare_sync(self, p: Project, analysis: bool = False,
                     on_progress=None) -> None:
        """Version bloquante (ligne de commande) : aperçu puis, au besoin, analyse du mouvement."""
        if not p.has_proxy():
            part = p.proxy_path.with_suffix(".part")
            try:
                run_ffmpeg(build_proxy_cmd(p.src, part), p.src.duration,
                           (lambda f, _i: on_progress("aperçu", f)) if on_progress else None)
                os.replace(part, p.proxy_path)
            finally:
                if part.exists():
                    part.unlink()
        if analysis:
            if not stabilize.available():
                raise ExportError("La stabilisation nécessite OpenCV : sudo apt install python3-opencv")
            o = p.orientation()
            if o is None or (o.key != stabilize.calibration_key(p.calibration) and p.src.layout != "equirect"):
                o = stabilize.analyze_video(p.src, p.calibration, p.proxy_path,
                                            (lambda f: on_progress("analyse du mouvement", f)) if on_progress else None)
                o.save(p.orient_path)

    # -- exports -----------------------------------------------------------
    def output_path(self, p: Project, s: ExportSettings, directory: str | None = None, ext: str | None = None) -> Path:
        d = Path(directory).expanduser() if directory else self.output_dir
        stem = _slug(Path(p.src.path).stem)
        tag = "360" if s.mode == "equirect" else s.projection
        ext = ext or ".mp4"
        base = d / f"{stem}_{tag}"
        out = Path(f"{base}{ext}")
        i = 2
        while out.exists():
            out = Path(f"{base}_{i}{ext}")
            i += 1
        return out

    def start_export(self, p: Project, overrides: dict | None = None, directory: str | None = None,
                     out_path: str | None = None) -> Job:
        s = p.export_settings(overrides)
        stab = p.stab_matrices()
        calib = p.calibration
        out = Path(out_path) if out_path else self.output_path(p, s, directory)
        out.parent.mkdir(parents=True, exist_ok=True)
        # on construit le plan tout de suite pour remonter les erreurs de paramètres à l'appelant
        plan = build_plan(p.src, calib, s, out, stab=stab, workroot=self.home)
        title = f"Export {'360°' if s.mode == 'equirect' else s.projection} : {out.name}"

        def run(job: Job) -> str:
            job.message = f"moteur {plan.engine}"
            return run_plan(plan, lambda f: setattr(job, "progress", f), job.cancel)

        return self.jobs.submit("export", title, run, p.id, on_done=lambda _j: plan.cleanup())

    def snapshot(self, p: Project, t: float, fmt: str = "jpg", directory: str | None = None,
                 out_path: str | None = None, overrides: dict | None = None) -> Path:
        s = p.export_settings(overrides)
        out = Path(out_path) if out_path else self.output_path(p, s, directory, f".{fmt}")
        out.parent.mkdir(parents=True, exist_ok=True)
        plan = build_snapshot_plan(p.src, p.calibration, s, t, out, stab=p.stab_matrices(), workroot=self.home)
        try:
            run_plan(plan)
        except Exception:
            raise
        return out

    def info(self) -> dict:
        ff = subprocess.run(["ffmpeg", "-version"], capture_output=True, text=True).stdout.splitlines()
        from . import __version__, render
        return {
            "version": __version__, "ffmpeg": ff[0] if ff else "introuvable",
            "opencv": render.available(), "output_dir": str(self.output_dir), "home": str(Path.home()),
            "encoders": sorted(e for e in self.encoders if e in ("libx264", "libx265")),
            "cpus": os.cpu_count() or 1,
        }
