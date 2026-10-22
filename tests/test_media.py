import subprocess

import pytest

from x3studio.media import MediaError, probe_source


def ff(*args):
    subprocess.run(["ffmpeg", "-v", "error", "-y", *map(str, args)], check=True)


def test_probe_two_streams(static_insv):
    s = probe_source(static_insv)
    assert s.layout == "two_streams"
    assert s.lens_size == (480, 480)
    assert s.fps == pytest.approx(30)
    assert s.duration == pytest.approx(1.5, abs=0.05)
    assert s.has_audio and not s.has_trailer
    assert s.lens_streams == [0, 1]
    assert s.frames == 45
    assert type(s).from_dict(s.to_dict()) == s


def test_probe_equirect(tmp_path):
    p = tmp_path / "pano.mp4"
    ff("-f", "lavfi", "-i", "testsrc2=size=1024x512:rate=25:duration=1", "-pix_fmt", "yuv420p", p)
    s = probe_source(p)
    assert s.layout == "equirect" and s.lens_size == (1024, 512) and not s.has_audio


def test_probe_side_by_side(static_insv, tmp_path):
    p = tmp_path / "sbs.insv"
    ff("-i", static_insv, "-filter_complex", "[0:v:0][0:v:1]hstack=inputs=2[v]", "-map", "[v]",
       "-c:v", "libx264", "-f", "mp4", p)
    s = probe_source(p)
    assert s.layout == "sbs_fisheye" and s.lens_size == (480, 480)


def test_probe_two_files(static_insv, tmp_path):
    a = tmp_path / "VID_20240101_120000_00_001.insv"
    b = tmp_path / "VID_20240101_120000_10_001.insv"
    ff("-i", static_insv, "-map", "0:v:0", "-c", "copy", "-f", "mp4", a)
    ff("-i", static_insv, "-map", "0:v:1", "-c", "copy", "-f", "mp4", b)
    s = probe_source(a)
    assert s.layout == "two_files" and s.lens_files == [str(a), str(b)]


def test_probe_rejects_unknown(tmp_path):
    p = tmp_path / "x.mp4"
    ff("-f", "lavfi", "-i", "testsrc2=size=640x360:rate=25:duration=1", "-pix_fmt", "yuv420p", p)
    with pytest.raises(MediaError):
        probe_source(p)
    junk = tmp_path / "junk.insv"
    junk.write_bytes(b"not a video")
    with pytest.raises(MediaError):
        probe_source(junk)
    with pytest.raises(MediaError):
        probe_source(tmp_path / "missing.insv")


def test_trailer_detection(static_insv, tmp_path):
    from x3studio.media import INSTA360_TRAILER_MAGIC
    p = tmp_path / "t.insv"
    p.write_bytes(static_insv.read_bytes() + b"\0" * 8 + INSTA360_TRAILER_MAGIC)
    assert probe_source(p).has_trailer
