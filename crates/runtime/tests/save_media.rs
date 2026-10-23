//! Save media integration: the synthetic save ROMs round-trip data
//! through a save file, and detection picks each medium from its ID.

use std::path::PathBuf;

use rom::Image;
use runtime::save::{Save, SaveKind};
use runtime::system::System;
use testutil::testrom;

const CART_BASE: u32 = 0x0800_0000;

/// The IWRAM capture block address shared by the save ROMs.
const CAPTURE: u32 = 0x0300_0004;

/// Loads a script-built ROM or skips the test when it has not been
/// built.
fn rom(name: &str) -> Option<Image> {
    let path = testrom::require(name)?;
    Some(
        Image::from_file(&path)
            .map_err(|e| panic!("script-built ROM must parse: {e}"))
            .expect("script-built ROM must parse"),
    )
}

/// A unique save file path under the system temp dir.
fn temp_save(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("oxyboy-{}-{}.sav", name, std::process::id()))
}

/// The phase byte address shared by the save ROMs.
const PHASE: u32 = 0x0300_0000;

#[test]
fn detection_picks_the_id_medium_for_every_rom() {
    for (name, kind) in [
        ("sramtest", SaveKind::Sram),
        ("flash64test", SaveKind::Flash64),
        ("flash128test", SaveKind::Flash128),
        // The EEPROM_V string does not distinguish the chip sizes;
        // both detect as the provisional 512B kind.
        ("eeprom512test", SaveKind::Eeprom512),
        ("eeprom8ktest", SaveKind::Eeprom512),
    ] {
        let Some(image) = rom(name) else {
            return;
        };
        assert_eq!(SaveKind::detect(image.bytes()), kind, "{name}");
        // A booted system carries the same kind.
        let system = System::new(image.bytes().to_vec(), CART_BASE);
        assert_eq!(system.bus().save().kind(), kind, "{name}");
    }
}

#[test]
fn sram_rom_round_trips_through_a_save_file() {
    let Some(image) = rom("sramtest") else {
        return;
    };
    let path = temp_save("sram");
    let mut system = System::new(image.bytes().to_vec(), CART_BASE);
    // Phase 0: the ROM writes i ^ A5h to 0x0E001000+i and captures.
    system.step_frame();
    for i in 0..16u32 {
        let expected = (i as u8) ^ 0xA5;
        assert_eq!(
            system.bus_mut().save_mut().read_u8((0x1000 + i) as u16),
            expected,
            "image byte {i}"
        );
    }
    system.bus().save().to_file(&path).expect("store save");

    // A fresh machine loads the file and re-runs the ROM in read-only
    // phase; the capture block must match the pattern.
    let image2 = rom("sramtest").expect("second load");
    let mut system2 = System::new(image2.bytes().to_vec(), CART_BASE);
    let loaded = Save::from_file(&path).expect("reload");
    *system2.bus_mut().save_mut() = loaded;
    system2.bus_mut().write_u8(PHASE, 1);
    system2.step_frame();
    for i in 0..16u32 {
        let expected = (i as u8) ^ 0xA5;
        assert_eq!(
            system2.bus_mut().read_u8(CAPTURE + i),
            expected,
            "round-trip byte {i}"
        );
    }
    std::fs::remove_file(&path).expect("cleanup save");
}

#[test]
fn flash64_rom_round_trips_through_a_save_file() {
    let Some(image) = rom("flash64test") else {
        return;
    };
    let path = temp_save("flash64");
    let mut system = System::new(image.bytes().to_vec(), CART_BASE);
    // Phase 0: erase sector 0, program i ^ 3Ch at 0x0E000100+i.
    system.step_frame();
    for i in 0..16u32 {
        let expected = (i as u8) ^ 0x3C;
        assert_eq!(
            system.bus_mut().save_mut().read_u8((0x0100 + i) as u16),
            expected,
            "image byte {i}"
        );
    }
    // The rest of the sector is erased.
    assert_eq!(
        system.bus_mut().save_mut().read_u8(0x0200),
        0xFF,
        "erased tail"
    );
    system.bus().save().to_file(&path).expect("store save");

    let image2 = rom("flash64test").expect("second load");
    let mut system2 = System::new(image2.bytes().to_vec(), CART_BASE);
    let loaded = Save::from_file(&path).expect("reload");
    *system2.bus_mut().save_mut() = loaded;
    system2.bus_mut().write_u8(PHASE, 1);
    system2.step_frame();
    for i in 0..16u32 {
        let expected = (i as u8) ^ 0x3C;
        assert_eq!(
            system2.bus_mut().read_u8(CAPTURE + i),
            expected,
            "round-trip byte {i}"
        );
    }
    std::fs::remove_file(&path).expect("cleanup save");
}

#[test]
fn flash128_rom_switches_banks_and_round_trips() {
    let Some(image) = rom("flash128test") else {
        return;
    };
    let path = temp_save("flash128");
    let mut system = System::new(image.bytes().to_vec(), CART_BASE);
    // Phase 0: bank 0 gets i ^ 3Ch, bank 1 gets i ^ 3Ch ^ 10h.
    system.step_frame();
    let image_bytes = system.bus().save().to_bytes();
    for i in 0..8u32 {
        assert_eq!(
            image_bytes[(0x0200 + i) as usize],
            (i as u8) ^ 0x3C,
            "bank 0"
        );
        assert_eq!(
            image_bytes[(0x1_0200 + i) as usize],
            ((i as u8) ^ 0x3C) + 0x10,
            "bank 1"
        );
    }
    system.bus().save().to_file(&path).expect("store save");

    let image2 = rom("flash128test").expect("second load");
    let mut system2 = System::new(image2.bytes().to_vec(), CART_BASE);
    let loaded = Save::from_file(&path).expect("reload");
    *system2.bus_mut().save_mut() = loaded;
    system2.bus_mut().write_u8(PHASE, 1);
    system2.step_frame();
    for i in 0..8u32 {
        let expected0 = (i as u8) ^ 0x3C;
        let expected1 = expected0 + 0x10;
        assert_eq!(
            system2.bus_mut().read_u8(CAPTURE + i),
            expected0,
            "bank 0 round-trip byte {i}"
        );
        assert_eq!(
            system2.bus_mut().read_u8(CAPTURE + 8 + i),
            expected1,
            "bank 1 round-trip byte {i}"
        );
    }
    std::fs::remove_file(&path).expect("cleanup save");
}

#[test]
fn eeprom512_rom_writes_and_reads_back_the_page() {
    let Some(image) = rom("eeprom512test") else {
        return;
    };
    let path = temp_save("eeprom512");
    let mut system = System::new(image.bytes().to_vec(), CART_BASE);
    // Phase 0: the ROM programs page 0x0A with 123456789ABCDEF0h via
    // DMA3 and reads it back into the capture block in the same run.
    system.step_frame();
    assert_eq!(
        system.bus().save().kind(),
        SaveKind::Eeprom512,
        "stream widths kept the 512B chip"
    );
    assert_eq!(
        &system.bus().save().to_bytes()[0x0A * 8..0x0A * 8 + 8],
        &[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0],
        "page image"
    );
    system.bus().save().to_file(&path).expect("store save");

    // A fresh machine with the loaded save re-reads the page.
    let image2 = rom("eeprom512test").expect("second load");
    let mut system2 = System::new(image2.bytes().to_vec(), CART_BASE);
    let loaded = Save::from_file(&path).expect("reload");
    *system2.bus_mut().save_mut() = loaded;
    system2.bus_mut().write_u8(PHASE, 1);
    system2.step_frame();
    let mut got = [0u8; 8];
    for (i, cell) in got.iter_mut().enumerate() {
        *cell = system2.bus_mut().read_u8(CAPTURE + i as u32);
    }
    assert_eq!(got, [0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0]);
    std::fs::remove_file(&path).expect("cleanup save");
}

#[test]
fn eeprom8k_rom_detects_width_from_stream_length() {
    let Some(image) = rom("eeprom8ktest") else {
        return;
    };
    // The ID string is the shared EEPROM_V; a fresh system starts at
    // the provisional 512B kind.
    assert_eq!(SaveKind::detect(image.bytes()), SaveKind::Eeprom512);
    let path = temp_save("eeprom8k");
    let mut system = System::new(image.bytes().to_vec(), CART_BASE);
    // Phase 0: page 200h with FEDCBA9876543210h; the 14-bit address
    // streams promote the chip to the 8K kind.
    system.step_frame();
    assert_eq!(
        system.bus().save().kind(),
        SaveKind::Eeprom8k,
        "the 14-bit streams promote the chip"
    );
    assert_eq!(
        &system.bus().save().to_bytes()[0x200 * 8..0x200 * 8 + 8],
        &[0xFE, 0xDC, 0xBA, 0x98, 0x76, 0x54, 0x32, 0x10],
        "page image"
    );
    system.bus().save().to_file(&path).expect("store save");

    // The exported image is 8K; loading it into a fresh system pins
    // the 8K kind, and the read phase captures the page.
    let image2 = rom("eeprom8ktest").expect("second load");
    let mut system2 = System::new(image2.bytes().to_vec(), CART_BASE);
    let loaded = Save::from_file(&path).expect("reload");
    assert_eq!(
        loaded.kind(),
        SaveKind::Eeprom8k,
        "the exported image pins 8K"
    );
    *system2.bus_mut().save_mut() = loaded;
    system2.bus_mut().write_u8(PHASE, 1);
    system2.step_frame();
    let mut got = [0u8; 8];
    for (i, cell) in got.iter_mut().enumerate() {
        *cell = system2.bus_mut().read_u8(CAPTURE + i as u32);
    }
    assert_eq!(got, [0xFE, 0xDC, 0xBA, 0x98, 0x76, 0x54, 0x32, 0x10]);
    std::fs::remove_file(&path).expect("cleanup save");
}
