//! Boots assembler-built test ROMs (see scripts/build-test-roms.sh).
//!
//! Skips loudly when the images are absent: a machine without
//! arm-none-eabi-binutils has no target/test-roms/ directory.

use rom::Image;
use runtime::cpu::Mode;
use runtime::ppu::{VISIBLE_LINES, WIDTH};
use runtime::system::System;

/// Cart execution base (the documented boot decision).
const CART_BASE: u32 = 0x0800_0000;

#[test]
fn mode3_rom_boots_to_painted_frame() {
    let Some(path) = testutil::testrom::require("mode3") else {
        return;
    };
    let image = Image::from_file(&path).expect("script-built ROM must parse");
    assert!(
        image.header().checksum_valid(),
        "script must patch the complement checksum"
    );

    let mut system = System::new(image.bytes().to_vec(), CART_BASE);
    system.step_frame();

    let frame = system.framebuffer();
    let pixel = |x: usize, y: usize| -> [u8; 3] {
        let at = (y * WIDTH + x) * 4;
        [frame[at], frame[at + 1], frame[at + 2]]
    };
    assert_eq!(pixel(0, 0), [255, 255, 255], "pixel 0 must be white");
    assert_eq!(pixel(1, 0), [255, 255, 255], "pixel 1 must be white");
    assert_eq!(pixel(2, 0), [255, 0, 0], "pixel 2 must be red");
    assert_eq!(pixel(3, 0), [255, 0, 0], "pixel 3 must be red");
    assert_eq!(pixel(4, 0), [0, 0, 255], "pixel 4 must be blue");
    assert_eq!(pixel(5, 0), [0, 0, 255], "pixel 5 must be blue");
    assert_eq!(pixel(6, 0), [0, 0, 0], "backdrop must stay black");
    assert_eq!(frame.len(), WIDTH * VISIBLE_LINES * 4);
}

#[test]
fn switest_rom_runs_cpufastset_and_lz77_without_bios() {
    let Some(path) = testutil::testrom::require("switest") else {
        return;
    };
    let image = Image::from_file(&path).expect("script-built ROM must parse");

    let mut system = System::new(image.bytes().to_vec(), CART_BASE);
    assert_eq!(
        system.bus_mut().read_u32(0x0000_0000),
        runtime::bus::BIOS_LATCH_BOOT,
        "no BIOS file: the window serves the read-protection latch"
    );

    // Run until the ROM sets its IWRAM done flag, bounded by a generous
    // cycle budget (the whole workload is a few hundred cycles; the slack
    // guards against waitstate or HLE cost changes).
    let done_addr = 0x0300_0000;
    let mut cycles = 0;
    while cycles < 100_000 {
        let spent = system.step();
        cycles += spent as u64;
        if system.bus_mut().read_u8(done_addr) == 0xC1 {
            break;
        }
    }
    assert_eq!(
        system.bus_mut().read_u8(done_addr),
        0xC1,
        "ROM must finish within the cycle budget"
    );

    let bus = system.bus_mut();
    for i in 0..4u32 {
        let word = bus.read_u32(0x0200_0000 + 4 * i);
        let want = 0x1312_1110u32.wrapping_add(0x0404_0404 * i);
        assert_eq!(word, want, "LZ77 word {i}");
    }
    for i in 0..8u32 {
        let word = bus.read_u32(0x0200_0100 + 4 * i);
        assert_eq!(word, 0x5A5A_A5A5, "CpuFastSet fill word {i}");
    }
}

/// Boots a real BIOS image when OXYBOY_TEST_BIOS names one, exercising
/// the non-HLE path end to end (BIOS reset, stack setup, header check).
/// No synthetic ROM can pass the Nintendo-logo check, so boot must stay
/// inside the BIOS (the documented lock-up) rather than jump to cart.
#[test]
fn real_bios_boots_and_initializes_the_documented_stacks() {
    let Some(path) = std::env::var_os("OXYBOY_TEST_BIOS") else {
        println!("skipping: set OXYBOY_TEST_BIOS to a 16 KiB BIOS image");
        return;
    };
    let bios = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("OXYBOY_TEST_BIOS {path:?} unreadable: {e}"));
    assert_eq!(bios.len(), 0x4000, "BIOS image must be 16 KiB");

    let mut cart = vec![0u8; 0x100];
    cart[0..4].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes());
    let mut system = System::with_bios(cart, bios);

    let mut cycles = 0u64;
    while cycles < 200_000 {
        cycles += system.step() as u64;
    }

    let pc = system.cpu.pc();
    assert!(
        pc < 0x0000_4000,
        "BIOS must lock up instead of jumping to the cart (pc={pc:#010X})"
    );
    // GBATEK "GBA BIOS RAM Usage": sp_usr=3007F00h (system mode) and
    // sp_svc=3007FE0h (supervisor mode), both set up during boot.
    let sp = system.cpu.reg_for_test(13);
    match system.cpu.mode() {
        Mode::System => assert_eq!(sp, 0x0300_7F00, "system-mode stack top"),
        Mode::Supervisor => assert_eq!(sp, 0x0300_7FE0, "supervisor stack top"),
        mode => panic!("unexpected boot mode {mode:?} at pc {pc:#010X}"),
    }
}
