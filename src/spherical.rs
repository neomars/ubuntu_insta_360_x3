//! Métadonnées « vidéo sphérique » (Google Spherical Video V1) dans un MP4 : boîte `uuid` ajoutée à
//! la piste vidéo ; exige que `moov` soit la dernière boîte (pas de `faststart`). Seule la fin du
//! fichier (`moov`) est lue/réécrite, quelle que soit la taille du fichier. Spec : `x3studio/spherical.py`.
//!
//! Les lecteurs (YouTube, VLC, Google Photos, casques VR...) reconnaissent ainsi une vidéo
//! équirectangulaire 360°. En cas de doute (structure inattendue), le fichier n'est pas touché.

use crate::error::Result;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// UUID de la boîte (`ffcc8263-f855-4a93-8814-587a02521fdd`).
pub const SPHERICAL_UUID: [u8; 16] =
    [0xff, 0xcc, 0x82, 0x63, 0xf8, 0x55, 0x4a, 0x93, 0x88, 0x14, 0x58, 0x7a, 0x02, 0x52, 0x1f, 0xdd];

const XML: &str = concat!(
    r#"<?xml version="1.0"?>"#,
    r#"<rdf:SphericalVideo xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" "#,
    r#"xmlns:GSpherical="http://ns.google.com/videos/1.0/spherical/">"#,
    "<GSpherical:Spherical>true</GSpherical:Spherical>",
    "<GSpherical:Stitched>true</GSpherical:Stitched>",
    "<GSpherical:StitchingSoftware>X3 Studio</GSpherical:StitchingSoftware>",
    "<GSpherical:ProjectionType>equirectangular</GSpherical:ProjectionType>",
    "</rdf:SphericalVideo>"
);

/// moov plus gros que cela : on ne tente rien.
const MAX_MOOV: u64 = 1 << 30;

/// Une boîte MP4 : position de début, taille totale, type, taille de l'en-tête (8 ou 16).
#[derive(Debug, Clone, Copy)]
struct BoxInfo {
    pos: usize,
    size: usize,
    typ: [u8; 4],
    header: usize,
}

/// Boîtes consécutives de `data[start..end]` ; `None` si la structure est invalide.
fn boxes(data: &[u8], start: usize, end: usize) -> Option<Vec<BoxInfo>> {
    let mut out = vec![];
    let mut pos = start;
    while pos + 8 <= end {
        let size32 = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        let typ: [u8; 4] = data[pos + 4..pos + 8].try_into().ok()?;
        let (size, header) = match size32 {
            1 => {
                if pos + 16 > end {
                    return None;
                }
                (usize::try_from(u64::from_be_bytes(data[pos + 8..pos + 16].try_into().ok()?)).ok()?, 16)
            }
            0 => (end - pos, 8),
            n => (n, 8),
        };
        if size < header || pos.checked_add(size)? > end {
            return None;
        }
        out.push(BoxInfo { pos, size, typ, header });
        pos += size;
    }
    Some(out)
}

/// Vrai si cette piste (`trak`) est une piste vidéo : `mdia/hdlr` de type `vide`.
fn is_video_trak(data: &[u8], trak: &BoxInfo) -> bool {
    let Some(children) = boxes(data, trak.pos + trak.header, trak.pos + trak.size) else { return false };
    let Some(mdia) = children.iter().find(|b| &b.typ == b"mdia") else { return false };
    let Some(inner) = boxes(data, mdia.pos + mdia.header, mdia.pos + mdia.size) else { return false };
    inner.iter().find(|b| &b.typ == b"hdlr").is_some_and(|h| {
        // version+flags (4), pre_defined (4), handler_type (4)
        let p = h.pos + h.header;
        h.size >= h.header + 12 && &data[p + 8..p + 12] == b"vide"
    })
}

fn has_spherical_box(data: &[u8], trak: &BoxInfo) -> bool {
    boxes(data, trak.pos + trak.header, trak.pos + trak.size).is_some_and(|children| {
        children.iter().any(|b| {
            &b.typ == b"uuid"
                && b.size >= b.header + 16
                && data[b.pos + b.header..b.pos + b.header + 16] == SPHERICAL_UUID
        })
    })
}

/// Retourne `Ok(true)` si les métadonnées sont présentes à la fin (ajoutées, ou déjà là), `Ok(false)`
/// si le fichier ne s'y prête pas (structure inattendue, `moov` pas en dernier...) : il n'est alors pas modifié.
pub fn inject_spherical_v1(path: &Path) -> Result<bool> {
    let mut f = OpenOptions::new().read(true).write(true).open(path)?;
    let total = f.seek(SeekFrom::End(0))?;

    // 1. boîtes de premier niveau : on cherche moov (qui doit finir le fichier)
    let mut pos = 0u64;
    let mut moov: Option<(u64, u64)> = None;
    while pos + 8 <= total {
        f.seek(SeekFrom::Start(pos))?;
        let mut head = [0u8; 16];
        let n = f.read(&mut head)?;
        if n < 8 {
            return Ok(false);
        }
        let mut size = u32::from_be_bytes(head[0..4].try_into()?) as u64;
        let typ: [u8; 4] = head[4..8].try_into()?;
        if size == 1 {
            if n < 16 {
                return Ok(false);
            }
            size = u64::from_be_bytes(head[8..16].try_into()?);
        } else if size == 0 {
            size = total - pos;
        }
        if size < 8 {
            return Ok(false);
        }
        if &typ == b"moov" {
            moov = Some((pos, size));
        }
        pos = match pos.checked_add(size) {
            Some(p) => p,
            None => return Ok(false),
        };
    }
    let Some((moov_pos, moov_size)) = moov else { return Ok(false) };
    if moov_pos + moov_size != total || moov_size > MAX_MOOV {
        return Ok(false);
    }

    // 2. on lit seulement moov
    f.seek(SeekFrom::Start(moov_pos))?;
    let mut mdata = vec![0u8; moov_size as usize];
    f.read_exact(&mut mdata)?;
    let Some(top) = boxes(&mdata, 0, mdata.len()) else { return Ok(false) };
    let Some(head) = top.first() else { return Ok(false) };
    if head.header != 8 || head.size != mdata.len() {
        return Ok(false);
    }
    let Some(children) = boxes(&mdata, head.header, mdata.len()) else { return Ok(false) };
    let Some(trak) = children.iter().find(|b| &b.typ == b"trak" && b.header == 8 && is_video_trak(&mdata, b)) else {
        return Ok(false);
    };
    if has_spherical_box(&mdata, trak) {
        return Ok(true);
    }

    // 3. boîte uuid ajoutée à la fin de la piste vidéo ; tailles de trak et de moov mises à jour
    let mut payload = SPHERICAL_UUID.to_vec();
    payload.extend_from_slice(XML.as_bytes());
    let mut uuid_box = Vec::with_capacity(8 + payload.len());
    uuid_box.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    uuid_box.extend_from_slice(b"uuid");
    uuid_box.extend_from_slice(&payload);
    let (Ok(new_trak), Ok(new_moov)) =
        (u32::try_from(trak.size + uuid_box.len()), u32::try_from(mdata.len() + uuid_box.len()))
    else {
        return Ok(false);
    };
    let trak_end = trak.pos + trak.size;
    let mut out = Vec::with_capacity(mdata.len() + uuid_box.len());
    out.extend_from_slice(&mdata[..trak_end]);
    out.extend_from_slice(&uuid_box);
    out.extend_from_slice(&mdata[trak_end..]);
    out[trak.pos..trak.pos + 4].copy_from_slice(&new_trak.to_be_bytes());
    out[0..4].copy_from_slice(&new_moov.to_be_bytes());
    f.seek(SeekFrom::Start(moov_pos))?;
    f.write_all(&out)?;
    f.flush()?;
    Ok(true)
}

/// Vrai si la boîte `uuid` sphérique est présente (lit la fin du fichier, où se trouve `moov`).
pub fn has_spherical_v1(path: &Path) -> bool {
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let Ok(n) = f.seek(SeekFrom::End(0)) else { return false };
    let tail = 8u64 << 20;
    if f.seek(SeekFrom::Start(n.saturating_sub(tail))).is_err() {
        return false;
    }
    let mut buf = vec![];
    if f.take(tail).read_to_end(&mut buf).is_err() {
        return false;
    }
    let first = SPHERICAL_UUID[0];
    buf.iter().enumerate().any(|(i, &b)| b == first && buf.len() - i >= 16 && buf[i..i + 16] == SPHERICAL_UUID)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((8 + payload.len()) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(typ);
        v.extend_from_slice(payload);
        v
    }

    fn hdlr(kind: &[u8; 4]) -> Vec<u8> {
        let mut p = vec![0u8; 4 + 4];
        p.extend_from_slice(kind);
        p.extend_from_slice(&[0u8; 12]);
        p.extend_from_slice(b"Handler\0");
        bx(b"hdlr", &p)
    }

    fn trak(kind: &[u8; 4]) -> Vec<u8> {
        let mdia = bx(b"mdia", &[bx(b"mdhd", &[0u8; 24]), hdlr(kind)].concat());
        bx(b"trak", &[bx(b"tkhd", &[0u8; 84]), mdia].concat())
    }

    /// ftyp + mdat + moov(mvhd, trak audio, trak vidéo)
    fn mp4(moov_last: bool) -> Vec<u8> {
        let moov = bx(b"moov", &[bx(b"mvhd", &[0u8; 100]), trak(b"soun"), trak(b"vide")].concat());
        let (ftyp, mdat) = (bx(b"ftyp", b"isom\0\0\x02\0isomiso2"), bx(b"mdat", &[7u8; 1000]));
        if moov_last {
            [ftyp, mdat, moov].concat()
        } else {
            [ftyp, moov, mdat].concat()
        }
    }

    #[test]
    fn injects_into_the_video_track_only_and_keeps_sizes_consistent() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.mp4");
        let original = mp4(true);
        std::fs::write(&p, &original).unwrap();
        assert!(!has_spherical_v1(&p));
        assert!(inject_spherical_v1(&p).unwrap());
        assert!(has_spherical_v1(&p));
        let data = std::fs::read(&p).unwrap();
        assert!(data.len() > original.len());
        // structure : ftyp, mdat, moov — et moov couvre exactement la fin du fichier
        let top = boxes(&data, 0, data.len()).expect("structure valide");
        assert_eq!(top.iter().map(|b| b.typ).collect::<Vec<_>>(), vec![*b"ftyp", *b"mdat", *b"moov"]);
        let moov = top[2];
        assert_eq!(moov.pos + moov.size, data.len());
        let kids = boxes(&data, moov.pos + moov.header, data.len()).unwrap();
        let traks: Vec<&BoxInfo> = kids.iter().filter(|b| &b.typ == b"trak").collect();
        assert_eq!(traks.len(), 2);
        assert!(!has_spherical_box(&data, traks[0]), "la piste audio reste intacte");
        assert!(is_video_trak(&data, traks[1]) && has_spherical_box(&data, traks[1]));
        // le contenu XML est bien là
        let text = String::from_utf8_lossy(&data);
        assert!(text.contains("<GSpherical:ProjectionType>equirectangular</GSpherical:ProjectionType>"));
        // les octets avant moov sont identiques
        assert_eq!(&data[..moov.pos], &original[..moov.pos]);
    }

    #[test]
    fn is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.mp4");
        std::fs::write(&p, mp4(true)).unwrap();
        assert!(inject_spherical_v1(&p).unwrap());
        let once = std::fs::read(&p).unwrap();
        assert!(inject_spherical_v1(&p).unwrap());
        assert_eq!(std::fs::read(&p).unwrap(), once);
    }

    #[test]
    fn refuses_without_touching_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.mp4");
        // moov pas en dernier (faststart)
        let data = mp4(false);
        std::fs::write(&p, &data).unwrap();
        assert!(!inject_spherical_v1(&p).unwrap());
        assert_eq!(std::fs::read(&p).unwrap(), data);
        // pas de piste vidéo
        let moov = bx(b"moov", &[bx(b"mvhd", &[0u8; 100]), trak(b"soun")].concat());
        let data = [bx(b"ftyp", b"isom"), bx(b"mdat", &[1u8; 10]), moov].concat();
        std::fs::write(&p, &data).unwrap();
        assert!(!inject_spherical_v1(&p).unwrap());
        assert_eq!(std::fs::read(&p).unwrap(), data);
        // fichiers invalides
        for junk in [vec![], vec![0u8; 5], b"not an mp4 at all, really".to_vec(), vec![0xff; 64]] {
            std::fs::write(&p, &junk).unwrap();
            assert!(!inject_spherical_v1(&p).unwrap());
            assert_eq!(std::fs::read(&p).unwrap(), junk);
            assert!(!has_spherical_v1(&p));
        }
        // moov tronqué (taille annoncée > fichier)
        let mut data = mp4(true);
        data.truncate(data.len() - 20);
        std::fs::write(&p, &data).unwrap();
        assert!(!inject_spherical_v1(&p).unwrap());
        assert!(inject_spherical_v1(&dir.path().join("absent.mp4")).is_err());
    }
}
