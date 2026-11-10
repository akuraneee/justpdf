//! Book covers: the first page of a document rendered small, cached on disk
//! (deflated RGB) so the library shows them instantly on the next launch.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Width in px covers are rendered at.
pub const COVER_W: u32 = 300;
/// Covers are at most this tall (tall strips are cropped to their top).
pub const COVER_MAX_H: u32 = COVER_W * 3 / 2;
/// Cached covers kept on disk; the oldest go first.
const DISK_MAX: usize = 600;

const MAGIC: &[u8; 4] = b"JPC1";

/// Stable cache key of a document URI (FNV-1a).
pub fn key(uri: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in uri.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Cover geometry for a first page of `pw` x `ph` points: the page scaled to
/// `COVER_W` wide (`page_w` x `page_h`) and the rendered height, which keeps
/// only the top of very tall pages.
pub fn dims(pw: f32, ph: f32) -> (u32, u32, u32) {
    let page_h = ((COVER_W as f32 * ph / pw.max(1.0)).round() as u32).clamp(1, 1 << 15);
    (COVER_W, page_h, page_h.min(COVER_MAX_H))
}

pub fn path(dir: &Path, key: u64) -> PathBuf {
    dir.join(format!("{key:016x}.cov"))
}

/// Keys of every cover in `dir`.
pub fn index(dir: &Path) -> HashSet<u64> {
    let Ok(rd) = std::fs::read_dir(dir) else { return HashSet::new() };
    rd.filter_map(|e| {
        let name = e.ok()?.file_name();
        let name = name.to_str()?;
        u64::from_str_radix(name.strip_suffix(".cov")?, 16).ok()
    })
    .collect()
}

/// Encodes RGBX pixels as the cache format: magic, w, h (u16 LE), deflated RGB.
pub fn encode(w: u32, h: u32, rgbx: &[u8]) -> Vec<u8> {
    let rgb: Vec<u8> = rgbx.as_chunks::<4>().0.iter().flat_map(|&[r, g, b, _]| [r, g, b]).collect();
    let mut out = Vec::with_capacity(8 + rgb.len() / 3);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(w as u16).to_le_bytes());
    out.extend_from_slice(&(h as u16).to_le_bytes());
    out.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(&rgb, 3));
    out
}

/// Decodes the cache format back to (w, h, RGBA).
pub fn decode(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if data.len() < 8 || &data[..4] != MAGIC {
        return None;
    }
    let w = u16::from_le_bytes([data[4], data[5]]) as u32;
    let h = u16::from_le_bytes([data[6], data[7]]) as u32;
    let n = w as usize * h as usize * 3;
    if w == 0 || h == 0 || w > 4096 || h > 4096 {
        return None;
    }
    let rgb = miniz_oxide::inflate::decompress_to_vec_with_limit(&data[8..], n).ok()?;
    if rgb.len() != n {
        return None;
    }
    Some((w, h, rgb.as_chunks::<3>().0.iter().flat_map(|&[r, g, b]| [r, g, b, 255]).collect()))
}

pub fn load(dir: &Path, key: u64) -> Option<(u32, u32, Vec<u8>)> {
    decode(&std::fs::read(path(dir, key)).ok()?)
}

pub fn save(dir: &Path, key: u64, w: u32, h: u32, rgbx: &[u8]) {
    let _ = std::fs::create_dir_all(dir);
    let p = path(dir, key);
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, encode(w, h, rgbx)).and_then(|_| std::fs::rename(&tmp, &p)).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Deletes the least recently written covers beyond the cache limit.
pub fn prune(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .filter_map(|e| {
            let e = e.ok()?;
            let m = e.metadata().ok()?.modified().ok()?;
            Some((m, e.path()))
        })
        .collect();
    if files.len() <= DISK_MAX {
        return;
    }
    files.sort();
    for (_, p) in &files[..files.len() - DISK_MAX] {
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let (w, h) = (7, 5);
        let px: Vec<u8> = (0..w * h * 4).map(|i| if i % 4 == 3 { 0 } else { (i * 37 % 251) as u8 }).collect();
        let (dw, dh, out) = decode(&encode(w, h, &px)).unwrap();
        assert_eq!((dw, dh), (w, h));
        for (a, b) in px.chunks(4).zip(out.chunks(4)) {
            assert_eq!(&a[..3], &b[..3]);
            assert_eq!(b[3], 255);
        }
        assert!(decode(b"JPC1\x07\x00\x05\x00garbage").is_none());
        assert!(decode(b"nope").is_none());
    }

    #[test]
    fn sizes() {
        assert_eq!(dims(600.0, 800.0), (300, 400, 400));
        // A webtoon strip: full height is huge, the cover keeps its top.
        assert_eq!(dims(800.0, 6000.0), (300, 2250, COVER_MAX_H));
        assert_eq!(dims(1000.0, 100.0), (300, 30, 30));
        assert_ne!(key("content://a"), key("content://b"));
    }

    #[test]
    fn disk() {
        let dir = std::env::temp_dir().join(format!("justpdf-covers-{}", std::process::id()));
        let px = vec![200u8; 4 * 4 * 4];
        save(&dir, 42, 4, 4, &px);
        assert!(index(&dir).contains(&42));
        assert_eq!(load(&dir, 42).unwrap().0, 4);
        assert!(load(&dir, 43).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
