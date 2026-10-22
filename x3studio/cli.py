"""Ligne de commande : ``x3studio`` (interface web, sonde, exports par lot, vidéo de démonstration)."""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from . import __version__
from .calibration import Calibration
from .media import MediaError, probe_source


def _progress(label: str):
    last = {"v": -1}
    tty = sys.stderr.isatty()

    def cb(f: float, *_):
        pct = int(f * 100)
        step = pct if tty else pct // 10 * 10
        if step != last["v"]:
            last["v"] = step
            sys.stderr.write((f"\r{label} : {step:3d} %" + ("\n" if pct >= 100 else "")) if tty
                             else f"{label} : {step} %\n")
            sys.stderr.flush()
    return cb


def _size(txt: str) -> tuple[int, int]:
    try:
        w, h = txt.lower().split("x")
        return int(w), int(h)
    except ValueError:
        raise argparse.ArgumentTypeError("format attendu : LARGEURxHAUTEUR (ex. 1920x1080)")


def _triplet(txt: str) -> dict:
    try:
        y, p, r = (float(v) for v in txt.split(","))
    except ValueError:
        raise argparse.ArgumentTypeError("format attendu : cap,inclinaison,roulis (ex. 0,0,-3.5)")
    return {"yaw": y, "pitch": p, "roll": r}


def _common_export_args(p: argparse.ArgumentParser) -> None:
    p.add_argument("input", help="fichier .insv (ou vidéo 360° équirectangulaire)")
    p.add_argument("-o", "--output", help="fichier de sortie (.mp4)")
    p.add_argument("--start", type=float, default=0.0, help="début (s)")
    p.add_argument("--end", type=float, default=0.0, help="fin (s, 0 = jusqu'à la fin)")
    p.add_argument("--speed", type=float, default=1.0, help="vitesse (0.25 à 8)")
    p.add_argument("--fps", type=float, default=0.0, help="images/s de sortie (0 = source)")
    p.add_argument("--codec", choices=("h264", "h265"), default="h264")
    p.add_argument("--crf", type=int, default=20, help="qualité x264/x265 (plus bas = meilleur)")
    p.add_argument("--preset", default="medium")
    p.add_argument("--no-audio", action="store_true")
    p.add_argument("--stabilize", choices=("off", "smooth", "lock", "horizon"), default="off",
                   help="stabilisation (nécessite OpenCV)")
    p.add_argument("--strength", type=float, default=1.0, help="lissage de la stabilisation (s)")
    p.add_argument("--heading", type=float, default=1.5, help="lissage du cap en mode horizon (s)")
    p.add_argument("--orient", type=_triplet, default={}, help="orientation : cap,inclinaison,roulis (°)")
    p.add_argument("--calibration", help="JSON de calibration des objectifs")
    p.add_argument("--engine", choices=("auto", "cv", "v360"), default="auto")
    p.add_argument("--quality", choices=("high", "fast"), default="high", help="moteur v360 : fondu de couture ou non")
    p.add_argument("--dry-run", action="store_true", help="affiche les commandes sans exécuter")
    p.add_argument("--output-dir", help="dossier de sortie par défaut")


def _prepare(args, need_stab: bool):
    from .app import App
    app = App(output_dir=Path(args.output_dir) if args.output_dir else None)
    p = app.open_project(args.input, prepare=False)
    if args.calibration:
        p.update_state({"calibration": Calibration.load(args.calibration).to_dict()})
    if need_stab and not args.dry_run:
        cbs: dict = {}
        app.prepare_sync(p, analysis=True,
                         on_progress=lambda lab, f: cbs.setdefault(lab, _progress(lab))(f))
    return app, p


def _stab(p, args):
    if args.stabilize == "off" and not args.orient:
        return None
    return p.stab_matrices(mode=args.stabilize, strength=args.strength, heading_strength=args.heading,
                           orientation=args.orient or {"yaw": 0, "pitch": 0, "roll": 0})


def _run(app, p, s, args, snapshot_t: float | None = None) -> int:
    from .pipeline import build_plan, build_snapshot_plan, run_plan
    stab = _stab(p, args)
    ext = ".jpg" if snapshot_t is not None else ".mp4"
    out = Path(args.output) if args.output else app.output_path(p, s, args.output_dir, ext)
    if not args.dry_run:
        out.parent.mkdir(parents=True, exist_ok=True)
    if snapshot_t is not None:
        plan = build_snapshot_plan(p.src, p.calibration, s, snapshot_t, out, stab=stab, workroot=app.home)
    else:
        plan = build_plan(p.src, p.calibration, s, out, stab=stab, workroot=app.home)
    if args.dry_run:
        print(plan.shell())
        plan.cleanup()
        return 0
    print(f"Moteur : {plan.engine} → {out}", file=sys.stderr)
    run_plan(plan, _progress("export") if snapshot_t is None else None)
    print(out)
    return 0


def _settings(args, **extra):
    from .pipeline import ExportSettings
    d = dict(start=args.start, end=args.end, speed=args.speed, fps=args.fps, codec=args.codec, crf=args.crf,
             preset=args.preset, audio=not args.no_audio, engine=args.engine, quality=args.quality)
    d.update(extra)
    return ExportSettings.from_dict(d)


def cmd_gui(args) -> int:
    from .server import serve
    serve(port=args.port, open_browser=not args.no_browser, path=args.input)
    return 0


def cmd_probe(args) -> int:
    src = probe_source(args.input)
    if args.json:
        print(json.dumps(src.to_dict(), indent=2, ensure_ascii=False))
        return 0
    layouts = {"two_streams": "deux flux fisheye dans un fichier", "two_files": "deux fichiers fisheye (_00_/_10_)",
               "sbs_fisheye": "deux fisheye côte à côte", "equirect": "équirectangulaire (déjà assemblée)"}
    print(f"Fichier   : {src.path}")
    print(f"Format    : {layouts[src.layout]}")
    print(f"Image     : {src.lens_size[0]}×{src.lens_size[1]}" + (" par objectif" if src.layout != "equirect" else ""))
    print(f"Cadence   : {src.fps:.3f} i/s — durée {src.duration:.2f} s ({src.frames} images) — codec {src.codec}")
    print(f"Audio     : {'oui' if src.has_audio else 'non'} — métadonnées Insta360 : {'oui' if src.has_trailer else 'non'}")
    for n in src.notes:
        print(f"Note      : {n}")
    return 0


def cmd_reframe(args) -> int:
    from .pipeline import DEFAULT_FOV
    kfs = json.loads(Path(args.keyframes).read_text()) if args.keyframes else []
    app, p = _prepare(args, args.stabilize != "off")
    w, h = args.size
    view = {"yaw": args.yaw, "pitch": args.pitch, "roll": args.roll,
            "fov": args.fov if args.fov else DEFAULT_FOV[args.projection]}
    s = _settings(args, mode="reframe", projection=args.projection, width=w, height=h, view=view, keyframes=kfs)
    return _run(app, p, s, args)


def cmd_equirect(args) -> int:
    app, p = _prepare(args, args.stabilize != "off")
    s = _settings(args, mode="equirect", stitch_width=args.width)
    return _run(app, p, s, args)


def cmd_snapshot(args) -> int:
    from .pipeline import DEFAULT_FOV
    app, p = _prepare(args, args.stabilize != "off")
    w, h = args.size
    view = {"yaw": args.yaw, "pitch": args.pitch, "roll": args.roll,
            "fov": args.fov if args.fov else DEFAULT_FOV[args.projection]}
    s = _settings(args, mode="reframe", projection=args.projection, width=w, height=h, view=view)
    return _run(app, p, s, args, snapshot_t=args.t)


def cmd_demo(args) -> int:
    from .synth import make_synthetic_insv
    out = Path(args.output)
    print(f"Génération de {out} ({args.seconds:.0f} s, mouvement « {args.motion} »)…", file=sys.stderr)
    make_synthetic_insv(out, seconds=args.seconds, fps=30, lens=args.lens, motion=args.motion)
    print(out)
    return 0


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(prog="x3studio", description="Traitement local des vidéos Insta360 X3.")
    ap.add_argument("--version", action="version", version=f"x3studio {__version__}")
    sub = ap.add_subparsers(dest="cmd")

    g = sub.add_parser("gui", help="ouvre l'interface web locale (défaut)")
    g.add_argument("input", nargs="?", help="vidéo à ouvrir directement")
    g.add_argument("--port", type=int, default=0)
    g.add_argument("--no-browser", action="store_true")
    g.set_defaults(fn=cmd_gui)

    pr = sub.add_parser("probe", help="analyse un fichier")
    pr.add_argument("input")
    pr.add_argument("--json", action="store_true")
    pr.set_defaults(fn=cmd_probe)

    rf = sub.add_parser("reframe", help="exporte une vidéo plate (recadrage d'une caméra virtuelle)")
    _common_export_args(rf)
    rf.add_argument("--projection", choices=("flat", "fisheye", "planet"), default="flat")
    rf.add_argument("--size", type=_size, default=(1920, 1080))
    rf.add_argument("--yaw", type=float, default=0.0)
    rf.add_argument("--pitch", type=float, default=0.0)
    rf.add_argument("--roll", type=float, default=0.0)
    rf.add_argument("--fov", type=float, default=0.0, help="champ horizontal (°) ; défaut selon la projection")
    rf.add_argument("--keyframes", help="JSON : [{t, yaw, pitch, roll, fov}, ...]")
    rf.set_defaults(fn=cmd_reframe)

    eq = sub.add_parser("equirect", help="exporte la vidéo 360° assemblée (équirectangulaire)")
    _common_export_args(eq)
    eq.add_argument("--width", type=int, default=0, help="largeur (0 = 2 × taille d'un objectif)")
    eq.set_defaults(fn=cmd_equirect)

    sn = sub.add_parser("snapshot", help="enregistre une image (JPEG) de la vue")
    _common_export_args(sn)
    sn.add_argument("--t", type=float, default=0.0, help="instant (s)")
    sn.add_argument("--projection", choices=("flat", "fisheye", "planet"), default="flat")
    sn.add_argument("--size", type=_size, default=(1920, 1080))
    sn.add_argument("--yaw", type=float, default=0.0)
    sn.add_argument("--pitch", type=float, default=0.0)
    sn.add_argument("--roll", type=float, default=0.0)
    sn.add_argument("--fov", type=float, default=0.0)
    sn.set_defaults(fn=cmd_snapshot)

    de = sub.add_parser("demo", help="génère un .insv synthétique pour essayer le logiciel sans caméra")
    de.add_argument("output")
    de.add_argument("--seconds", type=float, default=4.0)
    de.add_argument("--lens", type=int, default=960, help="taille d'un objectif (px)")
    de.add_argument("--motion", choices=("shake", "pan", "static"), default="shake")
    de.set_defaults(fn=cmd_demo)
    return ap


def main(argv: list[str] | None = None) -> int:
    ap = build_parser()
    argv = sys.argv[1:] if argv is None else argv
    if not argv:
        argv = ["gui"]
    elif argv[0] not in {a for s in ap._subparsers._group_actions for a in s.choices} | {"-h", "--help", "--version"}:
        argv = ["gui"] + argv      # `x3studio fichier.insv` ouvre directement l'interface
    args = ap.parse_args(argv)
    try:
        return args.fn(args)
    except (MediaError, RuntimeError, ValueError) as e:
        print(f"Erreur : {e}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
