//! BIOS-call high-level emulation (GBATEK "BIOS Functions").
//!
//! With no BIOS image mapped (the default), `SWI` dispatches here
//! instead of vectoring into the zero-filled BIOS array. Each function
//! models the documented register/memory behavior of its GBATEK entry;
//! the arithmetic (Div/Sqrt/ArcTan) and decompression algorithms follow
//! the mGBA HLE reference, the de-facto bit-exact model of the BIOS
//! routines. All sources are read and destinations written through the
//! bus, so region rules and VRAM write quirks apply.
//!
//! Cycle cost is coarse: a fixed call overhead, per-unit [`Bus`]
//! access costs for CpuSet/CpuFastSet, and a flat 10 cycles per output
//! byte for the decompressors.
//!
//! Not modeled (documented no-ops): the SIO reset bit of
//! RegisterRamReset (serial registers are not implemented), the affine
//! rotation/scaling functions (0Eh/0Fh), BitUnPack (10h), unaligned
//! Huffman bit widths, the sound driver work-area functions
//! (1Ah-1Fh), and MultiBoot (25h).

use crate::bus::Bus;
use crate::cpu::Cpu;

/// The BIOS interrupt-flag word the real IntrWait/VBlankIntrWait
/// protocol maintains at the IWRAM top (GBATEK SWI 04h).
pub(crate) const BIOS_INTR_FLAGS: u32 = 0x0300_7FF8;

/// Fixed modeled overhead of a BIOS call (entry, dispatch, return).
const CALL_OVERHEAD: u32 = 45;

/// Coarse per-output-byte cost of the decompression routines.
const DECOMP_CYCLES_PER_BYTE: u32 = 10;

/// Dispatches one BIOS call. `number` is the SWI comment field
/// already shifted per state: ARM callers supply bits 16-23 of the
/// instruction, THUMB callers bits 0-7; the real BIOS dispatches on
/// the same bytes read back from the instruction stream. Returns false
/// when the call redirected execution (SoftReset) so the caller skips
/// the sequential pc advance.
pub(crate) fn execute(cpu: &mut Cpu, bus: &mut Bus, number: u32) -> bool {
    // Entering the BIOS updates the read-protection latch; SoftReset
    // re-runs the boot path, which ends on the boot snapshot.
    bus.set_bios_latch(if number & 0xFF == 0x00 {
        crate::bus::BIOS_LATCH_BOOT
    } else {
        crate::bus::BIOS_LATCH_SWI
    });
    match number & 0xFF {
        0x00 => {
            soft_reset(cpu, bus);
            return false;
        }
        0x01 => register_ram_reset(cpu, bus),
        0x02 => {
            // Halt: wake on any enabled interrupt; IME and the I bit
            // are don't care (GBATEK SWI 02h).
            cpu.halt(None);
        }
        0x03 => {
            // Stop: modeled as Halt (the clock-stop level and its
            // narrower wake set are not modeled).
            cpu.halt(None);
        }
        0x04 => intr_wait(cpu, bus, false),
        0x05 => intr_wait(cpu, bus, true),
        0x06 => divide(cpu, cpu.gpr(0) as i32, cpu.gpr(1) as i32),
        0x07 => divide(cpu, cpu.gpr(1) as i32, cpu.gpr(0) as i32),
        0x08 => cpu.set_gpr(0, sqrt(cpu.gpr(0))),
        0x09 => {
            let (r, a, b) = arctan(cpu.gpr(0) as u16 as i16 as i32);
            cpu.set_gpr(0, r as u32);
            cpu.set_gpr(1, a as u32);
            cpu.set_gpr(3, b as u32);
        }
        0x0A => {
            let (r, a) = arctan2(
                cpu.gpr(0) as u16 as i16 as i32,
                cpu.gpr(1) as u16 as i16 as i32,
            );
            cpu.set_gpr(0, u32::from(r));
            if let Some(a) = a {
                cpu.set_gpr(1, a as u32);
            }
            cpu.set_gpr(3, 0x170);
        }
        0x0B => cpu_set(cpu, bus, false),
        0x0C => cpu_set(cpu, bus, true),
        0x0D => {
            // Documented GBA BIOS checksum (GBATEK SWI 0Dh); no BIOS
            // image exists to checksum in HLE, so the constant is the
            // answer rather than a computation.
            cpu.set_gpr(0, 0xBAAE_187F);
        }
        0x11 => lz77(cpu, bus, false),
        0x12 => lz77(cpu, bus, true),
        0x13 => huffman(cpu, bus),
        0x14 => rl(cpu, bus, false),
        0x15 => rl(cpu, bus, true),
        0x16 => diff_filter(cpu, bus, 1, 1),
        0x17 => diff_filter(cpu, bus, 1, 2),
        0x18 => diff_filter(cpu, bus, 2, 2),
        0x19 => sound_bias(cpu, bus),
        // Sound driver functions, affine math, BitUnPack, MultiBoot,
        // and everything else: documented no-ops (see module docs).
        _ => {}
    }
    cpu.charge(CALL_OVERHEAD);
    true
}

/// SWI 00h: SoftReset. Reads the reset-destination flag byte at
/// 0x03007FFA first, clears the 200h-byte IWRAM tail (stacks and BIOS
/// IRQ state), then rewrites the CPU (see [`Cpu::soft_reset`]).
fn soft_reset(cpu: &mut Cpu, bus: &mut Bus) {
    let flag = bus.read_u8(0x0300_7FFA);
    bus.zero_mem(0x0300_7E00, 0x200);
    cpu.soft_reset(flag);
}

/// SWI 01h: RegisterRamReset. Always forces screen blank (GBATEK:
/// "regardless of incoming R0"), then clears memory per r0 flag bits.
/// The SIO bit (5) is skipped: serial registers are not implemented.
fn register_ram_reset(cpu: &mut Cpu, bus: &mut Bus) {
    let flags = cpu.gpr(0);
    bus.write_u16(0x0400_0000, 0x0080);
    if flags & 0x01 != 0 {
        bus.zero_mem(0x0200_0000, 0x4_0000);
    }
    if flags & 0x02 != 0 {
        // Excludes the last 200h bytes (stacks and BIOS state).
        bus.zero_mem(0x0300_0000, 0x7E00);
    }
    if flags & 0x04 != 0 {
        bus.zero_mem(0x0500_0000, 0x400);
    }
    if flags & 0x08 != 0 {
        bus.zero_mem(0x0600_0000, 0x1_8000);
    }
    if flags & 0x10 != 0 {
        bus.zero_mem(0x0700_0000, 0x400);
    }
    if flags & 0x40 != 0 {
        // Master enable first so the PSG register writes land (the bus
        // ignores them while the PSG is off), then the registers, then
        // wave RAM.
        bus.write_u16(0x0400_0084, 0);
        for off in (0x060..0x084).step_by(2) {
            bus.write_u16(0x0400_0000 + off, 0);
        }
        bus.write_u16(0x0400_0088, 0x0200);
        for off in 0..0x10 {
            bus.write_u8(0x0400_0090 + off, 0);
        }
    }
    if flags & 0x80 != 0 {
        // Video registers (DISPCNT already forced blank), DMA, timers,
        // and interrupt control. DMA CNT_H zeroing runs the normal
        // disable path; IF acknowledges with all ones.
        for off in (0x002..0x060).step_by(2) {
            bus.write_u16(0x0400_0000 + off, 0);
        }
        for off in (0x0B0..0x0E0).step_by(2) {
            bus.write_u16(0x0400_0000 + off, 0);
        }
        for off in (0x100..0x110).step_by(2) {
            bus.write_u16(0x0400_0000 + off, 0);
        }
        bus.write_u16(0x0400_0200, 0);
        bus.write_u16(0x0400_0202, 0xFFFF);
        bus.write_u16(0x0400_0204, 0);
        bus.write_u16(0x0400_0208, 0);
    }
}

/// SWI 04h/05h: IntrWait and VBlankIntrWait (the latter "sets r0=1 and
/// r1=1 and does then execute IntrWait", GBATEK SWI 05h). IME is
/// forced on; with the discard flag set the waited bits of the BIOS
/// flag word are cleared before halting, otherwise an already-set flag
/// returns immediately. After each wake the CPU runs the IRQ handler,
/// then checks the flag word the handler sets: a waited bit is cleared
/// and the call returns, otherwise it halts again (GBATEK SWI 04h).
fn intr_wait(cpu: &mut Cpu, bus: &mut Bus, vblank: bool) {
    let (discard, mask) = if vblank {
        (true, 0x0001u16)
    } else {
        (cpu.gpr(0) != 0, cpu.gpr(1) as u16)
    };
    if vblank {
        cpu.set_gpr(0, 1);
        cpu.set_gpr(1, 1);
    }
    let flags = bus.read_u16(BIOS_INTR_FLAGS);
    if !discard && flags & mask != 0 {
        bus.write_u16(BIOS_INTR_FLAGS, flags & !mask);
        return;
    }
    if discard {
        bus.write_u16(BIOS_INTR_FLAGS, flags & !mask);
    }
    bus.write_u16(0x0400_0208, 1);
    cpu.halt(Some(mask));
}

/// SWI 06h/07h: Div and DivArm (the Arm form exchanges its operands).
/// Signed division with quotient in r0, remainder in r1, and
/// abs(quotient) in r3 (GBATEK example: -1234/10 = -123, -4, +123).
/// Division by zero hangs the real BIOS; the HLE returns stand-in
/// values instead of locking the machine up.
fn divide(cpu: &mut Cpu, num: i32, denom: i32) {
    let (q, r, a) = if denom == 0 {
        (if num < 0 { -1 } else { 1 }, num, 1)
    } else if num == i32::MIN && denom == -1 {
        (i32::MIN, 0, i32::MIN)
    } else {
        (num / denom, num % denom, num / denom)
    };
    cpu.set_gpr(0, q as u32);
    cpu.set_gpr(1, r as u32);
    cpu.set_gpr(3, a.wrapping_abs() as u32);
}

/// SWI 08h: Sqrt. Integer square root, floor semantics ("Sqrt(2)
/// would return 1", GBATEK SWI 08h).
fn sqrt(x: u32) -> u32 {
    let mut res = 0u32;
    let mut bit = 1u32 << 30;
    let mut n = x;
    while bit > n {
        bit >>= 2;
    }
    while bit != 0 {
        if n >= res + bit {
            n -= res + bit;
            res = (res >> 1) + bit;
        } else {
            res >>= 1;
        }
        bit >>= 2;
    }
    res
}

/// SWI 09h: ArcTan helper. The BIOS polynomial iteration (constants
/// per the mGBA reference); returns the result and the r1/r3 clobbers.
fn arctan(i: i32) -> (i16, i32, i32) {
    let a = -((i.wrapping_mul(i)) >> 14);
    let mut b = ((0xA9i32.wrapping_mul(a)) >> 14).wrapping_add(0x390);
    b = ((b.wrapping_mul(a)) >> 14).wrapping_add(0x91C);
    b = ((b.wrapping_mul(a)) >> 14).wrapping_add(0xFB6);
    b = ((b.wrapping_mul(a)) >> 14).wrapping_add(0x16AA);
    b = ((b.wrapping_mul(a)) >> 14).wrapping_add(0x2081);
    b = ((b.wrapping_mul(a)) >> 14).wrapping_add(0x3651);
    b = ((b.wrapping_mul(a)) >> 14).wrapping_add(0xA2F9);
    (((i.wrapping_mul(b)) >> 16) as i16, a, b)
}

/// SWI 0Ah: ArcTan2. Quadrant reduction around the ArcTan helper;
/// returns the angle (0000h-FFFFh covering 0..2pi) and the r1 clobber
/// (`None` on the direct-return quadrant edges, matching the BIOS).
fn arctan2(x: i32, y: i32) -> (u16, Option<i32>) {
    let call = |i: i32| -> (i32, i32) {
        let (r, a, _) = arctan(i);
        (i32::from(r), a)
    };
    if y == 0 {
        return (if x >= 0 { 0 } else { 0x8000 }, None);
    }
    if x == 0 {
        return (if y >= 0 { 0x4000 } else { 0xC000 }, None);
    }
    if y >= 0 {
        if x >= 0 {
            if x >= y {
                let (r, a) = call((y << 14) / x);
                return (r as u16, Some(a));
            }
        } else if -x >= y {
            let (r, a) = call((y << 14) / x);
            return ((r + 0x8000) as u16, Some(a));
        }
        let (r, a) = call((x << 14) / y);
        ((0x4000 - r) as u16, Some(a))
    } else if x <= 0 {
        if -x > -y {
            let (r, a) = call((y << 14) / x);
            return ((r + 0x8000) as u16, Some(a));
        }
        let (r, a) = call((x << 14) / y);
        ((0xC000 - r) as u16, Some(a))
    } else if x >= -y {
        let (r, a) = call((y << 14) / x);
        ((r + 0x1_0000) as u16, Some(a))
    } else {
        let (r, a) = call((x << 14) / y);
        ((0xC000 - r) as u16, Some(a))
    }
}

/// SWI 0Bh/0Ch: CpuSet and CpuFastSet. Copies or fills through the
/// bus in halfword/word (CpuSet) or word units (CpuFastSet, whose GBA
/// word count rounds up to a multiple of 8). The BIOS silently rejects
/// transfers whose source reaches into the BIOS area (GBATEK CpuSet
/// note); the HLE rejects the same way.
fn cpu_set(cpu: &mut Cpu, bus: &mut Bus, fast: bool) {
    let src = cpu.gpr(0);
    let dst = cpu.gpr(1);
    let ctrl = cpu.gpr(2);
    let fill = ctrl & 0x0100_0000 != 0;
    let (unit, count) = if fast {
        (4u32, (ctrl & 0x001F_FFFF).div_ceil(8) * 8)
    } else if ctrl & 0x0400_0000 != 0 {
        (4u32, ctrl & 0x001F_FFFF)
    } else {
        (2u32, ctrl & 0x001F_FFFF)
    };
    if count == 0 {
        return;
    }
    let span = u64::from(count) * u64::from(unit);
    let last = u64::from(src & 0x0FFF_FFFF) + span - 1;
    if src & 0x0FFF_FFFF < 0x4000 || last < 0x4000 {
        return;
    }
    let mut s = src;
    let mut d = dst;
    let mut first = 0u32;
    for i in 0..count {
        let v = if fill && i > 0 {
            first
        } else {
            cpu.charge(bus.access_cost(s, unit as u8));
            if unit == 4 {
                bus.read_u32(s)
            } else {
                u32::from(bus.read_u16(s))
            }
        };
        if i == 0 {
            first = v;
        }
        cpu.charge(bus.access_cost(d, unit as u8));
        if unit == 4 {
            bus.write_u32(d, v);
        } else {
            bus.write_u16(d, v as u16);
        }
        s = s.wrapping_add(unit);
        d = d.wrapping_add(unit);
    }
}

/// SWI 11h/12h: LZ77UnCompWram/Vram. Header type 1, 24-bit output
/// size; flag bytes MSB-first over 8-block groups; compressed blocks
/// copy N+3 bytes from `dest-disp-1`. The VRAM variant emits 16-bit
/// lanes (see [`put_byte`]).
fn lz77(cpu: &mut Cpu, bus: &mut Bus, vram: bool) {
    let mut src = cpu.gpr(0);
    let mut dest = cpu.gpr(1);
    let header = bus.read_u32(src);
    let out_bytes = (header >> 8) & 0xFF_FFFF;
    let mut remaining = out_bytes as usize;
    src = src.wrapping_add(4);
    let mut flag_byte = 0u8;
    let mut blocks = 0u32;
    let mut halfword = 0u16;
    while remaining > 0 {
        if blocks == 0 {
            flag_byte = bus.read_u8(src);
            src = src.wrapping_add(1);
            blocks = 8;
            continue;
        }
        if flag_byte & 0x80 != 0 {
            let block =
                (u16::from(bus.read_u8(src)) << 8) | u16::from(bus.read_u8(src.wrapping_add(1)));
            src = src.wrapping_add(2);
            let mut from = dest.wrapping_sub(u32::from(block & 0x0FFF)).wrapping_sub(1);
            let mut bytes = u32::from(block >> 12) + 3;
            while bytes > 0 {
                bytes -= 1;
                if remaining == 0 {
                    break;
                }
                remaining -= 1;
                let byte = bus.read_u8(from);
                put_byte(bus, &mut dest, &mut halfword, vram, byte);
                from = from.wrapping_add(1);
            }
        } else {
            let byte = bus.read_u8(src);
            src = src.wrapping_add(1);
            remaining -= 1;
            put_byte(bus, &mut dest, &mut halfword, vram, byte);
        }
        flag_byte <<= 1;
        blocks -= 1;
    }
    cpu.set_gpr(0, src);
    cpu.set_gpr(1, dest);
    cpu.set_gpr(3, 0);
    cpu.charge(DECOMP_CYCLES_PER_BYTE * out_bytes);
}

/// SWI 13h: HuffUnComp. Header type 2 with the bit width in bits 0-3;
/// a tree table follows a one-byte tree size, then the MSB-first
/// bitstream in 32-bit words. Only widths dividing 32 are modeled;
/// anything else returns untouched.
fn huffman(cpu: &mut Cpu, bus: &mut Bus) {
    let mut src = cpu.gpr(0) & 0xFFFF_FFFC;
    let mut dest = cpu.gpr(1);
    let header = bus.read_u32(src);
    let out_bytes = (header >> 8) & 0xFF_FFFF;
    let mut remaining = out_bytes as usize;
    let mut bits = header & 0xF;
    if bits == 0 {
        bits = 8;
    }
    if bits == 1 || 32 % bits != 0 {
        return;
    }
    let mask = if bits >= 32 {
        u32::MAX
    } else {
        (1 << bits) - 1
    };
    let tree_size = (u32::from(bus.read_u8(src.wrapping_add(4))) << 1) + 1;
    let tree_base = src.wrapping_add(5);
    src = src.wrapping_add(5).wrapping_add(tree_size);
    let mut node_ptr = tree_base;
    let mut node = bus.read_u8(node_ptr);
    let mut block = 0u32;
    let mut bits_seen = 0u32;
    while remaining > 0 {
        let mut bitstream = bus.read_u32(src);
        src = src.wrapping_add(4);
        for _ in 0..32 {
            if remaining == 0 {
                break;
            }
            let next = (node_ptr & !1) + u32::from(node & 0x3F) * 2 + 2;
            let right = bitstream & 0x8000_0000 != 0;
            bitstream <<= 1;
            let data = if right {
                if node & 0x40 != 0 {
                    Some(bus.read_u8(next.wrapping_add(1)))
                } else {
                    node_ptr = next.wrapping_add(1);
                    node = bus.read_u8(node_ptr);
                    None
                }
            } else if node & 0x80 != 0 {
                Some(bus.read_u8(next))
            } else {
                node_ptr = next;
                node = bus.read_u8(node_ptr);
                None
            };
            let Some(data) = data else {
                continue;
            };
            block |= (u32::from(data) & mask) << bits_seen;
            bits_seen += bits;
            node_ptr = tree_base;
            node = bus.read_u8(node_ptr);
            if bits_seen == 32 {
                bits_seen = 0;
                bus.write_u32(dest, block);
                dest = dest.wrapping_add(4);
                remaining = remaining.saturating_sub(4);
                block = 0;
            }
        }
    }
    cpu.set_gpr(0, src);
    cpu.set_gpr(1, dest);
    cpu.charge(DECOMP_CYCLES_PER_BYTE * out_bytes);
}

/// SWI 14h/15h: RLUnCompWram/Vram. Header type 3, 24-bit output size;
/// flag bit 7 selects a compressed run (one byte, length+3) or a
/// literal run (length+1 bytes); the output is padded to a 4-byte
/// boundary with zeros.
fn rl(cpu: &mut Cpu, bus: &mut Bus, vram: bool) {
    let mut src = cpu.gpr(0);
    let header = bus.read_u32(src & 0xFFFF_FFFC);
    let out_bytes = (header >> 8) & 0xFF_FFFF;
    let mut remaining = out_bytes;
    let mut padding = 4u32.wrapping_sub(remaining) & 3;
    src = src.wrapping_add(4);
    let mut dest = cpu.gpr(1);
    let mut halfword = 0u16;
    while remaining > 0 {
        let mut control = u32::from(bus.read_u8(src));
        src = src.wrapping_add(1);
        if control & 0x80 != 0 {
            control &= 0x7F;
            control += 3;
            let byte = bus.read_u8(src);
            src = src.wrapping_add(1);
            while control > 0 {
                control -= 1;
                if remaining == 0 {
                    break;
                }
                remaining -= 1;
                put_byte(bus, &mut dest, &mut halfword, vram, byte);
            }
        } else {
            control += 1;
            while control > 0 {
                control -= 1;
                if remaining == 0 {
                    break;
                }
                remaining -= 1;
                let byte = bus.read_u8(src);
                src = src.wrapping_add(1);
                put_byte(bus, &mut dest, &mut halfword, vram, byte);
            }
        }
    }
    if vram {
        if dest & 1 != 0 && padding > 0 {
            padding -= 1;
            dest = dest.wrapping_add(1);
        }
        while padding > 0 {
            bus.write_u16(dest, 0);
            padding = padding.saturating_sub(2);
            dest = dest.wrapping_add(2);
        }
    } else {
        while padding > 0 {
            bus.write_u8(dest, 0);
            padding -= 1;
            dest = dest.wrapping_add(1);
        }
    }
    cpu.set_gpr(0, src);
    cpu.set_gpr(1, dest);
    cpu.charge(DECOMP_CYCLES_PER_BYTE * out_bytes);
}

/// SWI 16h/17h/18h: Diff8bitUnFilterWram (8->8),
/// Diff8bitUnFilterVram (8->16 pairs), and Diff16bitUnFilter. Each
/// unit adds to the running total; header type 8.
fn diff_filter(cpu: &mut Cpu, bus: &mut Bus, in_width: u8, out_width: u8) {
    let mut src = cpu.gpr(0) & 0xFFFF_FFFC;
    let mut dest = cpu.gpr(1);
    let header = bus.read_u32(src);
    let out_bytes = (header >> 8) & 0xFF_FFFF;
    let mut remaining = out_bytes as usize;
    src = src.wrapping_add(4);
    let mut old = 0u16;
    let mut halfword = 0u16;
    while remaining > 0 {
        let raw = if in_width == 1 {
            u16::from(bus.read_u8(src))
        } else {
            bus.read_u16(src)
        };
        let new = raw.wrapping_add(old);
        if out_width > in_width {
            // 8 -> 16: byte pairs pack into halfword lanes; the pair
            // boundary follows source parity (the source is aligned).
            halfword >>= 8;
            halfword |= new << 8;
            if src & 1 != 0 {
                bus.write_u16(dest, halfword);
                dest = dest.wrapping_add(u32::from(out_width));
                remaining = remaining.saturating_sub(usize::from(out_width));
            }
        } else if out_width == 1 {
            bus.write_u8(dest, new as u8);
            dest = dest.wrapping_add(1);
            remaining -= 1;
        } else {
            bus.write_u16(dest, new);
            dest = dest.wrapping_add(2);
            remaining = remaining.saturating_sub(2);
        }
        old = new;
        src = src.wrapping_add(u32::from(in_width));
    }
    cpu.set_gpr(0, src);
    cpu.set_gpr(1, dest);
    cpu.charge(DECOMP_CYCLES_PER_BYTE * out_bytes);
}

/// SWI 19h: SoundBias. Steps SOUNDBIAS to the requested level: 0000h
/// for r0 == 0, else 0200h; upper bits unchanged (GBATEK SWI 19h).
/// The stepped transition delay is not modeled.
fn sound_bias(cpu: &mut Cpu, bus: &mut Bus) {
    let level = if cpu.gpr(0) == 0 { 0x0000u16 } else { 0x0200 };
    let cur = bus.read_u16(0x0400_0088);
    bus.write_u16(0x0400_0088, (cur & 0xFC00) | level);
}

/// One output byte of a decompression run. The VRAM variants write
/// 16-bit lanes: even bytes buffer, odd bytes flush the pair to
/// `dest ^ 1` (GBATEK: "Writing 16bit units to [dest-1]").
fn put_byte(bus: &mut Bus, dest: &mut u32, halfword: &mut u16, vram: bool, byte: u8) {
    if vram {
        if *dest & 1 != 0 {
            *halfword |= u16::from(byte) << 8;
            bus.write_u16(*dest ^ 1, *halfword);
        } else {
            *halfword = u16::from(byte);
        }
    } else {
        bus.write_u8(*dest, byte);
    }
    *dest = dest.wrapping_add(1);
}
