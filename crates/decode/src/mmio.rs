//! GBA MMIO register names for annotation.
//!
//! The table covers the halfword-mapped registers of the IO block at
//! `0x04000000` (GBATEK 3.06, "IO Register Summary"). Names are exact
//! matches on `address - 0x04000000`; a word access whose address is
//! the lower half of a pair names that pair's first register (for
//! example a word store at `0x040000B8` names `DMA0CNT`).

/// Every named IO register offset, in offset order.
pub const IO_NAMES: &[(u16, &str)] = &[
    (0x000, "DISPCNT"),
    (0x002, "GREENSWAP"),
    (0x004, "DISPSTAT"),
    (0x006, "VCOUNT"),
    (0x008, "BG0CNT"),
    (0x00A, "BG1CNT"),
    (0x00C, "BG2CNT"),
    (0x00E, "BG3CNT"),
    (0x010, "BG0HOFS"),
    (0x012, "BG0VOFS"),
    (0x014, "BG1HOFS"),
    (0x016, "BG1VOFS"),
    (0x018, "BG2HOFS"),
    (0x01A, "BG2VOFS"),
    (0x01C, "BG3HOFS"),
    (0x01E, "BG3VOFS"),
    (0x020, "BG2PA"),
    (0x022, "BG2PB"),
    (0x024, "BG2PC"),
    (0x026, "BG2PD"),
    (0x028, "BG2X"),
    (0x02C, "BG2Y"),
    (0x030, "BG3PA"),
    (0x032, "BG3PB"),
    (0x034, "BG3PC"),
    (0x036, "BG3PD"),
    (0x038, "BG3X"),
    (0x03C, "BG3Y"),
    (0x040, "WIN0H"),
    (0x042, "WIN1H"),
    (0x044, "WIN0V"),
    (0x046, "WIN1V"),
    (0x048, "WININ"),
    (0x04A, "WINOUT"),
    (0x04C, "MOSAIC"),
    (0x050, "BLDCNT"),
    (0x052, "BLDALPHA"),
    (0x054, "BLDY"),
    (0x060, "SOUND1CNT_L"),
    (0x062, "SOUND1CNT_H"),
    (0x064, "SOUND1CNT_X"),
    (0x068, "SOUND2CNT_L"),
    (0x06C, "SOUND2CNT_H"),
    (0x070, "SOUND3CNT_L"),
    (0x072, "SOUND3CNT_H"),
    (0x074, "SOUND3CNT_X"),
    (0x078, "SOUND4CNT_L"),
    (0x07C, "SOUND4CNT_H"),
    (0x080, "SOUNDCNT_L"),
    (0x082, "SOUNDCNT_H"),
    (0x084, "SOUNDCNT_X"),
    (0x088, "SOUNDBIAS"),
    (0x0A0, "FIFO_A"),
    (0x0A4, "FIFO_B"),
    (0x0B0, "DMA0SAD"),
    (0x0B4, "DMA0DAD"),
    (0x0B8, "DMA0CNT"),
    (0x0BC, "DMA1SAD"),
    (0x0C0, "DMA1DAD"),
    (0x0C4, "DMA1CNT"),
    (0x0C8, "DMA2SAD"),
    (0x0CC, "DMA2DAD"),
    (0x0D0, "DMA2CNT"),
    (0x0D4, "DMA3SAD"),
    (0x0D8, "DMA3DAD"),
    (0x0DC, "DMA3CNT"),
    (0x100, "TM0CNT_L"),
    (0x102, "TM0CNT_H"),
    (0x104, "TM1CNT_L"),
    (0x106, "TM1CNT_H"),
    (0x108, "TM2CNT_L"),
    (0x10A, "TM2CNT_H"),
    (0x10C, "TM3CNT_L"),
    (0x10E, "TM3CNT_H"),
    (0x120, "SIOCNT"),
    (0x122, "SIODATA32"),
    (0x128, "SIOMULTI0"),
    (0x12A, "SIOMULTI1"),
    (0x130, "KEYINPUT"),
    (0x132, "KEYCNT"),
    (0x200, "IE"),
    (0x202, "IF"),
    (0x204, "WAITCNT"),
    (0x208, "IME"),
];

/// The name of the IO register at `addr`, when `addr` lands in the
/// `0x04000000` block at a named offset.
#[must_use]
pub fn io_name(addr: u32) -> Option<&'static str> {
    let offset = addr.wrapping_sub(0x0400_0000);
    if offset > 0x3FF {
        return None;
    }
    let offset = u16::try_from(offset).ok()?;
    IO_NAMES
        .binary_search_by_key(&offset, |entry| entry.0)
        .ok()
        .map(|index| IO_NAMES[index].1)
}

#[cfg(test)]
mod tests {
    use super::io_name;

    #[test]
    fn mmio_names_exact_offsets_and_out_of_block() {
        assert_eq!(io_name(0x0400_0000), Some("DISPCNT"));
        assert_eq!(io_name(0x0400_0130), Some("KEYINPUT"));
        assert_eq!(io_name(0x0400_0208), Some("IME"));
        assert_eq!(io_name(0x0400_00B8), Some("DMA0CNT"));
        assert_eq!(io_name(0x03FF_FFFE), None);
        assert_eq!(io_name(0x0400_0400), None);
        assert_eq!(io_name(0x0400_0086), None);
    }
}
