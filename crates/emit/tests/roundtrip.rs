//! Corpus harness: emitted assembly re-assembles byte-identically.
//!
//! For every corpus ROM (`tests/rom-src/*.s` built by
//! `scripts/build-test-roms.sh`): analyze, emit, assemble with
//! `arm-none-eabi-as`, link at the analysis base with
//! `arm-none-eabi-ld`, convert with `arm-none-eabi-objcopy`, and
//! require byte equality with the input image. Skips loudly when the
//! built ROMs or the binutils toolchain are missing.

use std::path::{Path, PathBuf};
use std::process::Command;

use analysis::config::Config;
use analysis::{FlagMap, FunctionSpec, Thresholds, analyze};
use emit::{ModulePlan, emit_modules, emit_with_warnings};
use rom::Image;
use testutil::testrom;

/// Runs the full round trip over one corpus ROM.
fn round_trip(name: &str) {
    let Some(rom_path) = testrom::require(name) else {
        return;
    };
    let Some(tools) = Tools::find() else {
        println!(
            "skipping round trip of {name}: arm-none-eabi-as/ld/objcopy \
             not found (install arm-none-eabi-binutils)"
        );
        return;
    };

    let image = Image::from_file(&rom_path).expect("corpus ROM parses");
    let mut map = FlagMap::from_image(&image);
    let seeds = map.seed(&[]);
    let _analysis = analyze(&image, &mut map, &seeds, &seeds, Thresholds::default(), &[]);
    // The invariant is only meaningful over code: an empty analysis
    // round-trips as one all-data blob without exercising emission.
    assert!(
        map.runs()
            .any(|run| run.flags.contains(analysis::Flags::EXEC)),
        "{name}: analysis marked no code; the harness would not exercise \
         emission"
    );
    let (asm, warnings) = emit_with_warnings(&image, &map);
    for warning in &warnings {
        println!("  {} warning: {}", name, warning.message);
    }

    let out_dir = emitted_dir();
    std::fs::create_dir_all(&out_dir).expect("create emitted dir");
    let asm_path = out_dir.join(format!("{name}.s"));
    std::fs::write(&asm_path, &asm).expect("write emitted asm");

    let base = map.canonical().base();
    let bin = tools.assemble_and_link(&asm_path, &out_dir.join(name), base);
    let linked = std::fs::read(&bin).expect("linked binary");
    assert_eq!(
        linked.as_slice(),
        image.bytes(),
        "re-assembled {name} must be byte-identical (emitted: {})",
        asm_path.display()
    );
}

#[test]
fn mode3_round_trips_byte_identically() {
    round_trip("mode3");
}

#[test]
fn thumbmix_round_trips_byte_identically() {
    round_trip("thumbmix");
}

#[test]
fn mb_thumbptr_round_trips_byte_identically() {
    round_trip("mb-thumbptr");
}

/// The binutils toolchain, when present.
struct Tools {
    as_: PathBuf,
    ld: PathBuf,
    objcopy: PathBuf,
}

/// The module split each corpus ROM round-trips with: everything from
/// the claimed address on lands in a second module.
const MODULE_SPLITS: &[(&str, &str)] = &[
    ("mode3", "split.s thumb 0x020000C0\n"),
    ("mb-thumbptr", "split.s thumb 0x020000E4\n"),
];

#[test]
fn multi_module_round_trips_byte_identically() {
    for &(name, config_text) in MODULE_SPLITS {
        module_round_trip(name, config_text);
    }
}

/// Splits one corpus ROM into modules, then rebuilds it from the
/// separate objects and requires byte equality.
fn module_round_trip(name: &str, config_text: &str) {
    let Some(rom_path) = testrom::require(name) else {
        return;
    };
    let Some(tools) = Tools::find() else {
        println!(
            "skipping module round trip of {name}: arm-none-eabi-as/ld/objcopy \
             not found (install arm-none-eabi-binutils)"
        );
        return;
    };

    let image = Image::from_file(&rom_path).expect("corpus ROM parses");
    let config = Config::parse(config_text).expect("module config parses");
    let mut map = FlagMap::from_image(&image);
    let seeds = map.seed(
        &config
            .functions
            .iter()
            .map(FunctionSpec::to_seed)
            .collect::<Vec<_>>(),
    );
    analyze(
        &image,
        &mut map,
        &seeds,
        &seeds,
        Thresholds::default(),
        &config.tables,
    );
    let plan = ModulePlan::from_config(&config, &image);
    let emission = emit_modules(&image, &map, &plan);
    for warning in &emission.warnings {
        println!("  {name} warning: {}", warning.message);
    }

    let out_dir = emitted_dir().join(format!("{name}-modules"));
    std::fs::create_dir_all(&out_dir).expect("create module dir");
    let mut sources = Vec::new();
    for (stem, text) in &emission.files {
        let src = out_dir.join(format!("{stem}.s"));
        std::fs::write(&src, text).expect("write module asm");
        sources.push(src);
    }
    let script = out_dir.join("link.ld");
    std::fs::write(&script, &emission.script).expect("write linker script");

    let objects: Vec<PathBuf> = sources
        .iter()
        .map(|src| {
            let object = src.with_extension("o");
            assert!(tools.assemble(src, &object), "as failed for {src:?}");
            object
        })
        .collect();
    let refs: Vec<&Path> = objects.iter().map(|path| path.as_path()).collect();
    let Some(bin) = tools.link(&refs, &script, &out_dir.join("linked")) else {
        panic!(
            "link of {name} modules failed; script: {}",
            out_dir.display()
        );
    };
    let linked = std::fs::read(&bin).expect("linked binary");
    assert_eq!(
        linked.as_slice(),
        image.bytes(),
        "module rebuild of {name} must be byte-identical (emitted: {})",
        out_dir.display()
    );
}

#[test]
fn missing_global_fails_the_link() {
    let Some(rom_path) = testrom::require("mode3") else {
        return;
    };
    let Some(tools) = Tools::find() else {
        println!(
            "skipping missing-global check: arm-none-eabi-as/ld/objcopy \
             not found (install arm-none-eabi-binutils)"
        );
        return;
    };

    let image = Image::from_file(&rom_path).expect("corpus ROM parses");
    let config = Config::parse(MODULE_SPLITS[0].1).expect("module config parses");
    let mut map = FlagMap::from_image(&image);
    let seeds = map.seed(
        &config
            .functions
            .iter()
            .map(FunctionSpec::to_seed)
            .collect::<Vec<_>>(),
    );
    analyze(
        &image,
        &mut map,
        &seeds,
        &seeds,
        Thresholds::default(),
        &config.tables,
    );
    let plan = ModulePlan::from_config(&config, &image);
    let emission = emit_modules(&image, &map, &plan);

    let out_dir = emitted_dir().join("mode3-noglobal");
    std::fs::create_dir_all(&out_dir).expect("create module dir");
    let mut sources = Vec::new();
    for (stem, text) in &emission.files {
        let stripped: String = text
            .lines()
            .filter(|line| !line.starts_with(".global"))
            .map(|line| format!("{line}\n"))
            .collect();
        let src = out_dir.join(format!("{stem}.s"));
        std::fs::write(&src, stripped).expect("write module asm");
        sources.push(src);
    }
    let script = out_dir.join("link.ld");
    std::fs::write(&script, &emission.script).expect("write linker script");

    let objects: Vec<PathBuf> = sources
        .iter()
        .map(|src| {
            let object = src.with_extension("o");
            assert!(tools.assemble(src, &object), "as failed for {src:?}");
            object
        })
        .collect();
    let refs: Vec<&Path> = objects.iter().map(|path| path.as_path()).collect();
    assert!(
        tools
            .link(&refs, &script, &out_dir.join("linked"))
            .is_none(),
        "the link must fail without .global declarations"
    );
}

impl Tools {
    /// Locates the three tools on PATH, or `None` when any is missing.
    fn find() -> Option<Self> {
        Some(Self {
            as_: which("arm-none-eabi-as")?,
            ld: which("arm-none-eabi-ld")?,
            objcopy: which("arm-none-eabi-objcopy")?,
        })
    }

    /// Assembles one source file to an object.
    fn assemble(&self, src: &Path, object: &Path) -> bool {
        Command::new(&self.as_)
            .arg(src)
            .arg("-o")
            .arg(object)
            .status()
            .expect("as runs")
            .success()
    }

    /// Links objects with a linker script and extracts the raw binary;
    /// `None` when the link fails.
    fn link(&self, objects: &[&Path], script: &Path, stem: &Path) -> Option<PathBuf> {
        let elf = stem.with_extension("elf");
        let status = Command::new(&self.ld)
            .arg("-T")
            .arg(script)
            .args(["-e", "0"])
            .arg("-o")
            .arg(&elf)
            .args(objects)
            .status()
            .expect("ld runs");
        if !status.success() {
            return None;
        }
        let binary = stem.with_extension("bin");
        let status = Command::new(&self.objcopy)
            .args(["-O", "binary"])
            .arg(&elf)
            .arg(&binary)
            .status()
            .expect("objcopy runs");
        assert!(status.success(), "objcopy failed for {elf:?}");
        Some(binary)
    }

    /// Assembles, links at `base`, and extracts the raw binary.
    fn assemble_and_link(&self, asm: &Path, stem: &Path, base: u32) -> PathBuf {
        let object = stem.with_extension("o");
        let elf = stem.with_extension("elf");
        let binary = stem.with_extension("bin");
        let asm_arg = asm.display().to_string();
        let object_arg = object.display().to_string();
        let elf_arg = elf.display().to_string();
        let binary_arg = binary.display().to_string();
        let steps = [
            (&self.as_, vec![asm_arg, "-o".into(), object_arg.clone()]),
            (
                &self.ld,
                vec![
                    format!("-Ttext=0x{base:08X}"),
                    "-e".into(),
                    "0".into(),
                    "-o".into(),
                    elf_arg,
                    object_arg,
                ],
            ),
            (
                &self.objcopy,
                vec![
                    "-O".into(),
                    "binary".into(),
                    elf.display().to_string(),
                    binary_arg,
                ],
            ),
        ];
        for (program, args) in &steps {
            let status = Command::new(program)
                .args(args)
                .status()
                .expect("tool runs");
            assert!(status.success(), "{program:?} failed for {asm:?}");
        }
        binary
    }
}

/// Finds an executable on PATH.
fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The directory emitted assembly is written to.
fn emitted_dir() -> PathBuf {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while !root.join("target").exists() {
        match root.parent() {
            Some(parent) => root = parent.to_path_buf(),
            None => break,
        }
    }
    root.join("target").join("emitted")
}
