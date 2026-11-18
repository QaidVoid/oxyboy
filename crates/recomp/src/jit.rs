//! Compiles emitted C with the host compiler and maps the result.
//!
//! The library stays mapped for the [`Module`] lifetime; symbols are
//! resolved per call. Each build compiles in its own fresh scratch
//! directory under the system temp dir (owner-only, never reused), which
//! is removed when the module drops.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use libloading::{Library, Symbol};

use crate::abi::{GuestOps, GuestState};

/// A compiled and loaded translation unit.
pub struct Module {
    /// Keeps the mapped code alive.
    _lib: Library,
    dir: PathBuf,
}

/// Compiles `c_source` in `dir` and maps the resulting library.
fn compile_and_load(c_source: &str, dir: &std::path::Path) -> Result<Library, String> {
    let src = dir.join("mod.c");
    let so = dir.join("libmod.so");
    std::fs::write(&src, c_source).map_err(|e| format!("{}: {e}", src.display()))?;
    let output = Command::new("cc")
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
    if !output.status.success() {
        return Err(format!(
            "cc failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    unsafe { Library::new(&so) }.map_err(|e| format!("dlopen: {e}"))
}

/// Builds made by this process so far; part of every scratch name.
static BUILDS: AtomicU32 = AtomicU32::new(0);

/// Creates a new, empty scratch directory only this user can access.
/// Creation fails on an existing path, so a directory planted by
/// another user is never adopted; the name is retried instead.
fn scratch_dir(tag: &str) -> Result<PathBuf, String> {
    for _ in 0..16 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let dir = std::env::temp_dir().join(format!(
            "oxyboy-recomp-{tag}-{}-{}-{nanos:08x}",
            std::process::id(),
            BUILDS.fetch_add(1, Ordering::Relaxed)
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("{}: {e}", dir.display())),
        }
    }
    Err("could not create a fresh recomp scratch directory".to_string())
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
    /// names the scratch directory for debugging; every build gets a
    /// new one, so a rebuild never loads a stale library.
    ///
    /// # Errors
    ///
    /// Returns a printable message on filesystem, compiler, or loader
    /// failure.
    pub fn build(c_source: &str, tag: &str) -> Result<Self, String> {
        let dir = scratch_dir(tag)?;
        match compile_and_load(c_source, &dir) {
            Ok(lib) => Ok(Module { _lib: lib, dir }),
            Err(e) => {
                std::fs::remove_dir_all(&dir).ok();
                Err(e)
            }
        }
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
        // The library is still mapped here (fields drop after this
        // body); unlinking a mapped file is safe, the mapping stays
        // valid until the library unmaps.
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

    unsafe extern "C" fn fetch(
        _ctx: *mut core::ffi::c_void,
        st: *mut GuestState,
        _addr: u32,
        _width: u32,
    ) {
        unsafe { (*st).cycles += 1 };
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

    /// Calls the module's function at 0x08000100 with r0 = 0x1000 and
    /// returns the resulting r0.
    fn call_r0(module: &Module) -> u32 {
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
            thumb: 0,
            cycles: 0,
            icount: 0,
        };
        st.r[0] = 0x1000;
        let ops = GuestOps {
            ctx: std::ptr::from_ref(&host)
                .cast::<core::ffi::c_void>()
                .cast_mut(),
            load,
            store,
            fetch,
            budget: 1000,
            cycle_budget: u64::MAX,
        };
        module.call(0x0800_0100, &mut st, &ops).expect("call");
        st.r[0]
    }

    #[test]
    fn rebuilds_with_one_tag_load_their_own_code() {
        if !Module::compiler_available() {
            println!("skipping: no host C compiler on PATH");
            return;
        }
        let four = adding_function(4);
        let eight = adding_function(8);
        let first = Module::build(
            &crate::emit_module(&[(0x0800_0100, &four)]).c_source,
            "same",
        )
        .expect("compile");
        let second = Module::build(
            &crate::emit_module(&[(0x0800_0100, &eight)]).c_source,
            "same",
        )
        .expect("compile");
        assert_ne!(first.dir, second.dir, "every build gets its own directory");
        assert_eq!(call_r0(&first), 0x1004);
        assert_eq!(call_r0(&second), 0x1008, "not the first library again");
    }

    #[test]
    fn scratch_dirs_are_private_and_removed_on_drop() {
        if !Module::compiler_available() {
            println!("skipping: no host C compiler on PATH");
            return;
        }
        let f = test_function();
        let module = Module::build(
            &crate::emit_module(&[(0x0800_0100, &f)]).c_source,
            "private",
        )
        .expect("compile");
        let dir = module.dir.clone();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).expect("dir").permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        drop(module);
        assert!(!dir.exists(), "scratch dir removed");
        assert!(Module::build("not c", "broken").is_err());
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
            thumb: 0,
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
            fetch,
            budget: 1000,
            cycle_budget: u64::MAX,
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
        adding_function(4)
    }

    /// `f { r0 = r0 + add; mem[r0] = r1; ret }`.
    fn adding_function(add: u32) -> ir::Function {
        use ir::{Block, Function, Inst, Op, Terminator, Value, ValueId};
        let reg = |i: u32| Value::Id(ValueId(i));
        Function {
            name: "sub_08000100".to_string(),
            thumb: false,
            blocks: vec![Block {
                addr: 0x0800_0100,
                params: (0..20).map(ValueId).collect(),
                cycles: vec![
                    ir::CycleCharge {
                        addr: 0x0800_0100,
                        cycles: 0,
                    },
                    ir::CycleCharge {
                        addr: 0x0800_0104,
                        cycles: 0,
                    },
                ],
                insts: vec![
                    Inst {
                        guard: None,
                        dests: vec![ValueId(20)],
                        fallbacks: Vec::new(),
                        op: Op::Add {
                            set_flags: false,
                            a: reg(0),
                            b: Value::Const(add),
                        },
                        pc: 0x0800_0100,
                        state: Vec::new(),
                    },
                    Inst {
                        guard: None,
                        dests: vec![],
                        fallbacks: Vec::new(),
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
                    switches_state: false,
                },
            }],
        }
    }
}
