//! Multiboot execution: a multiboot-classified image boots
//! from EWRAM the way the BIOS would place it, and the same image run
//! through the recompiled path reaches the interpreted final state.

use analysis::FlagMap;
use rom::{Image, ImageKind};
use runtime::recomp::{Driver, TranslatedFn};
use runtime::system::System;
use testutil::synthetic::RomBuilder;

/// Mode-3 paint program placed at EWRAM 0xC0 and entered through the
/// header's entry branch. Assembled from the listing below with
/// `arm-none-eabi-as` (Binutils 2.32) and verified with `objdump`;
/// regenerate with the same tools if you edit the listing.
///
/// ```text
///     .arm
///     b start
/// start:
///     ldr r0, disp
///     ldr r1, mode3
///     strh r1, [r0]
///     ldr r0, vram
///     ldr r1, white
///     strh r1, [r0]
///     ldr r1, red
///     strh r1, [r0, #2]
///     ldr r1, blue
///     strh r1, [r0, #4]
/// idle:
///     b idle
/// disp:  .word 0x04000000
/// mode3: .word 0x00000403
/// vram:  .word 0x06000000
/// white: .word 0x00007fff
/// red:   .word 0x0000001f
/// blue:  .word 0x00007c00
/// ```
const PROGRAM: [u32; 17] = [
    0xE59F_0024,
    0xE59F_1024,
    0xE1C0_10B0,
    0xE59F_0020,
    0xE59F_1020,
    0xE1C0_10B0,
    0xE59F_101C,
    0xE1C0_10B2,
    0xE59F_1018,
    0xE1C0_10B4,
    0xEAFF_FFFE,
    0x0400_0000,
    0x0000_0403,
    0x0600_0000,
    0x0000_7FFF,
    0x0000_001F,
    0x0000_7C00,
];

/// The header entry branch: `b 0x020000c0` decoded at the multiboot
/// base, which is also what rom's multiboot classification requires.
const HEADER_ENTRY: u32 = 0xEA00_002E;

/// The program's park address in EWRAM.
const PARK: u32 = 0x0200_00E8;

/// Builds a multiboot-classified image: the entry branch decodes into
/// EWRAM at the multiboot base, and the program sits past the header.
fn multiboot_image(title: &str) -> Image {
    let mut bytes = RomBuilder::new()
        .title(title)
        .entry(HEADER_ENTRY)
        .min_size(0x140)
        .build();
    let mut code = Vec::new();
    for word in PROGRAM {
        code.extend_from_slice(&word.to_le_bytes());
    }
    let end = 0xC0 + code.len();
    bytes[0xC0..end].copy_from_slice(&code);
    Image::from_bytes(bytes).expect("parse")
}

/// Lifts and compiles the image's two regions (header entry branch and
/// the program) into a driver. `None` when no host C compiler exists.
fn build_driver(image: &Image, tag: &str) -> Option<Driver> {
    if !recomp::Module::compiler_available() {
        return None;
    }
    let map = FlagMap::from_image(image);
    let base = map.canonical().base();
    let marks = [(0x00u32, 0x04u32), (0xC0, 0x104)];
    let mut entries = Vec::new();
    let mut functions = Vec::new();
    for (start, end) in marks {
        let region = ir::lift::FunctionRegion {
            start: base + start,
            end: base + end,
            state: analysis::State::Arm,
        };
        let mut function = ir::lift::lift_function(image, &map, region);
        ir::opt::optimize(&mut function);
        entries.push(TranslatedFn {
            start: region.start,
            end: region.end,
            thumb: false,
        });
        functions.push(function);
    }
    let refs: Vec<(u32, &ir::Function)> = functions
        .iter()
        .enumerate()
        .map(|(i, f)| (entries[i].start, f))
        .collect();
    let source = recomp::emit_module(&refs);
    let module = recomp::Module::build(&source.c_source, tag).expect("build");
    Some(Driver::new(module, entries))
}

#[test]
fn multiboot_image_boots_from_ewram_and_renders() {
    let image = multiboot_image("MB1");
    assert_eq!(image.kind(), ImageKind::Multiboot);
    let mut system = System::with_multiboot(image.bytes().to_vec());
    assert_eq!(system.cpu.pc(), 0x0200_0000);
    // The header entry branch runs from EWRAM and reaches the program.
    system.step();
    assert_eq!(system.cpu.pc(), 0x0200_00C0);
    let mut steps = 0;
    while system.cpu.pc() != PARK && steps < 100 {
        system.step();
        steps += 1;
    }
    assert_eq!(system.cpu.pc(), PARK, "program parks after the fill");
    // One full frame renders the painted pixels.
    system.step_frame();
    let fb = system.framebuffer();
    assert_eq!(&fb[0..4], &[255, 255, 255, 255], "pixel 0: white (0x7fff)");
    assert_eq!(&fb[4..8], &[255, 0, 0, 255], "pixel 1: red (0x001f)");
    assert_eq!(&fb[8..12], &[0, 0, 255, 255], "pixel 2: blue (0x7c00)");
    assert_eq!(&fb[40..44], &[0, 0, 0, 255], "backdrop untouched");
}

#[test]
fn multiboot_recomp_path_matches_interpreted() {
    let image = multiboot_image("MB2");
    let Some(mut driver) = build_driver(&image, "mb-recomp") else {
        println!("skipping: no host C compiler on PATH");
        return;
    };
    assert!(driver.is_translated(0x0200_0000));
    assert!(driver.is_translated(0x0200_00C0));

    let mut interpreted = System::with_multiboot(image.bytes().to_vec());
    let mut hybrid = System::with_multiboot(image.bytes().to_vec());
    let mut steps = 0;
    while interpreted.cpu.pc() != PARK && steps < 100 {
        interpreted.step();
        steps += 1;
    }
    assert_eq!(interpreted.cpu.pc(), PARK, "interpreted run parks");
    let mut steps = 0;
    while hybrid.cpu.pc() != PARK && steps < 100 {
        driver.step(&mut hybrid).expect("native call");
        steps += 1;
    }
    assert_eq!(hybrid.cpu.pc(), PARK, "hybrid run parks");
    for reg in 0..4u8 {
        assert_eq!(
            interpreted.cpu.reg_for_test(reg),
            hybrid.cpu.reg_for_test(reg),
            "r{reg}"
        );
    }
    assert_eq!(
        interpreted.bus().ewram(),
        hybrid.bus().ewram(),
        "EWRAM image state"
    );
    interpreted.step_frame();
    hybrid.step_frame();
    assert_eq!(
        interpreted.framebuffer(),
        hybrid.framebuffer(),
        "rendered frame"
    );
}
