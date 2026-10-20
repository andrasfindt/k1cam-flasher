// SPDX-License-Identifier: MIT
//! `k1cam.upd`: the A/B update bundle (header and digests checked here; the camera
//! repeats the full validation) and the release information after it, which is
//! for showing only: the camera is sent the bundle alone.

use sha2::{Digest, Sha256};

pub const LAYOUT: &str = "k1cam-ab-8m-v1";

const MANIFEST: usize = 0x180;
const MANIFEST_SHA: usize = 0xc0;
const INFO_MAGIC: &[u8] = b"K1CAMINF";
const INFO_HEADER: usize = 0x50;
const INFO_TEXT_MAX: usize = 4096;

pub struct Bundle {
    /// The bundle without the release information: what the camera gets.
    pub data: Vec<u8>,
    pub board: String,
    pub layout: String,
    pub build: String,
    pub min_boot: u32,
    pub kernel_len: u32,
    pub rootfs_len: u32,
    pub sha256: [u8; 32],
    /// The release's os-release, from files that carry it.
    pub release: Option<String>,
}

impl Bundle {
    /// An os-release field, unquoted.
    #[cfg(any(feature = "gui", test))]
    pub fn release_value(&self, key: &str) -> Option<&str> {
        let v = crate::xu::record_value(self.release.as_deref()?, key)?;
        Some(v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(v))
    }
}

fn cstr(b: &[u8]) -> String {
    String::from_utf8_lossy(b.split(|&c| c == 0).next().unwrap_or(&[])).into_owned()
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn release_info(file: &[u8], end: usize) -> Result<String, String> {
    let info = &file[end..];
    if info.len() < INFO_HEADER || &info[..8] != INFO_MAGIC {
        return Err(format!("{} bytes after the bundle are not release information", info.len()));
    }
    let n = le32(info, 8) as usize;
    if le32(info, 12) != 0 || n != info.len() - INFO_HEADER {
        return Err("malformed release information header".into());
    }
    if info[0x10..0x30] != file[MANIFEST_SHA..MANIFEST_SHA + 32] {
        return Err("the release information does not match the bundle's manifest digest".into());
    }
    let text = &info[INFO_HEADER..];
    if Sha256::digest(text)[..] != info[0x30..0x50] {
        return Err("release information digest mismatch: the file is damaged".into());
    }
    if n == 0 || n > INFO_TEXT_MAX || !text.iter().all(|&c| (0x20..0x7f).contains(&c) || c == b'\t' || c == b'\n') {
        return Err("the release information is not 1 to 4096 bytes of printable text".into());
    }
    Ok(String::from_utf8_lossy(text).into_owned())
}

/// Reads and checks a .upd file (refusing anything too big to be one first).
pub fn load(path: &std::path::Path) -> Result<Bundle, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.len() > crate::ops::FLASH_SIZE as u64 {
        return Err(format!("{}: too large to be a k1cam update bundle", path.display()));
    }
    parse(std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?)
}

pub fn parse(mut data: Vec<u8>) -> Result<Bundle, String> {
    if data.len() < MANIFEST || &data[..8] != b"K1CAMUPD" {
        return Err("not a k1cam update bundle (.upd)".into());
    }
    let layout = cstr(&data[0x30..0x50]);
    if layout != LAYOUT {
        return Err(format!("bundle layout is {layout:?}, expected {LAYOUT:?}"));
    }
    let len = le32(&data, 0x98) as usize;
    if len < MANIFEST || len > data.len() {
        return Err(format!("bundle header says {len} bytes but the file has {}", data.len()));
    }
    let release = if len < data.len() { Some(release_info(&data, len)?) } else { None };
    data.truncate(len);

    let mut manifest = data[..MANIFEST].to_vec();
    manifest[MANIFEST_SHA..MANIFEST_SHA + 32].fill(0);
    if Sha256::digest(&manifest)[..] != data[MANIFEST_SHA..MANIFEST_SHA + 32] {
        return Err("bundle manifest digest mismatch: the file is damaged".into());
    }
    let mut lens = [0u32; 2];
    for (i, name) in ["kernel", "rootfs"].into_iter().enumerate() {
        let r = 0x100 + i * 64;
        let (off, n) = (le32(&data, r + 4) as usize, le32(&data, r + 8) as usize);
        if off < MANIFEST || off + n > len {
            return Err(format!("the {name} lies outside the bundle"));
        }
        if Sha256::digest(&data[off..off + n])[..] != data[r + 16..r + 48] {
            return Err(format!("{name} digest mismatch: the file is damaged"));
        }
        lens[i] = n as u32;
    }
    Ok(Bundle {
        board: cstr(&data[0x10..0x30]),
        layout,
        build: cstr(&data[0x50..0x90]),
        min_boot: le32(&data, 0x90),
        kernel_len: lens[0],
        rootfs_len: lens[1],
        sha256: Sha256::digest(&data).into(),
        release,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// k1cam-mkupd's output for the firmware tests' golden bundle
    /// (tests/bundle_check.py --golden) with release information.
    const GOLDEN: &[u8] = include_bytes!("../tests/data/golden.upd");
    const GOLDEN_TEXT: &str = "NAME=k1cam\nBUILD_ID=\"k1cam+test, 2026-01-01 00:00:00 +0000\"\n";

    fn hex(b: &[u8]) -> String {
        crate::xu::hex(b)
    }

    #[test]
    fn golden_vectors() {
        assert_eq!(hex(&Sha256::digest(GOLDEN)), "d0feccc486f3f9cc2542e15680f81652c79da5f8e7ab0d5ca2514691e5b34c09");
        let b = parse(GOLDEN.to_vec()).unwrap();
        // The bundle part is format v1's frozen golden vector.
        assert_eq!(hex(&b.sha256), "e4dfb5ff4554ce1e031222839b778618ca1ff9c7cdfbc9b507bdbaaac705c442");
        assert_eq!(b.data.len(), GOLDEN.len() - INFO_HEADER - GOLDEN_TEXT.len());
        assert_eq!((b.board.as_str(), b.layout.as_str(), b.build.as_str()), ("k1cam-t31l-gc2083", LAYOUT, "k1cam-test-1"));
        assert_eq!((b.min_boot, b.kernel_len, b.rootfs_len), (1, 5064, 12288));
        assert_eq!(b.release.as_deref(), Some(GOLDEN_TEXT));
        assert_eq!(b.release_value("BUILD_ID"), Some("k1cam+test, 2026-01-01 00:00:00 +0000"));
        assert_eq!(b.release_value("NAME"), Some("k1cam"));
        assert_eq!(b.release_value("ID"), None);
    }

    #[test]
    fn bundle_alone() {
        let plain = GOLDEN[..GOLDEN.len() - INFO_HEADER - GOLDEN_TEXT.len()].to_vec();
        let b = parse(plain).unwrap();
        assert!(b.release.is_none());
        assert_eq!(hex(&b.sha256), "e4dfb5ff4554ce1e031222839b778618ca1ff9c7cdfbc9b507bdbaaac705c442");
    }

    #[test]
    fn damage_is_refused() {
        let info = GOLDEN.len() - INFO_HEADER - GOLDEN_TEXT.len();
        let refused = |at: usize, want: &str| {
            let mut f = GOLDEN.to_vec();
            f[at] ^= 1;
            let e = parse(f).err().unwrap_or_default();
            assert!(e.contains(want), "byte {at:#x}: {e:?}, want {want:?}");
        };
        refused(0x50, "manifest digest"); // build id
        refused(0x180 + 100, "kernel digest");
        refused(info - 1, "rootfs digest");
        refused(info, "not release information"); // magic
        refused(info + 12, "header"); // flags
        refused(info + 0x10, "manifest digest"); // another bundle's
        refused(MANIFEST_SHA, "manifest digest"); // or a damaged one
        refused(info + INFO_HEADER + 3, "release information digest");
        for cut in [info + 1, info + INFO_HEADER, GOLDEN.len() - 1] {
            assert!(parse(GOLDEN[..cut].to_vec()).is_err(), "cut at {cut}");
        }
        let mut longer = GOLDEN.to_vec();
        longer.push(b'\n');
        assert!(parse(longer).is_err());
    }
}
