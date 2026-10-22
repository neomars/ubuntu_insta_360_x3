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
    """Retourne True si les métadonnées ont été ajoutées (seul ``moov``, en fin de fichier, est réécrit)."""
    path = Path(path)
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
            if size < 8:
                return False
            if typ == b"moov":
                moov = (pos, size)
            pos += size
        if not moov or moov[0] + moov[1] != total or moov[1] > (1 << 30):
            return False
        f.seek(moov[0])
        mdata = f.read(moov[1])
    try:
        top = list(_boxes(mdata, 0, len(mdata)))
        mhdr = top[0][3]
        if mhdr != 8:
            return False
        for tpos, tsize, ttyp, thdr in _boxes(mdata, mhdr, len(mdata)):
            if ttyp == b"trak" and thdr == 8 and b"vide" in mdata[tpos:tpos + tsize][:400]:
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
    except ValueError:
        return False
    return False


def has_spherical_v1(path: str | Path) -> bool:
    """Vrai si la boîte ``uuid`` sphérique est présente (lit la fin du fichier, où se trouve ``moov``)."""
    with open(path, "rb") as f:
        f.seek(0, 2)
        n = f.tell()
        f.seek(max(0, n - (8 << 20)))
        return SPHERICAL_UUID in f.read()
