import shutil
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent))
sys.path.insert(0, str(Path(__file__).parent))

if not shutil.which("ffmpeg"):  # pragma: no cover
    pytest.skip("ffmpeg est requis", allow_module_level=True)

from x3studio.app import App  # noqa: E402
from x3studio.synth import make_synthetic_insv, make_texture  # noqa: E402

LENS = 480


@pytest.fixture(scope="session")
def work(tmp_path_factory):
    return tmp_path_factory.mktemp("x3media")


@pytest.fixture(scope="session")
def texture():
    return make_texture(width=1024, seed=3)


@pytest.fixture(scope="session")
def static_insv(work, texture):
    p = work / "static.insv"
    make_synthetic_insv(p, seconds=1.5, fps=30, lens=LENS, motion="static", texture=texture)
    return p


@pytest.fixture(scope="session")
def shake_insv(work, texture):
    """(chemin, orientations vraies caméra->monde) : 2,5 s de secousses connues."""
    p = work / "shake.insv"
    mats = make_synthetic_insv(p, seconds=2.5, fps=30, lens=LENS, motion="shake", texture=texture)
    return p, mats


@pytest.fixture
def app(tmp_path):
    return App(home=tmp_path / "home", output_dir=tmp_path / "out")
