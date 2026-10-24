# oxyboy

A GBA (ARM7TDMI) analyzer, decompiler, and runtime in Rust: load a
`.gba` ROM, separate code from data, recover functions and control
flow, and emit (a) matching, re-assemblable assembly and (b) C-like
output, then execute the same ROM through a reference interpreter or
static recompilation to host code.

Two halves over one shared core:

1. **Analyzer/decompiler**: flag map, function discovery, switch-table
   recovery, IR lifting with per-function optimization, and three
   output forms: assembly (byte-identical on re-assembly), C
   translation units that pass `gcc -fsyntax-only`, and annotated IR
   text with recovered signatures.
2. **Runtime**: a cycle-accounting interpreter (PPU, APU, timers, DMA,
   save media, BIOS HLE) as the oracle, plus static recompilation with
   interpreter fallback for anything analysis cannot resolve.

## Build

```
cargo build --release
```

Optional: `arm-none-eabi-binutils` (re-assembly checks, test-ROM
builds) and a host C compiler (recompiled execution, C syntax checks).

## Quickstart

```
scripts/build-test-roms.sh            # corpus ROMs into target/test-roms
target/release/oxyboy info --rom target/test-roms/switest.gba
target/release/oxyboy analyze --rom target/test-roms/switest.gba
target/release/oxyboy disasm --rom target/test-roms/switest.gba --out switest.s
target/release/oxyboy lift --rom target/test-roms/switest.gba --c
target/release/oxyboy run --rom target/test-roms/mode3.gba --headless --frames 1 --dump-frame frame.png
scripts/example.sh                    # the whole chain, verified step by step
```

## Development

`scripts/check.sh` is the gate (fmt, clippy with `-D warnings`,
tests, doc build, non-ASCII check); `cargo test` covers unit tests,
per-crate integration tests, and CLI end-to-end suites. The planning
documents that drove development live in `plans/` (not part of the
shipped repository).
