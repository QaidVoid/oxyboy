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
use runtime::recomp::{Driver, TranslatedFn};
use runtime::system::System;
use testutil::synthetic::RomBuilder;

/// The jump-out park target: the micro program ends in a branch just
/// past the translated regions, so both runs execute the park branch
/// exactly once before the state compare (a self-spinning park would
/// run a different number of times interpreted vs natively budgeted).
const PARK: u32 = 0x0800_0140;

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
        0xEA00_0003, // b 0x140 (the park, outside both regions)
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
/// translated entry. `tag` namespaces the compile scratch directory:
/// tests in one binary run in parallel and must not share one.
fn build_driver(image: &Image, marks: &[(u32, u32)], tag: &str) -> Result<Driver, String> {
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
        let mut function = ir::lift::lift_function(image, &map, region, &[]);
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
    let module = recomp::Module::build(&source.c_source, tag)?;
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
    let mut driver = build_driver(&image, &marks, "recomp-micro").expect("module builds");
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

/// The boolean idiom that broke the hybrid: `cmp` + `movne rX,#1` +
/// `moveq rX,#0` + an unconditional store of rX. The guarded writes
/// must leave rX at the selected value when the store (outside the
/// guards) reads it - an earlier defect stored the not-taken arm's
/// never-assigned local.
#[test]
fn conditional_boolean_idiom_stores_the_selected_value() {
    if !recomp::Module::compiler_available() {
        println!("skipping: no host C compiler on PATH");
        return;
    }
    let words: [u32; 12] = [
        0xE3A0_0402, // mov r0, #0x02000000
        0xE3A0_1007, // mov r1, #7
        0xE351_0000, // cmp r1, #0
        0x13A0_2001, // movne r2, #1
        0x03A0_2000, // moveq r2, #0
        0xE480_2004, // str r2, [r0], #4
        0xE3A0_1000, // mov r1, #0
        0xE351_0000, // cmp r1, #0
        0x13A0_2001, // movne r2, #1
        0x03A0_2000, // moveq r2, #0
        0xE480_2004, // str r2, [r0], #4
        0xEA00_0003, // b 0x140 (the park)
    ];
    let mut bytes = RomBuilder::new().title("BOOL").build();
    let mut code = Vec::new();
    for word in words {
        code.extend_from_slice(&word.to_le_bytes());
    }
    bytes[0x100..0x100 + code.len()].copy_from_slice(&code);
    let image = Image::from_bytes(bytes).expect("parse");

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

    let mut driver = build_driver(&image, &[(0x100u32, 0x130u32)], "recomp-bool").expect("builds");
    let mut recompiled = System::new(image.bytes().to_vec(), 0x0800_0100);
    let mut steps = 0;
    while recompiled.cpu.pc() != PARK && steps < 10_000 {
        driver.step(&mut recompiled).expect("native call");
        steps += 1;
    }
    assert_eq!(recompiled.cpu.pc(), PARK, "recompiled run reaches the park");

    let word = |system: &System, addr: usize| {
        u32::from_le_bytes(
            system.bus().ewram()[addr - 0x0200_0000..addr - 0x0200_0000 + 4]
                .try_into()
                .unwrap(),
        )
    };
    assert_eq!(word(&interpreted, 0x0200_0000), 1, "NE stores 1");
    assert_eq!(word(&interpreted, 0x0200_0004), 0, "EQ stores 0");
    assert_eq!(word(&recompiled, 0x0200_0000), 1, "NE stores 1 natively");
    assert_eq!(word(&recompiled, 0x0200_0004), 0, "EQ stores 0 natively");
    assert_eq!(
        interpreted.cpu.reg_for_test(2),
        recompiled.cpu.reg_for_test(2),
        "r2"
    );
}

/// A function returning through `ldr pc, [sp], #4` resumes
/// at the loaded address on both paths. The pre-fix lift returned
/// through the entry pc, so the driver handed control back to the
/// chunk's own entry and re-ran it: the interpreted re-pass
/// incremented the memory counter a second time (2 vs 1).
#[test]
fn ldr_pc_return_resumes_at_the_loaded_address() {
    if !recomp::Module::compiler_available() {
        println!("skipping: no host C compiler on PATH");
        return;
    }
    const PARK: u32 = 0x0800_0128;
    let words: [u32; 11] = [
        0xE3A0_D402, // 0x100: mov sp, #0x02000000 (2 ror 8)
        0xE28D_D901, // 0x104: add sp, sp, #0x4000
        0xE3A0_3402, // 0x108: mov r3, #0x02000000
        0xE28F_0014, // 0x10c: add r0, pc, #20  ; r0 = 0x128 (the park)
        0xE52D_0004, // 0x110: push {r0}
        0xE593_1000, // 0x114: ldr r1, [r3]
        0xE281_1001, // 0x118: add r1, r1, #1
        0xE583_1000, // 0x11c: str r1, [r3]     ; counter++
        0xE49D_F004, // 0x120: ldr pc, [sp], #4 ; return to 0x128
        0xEAFF_FFFE, // 0x124: b . (dead; the walk ends at the return)
        0xEAFF_FFFE, // 0x128: b . (the park)
    ];
    let mut bytes = RomBuilder::new().title("LDRC").build();
    let mut code = Vec::new();
    for word in words {
        code.extend_from_slice(&word.to_le_bytes());
    }
    bytes[0x100..0x100 + code.len()].copy_from_slice(&code);
    let image = Image::from_bytes(bytes).expect("parse");

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

    let mut driver = build_driver(&image, &[(0x100u32, 0x128u32)], "recomp-ldrpc").expect("builds");
    let mut recompiled = System::new(image.bytes().to_vec(), 0x0800_0100);
    let mut steps = 0;
    while recompiled.cpu.pc() != PARK && steps < 10_000 {
        driver.step(&mut recompiled).expect("native call");
        steps += 1;
    }
    assert_eq!(recompiled.cpu.pc(), PARK, "recompiled run reaches the park");
    let word = |system: &System| u32::from_le_bytes(system.bus().ewram()[0..4].try_into().unwrap());
    assert_eq!(word(&interpreted), 1, "counter runs once");
    assert_eq!(word(&recompiled), 1, "counter runs once natively");
    assert_eq!(
        interpreted.cpu.reg_for_test(1),
        recompiled.cpu.reg_for_test(1),
        "r1"
    );
    assert_eq!(
        interpreted.cpu.reg_for_test(13),
        recompiled.cpu.reg_for_test(13),
        "sp restored by the post-indexed writeback"
    );
}

/// A guarded call (`blne helper`) must skip both the lr write and the
/// call when the condition fails, and take both when it holds. The
/// pre-fix lift called unconditionally (and wrote lr unguarded), so
/// the failing round ran the helper and stored its result first
/// (2 vs 1).
#[test]
fn guarded_bl_skips_the_call_when_the_condition_fails() {
    if !recomp::Module::compiler_available() {
        println!("skipping: no host C compiler on PATH");
        return;
    }
    const PARK: u32 = 0x0800_0140;
    let words: [u32; 17] = [
        0xE3A0_0402, // 0x100: mov r0, #0x02000000
        0xE3A0_1000, // 0x104: mov r1, #0        ; round 1: NE fails
        0xE351_0000, // 0x108: cmp r1, #0
        0x1B00_0008, // 0x10c: blne 0x134
        0xE3A0_2001, // 0x110: mov r2, #1
        0xE480_2004, // 0x114: str r2, [r0], #4  ; slot 0
        0xE3A0_1007, // 0x118: mov r1, #7        ; round 2: NE holds
        0xE351_0000, // 0x11c: cmp r1, #0
        0x1B00_0003, // 0x120: blne 0x134
        0xE3A0_2001, // 0x124: mov r2, #1
        0xE480_2004, // 0x128: str r2, [r0], #4  ; slot 2
        0xEA00_0003, // 0x12c: b 0x140 (the park)
        0xE1A0_0000, // 0x130: (dead padding)
        0xE3A0_2002, // 0x134: helper: mov r2, #2
        0xE480_2004, // 0x138: str r2, [r0], #4  ; slot 1
        0xE1A0_F00E, // 0x13c: mov pc, lr
        0xEAFF_FFFE, // 0x140: park: b .
    ];
    let mut bytes = RomBuilder::new().title("BLCC").build();
    let mut code = Vec::new();
    for word in words {
        code.extend_from_slice(&word.to_le_bytes());
    }
    bytes[0x100..0x100 + code.len()].copy_from_slice(&code);
    let image = Image::from_bytes(bytes).expect("parse");

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

    let mut driver = build_driver(
        &image,
        &[(0x100u32, 0x130u32), (0x134, 0x140)],
        "recomp-blcc",
    )
    .expect("builds");
    let mut recompiled = System::new(image.bytes().to_vec(), 0x0800_0100);
    let mut steps = 0;
    while recompiled.cpu.pc() != PARK && steps < 10_000 {
        driver.step(&mut recompiled).expect("native call");
        steps += 1;
    }
    assert_eq!(recompiled.cpu.pc(), PARK, "recompiled run reaches the park");

    let slots = |system: &System| {
        let ewram = system.bus().ewram();
        (0..3)
            .map(|slot| u32::from_le_bytes(ewram[slot * 4..slot * 4 + 4].try_into().unwrap()))
            .collect::<Vec<u32>>()
    };
    assert_eq!(slots(&interpreted), vec![1, 2, 1], "interpreted rounds");
    assert_eq!(slots(&recompiled), vec![1, 2, 1], "recompiled rounds");
    for i in 0..15u8 {
        assert_eq!(
            interpreted.cpu.reg_for_test(i),
            recompiled.cpu.reg_for_test(i),
            "r{i}"
        );
    }
}

/// Cycle parity: the recompiled run of the micro
/// corpus charges exactly the interpreter's cycle count for the same
/// instruction sequence - opcode fetches through the bus's real cost
/// model, multiply latency, and data-access waitstates included.
#[test]
fn recompiled_cycles_match_the_interpreter() {
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
    let mut driver = build_driver(&image, &marks, "recomp-cycles").expect("module builds");
    let mut recompiled = System::new(image.bytes().to_vec(), 0x0800_0100);
    let mut hybrid_cycles = 0u64;
    let mut steps = 0;
    while recompiled.cpu.pc() != PARK && steps < 10_000 {
        let charged = driver.step(&mut recompiled).expect("native call");
        hybrid_cycles += u64::from(charged);
        steps += 1;
    }
    assert_eq!(recompiled.cpu.pc(), PARK, "recompiled run reaches the park");
    assert_eq!(
        interpreted.cpu.cycles(),
        hybrid_cycles,
        "total cycles for the same instruction sequence"
    );
}

/// The host-call target: `r0 += r1; mov pc, lr`.
fn abi_image() -> Image {
    let words: [u32; 2] = [0xE080_0001, 0xE1A0_F00E];
    let mut bytes = RomBuilder::new().title("ABIC").build();
    let mut code = Vec::new();
    for word in words {
        code.extend_from_slice(&word.to_le_bytes());
    }
    bytes[0x100..0x100 + code.len()].copy_from_slice(&code);
    Image::from_bytes(bytes).expect("parse")
}

#[test]
fn host_call_passes_args_and_preserves_callee_state() {
    if !recomp::Module::compiler_available() {
        println!("skipping: no host C compiler on PATH");
        return;
    }
    let image = abi_image();
    let mut driver = build_driver(&image, &[(0x100, 0x108)], "recomp-abi").expect("module builds");
    let mut system = System::new(image.bytes().to_vec(), 0x0800_0100);

    // Distinct callee-saved state: the function must leave it alone.
    for i in 4..13u8 {
        system.cpu.set_reg_for_test(i, 0x300 + u32::from(i));
    }
    let r0 = driver
        .call_function(&mut system, 0x0800_0100, [40, 2, 7, 9])
        .expect("native call");
    assert_eq!(r0, 42, "r0+r1 returned in r0");
    assert_eq!(system.cpu.reg_for_test(1), 2, "r1 keeps the incoming arg");
    for i in 4..13u8 {
        assert_eq!(system.cpu.reg_for_test(i), 0x300 + u32::from(i), "r{i}");
    }
    assert_eq!(
        system.cpu.reg_for_test(14),
        runtime::recomp::HOST_RETURN_SENTINEL,
        "lr survives the call"
    );
    assert_eq!(
        system.cpu.pc(),
        runtime::recomp::HOST_RETURN_SENTINEL,
        "the guest returns to the sentinel"
    );
    assert_eq!(system.cpu.cpsr() & 0xF000_0000, 0, "plain add keeps flags");

    assert!(
        driver
            .call_function(&mut system, 0x0800_0104, [0; 4])
            .is_err(),
        "a non-entry address is rejected"
    );
}

/// `mov r4, #EWRAM; b .+8; filler; mov r0,#25; mov r1,#4; swi 6;
/// str r0,[r4]; b .`: the SWI sits in a block the native side can
/// only exit, so the HLE call must run interpreted over the state the
/// earlier native block computed.
fn swi_image() -> Image {
    let words: [u32; 8] = [
        0xE3A0_4402, // mov r4, #0x02000000
        0xEA00_0000, // b 0x10c
        0xE1A0_0000, // (filler, never executed)
        0xE3A0_0019, // mov r0, #25
        0xE3A0_1004, // mov r1, #4
        0xEF06_0000, // swi 6 (Div; number in comment bits 16-23)
        0xE584_0000, // str r0, [r4]
        0xEAFF_FFFE, // b .
    ];
    let mut bytes = RomBuilder::new().title("SWIR").build();
    let mut code = Vec::new();
    for word in words {
        code.extend_from_slice(&word.to_le_bytes());
    }
    bytes[0x100..0x100 + code.len()].copy_from_slice(&code);
    Image::from_bytes(bytes).expect("parse")
}

#[test]
fn swi_in_translated_function_dispatches_hle_and_matches_interpreted() {
    if !recomp::Module::compiler_available() {
        println!("skipping: no host C compiler on PATH");
        return;
    }
    let image = swi_image();
    let park = 0x0800_011C;

    let mut interpreted = System::new(image.bytes().to_vec(), 0x0800_0100);
    let mut steps = 0;
    let mut interpreted_cycles = 0;
    while interpreted.cpu.pc() != park && steps < 10_000 {
        interpreted_cycles += interpreted.step();
        steps += 1;
    }
    assert_eq!(
        interpreted.cpu.pc(),
        park,
        "interpreted run reaches the park"
    );

    let mut driver = build_driver(&image, &[(0x100, 0x120)], "recomp-swi").expect("module builds");
    assert!(
        driver.is_translated(0x0800_0100),
        "the SWI function is translated"
    );
    let mut hybrid = System::new(image.bytes().to_vec(), 0x0800_0100);
    let mut steps = 0;
    let mut hybrid_cycles = 0;
    while hybrid.cpu.pc() != park && steps < 10_000 {
        hybrid_cycles += driver.step(&mut hybrid).expect("native call");
        steps += 1;
    }
    assert_eq!(hybrid.cpu.pc(), park, "hybrid run reaches the park");

    // The HLE Div dispatched: quotient in r0, remainder in r1, abs
    // quotient in r3 (the swi module's documented convention), and the
    // store after the SWI landed the quotient in EWRAM.
    assert_eq!(hybrid.cpu.reg_for_test(0), 6, "25/4");
    assert_eq!(hybrid.cpu.reg_for_test(1), 1, "25%4");
    assert_eq!(hybrid.cpu.reg_for_test(3), 6, "|25/4|");
    let word = u32::from_le_bytes(hybrid.bus().ewram()[0..4].try_into().expect("slice"));
    assert_eq!(word, 6, "the post-SWI store used the native block's r4");

    // Full state parity with the interpreter.
    for i in 0..15u8 {
        assert_eq!(
            interpreted.cpu.reg_for_test(i),
            hybrid.cpu.reg_for_test(i),
            "r{i}"
        );
    }
    assert_eq!(
        interpreted.cpu.cpsr() & 0xF000_0000,
        hybrid.cpu.cpsr() & 0xF000_0000,
        "flags"
    );
    let mismatch = interpreted
        .bus()
        .ewram()
        .iter()
        .zip(hybrid.bus().ewram().iter())
        .position(|(a, b)| a != b);
    assert_eq!(mismatch, None, "EWRAM");

    // Cycle accounting within the documented tolerance: every unit
    // charges the scheduler (the sums are nonzero and of the same
    // order), while native chunks omit fetch and internal cycles, so
    // the video clock may lag the interpreted run by less than a
    // scanline over this program.
    assert!(hybrid_cycles > 0, "the hybrid run charged cycles");
    assert!(
        hybrid_cycles <= interpreted_cycles * 2,
        "hybrid {hybrid_cycles} vs interpreted {interpreted_cycles} cycles"
    );
    let drift = (i32::from(interpreted.vcount()) - i32::from(hybrid.vcount())).abs();
    assert!(drift <= 1, "scheduler drift {drift} scanlines");
}

/// Pointer-table dispatch: the translated dispatcher loads a function
/// pointer from a literal-pool table and `bx`es to never-lifted code;
/// the callee returns into the dispatcher's unlifted tail, which then
/// calls a second translated function. Exercises the hybrid fallback
/// end to end: native, stub exit, interpreter, native re-entry.
fn dispatch_image() -> Image {
    let words: [u32; 18] = [
        0xE3A0_5402, // 0x100: mov r5, #0x02000000
        0xE28F_E004, // 0x104: add lr, pc, #4        (lr = 0x110 return point)
        0xE59F_3010, // 0x108: ldr r3, [pc, #16]     (table at 0x120)
        0xE12F_FF13, // 0x10c: bx r3                 (unresolved indirect exit)
        0xE585_0000, // 0x110: str r0, [r5]          (return point, unlifted)
        0xEB00_0005, // 0x114: bl 0x130              (translated helper)
        0xE585_0004, // 0x118: str r0, [r5, #4]
        0xEAFF_FFFE, // 0x11c: b .
        0x0800_0140, // 0x120: .word unanalyzed callee
        0,           // 0x124: pad
        0,           // 0x128: pad
        0,           // 0x12c: pad
        0xE280_0015, // 0x130: helper: add r0, r0, #21
        0xE1A0_F00E, // 0x134: mov pc, lr
        0,           // 0x138: pad
        0,           // 0x13c: pad
        0xE3A0_0015, // 0x140: unanalyzed: mov r0, #21
        0xE12F_FF1E, // 0x144: bx lr
    ];
    let mut bytes = RomBuilder::new().title("DISP").build();
    let mut code = Vec::new();
    for word in words {
        code.extend_from_slice(&word.to_le_bytes());
    }
    bytes[0x100..0x100 + code.len()].copy_from_slice(&code);
    Image::from_bytes(bytes).expect("parse")
}

#[test]
fn pointer_table_dispatch_runs_unanalyzed_code_via_stubs() {
    if !recomp::Module::compiler_available() {
        println!("skipping: no host C compiler on PATH");
        return;
    }
    let image = dispatch_image();
    let park = 0x0800_011C;

    let mut interpreted = System::new(image.bytes().to_vec(), 0x0800_0100);
    let mut steps = 0;
    while interpreted.cpu.pc() != park && steps < 10_000 {
        interpreted.step();
        steps += 1;
    }
    assert_eq!(
        interpreted.cpu.pc(),
        park,
        "interpreted run reaches the park"
    );

    // Only the dispatcher and the helper are lifted; the 0x140 callee
    // is never analyzed and runs interpreted through the stub.
    let marks = [(0x100u32, 0x124u32), (0x130, 0x138)];
    let mut driver = build_driver(&image, &marks, "recomp-dispatch").expect("module builds");
    assert!(driver.is_translated(0x0800_0100));
    assert!(driver.is_translated(0x0800_0130));
    assert!(
        !driver.is_translated(0x0800_0140),
        "callee stays unanalyzed"
    );

    let mut hybrid = System::new(image.bytes().to_vec(), 0x0800_0100);
    let mut steps = 0;
    while hybrid.cpu.pc() != park && steps < 10_000 {
        driver.step(&mut hybrid).expect("native call");
        steps += 1;
    }
    assert_eq!(hybrid.cpu.pc(), park, "hybrid run reaches the park");

    // The dispatch produced the right results through both boundaries:
    // the unanalyzed callee's r0 landed in EWRAM, and the translated
    // helper's result followed it.
    let word = |sys: &System, index: usize| {
        u32::from_le_bytes(
            sys.bus().ewram()[index * 4..index * 4 + 4]
                .try_into()
                .unwrap(),
        )
    };
    assert_eq!(word(&hybrid, 0), 21, "unanalyzed callee result stored");
    assert_eq!(word(&hybrid, 1), 42, "helper result stored");

    // Full-state parity with the pure interpreter.
    for i in 0..15u8 {
        assert_eq!(
            interpreted.cpu.reg_for_test(i),
            hybrid.cpu.reg_for_test(i),
            "r{i}"
        );
    }
    assert_eq!(
        interpreted.cpu.cpsr() & 0xF000_0000,
        hybrid.cpu.cpsr() & 0xF000_0000,
        "flags"
    );
    let mismatch = interpreted
        .bus()
        .ewram()
        .iter()
        .zip(hybrid.bus().ewram().iter())
        .position(|(a, b)| a != b);
    assert_eq!(mismatch, None, "EWRAM");
    let mismatch = interpreted
        .bus()
        .iwram()
        .iter()
        .zip(hybrid.bus().iwram().iter())
        .position(|(a, b)| a != b);
    assert_eq!(mismatch, None, "IWRAM");
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
        let mut function = ir::lift::lift_function(&image, &map, region, &[]);
        ir::opt::optimize(&mut function);
        entries.push(TranslatedFn {
            start: region.start,
            end: region.end,
            thumb,
        });
        functions.push(function);
    }
    assert!(
        !entries.is_empty(),
        "discovery must find the entry function"
    );
    let refs: Vec<(u32, &ir::Function)> = functions
        .iter()
        .enumerate()
        .map(|(i, f)| (entries[i].start, f))
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

/// A multiboot-classified image so the canonical base is EWRAM and the
/// code under test is writable; SMC tests need self-modifiable bytes.
fn smc_image(title: &str, words: &[u32]) -> Image {
    // The entry word must branch into EWRAM for multiboot
    // classification (rom::classify); the image itself boots through
    // the explicit start PC, never through the BIOS.
    let mut bytes = RomBuilder::new()
        .title(title)
        .entry(0xEA00_002E)
        .min_size(0x600)
        .build();
    let mut code = Vec::new();
    for word in words {
        code.extend_from_slice(&word.to_le_bytes());
    }
    bytes[0x100..0x100 + code.len()].copy_from_slice(&code);
    Image::from_bytes(bytes).expect("parse")
}

/// Boots a multiboot image the way the BIOS would: the bytes land in
/// EWRAM at 0x02000000 and the CPU enters at `pc`.
fn boot_smc(image: &Image, pc: u32) -> System {
    let mut system = System::with_multiboot(image.bytes().to_vec());
    system.cpu.set_pc(pc);
    system
}

#[test]
fn self_patching_code_bails_and_reruns_patched() {
    if !recomp::Module::compiler_available() {
        println!("skipping: no host C compiler on PATH");
        return;
    }
    // F1 loads a replacement instruction and stores it over its own
    // next-but-one instruction, then parks. The store sits between the
    // loads and the victim so the native chunk must bail at the store
    // (resuming at 0x108) instead of executing the stale victim.
    let words = [
        0xE59F_0014, // 0x100: ldr r0, [pc, #0x14]   (patch value at 0x11c)
        0xE59F_1014, // 0x104: ldr r1, [pc, #0x14]   (target at 0x120)
        0xE581_0000, // 0x108: str r0, [r1]          (SMC store, pc = 0x108)
        0xE3A0_0001, // 0x10c: mov r0, #1            (victim; patched to #2)
        0xEAFF_FFFE, // 0x110: b .
        0,           // 0x114: pad
        0,           // 0x118: pad
        0xE3A0_0002, // 0x11c: .word mov r0, #2
        0x0200_010C, // 0x120: .word victim address
    ];
    let image = smc_image("SMC1", &words);
    let marks = [(0x100u32, 0x114u32)];
    let mut driver = build_driver(&image, &marks, "recomp-smc-self").expect("module builds");
    assert!(driver.is_translated(0x0200_0100));

    let mut system = boot_smc(&image, 0x0200_0100);
    let park = 0x0200_0110;
    let mut steps = 0;
    while system.cpu.pc() != park && steps < 100 {
        driver.step(&mut system).expect("step");
        steps += 1;
    }
    assert_eq!(system.cpu.pc(), park);
    // The patched instruction, not the stale one, produced r0.
    assert_eq!(system.cpu.reg_for_test(0), 2, "patched mov r0, #2 ran");
    // The function that patched itself no longer runs natively.
    assert!(!driver.is_translated(0x0200_0100));
    let ewram = system.bus().ewram();
    let victim = u32::from_le_bytes(ewram[0x10C..0x110].try_into().unwrap());
    assert_eq!(victim, 0xE3A0_0002, "the patch landed in EWRAM");
}

#[test]
fn dma_into_translated_code_invalidates_it() {
    if !recomp::Module::compiler_available() {
        println!("skipping: no host C compiler on PATH");
        return;
    }
    // F1 (page 0) arms DMA3 to copy one replacement word over F2's
    // first instruction (page 1), then branches through an
    // untranslated hop so the deferred DMA drain fires during an
    // interpreted step before F2's entry is reached. F2 must then run
    // interpreted and see the patched instruction.
    let words = [
        0xE59F_0028, // 0x100: ldr r0, [pc, #0x28]   (0x040000d4, DMA3SAD)
        0xE59F_1028, // 0x104: ldr r1, [pc, #0x28]   (source data 0x148)
        0xE580_1000, // 0x108: str r1, [r0]          (SAD = data)
        0xE59F_1024, // 0x10c: ldr r1, [pc, #0x24]   (0x02000400, victim)
        0xE580_1004, // 0x110: str r1, [r0, #4]      (DAD = victim)
        0xE59F_1020, // 0x114: ldr r1, [pc, #0x20]   (0x040000dc, DMA3CNT_L)
        0xE3A0_2001, // 0x118: mov r2, #1
        0xE580_2008, // 0x11c: str r2, [r0, #8]      (one word)
        0xE59F_2018, // 0x120: ldr r2, [pc, #0x18]   (0x8400: enable + word)
        0xE1C0_20BA, // 0x124: strh r2, [r0, #0xa]   (DMA3CNT_H, arms the DMA)
        0xE59F_3014, // 0x128: ldr r3, [pc, #0x14]   (untranslated hop, at 0x144)
        0xE12F_FF13, // 0x12c: bx r3
        0x0400_00D4, // 0x130: .word DMA3SAD
        0x0200_0148, // 0x134: .word source data
        0x0200_0400, // 0x138: .word victim instruction
        0x0400_00DC, // 0x13c: .word DMA3CNT_L
        0x0000_8400, // 0x140: .word DMA3CNT_H (enable | 32-bit)
        0x0200_0200, // 0x144: .word untranslated hop
        0xE3A0_0002, // 0x148: .word replacement mov r0, #2
    ];
    let mut full = words.to_vec();
    // F2 lives on its own 1 KiB page so the test can see that only it
    // was invalidated; the hop and the DMA literals share F1's page.
    full.resize((0x408 - 0x100) / 4, 0);
    // Untranslated hop (0x200) so interpreted steps drain the DMA
    // before F2's entry is reached.
    full[(0x200 - 0x100) / 4] = 0xE1A0_0000;
    full[(0x204 - 0x100) / 4] = 0xEA00_007D;
    // F2's original bytes: `mov r0, #1` then park. The DMA replaces
    // the first word.
    full[(0x400 - 0x100) / 4] = 0xE3A0_0001;
    full[(0x404 - 0x100) / 4] = 0xEAFF_FFFE;
    let image = smc_image("SMC2", &full);
    let marks = [(0x100u32, 0x130u32), (0x400, 0x40c)];
    let mut driver = build_driver(&image, &marks, "recomp-smc-dma").expect("module builds");
    assert!(driver.is_translated(0x0200_0100));
    assert!(driver.is_translated(0x0200_0400));

    let mut system = boot_smc(&image, 0x0200_0100);
    let park = 0x0200_0404;
    let mut steps = 0;
    while system.cpu.pc() != park && steps < 100 {
        driver.step(&mut system).expect("step");
        steps += 1;
    }
    assert_eq!(system.cpu.pc(), park);
    // The DMA patch replaced F2's first instruction, which then ran
    // interpreted off the patched bytes.
    assert_eq!(system.cpu.reg_for_test(0), 2, "patched mov r0, #2 ran");
    let ewram = system.bus().ewram();
    let victim = u32::from_le_bytes(ewram[0x400..0x404].try_into().unwrap());
    assert_eq!(victim, 0xE3A0_0002, "DMA wrote the patch");
    assert!(!driver.is_translated(0x0200_0400), "F2 invalidated");
    assert!(driver.is_translated(0x0200_0100), "F1's page untouched");
}

/// Places ARM `words` at ROM offset 0x100 and `data` words at their
/// ROM offsets.
fn arm_image(title: &str, words: &[u32], data: &[(usize, u32)]) -> Image {
    let mut bytes = RomBuilder::new().title(title).build();
    for (i, word) in words.iter().enumerate() {
        bytes[0x100 + 4 * i..0x104 + 4 * i].copy_from_slice(&word.to_le_bytes());
    }
    for (offset, word) in data {
        bytes[*offset..*offset + 4].copy_from_slice(&word.to_le_bytes());
    }
    Image::from_bytes(bytes).expect("parse")
}

/// Runs `image` from 0x08000100 to `park` interpreted and through
/// the translated `marks`, then asserts r0-r14, the flags, and EWRAM
/// agree. Returns the hybrid system for further checks.
fn assert_hybrid_matches(image: &Image, marks: &[(u32, u32)], tag: &str, park: u32) -> System {
    let mut interpreted = System::new(image.bytes().to_vec(), 0x0800_0100);
    let mut steps = 0;
    while interpreted.cpu.pc() != park && steps < 10_000 {
        interpreted.step();
        steps += 1;
    }
    assert_eq!(
        interpreted.cpu.pc(),
        park,
        "interpreted run reaches the park"
    );

    let mut driver = build_driver(image, marks, tag).expect("module builds");
    let mut hybrid = System::new(image.bytes().to_vec(), 0x0800_0100);
    let mut steps = 0;
    while hybrid.cpu.pc() != park && steps < 10_000 {
        driver.step(&mut hybrid).expect("native call");
        steps += 1;
    }
    assert_eq!(hybrid.cpu.pc(), park, "hybrid run reaches the park");

    for i in 0..15u8 {
        assert_eq!(
            interpreted.cpu.reg_for_test(i),
            hybrid.cpu.reg_for_test(i),
            "r{i}"
        );
    }
    assert_eq!(
        interpreted.cpu.cpsr() & 0xF000_0000,
        hybrid.cpu.cpsr() & 0xF000_0000,
        "flags"
    );
    assert_eq!(interpreted.bus().ewram(), hybrid.bus().ewram(), "EWRAM");
    hybrid
}

#[test]
fn caller_stops_when_a_translated_callee_exits_early() {
    if !recomp::Module::compiler_available() {
        println!("skipping: no host C compiler on PATH");
        return;
    }
    // The callee's SWI block is opaque, so the callee exits to the
    // interpreter; the caller must not continue natively past the call.
    let image = arm_image(
        "RCEX",
        &[
            0xE3A0_4402, // mov r4, #0x02000000
            0xE3A0_0019, // mov r0, #25
            0xE3A0_1004, // mov r1, #4
            0xEB00_0002, // bl helper
            0xE584_0000, // str r0, [r4]
            0xEA00_0002, // b park
            0xE1A0_0000, // (filler)
            0xEF06_0000, // helper: swi 6 (Div)
            0xE1A0_F00E, // mov pc, lr
            0xEAFF_FFFE, // park: b .
        ],
        &[],
    );
    let hybrid = assert_hybrid_matches(
        &image,
        &[(0x100, 0x118), (0x11C, 0x124)],
        "recomp-callee-exit",
        0x0800_0124,
    );
    let word = u32::from_le_bytes(hybrid.bus().ewram()[0..4].try_into().expect("slice"));
    assert_eq!(word, 6, "the store saw the SWI's quotient");
}

#[test]
fn loop_back_edge_keeps_the_previous_iteration_values() {
    if !recomp::Module::compiler_available() {
        println!("skipping: no host C compiler on PATH");
        return;
    }
    // A list walk that sums each node's predecessor: r1 carries the
    // previous r0 around the back edge while r0 advances.
    let image = arm_image(
        "RCLC",
        &[
            0xE3A0_0408, // mov r0, #0x08000000
            0xE280_0D07, // add r0, r0, #0x1C0
            0xE3A0_1000, // mov r1, #0
            0xE3A0_2000, // mov r2, #0
            0xE082_2001, // loop: add r2, r2, r1
            0xE1A0_1000, // mov r1, r0
            0xE590_0004, // ldr r0, [r0, #4]
            0xE350_0000, // cmp r0, #0
            0x1AFF_FFFA, // bne loop
            0xE3A0_4402, // mov r4, #0x02000000
            0xE584_2000, // str r2, [r4]
            0xEA00_0000, // b park
            0xE1A0_0000, // (filler)
            0xEAFF_FFFE, // park: b .
        ],
        &[(0x1C4, 0x0800_01C8), (0x1CC, 0x0800_01D0), (0x1D4, 0)],
    );
    let hybrid = assert_hybrid_matches(&image, &[(0x100, 0x134)], "recomp-loop-copy", 0x0800_0134);
    assert_eq!(hybrid.cpu.reg_for_test(2), 0x0800_01C0 + 0x0800_01C8);
}

#[test]
fn failed_guard_keeps_the_writeback_base() {
    if !recomp::Module::compiler_available() {
        println!("skipping: no host C compiler on PATH");
        return;
    }
    let image = arm_image(
        "RCWB",
        &[
            0xE3A0_1408, // mov r1, #0x08000000
            0xE281_1D07, // add r1, r1, #0x1C0
            0xE3A0_0001, // mov r0, #1
            0xE350_0000, // cmp r0, #0
            0x14D1_3001, // ldrneb r3, [r1], #1   (taken)
            0xE350_0001, // cmp r0, #1
            0x14D1_4001, // ldrneb r4, [r1], #1   (not taken)
            0x15B1_4001, // ldrne r4, [r1, #1]!   (not taken)
            0xE3A0_5402, // mov r5, #0x02000000
            0xE585_1000, // str r1, [r5]
            0xEA00_0000, // b park
            0xE1A0_0000, // (filler)
            0xEAFF_FFFE, // park: b .
        ],
        &[(0x1C0, 0x4433_2211)],
    );
    let hybrid = assert_hybrid_matches(&image, &[(0x100, 0x12C)], "recomp-writeback", 0x0800_0130);
    assert_eq!(hybrid.cpu.reg_for_test(1), 0x0800_01C1);
}
