//! Recompiled-vs-interpreted state equality.
//!
//! The micro corpus runs the same synthetic program twice, once
//! interpreted and once through the translated module, and compares
//! full machine state at the park point. The mode-3 ROM renders the
//! same golden frame through both paths.
//!
//! Skips loudly when the host C compiler or the built ROM is absent.

use analysis::{FlagMap, State, Thresholds};
use rom::Image;
use runtime::recomp::Driver;
use runtime::system::System;
use testutil::synthetic::RomBuilder;

/// The `b .` at the end of the micro program.
const PARK: u32 = 0x0800_012C;

/// The micro program: a counted loop over EWRAM, a `bl` into a second
/// function, a result store, and a park. Function two computes r3+r3.
fn micro_image() -> Image {
    let words: [u32; 14] = [
        0xE3A0_0402, // mov r0, #0x02000000
        0xE3A0_1007, // mov r1, #7
        0xE580_1000, // str r1, [r0]
        0xE280_2C01, // add r2, r0, #0x100
        0xE590_3000, // loop: ldr r3, [r0]
        0xE283_3001, // add r3, r3, #1
        0xE580_3000, // str r3, [r0]
        0xE353_000A, // cmp r3, #10
        0xBAFF_FFFA, // blt loop
        0xEB00_0001, // bl helper
        0xE582_4000, // str r4, [r2]
        0xEAFF_FFFE, // b .
        0xE093_4003, // helper: adds r4, r3, r3
        0xE1A0_F00E, // mov pc, lr
    ];
    let mut bytes = RomBuilder::new().title("RECM").build();
    let mut code = Vec::new();
    for word in words {
        code.extend_from_slice(&word.to_le_bytes());
    }
    bytes[0x100..0x100 + code.len()].copy_from_slice(&code);
    Image::from_bytes(bytes).expect("parse")
}

/// Lifts, optimizes, translates, compiles, and loads one function per
/// `(start, end)` mark. Marks are code ranges; each start becomes a
/// translated entry.
fn build_driver(image: &Image, marks: &[(u32, u32)]) -> Result<Driver, String> {
    let mut map = FlagMap::from_image(image);
    let base = map.canonical().base();
    for (start, end) in marks {
        map.mark_range(base + start, base + end, analysis::Flags::EXEC);
    }
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
        entries.push((region.start, false));
        functions.push(function);
    }
    let refs: Vec<(u32, &ir::Function)> = functions
        .iter()
        .enumerate()
        .map(|(i, f)| (entries[i].0, f))
        .collect();
    let source = recomp::emit_module(&refs);
    let module = recomp::Module::build(&source.c_source, "recomp-test")?;
    Ok(Driver::new(module, entries))
}

#[test]
fn micro_corpus_recompiled_state_matches_interpreted() {
    if !recomp::Module::compiler_available() {
        println!("skipping: no host C compiler on PATH");
        return;
    }
    let image = micro_image();

    let mut interpreted = System::new(image.bytes().to_vec(), 0x0800_0100);
    let mut steps = 0;
    while interpreted.cpu.pc() != PARK && steps < 10_000 {
        interpreted.step();
        steps += 1;
    }
    assert_eq!(
        interpreted.cpu.pc(),
        PARK,
        "interpreted run reaches the park"
    );

    let marks = [(0x100u32, 0x130u32), (0x130, 0x138)];
    let mut driver = build_driver(&image, &marks).expect("module builds");
    let mut recompiled = System::new(image.bytes().to_vec(), 0x0800_0100);
    let mut steps = 0;
    while recompiled.cpu.pc() != PARK && steps < 10_000 {
        driver.step(&mut recompiled).expect("native call");
        steps += 1;
    }
    assert_eq!(recompiled.cpu.pc(), PARK, "recompiled run reaches the park");

    let a = interpreted.bus();
    let b = recompiled.bus();
    // Full memory and IO compare (the acceptance criterion).
    let first_diff = |x: &[u8], y: &[u8]| -> Option<(usize, u8, u8)> {
        x.iter()
            .zip(y.iter())
            .enumerate()
            .find(|(_, (p, q))| p != q)
            .map(|(i, (p, q))| (i, *p, *q))
    };
    match first_diff(a.ewram(), b.ewram()) {
        None => {}
        Some((i, x, y)) => panic!("EWRAM differs at 0x{i:06x}: {x:#04x} vs {y:#04x}"),
    }
    match first_diff(a.iwram(), b.iwram()) {
        None => {}
        Some((i, x, y)) => panic!("IWRAM differs at 0x{i:06x}: {x:#04x} vs {y:#04x}"),
    }
    match first_diff(a.io_image(), b.io_image()) {
        None => {}
        Some((i, x, y)) => panic!("IO differs at register 0x{i:03x}: {x:#04x} vs {y:#04x}"),
    }
    // Registers r0-r14 and the condition flags.
    for i in 0..15u8 {
        assert_eq!(
            interpreted.cpu.reg_for_test(i),
            recompiled.cpu.reg_for_test(i),
            "r{i}"
        );
    }
    assert_eq!(
        interpreted.cpu.cpsr() & 0xF000_0000,
        recompiled.cpu.cpsr() & 0xF000_0000,
        "flags"
    );
    // The computed results, spelled out for a readable failure.
    let word = |addr: usize| {
        u32::from_le_bytes(
            b.ewram()[addr - 0x0200_0000..addr - 0x0200_0000 + 4]
                .try_into()
                .unwrap(),
        )
    };
    assert_eq!(word(0x0200_0000), 10, "loop counter");
    assert_eq!(word(0x0200_0100), 20, "helper result");
}

#[test]
fn mode3_rom_renders_the_same_frame_through_both_paths() {
    let Some(path) = testutil::testrom::require("mode3") else {
        println!("skipping: test ROMs not built (scripts/build-test-roms.sh)");
        return;
    };
    if !recomp::Module::compiler_available() {
        println!("skipping: no host C compiler on PATH");
        return;
    }
    let image = Image::from_file(&path).expect("script-built ROM must parse");

    // Discover functions with a cart-base map: this 256-byte image
    // classifies as multiboot (the documented small-image ambiguity)
    // but the test boots it at the cart base. Function starts from the
    // seed and traversal are spread over their bodies so the lift
    // regions run start to next start.
    let mut map = FlagMap::new(
        image.bytes().len(),
        analysis::Canonical::for_kind(rom::ImageKind::Cart),
    );
    let seeds = map.seed(&[]);
    analysis::analyze(&image, &mut map, &seeds, &seeds, Thresholds::default(), &[]);
    let base = map.canonical().base();
    let mut starts: Vec<u32> = (0..image.bytes().len() as u32)
        .filter(|offset| map.get(base + offset).contains(analysis::Flags::FUNC_START))
        .map(|offset| base + offset)
        .collect();
    starts.push(base + image.bytes().len() as u32);
    for window in starts.windows(2) {
        let flags = analysis::Flags::EXEC | analysis::Flags::FUNC_START;
        map.mark_range(window[0], window[1], flags);
    }
    let mut entries = Vec::new();
    let mut functions = Vec::new();
    for region in ir::lift::function_regions(&image, &map) {
        let thumb = matches!(region.state, State::Thumb);
        let mut function = ir::lift::lift_function(&image, &map, region);
        ir::opt::optimize(&mut function);
        entries.push((region.start, thumb));
        functions.push(function);
    }
    assert!(
        !entries.is_empty(),
        "discovery must find the entry function"
    );
    let refs: Vec<(u32, &ir::Function)> = functions
        .iter()
        .enumerate()
        .map(|(i, f)| (entries[i].0, f))
        .collect();
    let source = recomp::emit_module(&refs);
    let module = recomp::Module::build(&source.c_source, "mode3-recomp").expect("compile");
    let mut driver = Driver::new(module, entries);

    let mut interpreted = System::new(image.bytes().to_vec(), 0x0800_0000);
    interpreted.step_frame();
    let mut recompiled = System::new(image.bytes().to_vec(), 0x0800_0000);
    driver
        .run_frame(&mut recompiled)
        .expect("native calls succeed");
    assert_eq!(interpreted.framebuffer(), recompiled.framebuffer());
}
