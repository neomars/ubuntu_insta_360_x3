"""Gestionnaire de tâches en arrière-plan (aperçu, analyse, exports)."""
from __future__ import annotations

import itertools
import threading
import time
import traceback
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from typing import Callable

from .pipeline import Cancelled


@dataclass
class Job:
    id: str
    kind: str                      # proxy | analyze | export | snapshot
    title: str
    project_id: str = ""
    state: str = "queued"          # queued | running | done | error | cancelled
    progress: float = 0.0
    message: str = ""
    output: str = ""
    error: str = ""
    created: float = field(default_factory=time.time)
    started: float = 0.0
    finished: float = 0.0
    cancel: threading.Event = field(default_factory=threading.Event, repr=False)

    def to_dict(self) -> dict:
        eta = None
        if self.state == "running" and self.progress > 0.02:
            el = time.time() - self.started
            eta = el / self.progress * (1 - self.progress)
        return {
            "id": self.id, "kind": self.kind, "title": self.title, "project_id": self.project_id,
            "state": self.state, "progress": round(self.progress, 4), "message": self.message,
            "output": self.output, "error": self.error, "eta": eta,
            "elapsed": (self.finished or time.time()) - self.started if self.started else 0,
        }


class JobManager:
    def __init__(self):
        self.jobs: dict[str, Job] = {}
        self._ids = itertools.count(1)
        self._lock = threading.Lock()
        self._heavy = ThreadPoolExecutor(max_workers=1, thread_name_prefix="x3-heavy")
        self._light = ThreadPoolExecutor(max_workers=2, thread_name_prefix="x3-light")

    def submit(self, kind: str, title: str, fn: Callable[[Job], str | None], project_id: str = "",
               heavy: bool = True, on_done: Callable[[Job], None] | None = None) -> Job:
        with self._lock:
            job = Job(id=f"j{next(self._ids)}", kind=kind, title=title, project_id=project_id)
            self.jobs[job.id] = job
        (self._heavy if heavy else self._light).submit(self._run, job, fn, on_done)
        return job

    def _run(self, job: Job, fn, on_done=None) -> None:
        if job.cancel.is_set():
            job.state, job.finished = "cancelled", time.time()
            if on_done:
                on_done(job)
            return
        job.state, job.started = "running", time.time()
        try:
            out = fn(job)
            job.output = out or ""
            job.progress = 1.0
            job.state = "done"
        except Cancelled:
            job.state = "cancelled"
        except BaseException as e:  # noqa: BLE001 - on remonte toute erreur à l'interface
            job.state = "error"
            job.error = str(e) if str(e) else e.__class__.__name__
            if not isinstance(e, (RuntimeError, ValueError)):
                job.error += "\n" + traceback.format_exc(limit=4)
        finally:
            job.finished = time.time()
            if on_done:
                try:
                    on_done(job)
                except Exception:  # noqa: BLE001
                    pass

    def get(self, jid: str) -> Job | None:
        return self.jobs.get(jid)

    def cancel(self, jid: str) -> bool:
        j = self.jobs.get(jid)
        if not j or j.state in ("done", "error", "cancelled"):
            return False
        j.cancel.set()
        if j.state == "queued":
            j.state, j.finished = "cancelled", time.time()
        return True

    def list(self) -> list[dict]:
        return [j.to_dict() for j in sorted(self.jobs.values(), key=lambda j: j.created, reverse=True)][:50]

    def active(self, project_id: str, kind: str) -> Job | None:
        for j in self.jobs.values():
            if j.project_id == project_id and j.kind == kind and j.state in ("queued", "running"):
                return j
        return None

    def wait(self, jid: str, timeout: float = 600) -> Job:
        t0 = time.time()
        j = self.jobs[jid]
        while j.state in ("queued", "running"):
            if time.time() - t0 > timeout:
                raise TimeoutError(jid)
            time.sleep(0.05)
        return j
