//! Registers and register lists.

/// An ARM register, `R0`..`R15`.
///
/// THUMB encodings reference `R0`..`R7` directly and reach `R8`..`R15`
/// only through the high-register forms; everything is normalized to this
/// one type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Register {
    /// General purpose register R0.
    R0,
    /// General purpose register R1.
    R1,
    /// General purpose register R2.
    R2,
    /// General purpose register R3.
    R3,
    /// General purpose register R4.
    R4,
    /// General purpose register R5.
    R5,
    /// General purpose register R6.
    R6,
    /// General purpose register R7.
    R7,
    /// General purpose register R8 (high register in THUMB).
    R8,
    /// General purpose register R9 (high register in THUMB).
    R9,
    /// General purpose register R10 (high register in THUMB).
    R10,
    /// General purpose register R11 (high register in THUMB).
    R11,
    /// General purpose register R12, the intra-call scratch register.
    R12,
    /// R13, conventionally the stack pointer (banked per CPU mode).
    Sp,
    /// R14, the link register (banked per CPU mode).
    Lr,
    /// R15, the program counter.
    Pc,
}

impl Register {
    /// Decodes a 4-bit register field.
    #[must_use]
    pub fn from_bits(bits: u8) -> Self {
        match bits & 0xF {
            0x0 => Self::R0,
            0x1 => Self::R1,
            0x2 => Self::R2,
            0x3 => Self::R3,
            0x4 => Self::R4,
            0x5 => Self::R5,
            0x6 => Self::R6,
            0x7 => Self::R7,
            0x8 => Self::R8,
            0x9 => Self::R9,
            0xA => Self::R10,
            0xB => Self::R11,
            0xC => Self::R12,
            0xD => Self::Sp,
            0xE => Self::Lr,
            _ => Self::Pc,
        }
    }

    /// The 4-bit register field value.
    #[must_use]
    pub fn bits(self) -> u8 {
        match self {
            Self::R0 => 0x0,
            Self::R1 => 0x1,
            Self::R2 => 0x2,
            Self::R3 => 0x3,
            Self::R4 => 0x4,
            Self::R5 => 0x5,
            Self::R6 => 0x6,
            Self::R7 => 0x7,
            Self::R8 => 0x8,
            Self::R9 => 0x9,
            Self::R10 => 0xA,
            Self::R11 => 0xB,
            Self::R12 => 0xC,
            Self::Sp => 0xD,
            Self::Lr => 0xE,
            Self::Pc => 0xF,
        }
    }
}

/// A register list, one bit per register `R0`..`R15`, as encoded in ARM
/// block transfers (bit 0 = R0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegisterList(u16);

impl RegisterList {
    /// Builds a list from its raw encoding.
    #[must_use]
    pub fn from_bits(bits: u16) -> Self {
        Self(bits)
    }

    /// The raw 16-bit encoding.
    #[must_use]
    pub fn bits(self) -> u16 {
        self.0
    }

    /// Reports whether `register` is in the list.
    #[must_use]
    pub fn contains(self, register: Register) -> bool {
        self.0 & (1 << register.bits()) != 0
    }

    /// The number of registers in the list.
    #[must_use]
    pub fn len(self) -> u32 {
        self.0.count_ones()
    }

    /// Reports whether the list is empty.
    ///
    /// Empty lists decode (the encoding exists) but are UNPREDICTABLE in
    /// execution per the ARM ARM; analysis flags them.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}
