//! Address routing, mirroring, width rules, and
//! open-bus behavior of the bus, per GBATEK "GBA Memory Map".

use runtime::bus::{BIOS_LATCH_BOOT, Bus, Region, region_of};
use runtime::save::{Save, SaveKind};

/// A bus whose ROM is `rom[i] = i as u8` for 1 KiB.
fn bus_with_rom() -> Bus {
    let rom: Vec<u8> = (0..1024u32).map(|i| i as u8).collect();
    Bus::new(rom)
}

#[test]
fn region_classification() {
    let cases = [
        (0x0000_0000, Region::Bios),
        (0x0000_3FFF, Region::Bios),
        (0x0000_4000, Region::OpenBus),
        (0x00FF_FFFF, Region::OpenBus),
        (0x0100_0000, Region::OpenBus),
        (0x0200_0000, Region::Ewram),
        (0x02FF_FFFF, Region::Ewram),
        (0x0300_0000, Region::Iwram),
        (0x03FF_FFFF, Region::Iwram),
        (0x0400_0000, Region::Io),
        (0x0400_03FF, Region::Io),
        (0x0400_0400, Region::OpenBus),
        (0x04FF_FFFF, Region::OpenBus),
        (0x0500_0000, Region::Palette),
        (0x05FF_FFFF, Region::Palette),
        (0x0600_0000, Region::Vram),
        (0x06FF_FFFF, Region::Vram),
        (0x0700_0000, Region::Oam),
        (0x07FF_FFFF, Region::Oam),
        (0x0800_0000, Region::Rom),
        (0x0DFF_FFFF, Region::Rom),
        (0x0E00_0000, Region::Sram),
        (0x0FFF_FFFF, Region::Sram),
        // Top four address bits are unused: the space aliases.
        (0x1200_0000, Region::Ewram),
        (0x1A00_0000, Region::Rom),
        (0xFFFF_FFFF, Region::Sram),
    ];
    for (addr, want) in cases {
        assert_eq!(region_of(addr), want, "region_of({addr:#010x})");
    }
}

#[test]
fn ewram_round_trip_and_mirror() {
    let mut bus = Bus::new(Vec::new());
    bus.write_u8(0x0201_2345, 0x5A);
    assert_eq!(bus.read_u8(0x0201_2345), 0x5A);
    // 256 KiB repeat across the whole 16 MiB window.
    assert_eq!(bus.read_u8(0x0211_2345), 0x5A);
    assert_eq!(bus.read_u8(0x02F1_2345), 0x5A);
    assert_eq!(bus.region_of(0x0201_2345), Region::Ewram);
}

#[test]
fn iwram_round_trip_and_mirror() {
    let mut bus = Bus::new(Vec::new());
    bus.write_u16(0x0300_7FFE, 0xBEEF);
    assert_eq!(bus.read_u16(0x0300_7FFE), 0xBEEF);
    // 32 KiB repeat across the whole 16 MiB window.
    assert_eq!(bus.read_u16(0x0300_FFFE), 0xBEEF);
    assert_eq!(bus.read_u16(0x03F0_7FFE), 0xBEEF);
}

#[test]
fn palette_round_trip_and_mirror() {
    let mut bus = Bus::new(Vec::new());
    bus.write_u16(0x0500_0004, 0x7FFF);
    assert_eq!(bus.read_u16(0x0500_0004), 0x7FFF);
    // 1 KiB repeat across the whole 16 MiB window.
    assert_eq!(bus.read_u16(0x0500_0404), 0x7FFF);
    assert_eq!(bus.read_u16(0x05F0_0404), 0x7FFF);
}

#[test]
fn oam_round_trip_and_mirror() {
    let mut bus = Bus::new(Vec::new());
    bus.write_u16(0x0700_03FC, 0x00C0);
    assert_eq!(bus.read_u16(0x0700_03FC), 0x00C0);
    // 1 KiB repeat across the whole 16 MiB window.
    assert_eq!(bus.read_u16(0x0700_07FC), 0x00C0);
    assert_eq!(bus.read_u16(0x07F0_03FC), 0x00C0);
}

#[test]
fn vram_bg_bank_repeats_every_64k() {
    let mut bus = Bus::new(Vec::new());
    bus.write_u16(0x0600_0010, 0x1234);
    assert_eq!(bus.read_u16(0x0602_0010), 0x1234);
    assert_eq!(bus.read_u16(0x06F0_0010), 0x1234);
}

#[test]
fn vram_obj_bank_repeats_every_32k() {
    let mut bus = Bus::new(Vec::new());
    bus.write_u16(0x0601_0000, 0x5678);
    assert_eq!(bus.read_u16(0x0601_8000), 0x5678);
    bus.write_u16(0x0601_7FFE, 0xAAAA);
    assert_eq!(bus.read_u16(0x0601_FFFE), 0xAAAA);
}

#[test]
fn vram_mirror_quirk_does_not_alias_bg_bank() {
    let mut bus = Bus::new(Vec::new());
    // 0x06018000 must alias 0x06010000 (the OBJ bank), not 0x06008000 in
    // the BG bank, which a naive 96 KiB size mask would produce.
    bus.write_u16(0x0600_8000, 0x1111);
    bus.write_u16(0x0601_8000, 0x2222);
    assert_eq!(bus.read_u16(0x0601_0000), 0x2222);
    assert_eq!(bus.read_u16(0x0600_8000), 0x1111);
}

#[test]
fn rom_windows_mirror_the_image() {
    let mut bus = bus_with_rom();
    assert_eq!(bus.read_u8(0x0800_0000), 0x00);
    assert_eq!(bus.read_u8(0x0800_03FF), 0xFF);
    // Within the 32 MiB segment the image repeats at its own size.
    assert_eq!(bus.read_u8(0x0800_0400), 0x00);
    assert_eq!(bus.read_u8(0x0840_0000), 0x00);
    assert_eq!(bus.read_u8(0x0840_03FF), 0xFF);
    // WS1 and WS2 segments (0x0A000000, 0x0C000000) repeat the same cart.
    assert_eq!(bus.read_u8(0x0A00_0000), 0x00);
    assert_eq!(bus.read_u8(0x0AFF_FFFF), 0xFF);
    assert_eq!(bus.read_u8(0x0C00_0000), 0x00);
    assert_eq!(bus.read_u8(0x0DFF_FFFF), 0xFF);
}

#[test]
fn rom_writes_are_discarded() {
    let mut bus = bus_with_rom();
    bus.write_u8(0x0800_0000, 0xAA);
    bus.write_u16(0x0800_0000, 0xBBBB);
    bus.write_u32(0x0800_0000, 0xCCCC_CCCC);
    assert_eq!(bus.read_u8(0x0800_0000), 0x00);
    assert_eq!(bus.read_u16(0x0800_0000), 0x0100);
    assert_eq!(bus.read_u32(0x0800_0000), 0x0302_0100);
}

#[test]
fn rom_read_beyond_power_of_two_tail_is_open_bus() {
    // 3 KiB image: the tail 0x08000C00..0x08001000 has no chip behind it.
    let rom = vec![7u8; 3072];
    let mut bus = Bus::new(rom);
    assert_eq!(bus.read_u8(0x0800_0BFF), 7);
    assert_eq!(bus.read_u8(0x0800_0C00), 0);
    assert_eq!(bus.read_u8(0x0800_0FFF), 0);
    assert_eq!(bus.read_u8(0x0800_1000), 7);
}

#[test]
fn sram_round_trip_and_mirror() {
    let mut bus = Bus::new(Vec::new());
    *bus.save_mut() = Save::new(SaveKind::Sram);
    bus.write_u8(0x0E00_FFFF, 0xAA);
    assert_eq!(bus.read_u8(0x0E00_FFFF), 0xAA);
    // 64 KiB repeat across the whole 16 MiB window (0x0E000000-0x0FFFFFFF).
    assert_eq!(bus.read_u8(0x0E01_FFFF), 0xAA);
    assert_eq!(bus.read_u8(0x0F00_FFFF), 0xAA);
}

#[test]
fn sram_window_without_a_medium_floats_high() {
    let mut bus = Bus::new(Vec::new());
    assert_eq!(bus.save().kind(), SaveKind::None);
    bus.write_u8(0x0E00_0000, 0xAA);
    assert_eq!(bus.read_u8(0x0E00_0000), 0xFF);
}

#[test]
fn sram_wide_accesses_use_the_single_data_lane() {
    let mut bus = Bus::new(Vec::new());
    *bus.save_mut() = Save::new(SaveKind::Sram);
    // The 8-bit bus answers wide reads with the byte replicated into
    // every lane, and a halfword store writes the one byte the address
    // selects (the external save suite pins both rules).
    bus.write_u8(0x0E00_0000, 0x44);
    assert_eq!(bus.read_u16(0x0E00_0000), 0x4444);
    assert_eq!(bus.read_u32(0x0E00_0000), 0x4444_4444);
    bus.write_u16(0x0E00_0010, 0xBEEF);
    assert_eq!(bus.read_u8(0x0E00_0010), 0xEF);
    assert_eq!(bus.read_u8(0x0E00_0011), 0xFF);
    bus.write_u16(0x0E00_0011, 0xBEEF);
    assert_eq!(bus.read_u8(0x0E00_0011), 0xBE);
    bus.write_u32(0x0E00_0020, 0x1122_3344);
    assert_eq!(bus.read_u8(0x0E00_0020), 0x44);
    assert_eq!(bus.read_u8(0x0E00_0021), 0xFF);
    assert_eq!(bus.read_u8(0x0E00_0022), 0xFF);
    assert_eq!(bus.read_u8(0x0E00_0023), 0xFF);
    bus.write_u32(0x0E00_0023, 0x1122_3344);
    assert_eq!(bus.read_u8(0x0E00_0023), 0x11);
}

#[test]
fn vram_eight_bit_reads_return_the_addressed_byte() {
    let mut bus = Bus::new(Vec::new());
    bus.write_u16(0x0600_0010, 0xABCD);
    assert_eq!(bus.read_u8(0x0600_0010), 0xCD);
    assert_eq!(bus.read_u8(0x0600_0011), 0xAB);
}

#[test]
fn palette_and_oam_eight_bit_accesses() {
    let mut bus = Bus::new(Vec::new());
    // An 8-bit palette write fills both bytes of the halfword; an
    // 8-bit OAM write is ignored (GBATEK).
    bus.write_u8(0x0500_0000, 0x12);
    bus.write_u8(0x0700_0000, 0x12);
    assert_eq!(bus.read_u16(0x0500_0000), 0x1212);
    assert_eq!(bus.read_u16(0x0700_0000), 0);
    // Wide accesses work normally, and 8-bit reads return the
    // addressed byte.
    bus.write_u32(0x0500_0000, 0x7FFF_0321);
    bus.write_u16(0x0700_0000, 0x00C0);
    assert_eq!(bus.read_u32(0x0500_0000), 0x7FFF_0321);
    assert_eq!(bus.read_u16(0x0700_0000), 0x00C0);
    assert_eq!(bus.read_u8(0x0500_0000), 0x21);
    assert_eq!(bus.read_u8(0x0500_0003), 0x7F);
    assert_eq!(bus.read_u8(0x0700_0000), 0xC0);
    assert_eq!(bus.read_u8(0x0700_0001), 0x00);
}

#[test]
fn bios_region_is_write_protected_and_reads_return_the_protection_latch() {
    let mut bus = Bus::new(Vec::new());
    // With no BIOS image, protected reads return the latched BIOS
    // prefetch (GBATEK), lane-selected for narrow accesses.
    assert_eq!(bus.read_u32(0x0000_0000), BIOS_LATCH_BOOT);
    assert_eq!(bus.read_u16(0x0000_0002), (BIOS_LATCH_BOOT >> 16) as u16);
    assert_eq!(bus.read_u8(0x0000_0001), (BIOS_LATCH_BOOT >> 8) as u8);
    bus.write_u32(0x0000_0100, 0xFFFF_FFFF);
    bus.write_u8(0x0000_3FFF, 0xFF);
    assert_eq!(bus.read_u32(0x0000_0000), BIOS_LATCH_BOOT, "no write path");
    assert_eq!(bus.read_u8(0x0000_3FFF), (BIOS_LATCH_BOOT >> 24) as u8);
}

#[test]
fn mapped_bios_reads_the_image() {
    let mut bios = vec![0u8; 0x4000];
    bios[8..12].copy_from_slice(&0x0EA0_C0DEu32.to_le_bytes());
    let mut bus = Bus::with_bios(Vec::new(), bios);
    assert_eq!(bus.read_u32(0x0000_0008), 0x0EA0_C0DE);
    assert_eq!(bus.read_u8(0x0000_3FFF), 0);
}

#[test]
fn unmapped_addresses_return_open_bus() {
    let mut bus = bus_with_rom();
    bus.write_u32(0x0400_0000, 0xFFFF_FFFF); // IO stub absorbs writes
    for addr in [
        0x0000_4000,
        0x0000_8000,
        0x0100_0000,
        0x01FF_FFFF,
        0x0400_0400,
        0x04FF_FFFF,
    ] {
        assert_eq!(bus.read_u8(addr), 0, "u8 open bus at {addr:#010x}");
        assert_eq!(bus.read_u16(addr), 0, "u16 open bus at {addr:#010x}");
        assert_eq!(bus.read_u32(addr), 0, "u32 open bus at {addr:#010x}");
    }
}

#[test]
fn top_address_bits_alias_the_low_256m() {
    let mut bus = bus_with_rom();
    bus.write_u16(0x0200_0010, 0xCAFE);
    assert_eq!(bus.read_u16(0x1200_0010), 0xCAFE);
    assert_eq!(bus.read_u8(0xFF00_0000), bus.read_u8(0x0F00_0000));
    assert_eq!(bus.read_u32(0xF800_0000), bus.read_u32(0x0800_0000));
}

#[test]
fn no_panics_across_region_boundaries() {
    let mut bus = bus_with_rom();
    let mut probes = Vec::new();
    for sel in 0u32..=0x0F {
        let base = sel << 24;
        probes.extend([
            base,
            base.wrapping_sub(1),
            base + 0x3FFF,
            base + 0x4000,
            base + 0x3_FFFF,
            base + 0x4_0000,
            base + 0x1FF_FFFF,
            base + 0x1FF_FFFE,
            base + 0x0FFF_FFFF,
        ]);
    }
    probes.extend([
        0x0000_3FFF,
        0x0000_4000,
        0x0400_03FE,
        0x0400_0402,
        0x0601_7FFE,
        0x0601_8000,
    ]);
    for addr in probes {
        let _ = bus.read_u8(addr);
        let _ = bus.read_u16(addr);
        let _ = bus.read_u32(addr);
        bus.write_u8(addr, 0xA5);
        bus.write_u16(addr, 0x5AA5);
        bus.write_u32(addr, 0xA55A_5AA5);
    }
}

#[test]
fn vram_byte_writes_fill_both_halves_of_the_cell() {
    let mut bus = Bus::new(Vec::new());
    bus.write_u8(0x0600_0100, 0xAB);
    // The byte lands in both bytes of the containing halfword (GBATEK
    // "VRAM": 8-bit writes store to upper and lower bytes).
    assert_eq!(bus.read_u8(0x0600_0100), 0xAB);
    assert_eq!(bus.read_u16(0x0600_0100), 0xABAB);
    // A byte write to the high lane fills the same halfword.
    bus.write_u8(0x0600_0103, 0x5A);
    assert_eq!(bus.read_u16(0x0600_0102), 0x5A5A);
}
