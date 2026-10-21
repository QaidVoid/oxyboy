//! The ARM condition field.

/// The ARM condition field, bits 31..28 of every ARM instruction.
///
/// Condition `0b1111` is deliberately not representable: it is reserved on
/// ARMv4T (ARM ARM section A3.3) and the ARM7TDMI TRM marks instructions
/// with that condition UNPREDICTABLE, so the decoder returns
/// [`crate::ArmInstr::Undefined`] for it instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Condition {
    /// `0b0000`: equal, Z set.
    Eq,
    /// `0b0001`: not equal, Z clear.
    Ne,
    /// `0b0010`: unsigned higher or same, C set.
    Cs,
    /// `0b0011`: unsigned lower, C clear.
    Cc,
    /// `0b0100`: negative, N set.
    Mi,
    /// `0b0101`: positive or zero, N clear.
    Pl,
    /// `0b0110`: overflow, V set.
    Vs,
    /// `0b0111`: no overflow, V clear.
    Vc,
    /// `0b1000`: unsigned higher, C set and Z clear.
    Hi,
    /// `0b1001`: unsigned lower or same, C clear or Z set.
    Ls,
    /// `0b1010`: signed greater or equal, N equals V.
    Ge,
    /// `0b1011`: signed less than, N not equal V.
    Lt,
    /// `0b1100`: signed greater than, Z clear and N equals V.
    Gt,
    /// `0b1101`: signed less or equal, Z set or N not equal V.
    Le,
    /// `0b1110`: always executed.
    Al,
}

impl Condition {
    /// Decodes the 4-bit condition field.
    ///
    /// Returns `None` for `0b1111`, which is reserved on ARMv4T.
    #[must_use]
    pub fn from_bits(bits: u8) -> Option<Self> {
        Some(match bits {
            0x0 => Self::Eq,
            0x1 => Self::Ne,
            0x2 => Self::Cs,
            0x3 => Self::Cc,
            0x4 => Self::Mi,
            0x5 => Self::Pl,
            0x6 => Self::Vs,
            0x7 => Self::Vc,
            0x8 => Self::Hi,
            0x9 => Self::Ls,
            0xA => Self::Ge,
            0xB => Self::Lt,
            0xC => Self::Gt,
            0xD => Self::Le,
            0xE => Self::Al,
            0xF => return None,
            _ => return None,
        })
    }

    /// The 4-bit condition field value.
    #[must_use]
    pub fn bits(self) -> u8 {
        match self {
            Self::Eq => 0x0,
            Self::Ne => 0x1,
            Self::Cs => 0x2,
            Self::Cc => 0x3,
            Self::Mi => 0x4,
            Self::Pl => 0x5,
            Self::Vs => 0x6,
            Self::Vc => 0x7,
            Self::Hi => 0x8,
            Self::Ls => 0x9,
            Self::Ge => 0xA,
            Self::Lt => 0xB,
            Self::Gt => 0xC,
            Self::Le => 0xD,
            Self::Al => 0xE,
        }
    }
}
