//! The deferred-step primitive behind the diff harness's catch-up
//! mode: `System::step_deferred` runs one CPU instruction with the
//! video clock parked, and replaying the returned cycles through
//! `System::advance_clock` must land the machine exactly where plain
//! `System::step` steps would have.

use runtime::system::System;

/// `b +4` in ARM state: one forward word, so the pc moves.
const SKIP: u32 = 0xEAFF_FFFF;

/// A two-word ROM: `b +4` then `b .`, so the first step's pc move is
/// observable.
fn booted() -> System {
    let mut rom = vec![0u8; 0x20];
    rom[..4].copy_from_slice(&SKIP.to_le_bytes());
    rom[4..8].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes());
    System::new(rom, 0x0800_0000)
}

#[test]
fn deferred_steps_replayed_equal_plain_steps() {
    let mut plain = booted();
    let mut plain_cycles = 0u32;
    for _ in 0..64 {
        plain_cycles += plain.step();
    }

    let mut deferred = booted();
    let mut parked = 0u32;
    for _ in 0..64 {
        parked += deferred.step_deferred();
    }
    assert_eq!(deferred.vcount(), 0, "the clock stayed parked");
    deferred.advance_clock(parked);

    assert_eq!(plain_cycles, parked);
    assert_eq!(deferred.vcount(), plain.vcount());
    assert_eq!(deferred.cpu.pc(), plain.cpu.pc());
    assert_eq!(deferred.cpu.cpsr(), plain.cpu.cpsr());
}

#[test]
fn a_deferred_step_moves_the_cpu_but_not_the_frame() {
    let mut system = booted();
    let before = system.vcount();
    let cycles = system.step_deferred();
    assert_eq!(system.vcount(), before, "no clock advance");
    assert_eq!(system.cpu.pc(), 0x0800_0004, "the pc did move");
    assert!(cycles > 0);
}
