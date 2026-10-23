//! Prescaler periods at instruction granularity,
//! cascade chaining, reload behavior, and one overflow IRQ per wrap.
//!
//! Tests drive `System::step` and accumulate its returned cycle counts,
//! asserting the exact floor-division tick count including the residual
//! the engine carries between steps.

use runtime::system::System;

/// Timer N register base (`0x04000100 + 4*N`).
fn tm(n: u32) -> u32 {
    0x0400_0100 + 4 * n
}
const CNT_L: u32 = 0;
const CNT_H: u32 = 2;

/// CNT_H builder: `(prescaler, cascade, irq, enable)`.
fn cnt_h(prescaler: u16, cascade: bool, irq: bool, enable: bool) -> u16 {
    prescaler
        | if cascade { 0x0004 } else { 0 }
        | if irq { 0x0040 } else { 0 }
        | if enable { 0x0080 } else { 0 }
}

/// `b .` (EA FF FF FE): parks the CPU while timers run.
fn parked() -> Vec<u8> {
    vec![0xEA, 0xFF, 0xFF, 0xFE]
}

#[test]
fn prescaler_ticks_follow_the_elapsed_cycle_budget_exactly() {
    let mut system = System::new(parked(), 0x0800_0000);
    let bus = system.bus_mut();
    for (n, prescaler) in [(0u32, 0u16), (1, 1), (2, 2), (3, 3)] {
        bus.write_u16(tm(n) + CNT_L, 0);
        bus.write_u16(tm(n) + CNT_H, cnt_h(prescaler, false, false, true));
    }

    // Ten steps of whatever length the parked branch takes: each timer
    // must sit at exactly floor(elapsed / period) ticks, with the
    // remainder carried across steps.
    let mut elapsed = 0u32;
    for _ in 0..10 {
        elapsed += system.step();
    }
    assert_eq!(
        u32::from(system.bus().read_u16(tm(0) + CNT_L)),
        elapsed % 0x1_0000
    );
    assert_eq!(system.bus().read_u16(tm(1) + CNT_L), (elapsed / 64) as u16);
    assert_eq!(system.bus().read_u16(tm(2) + CNT_L), (elapsed / 256) as u16);
    assert_eq!(
        system.bus().read_u16(tm(3) + CNT_L),
        (elapsed / 1024) as u16
    );

    // The residual survives: more steps keep the same accounting.
    for _ in 0..10 {
        elapsed += system.step();
    }
    assert_eq!(system.bus().read_u16(tm(1) + CNT_L), (elapsed / 64) as u16);
}

#[test]
fn reload_applies_on_overflow_and_on_reenable() {
    let mut system = System::new(parked(), 0x0800_0000);
    let bus = system.bus_mut();
    bus.write_u16(tm(0) + CNT_L, 0xFFFE);
    bus.write_u16(tm(0) + CNT_H, cnt_h(0, false, false, true));
    for _ in 0..4 {
        system.step();
    }
    // Enough cycles for several ticks; the value is reload plus the
    // ticks since enable modulo the wraps.
    let counter = system.bus().read_u16(tm(0) + CNT_L);
    assert_ne!(counter, 0xFFFF, "the counter must have ticked");

    // A write while enabled changes only the reload latch.
    let bus = system.bus_mut();
    bus.write_u16(tm(0) + CNT_L, 0x1234);
    assert_ne!(system.bus().read_u16(tm(0) + CNT_L), 0x1234);

    // Disable, then enable: the counter takes the new reload.
    let bus = system.bus_mut();
    bus.write_u16(tm(0) + CNT_H, cnt_h(0, false, false, false));
    bus.write_u16(tm(0) + CNT_H, cnt_h(0, false, false, true));
    assert_eq!(system.bus().read_u16(tm(0) + CNT_L), 0x1234);
}

#[test]
fn cascade_ticks_chain_across_three_channels() {
    let mut system = System::new(parked(), 0x0800_0000);
    let bus = system.bus_mut();
    // Timer 0 on the clock wrapping every 2 ticks (reload 0xFFFE);
    // timers 1 and 2 tick from the predecessor's overflow, timer 1
    // wrapping every tick (reload 0xFFFF), timer 2 never (reload 0).
    bus.write_u16(tm(0) + CNT_L, 0xFFFE);
    bus.write_u16(tm(0) + CNT_H, cnt_h(0, false, false, true));
    bus.write_u16(tm(1) + CNT_L, 0xFFFF);
    bus.write_u16(tm(1) + CNT_H, cnt_h(0, true, false, true));
    bus.write_u16(tm(2) + CNT_L, 0);
    bus.write_u16(tm(2) + CNT_H, cnt_h(0, true, false, true));

    let mut elapsed = 0u32;
    for _ in 0..20 {
        elapsed += system.step();
    }

    // Closed forms: a counter with reload r and period 0x10000-r ticks
    // wraps floor(elapsed / period) times and sits at r + (elapsed mod
    // period), with the wrap instant itself reading r again.
    let (p0, p1) = (0x1_0000 - 0xFFFE, 0x1_0000 - 0xFFFF);
    let t0_overflows = elapsed / p0;
    let t1_overflows = t0_overflows / p1;
    assert!(t1_overflows >= 2, "test must run long enough to chain");

    let t0 = system.bus().read_u16(tm(0) + CNT_L);
    let t1 = system.bus().read_u16(tm(1) + CNT_L);
    let t2 = system.bus().read_u16(tm(2) + CNT_L);
    let k0 = elapsed % p0;
    let k1 = t0_overflows % p1;
    let expected_t0 = if k0 == 0 {
        0xFFFE
    } else {
        (0xFFFEu32 + k0) as u16
    };
    let expected_t1 = if k1 == 0 {
        0xFFFF
    } else {
        (0xFFFFu32 + k1) as u16
    };
    assert_eq!(t0, expected_t0);
    assert_eq!(t1, expected_t1);
    assert_eq!(t2, (t1_overflows % 0x1_0000) as u16);
}

#[test]
fn overflow_irq_fires_exactly_once_per_wrap() {
    let mut system = System::new(parked(), 0x0800_0000);
    let bus = system.bus_mut();
    bus.write_u16(tm(0) + CNT_L, 0xFFF0);
    bus.write_u16(tm(0) + CNT_H, cnt_h(0, false, true, true));

    let mut elapsed = 0u32;
    let mut wraps = 0u32;
    for _ in 0..200 {
        elapsed += system.step();
        while system.bus().int_flags() & 0x0008 != 0 {
            system.bus_mut().write_u16(0x0400_0202, 0x0008);
            wraps += 1;
        }
    }
    // Reload 0xFFF0 with prescaler 1: the counter wraps every 16 ticks,
    // and each wrap must have raised IF bit 3 exactly once.
    let expected = elapsed / (0x1_0000 - 0xFFF0);
    assert!(
        expected > 5,
        "test must run long enough to wrap: {expected}"
    );
    assert_eq!(wraps, expected);
}

#[test]
fn each_channel_raises_its_own_if_bit() {
    let mut system = System::new(parked(), 0x0800_0000);
    let bus = system.bus_mut();
    bus.write_u16(tm(2) + CNT_L, 0xFFFE);
    bus.write_u16(tm(2) + CNT_H, cnt_h(0, false, true, true));
    for _ in 0..4 {
        system.step();
    }
    let flags = system.bus().int_flags();
    assert_eq!(flags & 0x0078, 0x0020, "timer 2 is IF bit 5 and only that");
}
