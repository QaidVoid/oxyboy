//! Throwaway probe: cycles of 4 parked steps. Delete after use.
use runtime::system::System;

#[test]
fn probe_parked_cycles() {
    let mut system = System::new(vec![0xEA, 0xFF, 0xFF, 0xFE], 0x0800_0000);
    for k in 0..4 {
        let c = system.step();
        eprintln!(
            "step {k}: charged {c}, pc={:08x}",
            system.cpu.pc()
        );
    }
}
