// SPDX-License-Identifier: GPL-2.0-only
//! SPI NOR descriptors generated from thingino-dfu (see tools/gen-cloner-data).

use super::ChipInfo;

const DB: &[u8] = include_bytes!("data/chips.bin");
const DESC: usize = 972;
const REC: usize = 32 + 8 + DESC;

/// Returns the chip and its 972-byte erase+write descriptor.
pub fn find(jedec: u32) -> Option<(ChipInfo, Vec<u8>)> {
    DB.chunks_exact(REC).find_map(|r| {
        let id = u32::from_le_bytes(r[32..36].try_into().unwrap());
        if id != jedec {
            return None;
        }
        let name = String::from_utf8_lossy(r[..32].split(|&b| b == 0).next().unwrap()).into_owned();
        let size = u32::from_le_bytes(r[36..40].try_into().unwrap()) as usize;
        Some((ChipInfo { name, jedec: id, size }, r[40..].to_vec()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn k1_chip_is_known() {
        let (chip, desc) = find(0x204017).expect("XM25QH64C");
        assert_eq!(chip.size, 8 * 1024 * 1024);
        assert_eq!(desc.len(), DESC);
        assert_eq!(&desc[..4], b"\0GBD");
        assert_eq!(desc[0xc8 + 0x14], 1, "write descriptor erases");
    }
}
