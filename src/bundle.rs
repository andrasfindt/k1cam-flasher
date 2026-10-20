// SPDX-License-Identifier: MIT
//! `.upd` A/B update bundle header checks (full validation is repeated by the camera).

use sha2::{Digest, Sha256};

pub const LAYOUT: &str = "k1cam-ab-8m-v1";

pub struct Bundle {
    pub data: Vec<u8>,
    pub build: String,
    pub min_boot: u32,
    pub sha256: [u8; 32],
}

fn cstr(b: &[u8]) -> String {
    String::from_utf8_lossy(b.split(|&c| c == 0).next().unwrap_or(&[])).into_owned()
}

pub fn parse(data: Vec<u8>) -> Result<Bundle, String> {
    if data.len() < 0x100 + 128 || &data[..8] != b"K1CAMUPD" {
        return Err("not a k1cam update bundle (.upd)".into());
    }
    let layout = cstr(&data[0x30..0x50]);
    if layout != LAYOUT {
        return Err(format!("bundle layout is {layout:?}, expected {LAYOUT:?}"));
    }
    let len = u32::from_le_bytes(data[0x98..0x9c].try_into().unwrap()) as usize;
    if len != data.len() {
        return Err(format!("bundle header says {len} bytes but the file has {}", data.len()));
    }
    let build = cstr(&data[0x50..0x90]);
    let min_boot = u32::from_le_bytes(data[0x90..0x94].try_into().unwrap());
    let sha256 = Sha256::digest(&data).into();
    Ok(Bundle { data, build, min_boot, sha256 })
}
