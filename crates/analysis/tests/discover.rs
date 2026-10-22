//! Golden discovery over a synthetic ROM with known functions.
//!
//! The ROM (assembled and objdump-verified with Binutils 2.32) holds:
//! an ARM entry that BLs a helper twice, BLs an undefined word once,
//! then BXes into THUMB code; a THUMB region where `thumb_main` BLs
//! `thumb_helper` twice. Discovery must find exactly the entry, the ARM
//! helper (BL target and gated push site), and the THUMB helper (BL
//! target), reject the undefined BL target, and leave `thumb_main`
//! undiscovered: it is a push site with no incoming BL evidence.
//!
//! ```text
//!     .arm
//! 00: mov r0, #1            @ multiboot-heuristic shield
//! 04: bl arm_helper         @ call 1 -> 0x24
//! 08: bl arm_helper         @ call 2
//! 0C: bl junk               @ call -> 0x30 (undefined)
//! 10: ldr r0, [pc, #4]      @ pool at 0x1C
//! 14: bx r0                 @ -> THUMB 0xC0
//! 18: .word 0
//! 1C: .word thumb_main + 1
//! 24: push {r4, lr}         @ ARM push site, 2 calls, length 3
//! 28: mov r4, #5
//! 2C: bx lr
//! 30: .word 0xFFFFFFFF      @ junk: BL target, undefined body
//!
//!     .thumb
//! C0: push {lr}             @ push site, 0 incoming calls: not a candidate
//! C2: bl thumb_helper       @ call 1 -> 0xE0
//! C6: bl thumb_helper       @ call 2
//! CA: pop {pc}
//! E0: push {lr}             @ BL target, 2 calls, length 3
//! E2: movs r1, #2
//! E4: pop {pc}
//! ```

use analysis::{
    Call, Config, FlagMap, Flags, Function, FunctionSpec, Seed, State, Thresholds, discover,
};
use rom::Image;
use testutil::synthetic::RomBuilder;

const F: Flags = Flags::FUNC_START;

fn discovery_rom() -> Image {
    let mut bytes = RomBuilder::new().title("DISC").build();
    bytes[0x00..0x34].copy_from_slice(&[
        0x01, 0x00, 0xA0, 0xE3, 0x06, 0x00, 0x00, 0xEB, 0x05, 0x00, 0x00, 0xEB, 0x07, 0x00, 0x00,
        0xEB, 0x04, 0x00, 0x9F, 0xE5, 0x10, 0xFF, 0x2F, 0xE1, 0x00, 0x00, 0x00, 0x00, 0xC1, 0x00,
        0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x10, 0x40, 0x2D, 0xE9, 0x05, 0x40, 0xA0, 0xE3, 0x1E,
        0xFF, 0x2F, 0xE1, 0xFF, 0xFF, 0xFF, 0xFF,
    ]);
    bytes[0xC0..0xCC].copy_from_slice(&[
        0x00, 0xB5, 0x00, 0xF0, 0x0D, 0xF8, 0x00, 0xF0, 0x0B, 0xF8, 0x00, 0xBD,
    ]);
    bytes[0xE0..0xE6].copy_from_slice(&[0x00, 0xB5, 0x02, 0x21, 0x00, 0xBD]);
    Image::from_bytes(bytes).expect("parse")
}

#[test]
fn discovery_finds_exactly_the_known_functions() {
    let image = discovery_rom();
    let mut map = FlagMap::from_image(&image);
    let seeds = map.seed(&[]);
    let discovery = discover(&image, &mut map, &seeds, Thresholds::default());

    assert_eq!(
        discovery.functions,
        vec![
            Function {
                addr: 0x0800_0000,
                state: State::Arm,
            },
            Function {
                addr: 0x0800_0024,
                state: State::Arm,
            },
            Function {
                addr: 0x0800_00E0,
                state: State::Thumb,
            },
        ]
    );
    assert_eq!(
        discovery.rejected,
        vec![Function {
            addr: 0x0800_0030,
            state: State::Arm,
        }]
    );
    assert_eq!(
        discovery.calls,
        vec![
            Call {
                site: 0x0800_0004,
                state: State::Arm,
                target: 0x0800_0024,
            },
            Call {
                site: 0x0800_0008,
                state: State::Arm,
                target: 0x0800_0024,
            },
            Call {
                site: 0x0800_000C,
                state: State::Arm,
                target: 0x0800_0030,
            },
            Call {
                site: 0x0800_00C2,
                state: State::Thumb,
                target: 0x0800_00E0,
            },
            Call {
                site: 0x0800_00C6,
                state: State::Thumb,
                target: 0x0800_00E0,
            },
        ]
    );

    // Accepted functions are marked as function starts; the evidence-less
    // push site at 0xC0 is not.
    assert!(map.get(0x0800_0024).contains(F));
    assert!(map.get(0x0800_00E0).contains(F));
    assert!(!map.get(0x0800_00C0).contains(F));
}

#[test]
fn config_out_reload_produces_no_new_discoveries() {
    let image = discovery_rom();
    let mut map = FlagMap::from_image(&image);
    let seeds = map.seed(&[]);
    let first = discover(&image, &mut map, &seeds, Thresholds::default());

    let config = Config {
        functions: first
            .functions
            .iter()
            .map(|f| FunctionSpec::bare(f.addr, f.state))
            .collect(),
    };
    let reloaded = Config::parse(&config.to_string()).expect("reload");
    let seeds: Vec<Seed> = reloaded
        .functions
        .iter()
        .map(FunctionSpec::to_seed)
        .collect();

    let mut map = FlagMap::from_image(&image);
    let _ = map.seed(&seeds);
    let second = discover(&image, &mut map, &seeds, Thresholds::default());
    assert_eq!(second.functions, first.functions);
    assert_eq!(second.rejected, first.rejected);
    assert_eq!(second.calls.len(), first.calls.len());
}

#[test]
fn provided_functions_are_accepted_without_scoring() {
    let image = discovery_rom();
    let mut map = FlagMap::from_image(&image);
    let user = [Seed {
        addr: 0x0800_0030,
        state: State::Arm,
    }];
    let seeds = map.seed(&user);
    let discovery = discover(&image, &mut map, &seeds, Thresholds::default());
    assert!(discovery.functions.contains(&Function {
        addr: 0x0800_0030,
        state: State::Arm,
    }));
    assert!(discovery.rejected.is_empty());
}

#[test]
fn push_site_gate_requires_decodable_length() {
    // thumb_main (0xC0) has no incoming BLs, so it is only ever a
    // push-site candidate: at min_calls 0 it is discovered while its
    // 5-instruction body meets min_length, and dropped past it. With the
    // default min_calls 2 the golden test already shows it stays
    // undiscovered.
    let image = discovery_rom();
    let lenient = Thresholds {
        min_calls: 0,
        min_length: 3,
        ..Thresholds::default()
    };
    let mut map = FlagMap::from_image(&image);
    let seeds = map.seed(&[]);
    let loose = discover(&image, &mut map, &seeds, lenient);
    assert!(loose.functions.contains(&Function {
        addr: 0x0800_00C0,
        state: State::Thumb,
    }));

    let strict = Thresholds {
        min_calls: 0,
        min_length: 6,
        ..Thresholds::default()
    };
    let mut map = FlagMap::from_image(&image);
    let seeds = map.seed(&[]);
    let tight = discover(&image, &mut map, &seeds, strict);
    assert!(!tight.functions.contains(&Function {
        addr: 0x0800_00C0,
        state: State::Thumb,
    }));
}
