//! Round-trip harness: decode -> text -> assemble -> identical bytes.
//!
//! One or more samples per ARM and THUMB instruction class are rendered
//! with [`decode::arm_text`] / [`decode::thumb_text`], assembled, and
//! the resulting `.text` bytes are compared to the original encodings.
//!
//! Backend probing: GNU `arm-none-eabi-as` first (the reference
//! assembler), then clang's integrated assembler, which accepts the same
//! unified-syntax input with two known gaps under clang, both excluded
//! with a loud note there and fully covered under GNU as:
//!
//! - THUMB `add rd, pc, #imm`: clang only offers the `adr`
//!   pseudo-instruction, whose aligned-PC semantics cannot express
//!   arbitrary offset fields.
//! - THUMB conditional and unconditional branches: clang's Thumb-1 `b`
//!   rejects dot expressions, while GNU as discards the numeric `#imm`
//!   form entirely (every constant assembles to branch-to-self), so the
//!   renderer's exact form only exists under GNU as.
//!
//! Without any backend the harness prints a loud skip notice and passes;
//! set `OXYBOY_ROUNDTRIP_STRICT=1` to make a missing backend an error.

use std::path::PathBuf;
use std::process::Command;

use decode::{ThumbInstr, arm_text, decode_thumb, thumb_text};

/// Which assembler is driving the round-trip.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    GnuAs,
    Clang,
}

/// Locates an assembler on PATH: GNU as first, clang as fallback.
fn find_backend() -> Option<(Backend, PathBuf)> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let gnu_as = dir.join("arm-none-eabi-as");
        if gnu_as.exists() {
            return Some((Backend::GnuAs, gnu_as));
        }
        let clang = dir.join("clang");
        if clang.exists() {
            return Some((Backend::Clang, clang));
        }
    }
    None
}

/// Extracts the `.text` section bytes from an ELF32 object file.
fn elf_text_bytes(object: &[u8]) -> Vec<u8> {
    assert_eq!(&object[0..4], b"\x7fELF", "not an ELF object");
    let shoff = u32::from_le_bytes(object[0x20..0x24].try_into().unwrap()) as usize;
    let shentsize = u16::from_le_bytes(object[0x2E..0x30].try_into().unwrap()) as usize;
    let shnum = u16::from_le_bytes(object[0x30..0x32].try_into().unwrap()) as usize;
    let shstrndx = u16::from_le_bytes(object[0x32..0x34].try_into().unwrap()) as usize;
    let section = |index: usize| -> (u32, u32, u32) {
        let base = shoff + index * shentsize;
        (
            u32::from_le_bytes(object[base..base + 4].try_into().unwrap()),
            u32::from_le_bytes(object[base + 0x10..base + 0x14].try_into().unwrap()),
            u32::from_le_bytes(object[base + 0x14..base + 0x18].try_into().unwrap()),
        )
    };
    let (strtab_off, strtab_size) = {
        let base = shoff + shstrndx * shentsize;
        (
            u32::from_le_bytes(object[base + 0x10..base + 0x14].try_into().unwrap()),
            u32::from_le_bytes(object[base + 0x14..base + 0x18].try_into().unwrap()),
        )
    };
    let string = |offset: u32| -> String {
        let start = strtab_off as usize + offset as usize;
        let end = object[start..strtab_off as usize + strtab_size as usize]
            .iter()
            .position(|b| *b == 0)
            .unwrap();
        String::from_utf8(object[start..start + end].to_vec()).unwrap()
    };
    for index in 0..shnum {
        let (name, offset, size) = section(index);
        if string(name) == ".text" {
            return object[offset as usize..(offset + size) as usize].to_vec();
        }
    }
    panic!("object has no .text section");
}

/// Assembles `source` and returns the `.text` bytes.
fn assemble(backend: (Backend, &PathBuf), source: &str, workdir: &std::path::Path) -> Vec<u8> {
    let src = workdir.join("rt.s");
    let obj = workdir.join("rt.o");
    std::fs::write(&src, source).expect("write round-trip source");
    let (kind, path) = backend;
    let mut command = match kind {
        Backend::GnuAs => {
            let mut c = Command::new(path);
            c.arg("-mcpu=arm7tdmi");
            c
        }
        Backend::Clang => {
            let mut c = Command::new(path);
            c.args(["--target=arm-none-eabi", "-mcpu=arm7tdmi"]);
            c
        }
    };
    command.arg("-c").arg(&src).arg("-o").arg(&obj);
    let output = command.output().expect("spawn assembler");
    assert!(
        output.status.success(),
        "assembler failed:\nstdout: {}\nstderr: {}\nsource:\n{source}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let object = std::fs::read(&obj).expect("read object file");
    elf_text_bytes(&object)
}

/// Renders and assembles a word list, asserting byte identity. `skip`
/// reports samples deliberately excluded under the active backend.
fn roundtrip_arm(words: &[u32], backend: (Backend, &PathBuf), workdir: &std::path::Path) {
    let mut source = String::from(".syntax unified\n.cpu arm7tdmi\n.arm\n");
    for word in words {
        source.push_str(&arm_text(*word));
        source.push('\n');
    }
    let got = assemble(backend, &source, workdir);
    let expected: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    assert_eq!(
        got.len(),
        expected.len(),
        "assembled {} bytes for {} ARM words",
        got.len(),
        words.len()
    );
    for (index, (g, e)) in got.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            g,
            e,
            "ARM word {} ({:#010X}) did not round-trip",
            index / 4,
            words[index / 4]
        );
    }
}

/// Renders and assembles a THUMB halfword stream, asserting byte
/// identity. `skip` marks encodings excluded under the active backend.
fn roundtrip_thumb(halfwords: &[u16], backend: (Backend, &PathBuf), workdir: &std::path::Path) {
    let skip_pc_add = backend.0 == Backend::Clang;
    if skip_pc_add {
        println!(
            "note: clang backend excludes THUMB `add rd, pc, #imm` and \
             THUMB branch samples (GNU as only); install \
             arm-none-eabi-binutils for full coverage"
        );
    }
    let mut source = String::from(".syntax unified\n.cpu arm7tdmi\n.thumb\n");
    let mut expected: Vec<u8> = Vec::new();
    let mut index = 0;
    while index < halfwords.len() {
        let first = halfwords[index];
        let next = halfwords.get(index + 1).copied().unwrap_or(0);
        // THUMB PC-relative add and THUMB branches: GNU-only samples,
        // kept out of the clang stream.
        if skip_pc_add && matches!(first, 0xA001 | 0xD0FF | 0xD1FD | 0xE7FE | 0xE003) {
            index += 1;
            continue;
        }
        match decode_thumb(first, next) {
            ThumbInstr::LongBranchWithLink { .. } => {
                source.push_str(&thumb_text(first, next));
                source.push('\n');
                expected.extend_from_slice(&first.to_le_bytes());
                expected.extend_from_slice(&next.to_le_bytes());
                index += 2;
            }
            _ => {
                source.push_str(&thumb_text(first, next));
                source.push('\n');
                expected.extend_from_slice(&first.to_le_bytes());
                index += 1;
            }
        }
    }
    let got = assemble(backend, &source, workdir);
    if got.len() != expected.len() || got != expected {
        let dump = |bytes: &[u8]| -> String {
            bytes
                .chunks(2)
                .map(|c| format!("{:02x}{:02x}", c[0], c.get(1).copied().unwrap_or(0)))
                .collect::<Vec<_>>()
                .join(" ")
        };
        panic!(
            "THUMB round-trip mismatch\ngot:      {}\nexpected: {}\nsource:\n{source}",
            dump(&got),
            dump(&expected)
        );
    }
}

#[test]
fn arm_and_thumb_roundtrip() {
    let Some(found) = find_backend() else {
        let message = "round-trip skipped: no ARM assembler found \
             (install arm-none-eabi-binutils, or clang as fallback)";
        if std::env::var_os("OXYBOY_ROUNDTRIP_STRICT").is_some() {
            panic!("{message}");
        }
        println!("{message}");
        return;
    };
    println!(
        "round-trip backend: {} ({})",
        found.1.display(),
        Command::new(&found.1)
            .arg("--version")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.lines().next().map(str::to_string))
            .unwrap_or_else(|| "version unknown".to_string())
    );
    let workdir = std::env::temp_dir().join("oxyboy-roundtrip");
    std::fs::create_dir_all(&workdir).expect("create workdir");

    // One or more samples per ARM class; only predictable encodings
    // (UNPREDICTABLE forms have no reproducible assembly).
    let arm_words: &[u32] = &[
        // Data processing.
        0xE3A0_0001, // mov r0, #1
        0x03A0_0001, // moveq r0, #1
        0xE3E0_20FF, // mvn r2, #255
        0xE284_3305, // add r3, r4, #335544320 (rotated immediate)
        0xE1A0_0000, // mov r0, r0
        0xE1A0_0021, // mov r0, r1, lsr #32
        0xE1A0_0061, // mov r0, r1, rrx
        0xE084_3655, // add r3, r4, r5, asr r6
        0xE052_23F7, // adcs r2, r2, r7, ror r3 (register shift)
        // Multiply, multiply long, swap.
        0xE037_4392, // mlas r7, r2, r3, r4
        0xE0B5_4796, // umlals r4, r5, r6, r7
        0xE103_1092, // swp r1, r2, [r3]
        // PSR transfer.
        0xE14F_0000, // mrs r0, spsr
        0xE12F_F000, // msr cpsr_fsxc, r0
        0xE361_F010, // msr spsr_c, #16
        // Single word/byte transfer.
        0xE591_0000, // ldr r0, [r1, #0]
        0xE5A1_0004, // str r0, [r1, #4]!
        0xE4D2_0000, // ldrb r0, [r2], #0
        0xE4B1_0004, // ldrt r0, [r1], #4
        0xE421_0004, // strt r0, [r1], #-4
        0xE731_0182, // ldr r0, [r1, -r2, lsl #3]!
        // Halfword/signed transfer.
        0xE1F1_00B4, // ldrh r0, [r1, #4]!
        0xE141_00B4, // strh r0, [r1, #-4]
        0xE091_00F2, // ldrsh r0, [r1], r2
        0xE194_30D5, // ldrsb r3, [r4, r5]
        // Block transfer.
        0xE8F0_8002, // ldmia r0!, {r1, pc}^
        0xE9AD_000F, // stmib sp!, {r0, r1, r2, r3}
        0xE8D0_0006, // ldmia r0, {r1, r2}^
        // Branch and exchange.
        0xEA00_0000, // b .+8
        0xEB00_0000, // bl .+8
        0xEAFF_FFFF, // b .+4
        0x8AFF_FFFF, // bhi .+4
        0xEBFF_FFFF, // bl .+4
        0xE12F_FF13, // bx r3
        // SWI, undefined raw forms.
        0xEF12_3456, // swi #0x123456
        0xEE00_0000, // .inst 0xEE000000
        0xFF00_0000, // .inst 0xFF000000
        0xE180_0090, // .inst 0xE1800090
    ];
    // THUMB samples, one per format; the PC-relative add stays first so
    // GNU as sees it 4-aligned.
    let thumb_halfwords: &[u16] = &[
        0xA001, // add r0, pc, #4 (GNU as only)
        0x0008, // lsls r0, r1, #0
        0x0C03, // lsls r3, r0, #3
        0x0848, // lsrs r0, r1, #1
        0x1048, // asrs r0, r1, #1
        0x1C08, // adds r0, r1, #0
        0x1808, // adds r0, r1, r0
        0x1FC8, // subs r0, r1, #7
        0x2001, // movs r0, #1
        0x2800, // cmp r0, #0
        0x3001, // adds r0, #1
        0x3801, // subs r0, #1
        0x4008, // ands r0, r1
        0x4248, // rsbs r0, r1, #0
        0x4352, // muls r2, r2
        0x4208, // tst r0, r1
        0x43C8, // mvns r0, r1
        0x4480, // add r8, r0
        0x4588, // cmp r8, r1
        0x469C, // mov r12, r3
        0x4718, // bx r3
        0x4770, // bx lr
        0x4801, // ldr r0, [pc, #4]
        0x5088, // str r0, [r1, r2]
        0x5C88, // ldrb r0, [r1, r2]
        0x5688, // ldrsb r0, [r1, r2]
        0x5888, // ldr r0, [r1, r2]
        0x5A88, // ldrh r0, [r1, r2]
        0x5288, // strh r0, [r1, r2]
        0x6088, // str r0, [r1, #8]
        0x78C8, // ldrb r0, [r1, #3]
        0x80C8, // strh r0, [r1, #6]
        0x8888, // ldrh r0, [r1, #4]
        0x9001, // str r0, [sp, #4]
        0x9404, // str r4, [sp, #16]
        0x9802, // ldr r0, [sp, #8]
        0xA803, // add r0, sp, #12
        0xB008, // add sp, #32
        0xB088, // sub sp, #32
        0xB510, // push {r4, lr}
        0xBD10, // pop {r4, pc}
        0xB410, // push {r4}
        0xB500, // push {lr}
        0xCB03, // ldmia r3!, {r0, r1}
        0xC303, // stmia r3!, {r0, r1}
        0xD0FF, // beq #-2
        0xD1FD, // bne #-6
        0xE7FE, // b #-4
        0xE003, // b #6
        0xDE00, // .2byte 0xDE00
        0xE800, // .2byte 0xE800
        0xF7FF, 0xFFCE, // bl #-100
        0xF000, 0xF800, // bl #0
    ];

    roundtrip_arm(arm_words, (found.0, &found.1), &workdir);
    roundtrip_thumb(thumb_halfwords, (found.0, &found.1), &workdir);
}
