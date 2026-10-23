//! Compiles emitted C with the host compiler and maps the result.
//!
//! The library stays mapped for the [`Module`] lifetime; symbols are
//! resolved per call. Compilation happens in a scratch directory under
//! the system temp dir and is cleaned up when the module drops.

use std::path::PathBuf;
use std::process::Command;

use libloading::{Library, Symbol};

use crate::abi::{GuestOps, GuestState};

/// A compiled and loaded translation unit.
pub struct Module {
    /// Keeps the mapped code alive; dropped before the scratch dir.
    _lib: Library,
    dir: PathBuf,
}

impl Module {
    /// True when a working host C compiler is on PATH. Callers that
    /// cannot do without should skip loudly when this is false.
    #[must_use]
    pub fn compiler_available() -> bool {
        Command::new("cc")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Compiles `c_source` into a shared library and maps it. `tag`
    /// namespaces the scratch directory.
    ///
    /// # Errors
    ///
    /// Returns a printable message on filesystem, compiler, or loader
    /// failure.
    pub fn build(c_source: &str, tag: &str) -> Result<Self, String> {
        let dir = std::env::temp_dir().join(format!("oxyboy-recomp-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let src = dir.join("mod.c");
        let so = dir.join("libmod.so");
        std::fs::write(&src, c_source).map_err(|e| format!("{}: {e}", src.display()))?;
        let status = Command::new("cc")
            .args([
                "-shared",
                "-fPIC",
                "-O1",
                "-fno-strict-aliasing",
                "-Wall",
                "-x",
                "c",
            ])
            .arg(&src)
            .arg("-o")
            .arg(&so)
            .output()
            .map_err(|e| format!("cc: {e}"))?;
        if !status.status.success() {
            return Err(format!(
                "cc failed:\n{}",
                String::from_utf8_lossy(&status.stderr)
            ));
        }
        let lib = unsafe { Library::new(&so) }.map_err(|e| format!("dlopen: {e}"))?;
        Ok(Module { _lib: lib, dir })
    }

    /// Calls the translated function for `start` with the state and
    /// ops table.
    ///
    /// # Errors
    ///
    /// Returns a printable message when the symbol is missing.
    pub fn call(&self, start: u32, st: &mut GuestState, ops: &GuestOps) -> Result<(), String> {
        let name = crate::symbol(start);
        let func: Symbol<unsafe extern "C" fn(*mut GuestState, *const GuestOps)> =
            unsafe { self._lib.get(name.as_bytes()) }.map_err(|e| format!("{name}: {e}"))?;
        unsafe { func(st, ops) };
        Ok(())
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        // The library unmaps first (declared before `dir`).
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{SIZE_BYTE, SIZE_WORD};
    use std::cell::RefCell;

    /// Flat 64 KiB RAM host with the evaluator's word semantics.
    struct Host {
        mem: RefCell<Vec<u8>>,
    }

    unsafe extern "C" fn load(
        ctx: *mut core::ffi::c_void,
        _st: *mut GuestState,
        addr: u32,
        size: u32,
        _sext: u32,
        _mode: u32,
    ) -> u32 {
        let host = unsafe { &*(ctx.cast::<Host>()) };
        let mem = host.mem.borrow_mut();
        let read = |a: u32| mem[(a & 0xFFFF) as usize];
        match size {
            SIZE_WORD => {
                u32::from(read(addr))
                    | (u32::from(read(addr + 1)) << 8)
                    | (u32::from(read(addr + 2)) << 16)
                    | (u32::from(read(addr + 3)) << 24)
            }
            SIZE_BYTE => u32::from(read(addr)),
            _ => panic!("test host: unsupported size {size}"),
        }
    }

    unsafe extern "C" fn store(
        ctx: *mut core::ffi::c_void,
        _st: *mut GuestState,
        addr: u32,
        value: u32,
        size: u32,
        _pc: u32,
    ) -> u32 {
        let host = unsafe { &*(ctx.cast::<Host>()) };
        let mut mem = host.mem.borrow_mut();
        let n = match size {
            SIZE_WORD => 4,
            SIZE_BYTE => 1,
            _ => panic!("test host: unsupported size {size}"),
        };
        for i in 0..n {
            mem[((addr + i) & 0xFFFF) as usize] = (value >> (i * 8)) as u8;
        }
        0
    }

    #[test]
    fn compiles_and_executes_a_translated_function() {
        if !Module::compiler_available() {
            println!("skipping: no host C compiler on PATH");
            return;
        }
        let f = test_function();
        let source = crate::emit_module(&[(0x0800_0100, &f)]).c_source;
        let module = Module::build(&source, "e2e").expect("compile");

        let host = Host {
            mem: RefCell::new(vec![0; 0x1_0000]),
        };
        let mut st = GuestState {
            r: [0; 16],
            n: 0,
            z: 0,
            c: 0,
            v: 0,
            pc: 0,
            exited: 0,
            cycles: 0,
            icount: 0,
        };
        st.r[0] = 0x1000;
        st.r[1] = 0xABCD_EF01;
        let ops = GuestOps {
            ctx: std::ptr::from_ref(&host)
                .cast::<core::ffi::c_void>()
                .cast_mut(),
            load,
            store,
            budget: 1000,
        };
        module.call(0x0800_0100, &mut st, &ops).expect("call");
        let mem = host.mem.borrow();
        // The function stored r1 at r0+4 and returned r0+4 in r0.
        assert_eq!(&mem[0x1004..0x1008], &[0x01, 0xEF, 0xCD, 0xAB]);
        assert_eq!(st.r[0], 0x1004);
        assert_eq!(st.exited, 0);
        assert_eq!(st.icount, 2);
    }

    /// `f { r0 = r0 + 4; mem[r0] = r1; ret }`.
    fn test_function() -> ir::Function {
        use ir::{Block, Function, Inst, Op, Terminator, Value, ValueId};
        let reg = |i: u32| Value::Id(ValueId(i));
        Function {
            name: "sub_08000100".to_string(),
            blocks: vec![Block {
                addr: 0x0800_0100,
                params: (0..20).map(ValueId).collect(),
                insts: vec![
                    Inst {
                        guard: None,
                        dests: vec![ValueId(20)],
                        op: Op::Add {
                            set_flags: false,
                            a: reg(0),
                            b: Value::Const(4),
                        },
                        pc: 0x0800_0100,
                        state: Vec::new(),
                    },
                    Inst {
                        guard: None,
                        dests: vec![],
                        op: Op::Store {
                            addr: reg(20),
                            value: reg(1),
                            size: ir::Size::Word,
                        },
                        pc: 0x0800_0104,
                        state: (0..20).map(reg).collect(),
                    },
                ],
                term: Terminator::Return {
                    state: (0..20)
                        .map(|slot| if slot == 0 { reg(20) } else { reg(slot) })
                        .collect(),
                },
            }],
        }
    }
}
