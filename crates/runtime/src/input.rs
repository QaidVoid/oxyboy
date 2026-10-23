//! The keypad: key identification and the KEYINPUT register image.

/// One GBA keypad button, in KEYINPUT bit order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    /// Button A (KEYINPUT bit 0).
    A,
    /// Button B (bit 1).
    B,
    /// Select (bit 2).
    Select,
    /// Start (bit 3).
    Start,
    /// Right (bit 4).
    Right,
    /// Left (bit 5).
    Left,
    /// Up (bit 6).
    Up,
    /// Down (bit 7).
    Down,
    /// Button R (bit 8).
    R,
    /// Button L (bit 9).
    L,
}

impl Key {
    /// The KEYINPUT/KEYCNT bit for this key.
    pub fn bit(self) -> u16 {
        1 << self as u16
    }
}
