//! CRC-32 (reflected, polynomial 0xEDB88320 — the standard IEEE/zlib CRC-32)
//! for TTA format validation.

/// Build the CRC-32 table at first use. The table is small and the build is
/// branch-free; a `OnceLock` keeps the hot path allocation-free.
fn crc32_table() -> &'static [u32; 256] {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (i, entry) in table.iter_mut().enumerate() {
            let mut crc = i as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
            *entry = crc;
        }
        table
    })
}

/// Standard CRC-32 (init 0xFFFFFFFF, reflected, final XOR) over `data`.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let table = crc32_table();
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = (crc >> 8) ^ table[((crc ^ b as u32) & 0xFF) as usize];
    }
    crc ^ 0xFFFF_FFFF
}
