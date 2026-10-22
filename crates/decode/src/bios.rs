//! GBA BIOS software-interrupt call table.
//!
//! Names and numbers are verbatim from GBATEK 3.06, "BIOS Function
//! Summary", GBA column (03h is `Stop`, per the "BIOS Halt Functions"
//! section heading "SWI 03h (GBA) - Stop"). Numbers 2Bh and up are not
//! BIOS calls: GBATEK lists them as crash entries, so [`swi_name`]
//! returns `None` for them.

/// Every documented GBA BIOS call, in number order.
pub const SWI_NAMES: &[(u8, &str)] = &[
    (0x00, "SoftReset"),
    (0x01, "RegisterRamReset"),
    (0x02, "Halt"),
    (0x03, "Stop"),
    (0x04, "IntrWait"),
    (0x05, "VBlankIntrWait"),
    (0x06, "Div"),
    (0x07, "DivArm"),
    (0x08, "Sqrt"),
    (0x09, "ArcTan"),
    (0x0A, "ArcTan2"),
    (0x0B, "CpuSet"),
    (0x0C, "CpuFastSet"),
    (0x0D, "GetBiosChecksum"),
    (0x0E, "BgAffineSet"),
    (0x0F, "ObjAffineSet"),
    (0x10, "BitUnPack"),
    (0x11, "LZ77UnCompReadNormalWrite8bit"),
    (0x12, "LZ77UnCompReadNormalWrite16bit"),
    (0x13, "HuffUnCompReadNormal"),
    (0x14, "RLUnCompReadNormalWrite8bit"),
    (0x15, "RLUnCompReadNormalWrite16bit"),
    (0x16, "Diff8bitUnFilterWrite8bit"),
    (0x17, "Diff8bitUnFilterWrite16bit"),
    (0x18, "Diff16bitUnFilter"),
    (0x19, "SoundBias"),
    (0x1A, "SoundDriverInit"),
    (0x1B, "SoundDriverMode"),
    (0x1C, "SoundDriverMain"),
    (0x1D, "SoundDriverVSync"),
    (0x1E, "SoundChannelClear"),
    (0x1F, "MidiKey2Freq"),
    (0x20, "SoundWhatever0"),
    (0x21, "SoundWhatever1"),
    (0x22, "SoundWhatever2"),
    (0x23, "SoundWhatever3"),
    (0x24, "SoundWhatever4"),
    (0x25, "MultiBoot"),
    (0x26, "HardReset"),
    (0x27, "CustomHalt"),
    (0x28, "SoundDriverVSyncOff"),
    (0x29, "SoundDriverVSyncOn"),
    (0x2A, "SoundGetJumpList"),
];

/// The GBATEK name of a documented GBA BIOS call, or `None` for
/// numbers the BIOS does not implement (2Bh and up crash).
#[must_use]
pub fn swi_name(number: u8) -> Option<&'static str> {
    SWI_NAMES
        .binary_search_by(|(known, _)| known.cmp(&number))
        .ok()
        .map(|index| SWI_NAMES[index].1)
}
