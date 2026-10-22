"""Cohérence des métadonnées de packaging (pyproject, paquet Python, debian/, page de manuel, menu)."""
import re
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).parent.parent

import x3studio  # noqa: E402


def read(rel: str) -> str:
    return (ROOT / rel).read_text(encoding="utf-8")


def test_version_is_consistent_everywhere():
    py = re.search(r'^version = "([^"]+)"', read("pyproject.toml"), re.M).group(1)
    deb = re.match(r"x3studio \(([^)]+)\)", read("debian/changelog")).group(1)
    man = re.search(r'x3studio ([0-9.]+)"', read("packaging/x3studio.1")).group(1)
    assert x3studio.__version__ == py == deb == man


def test_debian_control_essentials():
    c = read("debian/control")
    assert "Architecture: all" in c and "${python3:Depends}" in c
    assert re.search(r"^Depends:.*\n(?:\s+.*\n)*?\s+ffmpeg,", c, re.M)       # FFmpeg est indispensable
    assert "python3-numpy" in c
    assert "python3-opencv" in c.split("Recommends:")[1].split("Description:")[0]   # facultatif
    assert "dh-sequence-python3" in c                                          # sinon ${python3:Depends} reste vide
    assert read("debian/source/format").strip() == "3.0 (native)"
    assert read("debian/changelog").split(";")[0].split()[-1] in {"noble", "jammy", "unstable"}
    first_line = re.search(r"^Description: (.*)$", c, re.M).group(1)
    assert len(first_line) < 80


def test_desktop_entry_matches_installed_files():
    d = dict(l.split("=", 1) for l in read("packaging/x3studio.desktop").splitlines() if "=" in l and not l.startswith("["))
    assert d["Exec"].split()[0] == "x3studio" and d["Icon"] == "x3studio"
    assert (ROOT / "packaging/x3studio.svg").exists()
    inst = read("debian/x3studio.install")
    assert "x3studio.desktop usr/share/applications" in inst and "x3studio.svg usr/share/icons/hicolor/scalable/apps" in inst
    assert d["Categories"].endswith(";")


def test_man_page_documents_every_command():
    from x3studio.cli import build_parser
    man = read("packaging/x3studio.1")
    sub = next(a for a in build_parser()._actions if a.dest == "cmd")
    for name in sub.choices:
        assert f".B {name}" in man, name


def test_pyproject_ships_web_assets_and_entry_point():
    p = read("pyproject.toml")
    assert 'x3studio = "x3studio.cli:main"' in p and 'x3studio = ["web/*"]' in p
    assert all((ROOT / "x3studio/web" / f).exists() for f in ("index.html", "app.js", "viewer.js", "geometry.js", "style.css"))


def test_all_modules_parse_as_python_3_10():
    """Ubuntu 22.04 fournit Python 3.10 : aucune syntaxe plus récente."""
    import ast
    for f in (ROOT / "x3studio").glob("*.py"):
        ast.parse(f.read_text(encoding="utf-8"), filename=str(f), feature_version=(3, 10))
