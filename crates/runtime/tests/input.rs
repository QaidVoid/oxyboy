//! Scripted input reaches a polled ROM, and the
//! KEYCNT key IRQ fires per configuration.
//!
//! KEYINPUT (0x0400130) reads pressed keys active low (GBATEK "KEYINPUT":
//! pressed bit = 0). KEYCNT (0x0400132) selects a mask, an OR/AND
//! condition, and the request enable; the request raises IF bit 12 on the
//! condition's false -> true edge. The ROM-level test boots the assembled
//! `inputtest` micro-ROM, which polls KEYINPUT into an IWRAM snapshot and
//! counts key-IRQ entries; the driver presses and releases keys between
//! frames the way a scripted input API would.

use rom::Image;
use runtime::bus::Bus;
use runtime::input::Key;
use runtime::system::System;
use testutil::testrom;

const KEYINPUT: u32 = 0x0400_0130;
const KEYCNT: u32 = 0x0400_0132;
const IF: u32 = 0x0400_0202;
/// IF bit 12: the keypad request.
const KEY_IF: u16 = 0x1000;

fn keyinput(bus: &mut Bus) -> u16 {
    bus.read_u16(KEYINPUT)
}

fn key_irq(bus: &mut Bus) -> bool {
    bus.read_u16(IF) & KEY_IF != 0
}

/// Enables the KEYCNT request for `mask` in OR (false) or AND (true)
/// condition mode.
fn arm_keycnt(bus: &mut Bus, mask: u16, and: bool) {
    let cond = if and { 0x8000 } else { 0 };
    bus.write_u16(KEYCNT, 0x4000 | cond | mask);
}

#[test]
fn keyinput_reads_active_low_with_defaults() {
    let mut bus = Bus::new(Vec::new());
    assert_eq!(keyinput(&mut bus), 0x03FF, "no keys pressed at reset");

    bus.press(Key::A);
    assert_eq!(keyinput(&mut bus), 0x03FE, "pressed A reads 0");
    bus.press(Key::L);
    assert_eq!(keyinput(&mut bus), 0x01FE, "L is bit 9");
    bus.release(Key::A);
    bus.release(Key::L);
    assert_eq!(keyinput(&mut bus), 0x03FF);

    // A face-mash across both bit bytes composes independently.
    bus.press(Key::Start);
    bus.press(Key::Right);
    bus.press(Key::Up);
    assert_eq!(keyinput(&mut bus), 0x03FF & !(0x08 | 0x10 | 0x40));
}

#[test]
fn keyinput_is_read_only() {
    let mut bus = Bus::new(Vec::new());
    bus.write_u16(KEYINPUT, 0x0000);
    assert_eq!(
        keyinput(&mut bus),
        0x03FF,
        "writes to KEYINPUT are discarded"
    );
    bus.write_u8(KEYINPUT + 1, 0x00);
    assert_eq!(keyinput(&mut bus), 0x03FF);
}

#[test]
fn key_irq_or_condition_fires_once_per_press() {
    let mut bus = Bus::new(Vec::new());
    arm_keycnt(&mut bus, 0x0001, false); // A, OR, enabled

    bus.press(Key::A);
    assert!(key_irq(&mut bus), "the press raises the request");
    bus.write_u16(IF, KEY_IF); // acknowledge
    // Holding the key must not re-fire (edge, not level).
    bus.press(Key::A);
    assert!(!key_irq(&mut bus), "no second request while held");

    bus.release(Key::A);
    assert!(!key_irq(&mut bus), "release clears the condition quietly");
    bus.press(Key::A);
    assert!(key_irq(&mut bus), "a fresh press fires again");

    // Keys outside the mask stay quiet.
    bus.write_u16(IF, KEY_IF);
    bus.press(Key::B);
    bus.release(Key::A);
    assert!(!key_irq(&mut bus));
}

#[test]
fn key_irq_and_condition_needs_all_selected() {
    let mut bus = Bus::new(Vec::new());
    arm_keycnt(&mut bus, 0x0003, true); // A+B, AND, enabled

    bus.press(Key::A);
    assert!(!key_irq(&mut bus), "one of two is not all");
    bus.press(Key::B);
    assert!(key_irq(&mut bus), "A+B satisfies AND");
    bus.write_u16(IF, KEY_IF);

    // Dropping one key clears the condition; re-completing re-fires.
    bus.release(Key::B);
    bus.press(Key::B);
    assert!(key_irq(&mut bus));
}

#[test]
fn key_irq_disabled_or_empty_mask_never_fires() {
    let mut bus = Bus::new(Vec::new());
    // Mask set but bit 14 clear.
    bus.write_u16(KEYCNT, 0x0001);
    bus.press(Key::A);
    assert!(!key_irq(&mut bus), "disabled means no request");

    // Enabled with an empty mask (the degenerate AND case).
    bus.write_u16(KEYCNT, 0xC000);
    for key in [Key::A, Key::B, Key::Start, Key::L] {
        bus.press(key);
    }
    assert!(!key_irq(&mut bus), "an empty mask selects nothing");
}

#[test]
fn keycnt_rewrite_over_held_keys_fires() {
    let mut bus = Bus::new(Vec::new());
    bus.press(Key::A);
    assert!(!key_irq(&mut bus));

    // Arming while the condition already holds raises the request
    // (the condition edge happened before the write; the write makes
    // it visible).
    arm_keycnt(&mut bus, 0x0001, false);
    assert!(key_irq(&mut bus));
}

#[test]
fn system_press_reaches_the_polled_register() {
    let mut system = System::new(vec![0xFE, 0xFF, 0xFF, 0xEA], 0x0800_0000);
    system.press(Key::Start);
    system.step_frame();
    assert_eq!(system.bus_mut().read_u16(KEYINPUT), 0x03F7);
    system.release(Key::Start);
    assert_eq!(system.bus_mut().read_u16(KEYINPUT), 0x03FF);
}

/// ROM-level check: the assembled `inputtest` micro-ROM
/// polls KEYINPUT into an IWRAM snapshot and counts key-IRQ entries.
/// The driver scripts presses and releases between frames and checks
/// both paths against the ROM's own observations.
#[test]
fn input_rom_polled_input_and_key_irq() {
    let Some(path) = testrom::require("inputtest") else {
        return;
    };
    let image = Image::from_file(&path).expect("script-built ROM must parse");
    let mut system = System::new(image.bytes().to_vec(), 0x0800_0000);
    let word = |system: &mut System, addr: u32| system.bus_mut().read_u32(addr);
    let half = |system: &mut System, addr: u32| system.bus_mut().read_u16(addr);
    const SNAPSHOT: u32 = 0x0300_0000;
    const ENTRIES: u32 = 0x0300_0004;
    const CONTRACT: u32 = 0x0300_0008;

    // Frame 1: the ROM parks on the poll loop, nothing pressed.
    system.step_frame();
    assert_eq!(half(&mut system, SNAPSHOT), 0x03FF, "nothing pressed");
    assert_eq!(word(&mut system, ENTRIES), 0);

    // Press A: the next frame dispatches the key IRQ once and the
    // poll loop observes the pressed bit.
    system.press(Key::A);
    system.step_frame();
    assert_eq!(half(&mut system, SNAPSHOT), 0x03FE, "the ROM polled A down");
    assert_eq!(word(&mut system, ENTRIES), 1, "exactly one key IRQ");
    assert_eq!(word(&mut system, CONTRACT), 0x0001, "r0 == 0x04000000");

    // Holding A across another frame fires nothing more.
    system.step_frame();
    assert_eq!(word(&mut system, ENTRIES), 1, "edge, not level");

    // Release: the snapshot follows, no new request.
    system.release(Key::A);
    system.step_frame();
    assert_eq!(half(&mut system, SNAPSHOT), 0x03FF);
    assert_eq!(word(&mut system, ENTRIES), 1);

    // A key outside the mask neither fires nor disturbs the poll.
    system.press(Key::B);
    system.step_frame();
    assert_eq!(half(&mut system, SNAPSHOT), 0x03FD);
    assert_eq!(word(&mut system, ENTRIES), 1);

    // A re-press of the masked key fires again.
    system.press(Key::A);
    system.step_frame();
    assert_eq!(word(&mut system, ENTRIES), 2);

    // Switch the config to AND over A+B (the driver writes KEYCNT
    // the way a ROM would). A is already down; B completes it.
    system.bus_mut().write_u16(KEYCNT, 0xC003);
    system.step_frame();
    assert_eq!(word(&mut system, ENTRIES), 2, "line already high: no edge");

    system.release(Key::B);
    system.press(Key::B);
    system.step_frame();
    assert_eq!(word(&mut system, ENTRIES), 3, "AND condition re-completed");
    assert_eq!(word(&mut system, CONTRACT), 0x0001);
}
