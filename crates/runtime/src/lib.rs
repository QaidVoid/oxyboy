//! GBA runtime: bus, CPU interpreter, hardware devices, and the
//! recompilation host.

pub mod apu;
pub mod bus;
pub mod cpu;
pub mod dma;
pub mod input;
pub mod ppu;
pub mod save;
mod swi;
pub mod system;
pub mod timer;
