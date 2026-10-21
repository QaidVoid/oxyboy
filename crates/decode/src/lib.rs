//! ARMv4T (ARM7TDMI) instruction decoding for ARM and THUMB state.
//!
//! Decoding is total: every 32-bit ARM word and every 16-bit THUMB
//! halfword produces a defined instruction or a typed
//! [`ArmInstr::Undefined`] / [`ThumbInstr::Undefined`] value, never a
//! panic or an error. Encodings the ARM7TDMI would not execute (reserved
//! condition `0b1111`, the coprocessor space, the undefined-instruction
//! spaces) come back as `Undefined`.
//!
//! Offsets are position-independent; computing absolute branch and
//! literal-pool targets is the caller's job because it needs the
//! instruction address.
//!
//! # Examples
//!
//! ```
//! use decode::{decode_arm, ArmInstr, DataOp, ShifterOperand};
//!
//! // mov r0, #1
//! match decode_arm(0xE3A0_0001) {
//!     ArmInstr::DataProcessing { opcode, operand2, .. } => {
//!         assert_eq!(opcode, DataOp::Mov);
//!         assert!(matches!(operand2, ShifterOperand::Immediate { .. }));
//!     }
//!     other => panic!("unexpected decode: {other:?}"),
//! }
//! ```

pub mod arm;
pub mod condition;
pub mod register;
pub mod thumb;

pub use arm::{
    ArmInstr, DataOp, HalfwordKind, HalfwordOffset, PsrSource, RegisterShift, ShiftAmount,
    ShiftKind, ShiftType, ShifterOperand, TransferOffset, decode_arm,
};
pub use condition::Condition;
pub use register::{Register, RegisterList};
pub use thumb::{
    ThumbAluOp, ThumbHiOp, ThumbImmOp, ThumbInstr, ThumbMemOp, ThumbShift, ThumbSignedMemOp,
    decode_thumb,
};

/// Extracts an unsigned bit field `hi..=lo` (inclusive) from `word`.
pub(crate) fn bits(word: u32, hi: u32, lo: u32) -> u32 {
    debug_assert!(hi >= lo && hi < 32);
    (word >> lo) & ((1u32 << (hi - lo + 1)) - 1)
}

/// Extracts an unsigned bit field `hi..=lo` (inclusive) from `halfword`.
pub(crate) fn thumb_bits(halfword: u16, hi: u32, lo: u32) -> u16 {
    debug_assert!(hi >= lo && hi < 16);
    (halfword >> lo) & ((1u16 << (hi - lo + 1)) - 1)
}

/// Sign-extends the low `width` bits of `value` to a full `i32`.
pub(crate) fn sign_extend(value: u32, width: u32) -> i32 {
    let shift = 32 - width;
    ((value << shift) as i32) >> shift
}

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_wiring() {
        assert_eq!(env!("CARGO_PKG_NAME"), "decode");
    }

    #[test]
    fn bit_helpers() {
        assert_eq!(super::bits(0xE3A0_0001, 11, 0), 1);
        assert_eq!(super::thumb_bits(0x4770, 15, 8), 0x47);
        assert_eq!(super::sign_extend(0xFFFF_FFFE, 24), -2);
    }
}
