//! Boots assembler-built test ROMs (see scripts/build-test-roms.sh).
//!
//! Skips loudly when the images are absent: a machine without
//! arm-none-eabi-binutils has no target/test-roms/ directory.

use rom::Image;
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
