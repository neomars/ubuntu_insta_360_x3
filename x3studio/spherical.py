"""Ajout des métadonnées « vidéo sphérique » (Google Spherical Video V1) dans un MP4.

Les lecteurs (YouTube, VLC, Google Photos, casques VR...) reconnaissent ainsi une
vidéo équirectangulaire 360°. La boîte ``uuid`` est ajoutée à la piste vidéo ;
cela exige que ``moov`` soit la dernière boîte du fichier (pas de ``faststart``),
ce qui évite de recalculer les offsets de ``mdat``.
"""
from __future__ import annotations

import struct
from pathlib import Path

SPHERICAL_UUID = bytes.fromhex("ffcc8263f8554a938814587a02521fdd")

XML = (
    '<?xml version="1.0"?>'
    '<rdf:SphericalVideo xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" '
    'xmlns:GSpherical="http://ns.google.com/videos/1.0/spherical/">'
    "<GSpherical:Spherical>true</GSpherical:Spherical>"
    "<GSpherical:Stitched>true</GSpherical:Stitched>"
    "<GSpherical:StitchingSoftware>X3 Studio</GSpherical:StitchingSoftware>"
    "<GSpherical:ProjectionType>equirectangular</GSpherical:ProjectionType>"
    "</rdf:SphericalVideo>"
)


def _boxes(data: bytes, start: int, end: int):
    pos = start
    while pos + 8 <= end:
        size, typ = struct.unpack(">I4s", data[pos:pos + 8])
        hdr = 8
        if size == 1:
            size = struct.unpack(">Q", data[pos + 8:pos + 16])[0]
            hdr = 16
        elif size == 0:
            size = end - pos
        if size < hdr or pos + size > end:
            raise ValueError("boîte MP4 invalide")
        yield pos, size, typ, hdr
        pos += size


def inject_spherical_v1(path: str | Path) -> bool:
    """Retourne True si les métadonnées ont été ajoutées."""
    path = Path(path)
    data = path.read_bytes() if path.stat().st_size < (1 << 31) else None
    if data is None:  # gros fichier : on ne lit que la fin (moov)
        return _inject_large(path)
    top = list(_boxes(data, 0, len(data)))
    if not top or top[-1][2] != b"moov":
        return False
    mpos, msize, _, mhdr = top[-1]
    if mhdr != 8:
        return False
    for tpos, tsize, ttyp, thdr in _boxes(data, mpos + mhdr, mpos + msize):
        if ttyp != b"trak" or thdr != 8:
            continue
        if b"vide" not in data[tpos:tpos + tsize][:400]:
            continue
        payload = SPHERICAL_UUID + XML.encode()
        box = struct.pack(">I4s", 8 + len(payload), b"uuid") + payload
        tend = tpos + tsize
        new = bytearray(data[:tend] + box + data[tend:])
        struct.pack_into(">I", new, tpos, tsize + len(box))
        struct.pack_into(">I", new, mpos, msize + len(box))
        tmp = path.with_suffix(path.suffix + ".tmp")
        tmp.write_bytes(bytes(new))
        tmp.replace(path)
        return True
    return False


def _inject_large(path: Path) -> bool:  # pragma: no cover - fichiers > 2 Gio
    """Variante pour les gros fichiers : seul moov (en fin de fichier) est réécrit."""
    with open(path, "rb") as f:
        f.seek(0, 2)
        total = f.tell()
        pos = 0
        moov = None
        while pos + 8 <= total:
            f.seek(pos)
            size, typ = struct.unpack(">I4s", f.read(8))
            if size == 1:
                size = struct.unpack(">Q", f.read(8))[0]
            elif size == 0:
                size = total - pos
            if typ == b"moov":
                moov = (pos, size)
            pos += size
        if not moov or moov[0] + moov[1] != total:
            return False
        f.seek(moov[0])
        mdata = f.read(moov[1])
    top = list(_boxes(mdata, 0, len(mdata)))
    mhdr = top[0][3]
    for tpos, tsize, ttyp, thdr in _boxes(mdata, mhdr, len(mdata)):
        if ttyp == b"trak" and b"vide" in mdata[tpos:tpos + tsize][:400]:
            payload = SPHERICAL_UUID + XML.encode()
            box = struct.pack(">I4s", 8 + len(payload), b"uuid") + payload
            tend = tpos + tsize
            new = bytearray(mdata[:tend] + box + mdata[tend:])
            struct.pack_into(">I", new, tpos, tsize + len(box))
            struct.pack_into(">I", new, 0, len(mdata) + len(box))
            with open(path, "r+b") as f:
                f.seek(moov[0])
                f.write(bytes(new))
            return True
    return False


def has_spherical_v1(path: str | Path) -> bool:
    data = Path(path).read_bytes()
    return SPHERICAL_UUID in data
