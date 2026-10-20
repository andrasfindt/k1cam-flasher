// SPDX-License-Identifier: MIT
//! What the GUI and the command line both do: back up and flash the whole NOR
//! (cloner), and dump and update a running camera (A/B).

use crate::bundle::Bundle;
use crate::job::Job;
use crate::{cloner, xu};
use sha2::{Digest, Sha256};
use std::path::Path;

pub const FLASH_SIZE: usize = 8 * 1024 * 1024;

pub fn default_backup_name() -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("k1cam-backup-{t}.bin")
}

pub fn backup(j: &Job, path: &Path) -> Result<String, String> {
    let data = cloner::Cloner::read_flash(j)?;
    std::fs::write(path, &data).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!("{} sha256 {}", path.display(), xu::hex(&Sha256::digest(&data))))
}

pub fn flash(j: &Job, image: &Path, backup_first: bool, verify: bool, reboot: bool) -> Result<String, String> {
    let data = std::fs::read(image).map_err(|e| format!("{}: {e}", image.display()))?;
    if data.len() != FLASH_SIZE {
        return Err(format!("{} must be exactly 8 MiB (got {} bytes)", image.display(), data.len()));
    }
    if backup_first {
        let mut name = image.file_stem().unwrap_or_default().to_os_string();
        name.push(format!("-backup-{}.bin", default_backup_name().trim_start_matches("k1cam-backup-").trim_end_matches(".bin")));
        let path = image.with_file_name(name);
        j.log("Backing up the current flash first");
        j.log(backup(j, &path)?);
    }
    cloner::Cloner::write_flash(j, &data, false)?;
    if verify {
        j.log("Verifying");
        let back = cloner::Cloner::read_flash(j)?;
        if back != data {
            return Err("verification failed: flash contents differ from the image".into());
        }
        j.log("Verified");
    }
    if reboot {
        cloner::Cloner::reboot(j)?;
    }
    Ok("flashed".into())
}

pub fn dump(j: &Job, path: &Path, live: bool) -> Result<String, String> {
    let (data, consistency) = xu::Camera::open()?.dump(j, live)?;
    std::fs::write(path, &data).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!("{} ({consistency}), sha256 {}", path.display(), xu::hex(&Sha256::digest(&data))))
}

pub fn update(j: &Job, b: &Bundle, reboot: bool) -> Result<String, String> {
    j.log(format!("Bundle {} ({} bytes)", b.build, b.data.len()));
    xu::Camera::open()?.upgrade(j, b, reboot)
}
