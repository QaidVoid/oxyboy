//! BIOS-call HLE tests (GBATEK "BIOS Functions"): every expected
//! value below comes from the GBATEK entry of the call, the mGBA HLE
//! reference arithmetic, or direct execution of the call against a
//! hand-built compressed stream.

use runtime::bus::Bus;
use runtime::cpu::{Cpu, Mode};

/// Runs one ARM `SWI number` at 0x08000000 with `extra` mapped at ROM
/// offset 0x100 (decompression streams live there), returning the CPU
/// and bus after the call.
fn run_arm_swi(number: u32, extra: &[u8], setup: impl FnOnce(&mut Cpu, &mut Bus)) -> (Cpu, Bus) {
    let mut rom = vec![0; 0x400];
    rom[..4].copy_from_slice(&(0xEF00_0000u32 | (number << 16)).to_le_bytes());
    rom[0x100..0x100 + extra.len()].copy_from_slice(extra);
    let mut bus = Bus::new(rom);
    let mut cpu = Cpu::new(0x0800_0000);
    setup(&mut cpu, &mut bus);
    cpu.step(&mut bus);
    (cpu, bus)
}

/// The LZ77 test stream: header type 1, 11 bytes out; eight literals
/// "ABCDA B!" then a compressed block copying 3 bytes from disp 4
/// (source bytes 3..6 = "D A").
fn lz77_stream() -> Vec<u8> {
    let mut v = 0x0000_0B10u32.to_le_bytes().to_vec();
    v.push(0x00);
    v.extend_from_slice(b"ABCDA B!");
    v.push(0x80);
    v.push(0x00); // block: len (3-3) in bits 12-15, disp 4
    v.push(0x04);
    v
}

#[test]
fn cpu_fast_set_copies_and_fills_in_word_units() {
    // Copy: 8 words EWRAM -> EWRAM.
    let (_, mut bus) = run_arm_swi(0x0C, &[], |cpu, bus| {
        for i in 0..8 {
            bus.write_u32(0x0200_0000 + 4 * i, 0x1111_0000 + i);
        }
        cpu.set_reg_for_test(0, 0x0200_0000);
        cpu.set_reg_for_test(1, 0x0200_0100);
        cpu.set_reg_for_test(2, 8);
    });
    for i in 0..8 {
        assert_eq!(bus.read_u32(0x0200_0100 + 4 * i), 0x1111_0000 + i);
    }

    // Fill: bit 24 fixes the source word; the GBA rounds the word
    // count up to a multiple of 8 (GBATEK CpuFastSet), so 3 becomes
    // 8 words and 32 bytes land.
    let (_, mut bus) = run_arm_swi(0x0C, &[], |cpu, bus| {
        bus.write_u32(0x0200_0000, 0x5A5A_A5A5);
        cpu.set_reg_for_test(0, 0x0200_0000);
        cpu.set_reg_for_test(1, 0x0200_0200);
        cpu.set_reg_for_test(2, 0x0100_0000 | 3);
    });
    for i in 0..8 {
        assert_eq!(bus.read_u32(0x0200_0200 + 4 * i), 0x5A5A_A5A5);
    }
}

#[test]
fn cpu_set_halfword_and_word_modes_with_fill() {
    // 16-bit copy of five halfwords.
    let (_, mut bus) = run_arm_swi(0x0B, &[], |cpu, bus| {
        for i in 0..5 {
            bus.write_u16(0x0200_0000 + 2 * i, 0x2200 + i as u16);
        }
        cpu.set_reg_for_test(0, 0x0200_0000);
        cpu.set_reg_for_test(1, 0x0200_0100);
        cpu.set_reg_for_test(2, 5);
    });
    for i in 0..5 {
        assert_eq!(bus.read_u16(0x0200_0100 + 2 * i), 0x2200 + i as u16);
    }

    // 32-bit fill: bit 26 selects word units, bit 24 fixes the source.
    let (_, mut bus) = run_arm_swi(0x0B, &[], |cpu, bus| {
        bus.write_u32(0x0200_0000, 0xDEAD_C0DE);
        cpu.set_reg_for_test(0, 0x0200_0000);
        cpu.set_reg_for_test(1, 0x0200_0200);
        cpu.set_reg_for_test(2, 0x0500_0000 | 3);
    });
    for i in 0..3 {
        assert_eq!(bus.read_u32(0x0200_0200 + 4 * i), 0xDEAD_C0DE);
    }

    // The BIOS rejects sources reaching into the BIOS area (GBATEK
    // CpuSet note); the destination stays untouched.
    let (_, mut bus) = run_arm_swi(0x0B, &[], |cpu, _| {
        cpu.set_reg_for_test(0, 0x0000_0100);
        cpu.set_reg_for_test(1, 0x0200_0300);
        cpu.set_reg_for_test(2, 2);
    });
    assert_eq!(bus.read_u32(0x0200_0300), 0);
}

#[test]
fn div_returns_quotient_remainder_and_abs() {
    // GBATEK SWI 06h example: -1234 / 10 = -123, -4, +123.
    let (cpu, _) = run_arm_swi(0x06, &[], |cpu, _| {
        cpu.set_reg_for_test(0, (-1234i32) as u32);
        cpu.set_reg_for_test(1, 10);
    });
    assert_eq!(cpu.reg_for_test(0), (-123i32) as u32);
    assert_eq!(cpu.reg_for_test(1), (-4i32) as u32);
    assert_eq!(cpu.reg_for_test(3), 123);

    let (cpu, _) = run_arm_swi(0x06, &[], |cpu, _| {
        cpu.set_reg_for_test(0, 7);
        cpu.set_reg_for_test(1, (-2i32) as u32);
    });
    assert_eq!(cpu.reg_for_test(0), (-3i32) as u32);
    assert_eq!(cpu.reg_for_test(1), 1);
    assert_eq!(cpu.reg_for_test(3), 3);

    // DivArm exchanges the operands (r1/r0, GBATEK SWI 07h).
    let (cpu, _) = run_arm_swi(0x07, &[], |cpu, _| {
        cpu.set_reg_for_test(0, 10);
        cpu.set_reg_for_test(1, (-1234i32) as u32);
    });
    assert_eq!(cpu.reg_for_test(0), (-123i32) as u32);
    assert_eq!(cpu.reg_for_test(1), (-4i32) as u32);
    assert_eq!(cpu.reg_for_test(3), 123);

    // INT_MIN / -1 and division by zero: the real BIOS hangs; the HLE
    // returns defined stand-ins without locking the machine up.
    let (cpu, _) = run_arm_swi(0x06, &[], |cpu, _| {
        cpu.set_reg_for_test(0, i32::MIN as u32);
        cpu.set_reg_for_test(1, (-1i32) as u32);
    });
    assert_eq!(cpu.reg_for_test(0), i32::MIN as u32);
    assert_eq!(cpu.reg_for_test(1), 0);
    let (cpu, _) = run_arm_swi(0x06, &[], |cpu, _| {
        cpu.set_reg_for_test(0, 12);
        cpu.set_reg_for_test(1, 0);
    });
    assert_eq!(
        cpu.reg_for_test(1),
        12,
        "the dividend stands in as remainder"
    );
}

#[test]
fn sqrt_floors_the_root() {
    for (x, want) in [
        (0u32, 0u32),
        (1, 1),
        (2, 1),
        (3, 1),
        (4, 2),
        (15, 3),
        (16, 4),
        (0xFFFF, 0xFF),
        (0x1_0000, 0x100),
        (0xFFFF_FFFF, 0xFFFF),
    ] {
        let (cpu, _) = run_arm_swi(0x08, &[], |cpu, _| {
            cpu.set_reg_for_test(0, x);
        });
        assert_eq!(cpu.reg_for_test(0), want, "sqrt({x:#x})");
    }
}

#[test]
fn arctan2_quadrant_edges_and_angles() {
    // Axis cases return directly (GBATEK SWI 0Ah: 0000h-FFFFh over
    // 0..2pi).
    for (x, y, want) in [
        (0x4000i32, 0i32, 0x0000u32),
        (-0x4000, 0, 0x8000),
        (0, 0x4000, 0x4000),
        (0, -0x4000, 0xC000),
    ] {
        let (cpu, _) = run_arm_swi(0x0A, &[], |cpu, _| {
            cpu.set_reg_for_test(0, x as u32);
            cpu.set_reg_for_test(1, y as u32);
        });
        assert_eq!(cpu.reg_for_test(0), want, "arctan2({x}, {y})");
    }
    // Diagonals: 45 degrees = 0x2000, 135 = 0x6000, 225 = 0xA000,
    // 315 = 0xE000. The BIOS polynomial approximates, so allow small
    // slack around the exact angle.
    for (x, y, want) in [
        (0x4000i32, 0x4000i32, 0x2000u32),
        (-0x4000, 0x4000, 0x6000),
        (-0x4000, -0x4000, 0xA000),
        (0x4000, -0x4000, 0xE000),
    ] {
        let (cpu, _) = run_arm_swi(0x0A, &[], |cpu, _| {
            cpu.set_reg_for_test(0, x as u32);
            cpu.set_reg_for_test(1, y as u32);
        });
        let got = cpu.reg_for_test(0);
        let dist = got.wrapping_sub(want);
        let dist = dist.min(u32::MAX - dist) as i64;
        assert!(dist <= 0x40, "arctan2({x}, {y}) = {got:#x}, want {want:#x}");
    }
    // ArcTan of 1 (tan = 0x4000 in 2.14) is pi/4 = 0x2000.
    let (cpu, _) = run_arm_swi(0x09, &[], |cpu, _| {
        cpu.set_reg_for_test(0, 0x4000);
    });
    let got = cpu.reg_for_test(0);
    let dist = got.wrapping_sub(0x2000).min(0x2000u32.wrapping_sub(got)) as i64;
    assert!(dist <= 0x40, "arctan(1) = {got:#x}, want 0x2000");
}

#[test]
fn intr_wait_returns_immediately_on_an_old_flag() {
    let (cpu, mut bus) = run_arm_swi(0x04, &[], |cpu, bus| {
        bus.write_u16(0x0300_7FF8, 0x0001);
        cpu.set_reg_for_test(0, 0);
        cpu.set_reg_for_test(1, 0x0001);
    });
    // No halt: the call returned, and the covered flag was consumed.
    assert_eq!(cpu.pc(), 0x0800_0004);
    assert_eq!(bus.read_u16(0x0300_7FF8), 0);
}

#[test]
fn intr_wait_halt_wakes_only_on_a_matching_flag() {
    let (mut cpu, mut bus) = run_arm_swi(0x04, &[], |cpu, bus| {
        cpu.set_reg_for_test(0, 1); // discard old flags
        cpu.set_reg_for_test(1, 0x0001);
        bus.write_u16(0x0400_0200, 0x0003); // IE: vblank + hblank
    });
    assert_eq!(cpu.pc(), 0x0800_0004, "halted at the return address");
    // Non-matching IF stays halted.
    bus.raise_if(0x0002);
    let before = cpu.cycles();
    cpu.step(&mut bus);
    assert_eq!(cpu.pc(), 0x0800_0004, "still halted");
    assert!(cpu.cycles() > before);
    // The waited flag releases the halt and is consumed.
    bus.raise_if(0x0001);
    cpu.step(&mut bus);
    assert_eq!(cpu.pc(), 0x0800_0008, "woke and moved on");
    assert_eq!(bus.read_u16(0x0300_7FF8) & 1, 0, "flag cleared on return");
}

#[test]
fn vblank_intr_wait_discards_an_old_vblank_flag() {
    let (mut cpu, mut bus) = run_arm_swi(0x05, &[], |_, bus| {
        bus.write_u16(0x0300_7FF8, 0x0001); // stale V-Blank flag
        bus.write_u16(0x0400_0200, 0x0001);
    });
    // r0=1, r1=1 per GBATEK SWI 05h; the old flag did not release it.
    assert_eq!(cpu.reg_for_test(0), 1);
    assert_eq!(cpu.reg_for_test(1), 1);
    assert_eq!(cpu.pc(), 0x0800_0004, "halted despite the old flag");
    // A fresh V-Blank IRQ wakes it.
    bus.raise_if(0x0001);
    cpu.step(&mut bus);
    assert_eq!(cpu.pc(), 0x0800_0008);
}

#[test]
fn halt_wakes_on_any_enabled_irq_even_with_ime_clear() {
    let (mut cpu, mut bus) = run_arm_swi(0x02, &[], |_, bus| {
        bus.write_u16(0x0400_0200, 0x0010); // IE: timer 0
        // IME deliberately 0: Halt passes through (GBATEK SWI 02h).
    });
    assert_eq!(cpu.pc(), 0x0800_0004, "halted");
    bus.raise_if(0x0001); // wrong bit: stays halted
    cpu.step(&mut bus);
    assert_eq!(cpu.pc(), 0x0800_0004);
    bus.raise_if(0x0010);
    cpu.step(&mut bus);
    assert_eq!(cpu.pc(), 0x0800_0008, "woke");
}

#[test]
fn soft_reset_clears_the_iwram_tail_and_redirects() {
    // Flag 0: reset into ROM, System mode, documented stacks.
    let (cpu, mut bus) = run_arm_swi(0x00, &[], |cpu, bus| {
        bus.write_u8(0x0300_7FFA, 0);
        bus.write_u32(0x0300_7E00, 0xFFFF_FFFF);
        bus.write_u32(0x0300_7D00, 0x1234_5678); // below the tail
        cpu.set_reg_for_test(4, 0xdead_beef);
    });
    assert_eq!(cpu.pc(), 0x0800_0000);
    assert_eq!(cpu.mode(), Mode::System);
    assert_eq!(cpu.reg_for_test(13), 0x0300_7F00, "SP_sys");
    assert_eq!(cpu.reg_for_test(14), 0);
    assert_eq!(cpu.reg_for_test(4), 0, "r0-r12 zeroed");
    assert_eq!(cpu.cpsr() & 0x80, 0x80, "IRQs masked on the reset path");
    assert_eq!(bus.read_u32(0x0300_7E00), 0, "IWRAM tail cleared");
    assert_eq!(bus.read_u32(0x0300_7D00), 0x1234_5678, "below preserved");

    // Flag nonzero: reset into EWRAM (GBATEK SWI 00h).
    let (cpu, _) = run_arm_swi(0x00, &[], |_, bus| {
        bus.write_u8(0x0300_7FFA, 1);
    });
    assert_eq!(cpu.pc(), 0x0200_0000);
}

#[test]
fn register_ram_reset_clears_memory_and_registers_per_flag() {
    let (_, mut bus) = run_arm_swi(0x01, &[], |cpu, bus| {
        for i in 0..0x10 {
            bus.write_u32(0x0200_0000 + 4 * i, 0xFF);
            bus.write_u32(0x0500_0000 + 4 * i, 0xFF);
            bus.write_u32(0x0600_0000 + 4 * i, 0xFF);
            bus.write_u32(0x0700_0000 + 4 * i, 0xFF);
        }
        bus.write_u32(0x0300_0000, 0xFF);
        bus.write_u32(0x0300_7E00, 0xFF); // tail must survive
        cpu.set_reg_for_test(0, 0x1F);
    });
    assert_eq!(bus.read_u32(0x0200_0000), 0);
    assert_eq!(bus.read_u32(0x0500_0000), 0);
    assert_eq!(bus.read_u32(0x0600_0000), 0);
    assert_eq!(bus.read_u32(0x0700_0000), 0);
    assert_eq!(bus.read_u32(0x0300_0000), 0);
    assert_eq!(bus.read_u32(0x0300_7E00), 0xFF, "tail excluded");
    assert_eq!(bus.read_u16(0x0400_0000), 0x0080, "forced blank");

    // Flag 0x80: interrupt control and WAITCNT reset, IF acked.
    let (_, mut bus) = run_arm_swi(0x01, &[], |cpu, bus| {
        bus.write_u16(0x0400_0200, 0x1234);
        bus.write_u16(0x0400_0202, 0x1234);
        bus.write_u16(0x0400_0204, 0x4317);
        bus.write_u16(0x0400_0208, 1);
        cpu.set_reg_for_test(0, 0x80);
    });
    assert_eq!(bus.read_u16(0x0400_0200), 0, "IE");
    assert_eq!(bus.read_u16(0x0400_0202), 0, "IF acked");
    assert_eq!(bus.read_u16(0x0400_0204), 0, "WAITCNT");
    assert_eq!(bus.read_u16(0x0400_0208), 0, "IME");
}

#[test]
fn sound_bias_sets_the_requested_level() {
    let (_, mut bus) = run_arm_swi(0x19, &[], |cpu, _| {
        cpu.set_reg_for_test(0, 0);
    });
    assert_eq!(bus.read_u16(0x0400_0088) & 0x03FF, 0x0000);

    let (_, mut bus) = run_arm_swi(0x19, &[], |cpu, bus| {
        bus.write_u16(0x0400_0088, 0x0400); // upper bits to keep
        cpu.set_reg_for_test(0, 1);
    });
    assert_eq!(bus.read_u16(0x0400_0088), 0x0400 | 0x0200);
}

#[test]
fn lz77_wram_decodes_literals_and_backrefs() {
    let (cpu, mut bus) = run_arm_swi(0x11, &lz77_stream(), |cpu, _| {
        cpu.set_reg_for_test(0, 0x0800_0100);
        cpu.set_reg_for_test(1, 0x0200_0000);
    });
    let mut want = b"ABCDA B!".to_vec();
    want.extend_from_slice(b"DA ");
    for (i, w) in want.iter().enumerate() {
        assert_eq!(bus.read_u8(0x0200_0000 + i as u32), *w, "byte {i}");
    }
    assert_eq!(cpu.reg_for_test(1), 0x0200_0000 + 11, "dest advanced");
    assert_eq!(cpu.reg_for_test(3), 0);
}

#[test]
fn lz77_vram_writes_halfword_lanes_with_the_same_bytes() {
    // The same stream into VRAM at an odd destination: the bytes must
    // land at consecutive addresses through halfword lane writes, and
    // the backref must read through the containing halfword.
    let (_, mut bus) = run_arm_swi(0x12, &lz77_stream(), |cpu, _| {
        cpu.set_reg_for_test(0, 0x0800_0100);
        cpu.set_reg_for_test(1, 0x0600_0101);
    });
    let mut want = b"ABCDA B!".to_vec();
    want.extend_from_slice(b"DA ");
    for (i, w) in want.iter().enumerate() {
        let addr = 0x0600_0101 + i as u32;
        let h = bus.read_u16(addr & !1);
        let got = if addr & 1 == 0 {
            h as u8
        } else {
            (h >> 8) as u8
        };
        assert_eq!(got, *w, "VRAM byte {i}");
    }
}

#[test]
fn rl_uncomp_runs_literals_and_zero_padding() {
    // Header type 3, 9 bytes out: a 5-byte run of 0xAA, three
    // literals, one literal; then 3 zero bytes pad to the 4-byte
    // boundary.
    let mut stream = 0x0000_0903u32.to_le_bytes().to_vec();
    stream.push(0x82);
    stream.push(0xAA);
    stream.push(0x02);
    stream.push(0x11);
    stream.push(0x22);
    stream.push(0x33);
    stream.push(0x00);
    stream.push(0x44);
    let (cpu, mut bus) = run_arm_swi(0x14, &stream, |cpu, _| {
        cpu.set_reg_for_test(0, 0x0800_0100);
        cpu.set_reg_for_test(1, 0x0200_0000);
    });
    let mut want = vec![0xAA; 5];
    want.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]);
    want.extend_from_slice(&[0, 0, 0]);
    for (i, w) in want.iter().enumerate() {
        assert_eq!(bus.read_u8(0x0200_0000 + i as u32), *w, "byte {i}");
    }
    assert_eq!(cpu.reg_for_test(1), 0x0200_0000 + 12, "dest past padding");
}

#[test]
fn huffman_walks_the_tree_in_four_bit_units() {
    // Tree: root 0xC0 (both children terminal, offset 0); child0 =
    // 0x05, child1 = 0x0A. The bitstream 0x55555555 is alternating
    // 0,1 bits, so every unit decodes left then right and each byte
    // assembles 0xA5. 8 bytes out take 16 stream bits.
    let mut stream = 0x0000_0824u32.to_le_bytes().to_vec();
    stream.push(0x01); // (1<<1)+1 = 3 tree bytes
    stream.push(0xC0);
    stream.push(0x05);
    stream.push(0x0A);
    stream.extend_from_slice(&0x5555_5555u32.to_le_bytes());
    let (cpu, mut bus) = run_arm_swi(0x13, &stream, |cpu, _| {
        cpu.set_reg_for_test(0, 0x0800_0100);
        cpu.set_reg_for_test(1, 0x0200_0000);
    });
    for i in 0..8 {
        assert_eq!(bus.read_u8(0x0200_0000 + i), 0xA5, "byte {i}");
    }
    assert_eq!(cpu.reg_for_test(1), 0x0200_0008);
}

#[test]
fn diff_filters_accumulate_deltas() {
    // SWI 16 (8->8): 10, +1, +1, -1 (0xFF wraps).
    let mut stream = 0x0000_0481u32.to_le_bytes().to_vec();
    stream.extend_from_slice(&[0x0A, 0x01, 0x01, 0xFF]);
    let (_, bus) = run_arm_swi(0x16, &stream, |cpu, _| {
        cpu.set_reg_for_test(0, 0x0800_0100);
        cpu.set_reg_for_test(1, 0x0200_0000);
    });
    assert_eq!(
        &bus.ewram()[..4],
        &[0x0A, 0x0B, 0x0C, 0x0B],
        "8-bit accumulation"
    );

    // SWI 18 (16->16): halfword deltas.
    let mut stream = 0x0000_0882u32.to_le_bytes().to_vec();
    stream.extend_from_slice(&0x000Au16.to_le_bytes());
    stream.extend_from_slice(&0x0001u16.to_le_bytes());
    stream.extend_from_slice(&0x00FFu16.to_le_bytes());
    stream.extend_from_slice(&0x0002u16.to_le_bytes());
    let (_, mut bus) = run_arm_swi(0x18, &stream, |cpu, _| {
        cpu.set_reg_for_test(0, 0x0800_0100);
        cpu.set_reg_for_test(1, 0x0200_0000);
    });
    assert_eq!(bus.read_u16(0x0200_0000), 0x000A);
    assert_eq!(bus.read_u16(0x0200_0002), 0x000B);
    assert_eq!(bus.read_u16(0x0200_0004), 0x010A);
    assert_eq!(bus.read_u16(0x0200_0006), 0x010C);

    // SWI 17 (8->16): byte pairs widen to halfword lanes in VRAM.
    let mut stream = 0x0000_0481u32.to_le_bytes().to_vec();
    stream.extend_from_slice(&[0x10, 0x01, 0x02, 0x01]);
    let (_, mut bus) = run_arm_swi(0x17, &stream, |cpu, _| {
        cpu.set_reg_for_test(0, 0x0800_0100);
        cpu.set_reg_for_test(1, 0x0600_0000);
    });
    assert_eq!(bus.read_u16(0x0600_0000), 0x1110);
    assert_eq!(bus.read_u16(0x0600_0002), 0x1413);
}

#[test]
fn diff16_with_an_odd_size_stops_after_the_last_halfword() {
    // Header size 3 is not a halfword multiple; the unit that covers
    // the odd byte is the last one written.
    let mut stream = 0x0000_0382u32.to_le_bytes().to_vec();
    stream.extend_from_slice(&0x0005u16.to_le_bytes());
    stream.extend_from_slice(&0x0001u16.to_le_bytes());
    stream.extend_from_slice(&0x0001u16.to_le_bytes());
    let (_, mut bus) = run_arm_swi(0x18, &stream, |cpu, _| {
        cpu.set_reg_for_test(0, 0x0800_0100);
        cpu.set_reg_for_test(1, 0x0200_0000);
    });
    assert_eq!(bus.read_u16(0x0200_0000), 0x0005);
    assert_eq!(bus.read_u16(0x0200_0002), 0x0006);
    assert_eq!(bus.read_u16(0x0200_0004), 0x0000);
}

#[test]
fn thumb_swi_dispatches_the_same_calls() {
    // THUMB SWI carries the number in 8 bits; a Div through the THUMB
    // encoding must produce the same result.
    let mut rom = vec![0; 0x40];
    rom[0] = 0x06; // swi 0x06 (halfword 0xDF06)
    rom[1] = 0xDF;
    let mut bus = Bus::new(rom);
    let mut cpu = Cpu::new(0x0800_0000);
    cpu.set_cpsr_raw(0x33);
    cpu.set_reg_for_test(0, (-1234i32) as u32);
    cpu.set_reg_for_test(1, 10);
    cpu.step(&mut bus);
    assert_eq!(cpu.reg_for_test(0), (-123i32) as u32);
    assert_eq!(cpu.reg_for_test(3), 123);
}

#[test]
fn arm_swi_number_lives_in_comment_bits() {
    // ARM toolchains encode `swi 6` as 0xEF060000: the call number
    // sits in bits 16-23 and the low bits stay zero. Dispatching on
    // the low byte instead would treat this call as SoftReset 00h and
    // wipe the registers.
    let mut rom = vec![0; 0x40];
    rom[..4].copy_from_slice(&0xEF06_0000u32.to_le_bytes());
    rom[4..8].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes());
    let mut bus = Bus::new(rom);
    let mut cpu = Cpu::new(0x0800_0000);
    cpu.set_reg_for_test(0, (-1234i32) as u32);
    cpu.set_reg_for_test(1, 10);
    cpu.set_reg_for_test(12, 0xDEAD_BEEF);
    cpu.step(&mut bus);
    assert_eq!(cpu.reg_for_test(0), (-123i32) as u32);
    assert_eq!(cpu.reg_for_test(1), (-4i32) as u32);
    assert_eq!(cpu.reg_for_test(3), 123);
    assert_eq!(cpu.reg_for_test(12), 0xDEAD_BEEF, "registers preserved");
}
