//! Multiboot analysis and EWRAM copy-hint detection.
//!
//! Every byte layout was assembled with Binutils 2.32 and dumped with
//! objdump before being embedded.

use analysis::{CopyHint, FlagMap, Thresholds, analyze};
use rom::{Image, ImageKind};
use testutil::synthetic::RomBuilder;

#[test]
fn multiboot_image_analyzes_from_ewram() {
    // ```text
    // 00: b 0x40            @ header entry: branch inside EWRAM
    // 40: mov r0, #1
    // 44: bl 0x54
    // 48: bx lr
    // 54: bx lr
    // ```
    let mut bytes = RomBuilder::new().title("MBBOOT").build();
    bytes[0x00..0x04].copy_from_slice(&[0x0E, 0x00, 0x00, 0xEA]);
    bytes[0x40..0x4C].copy_from_slice(&[
        0x01, 0x00, 0xA0, 0xE3, 0x02, 0x00, 0x00, 0xEB, 0x1E, 0xFF, 0x2F, 0xE1,
    ]);
    bytes[0x54..0x58].copy_from_slice(&[0x1E, 0xFF, 0x2F, 0xE1]);
    let image = Image::from_bytes(bytes).expect("parse");
    assert_eq!(image.kind(), ImageKind::Multiboot);

    let mut map = FlagMap::from_image(&image);
    let seeds = map.seed(&[]);
    assert_eq!(
        seeds.iter().map(|seed| seed.addr).collect::<Vec<_>>(),
        vec![0x0200_0000]
    );

    let analysis = analyze(&image, &mut map, &seeds, &seeds, Thresholds::default(), &[]);
    let mut addrs: Vec<_> = analysis
        .discovery
        .functions
        .iter()
        .map(|function| function.addr)
        .collect();
    addrs.sort_unstable();
    assert_eq!(addrs, vec![0x0200_0000, 0x0200_0054]);
    assert!(analysis.unresolved.is_empty());
}

#[test]
fn copy_then_jump_into_ewram_records_a_hint() {
    // ```text
    // 00: mov r0, #1        @ multiboot-heuristic guard
    // 04: ldr r0, [pc, #24] @ source     0x08000080
    // 08: ldr r1, [pc, #24] @ dest       0x02000100
    // 0C: ldr r2, [pc, #24] @ control    0x40 words
    // 10: bl 0x50           @ CpuSet-shaped copy call
    // 14: ldr r3, [pc, #20] @ 0x02000101
    // 18: bx r3             @ jump into the copied region: hint
    // 24: .word 0x08000080
    // 28: .word 0x02000100
    // 2C: .word 0x00000040
    // 30: .word 0x02000101
    // 50: bx lr             @ the copy wrapper
    // ```
    let mut bytes = RomBuilder::new().title("COPY").build();
    bytes[0x00..0x1C].copy_from_slice(&[
        0x01, 0x00, 0xA0, 0xE3, 0x18, 0x00, 0x9F, 0xE5, 0x18, 0x10, 0x9F, 0xE5, 0x18, 0x20, 0x9F,
        0xE5, 0x0E, 0x00, 0x00, 0xEB, 0x14, 0x30, 0x9F, 0xE5, 0x13, 0xFF, 0x2F, 0xE1,
    ]);
    bytes[0x24..0x34].copy_from_slice(&[
        0x80, 0x00, 0x00, 0x08, 0x00, 0x01, 0x00, 0x02, 0x40, 0x00, 0x00, 0x00, 0x01, 0x01, 0x00,
        0x02,
    ]);
    bytes[0x50..0x54].copy_from_slice(&[0x1E, 0xFF, 0x2F, 0xE1]);
    let image = Image::from_bytes(bytes).expect("parse");
    assert_eq!(image.kind(), ImageKind::Cart);

    let mut map = FlagMap::from_image(&image);
    let seeds = map.seed(&[]);
    let analysis = analyze(&image, &mut map, &seeds, &seeds, Thresholds::default(), &[]);
    assert_eq!(
        analysis.copy_hints,
        vec![CopyHint {
            site: 0x0800_0018,
            source: 0x0800_0080,
            dest: 0x0200_0100,
            bytes: 0x100,
        }]
    );
    // Without the jump there is no hint: a copy call alone proves
    // nothing about executing the copied bytes.
    let mut plain = RomBuilder::new().title("CPY0").build();
    plain[0x00..0x1C].copy_from_slice(&[
        0x01, 0x00, 0xA0, 0xE3, 0x18, 0x00, 0x9F, 0xE5, 0x18, 0x10, 0x9F, 0xE5, 0x18, 0x20, 0x9F,
        0xE5, 0x0E, 0x00, 0x00, 0xEB, 0x1E, 0xFF, 0x2F, 0xE1, 0x00, 0x00, 0x00, 0x00,
    ]);
    plain[0x24..0x34].copy_from_slice(&[
        0x80, 0x00, 0x00, 0x08, 0x00, 0x01, 0x00, 0x02, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ]);
    plain[0x50..0x54].copy_from_slice(&[0x1E, 0xFF, 0x2F, 0xE1]);
    let plain_image = Image::from_bytes(plain).expect("parse");
    let mut plain_map = FlagMap::from_image(&plain_image);
    let plain_seeds = plain_map.seed(&[]);
    let plain_analysis = analyze(
        &plain_image,
        &mut plain_map,
        &plain_seeds,
        &plain_seeds,
        Thresholds::default(),
        &[],
    );
    assert!(plain_analysis.copy_hints.is_empty());
    // A plain copy call still discovers the entry and the wrapper.
    assert_eq!(plain_analysis.discovery.functions.len(), 2);
}

#[test]
fn copy_hint_requires_ewram_destination_and_jump_inside() {
    // Same shape, but the BX target is outside the copied region, so no
    // hint fires.
    let mut bytes = RomBuilder::new().title("CPY1").build();
    bytes[0x00..0x1C].copy_from_slice(&[
        0x01, 0x00, 0xA0, 0xE3, 0x18, 0x00, 0x9F, 0xE5, 0x18, 0x10, 0x9F, 0xE5, 0x18, 0x20, 0x9F,
        0xE5, 0x0E, 0x00, 0x00, 0xEB, 0x14, 0x30, 0x9F, 0xE5, 0x13, 0xFF, 0x2F, 0xE1,
    ]);
    bytes[0x24..0x34].copy_from_slice(&[
        0x80, 0x00, 0x00, 0x08, 0x00, 0x01, 0x00, 0x02, 0x40, 0x00, 0x00, 0x00, 0x20, 0x02, 0x00,
        0x02,
    ]);
    bytes[0x50..0x54].copy_from_slice(&[0x1E, 0xFF, 0x2F, 0xE1]);
    let image = Image::from_bytes(bytes).expect("parse");
    let mut map = FlagMap::from_image(&image);
    let seeds = map.seed(&[]);
    let analysis = analyze(&image, &mut map, &seeds, &seeds, Thresholds::default(), &[]);
    assert!(analysis.copy_hints.is_empty());
}
