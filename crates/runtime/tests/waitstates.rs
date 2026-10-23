//! WAITCNT waitstate timing (GBATEK "4000204h - WAITCNT"): first
//! access codes 0..3 cost 4, 3, 2, 8 cycles; second access is one bit
//! per region (WS0 2/1, WS1 4/1, WS2 8/1); SRAM bits apply to any
//! access; a 32-bit ROM access is two 16-bit lanes with the second
//! always sequential; each 128 KiB block start forces non-sequential;
//! prefetch (bit 14) serves the buffered successor fetch in one cycle.
//! Every expected number below is transcribed from those tables.

use runtime::bus::Bus;
use runtime::cpu::Cpu;

fn set_waitcnt(bus: &mut Bus, value: u16) {
    bus.write_u16(0x0400_0204, value);
    assert_eq!(bus.waitcnt(), value, "WAITCNT is plain RW storage");
}

#[test]
fn ws0_first_and_second_access_match_the_gbatek_table() {
    let first = [4, 3, 2, 8];
    for c in 0..4u16 {
        for s in 0..2u16 {
            let mut bus = Bus::new(rom());
            set_waitcnt(&mut bus, (c << 2) | (s << 4));
            let n = bus.access_cost(0x0800_0002, 2);
            assert_eq!(n, first[c as usize], "WS0 first code {c}");
            // Consecutive halfword: sequential.
            bus.access_cost(0x0800_0000, 2);
            let s_cost = bus.access_cost(0x0800_0002, 2);
            let want = if s == 0 { 2 } else { 1 };
            assert_eq!(s_cost, want, "WS0 second bit {s} (first {c})");
        }
    }
}

#[test]
fn ws1_first_and_second_access_match_the_gbatek_table() {
    let first = [4, 3, 2, 8];
    for c in 0..4u16 {
        for s in 0..2u16 {
            let mut bus = Bus::new(rom());
            set_waitcnt(&mut bus, (c << 5) | (s << 7));
            let n = bus.access_cost(0x0A00_0002, 2);
            assert_eq!(n, first[c as usize], "WS1 first code {c}");
            bus.access_cost(0x0A00_0000, 2);
            let s_cost = bus.access_cost(0x0A00_0002, 2);
            let want = if s == 0 { 4 } else { 1 };
            assert_eq!(s_cost, want, "WS1 second bit {s} (first {c})");
        }
    }
}

#[test]
fn ws2_first_and_second_access_match_the_gbatek_table() {
    let first = [4, 3, 2, 8];
    for c in 0..4u16 {
        for s in 0..2u16 {
            let mut bus = Bus::new(rom());
            set_waitcnt(&mut bus, (c << 8) | (s << 10));
            let n = bus.access_cost(0x0C00_0002, 2);
            assert_eq!(n, first[c as usize], "WS2 first code {c}");
            bus.access_cost(0x0C00_0000, 2);
            let s_cost = bus.access_cost(0x0C00_0002, 2);
            let want = if s == 0 { 8 } else { 1 };
            assert_eq!(s_cost, want, "WS2 second bit {s} (first {c})");
        }
    }
}

#[test]
fn sram_wait_codes_apply_to_any_access() {
    let table = [4, 3, 2, 8];
    for c in 0..4u16 {
        let mut bus = Bus::new(rom());
        set_waitcnt(&mut bus, c);
        assert_eq!(bus.access_cost(0x0E00_0000, 1), table[c as usize]);
        bus.access_cost(0x0E00_0000, 1);
        let s = bus.access_cost(0x0E00_0001, 1);
        assert_eq!(s, table[c as usize], "SRAM seq waits equal N waits");
    }
}

#[test]
fn sequential_timing_needs_consecutive_addresses_in_one_region() {
    let mut bus = Bus::new(rom());
    assert_eq!(bus.access_cost(0x0800_0000, 2), 4);
    assert_eq!(bus.access_cost(0x0800_0002, 2), 2, "consecutive: S");
    // A gap breaks the chain (the next access re-establishes it).
    assert_eq!(bus.access_cost(0x0800_0006, 2), 4, "gap: N");
    assert_eq!(bus.access_cost(0x0800_0008, 2), 2, "chain resumes");
    // A detour through another region breaks it again.
    bus.access_cost(0x0300_0000, 4);
    assert_eq!(bus.access_cost(0x0800_000A, 2), 4, "after IWRAM: N");
    // Same address again is not sequential.
    assert_eq!(bus.access_cost(0x0800_000A, 2), 4, "repeat: N");
    assert_eq!(bus.access_cost(0x0800_000C, 2), 2, "resume: S");
}

#[test]
fn word_rom_access_is_n_plus_always_sequential_lane() {
    let mut bus = Bus::new(rom());
    // Cold word: N lane + always-S lane.
    assert_eq!(bus.access_cost(0x0800_0000, 4), 4 + 2);
    // A word that continues a sequential halfword chain: S + S.
    bus.access_cost(0x0800_0200, 2);
    assert_eq!(bus.access_cost(0x0800_0202, 4), 2 + 2);
}

#[test]
fn gamepak_forces_nonseq_at_128k_block_starts() {
    let mut bus = Bus::new(rom());
    bus.access_cost(0x0801_FFFC, 2);
    assert_eq!(bus.access_cost(0x0801_FFFE, 2), 2, "still inside block");
    assert_eq!(bus.access_cost(0x0802_0000, 2), 4, "block start: N");
    // A word straddling the boundary: S lane into an N lane.
    bus.access_cost(0x0803_FFFC, 2);
    assert_eq!(bus.access_cost(0x0803_FFFE, 4), 2 + 4);
}

#[test]
fn documented_cart_setting_4317_matches_the_gbatek_prose() {
    // GBATEK: "Currently manufactured cartridges are using ... WS0/ROM=
    // 3,1 clks; SRAM=8 clks; WS2/EEPROM: 8,8 clks; prefetch enabled;
    // that is, WAITCNT=4317h".
    let mut bus = Bus::new(rom());
    set_waitcnt(&mut bus, 0x4317);
    assert_eq!(bus.access_cost(0x0800_0002, 2), 3, "WS0 N=3");
    bus.access_cost(0x0800_0000, 2);
    assert_eq!(bus.access_cost(0x0800_0002, 2), 1, "WS0 S=1");
    assert_eq!(bus.access_cost(0x0A00_0002, 2), 4, "WS1 N=4 (untouched)");
    assert_eq!(bus.access_cost(0x0C00_0002, 2), 8, "WS2 N=8");
    bus.access_cost(0x0C00_0000, 2);
    assert_eq!(bus.access_cost(0x0C00_0002, 2), 8, "WS2 S=8");
    assert_eq!(bus.access_cost(0x0E00_0000, 1), 8, "SRAM=8");
    assert_ne!(bus.waitcnt() & 0x4000, 0, "prefetch enabled");
}

#[test]
fn prefetch_hit_serves_the_successor_fetch_in_one_cycle() {
    let mut bus = Bus::new(rom());
    set_waitcnt(&mut bus, 0x4000);
    assert_eq!(bus.fetch_cost(0x0800_0000, 2), 4, "cold: N");
    assert_eq!(bus.fetch_cost(0x0800_0002, 2), 1, "buffered: hit");
    assert_eq!(bus.fetch_cost(0x0800_0004, 2), 1, "chain keeps hitting");
}

#[test]
fn any_nonfetch_bus_operation_invalidates_the_prefetch_buffer() {
    let mut bus = Bus::new(rom());
    set_waitcnt(&mut bus, 0x4000);
    bus.fetch_cost(0x0800_0000, 2);
    bus.access_cost(0x0300_0000, 4);
    assert_eq!(bus.fetch_cost(0x0800_0002, 2), 4, "invalidated and non-seq");
    // A jump target likewise misses (no buffered successor).
    assert_eq!(bus.fetch_cost(0x0800_1000, 2), 4, "cold after jump: N");
}

#[test]
fn prefetch_disabled_costs_the_plain_sequential_time() {
    let mut bus = Bus::new(rom());
    assert_eq!(bus.fetch_cost(0x0800_0000, 2), 4);
    assert_eq!(bus.fetch_cost(0x0800_0002, 2), 2, "S, not 1");
}

#[test]
fn word_fetches_prefetch_the_word_successor() {
    let mut bus = Bus::new(rom());
    set_waitcnt(&mut bus, 0x4000);
    assert_eq!(bus.fetch_cost(0x0800_0000, 4), 4 + 2, "two lanes");
    assert_eq!(bus.fetch_cost(0x0800_0004, 4), 1, "buffered word hit");
}

#[test]
fn tighter_waitstates_execute_rom_code_faster() {
    let run = |waitcnt: u16| -> u64 {
        // b . (ARM), verified by assembling; baked into the image
        // because ROM writes are discarded.
        let mut image = rom();
        image[..4].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes());
        let mut bus = Bus::new(image);
        set_waitcnt(&mut bus, waitcnt);
        let mut cpu = Cpu::new(0x0800_0000);
        let mut total = 0;
        for _ in 0..1000 {
            total += u64::from(cpu.step(&mut bus));
        }
        total
    };
    let loose = run(0x0000);
    let tight = run(0x4317);
    assert_eq!(loose, 1000 * (4 + 2), "default: N+S lanes");
    assert_eq!(tight, 1000 * (3 + 1), "3,1 waits + prefetch hits");
    assert!(tight < loose);
}

fn rom() -> Vec<u8> {
    vec![0; 0x1000]
}
