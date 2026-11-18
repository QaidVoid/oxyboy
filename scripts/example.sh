#!/usr/bin/env bash
# Worked example: one ROM through the whole oxyboy toolchain.
# Runs info, analyze, disasm with a reassembly check,
# lift --c with a gcc syntax check, recomp, and a headless run,
# writing every artifact into an output directory.
#
# Usage: scripts/example.sh [ROM] [OUT-DIR]
# Defaults: ROM=target/test-roms/switest.gba OUT=target/example
#
# Requires the binary to be built (cargo build) and the corpus ROMs
# (scripts/build-test-roms.sh). The reassembly check additionally
# needs arm-none-eabi-binutils and the gcc check needs a host gcc;
# both skip loudly when missing.
set -euo pipefail

ROM=${1:-target/test-roms/switest.gba}
OUT=${2:-target/example}
if [ -x target/release/oxyboy ] && [ target/release/oxyboy -nt target/debug/oxyboy ]; then
    BIN=target/release/oxyboy
else
    BIN=target/debug/oxyboy
fi

if [ ! -x "$BIN" ]; then
    echo "example: $BIN not built (run: cargo build)" >&2
    exit 1
fi
if [ ! -f "$ROM" ]; then
    echo "example: $ROM missing (run: scripts/build-test-roms.sh)" >&2
    exit 1
fi
KIND=$("$BIN" info --rom "$ROM" --json | grep -m1 '"kind"' | tr -d ' ",:' | sed 's/kind//')
case "$KIND" in
    multiboot) BASE=0x02000000 ;;
    *) BASE=0x08000000 ;;
esac

mkdir -p "$OUT"
echo "== info"
"$BIN" info --rom "$ROM" | tee "$OUT/info.txt"
"$BIN" info --rom "$ROM" --json > "$OUT/info.json"

echo "== analyze (discovery, --config-out)"
"$BIN" analyze --rom "$ROM" | tee "$OUT/analyze.txt"
"$BIN" analyze --rom "$ROM" --json > "$OUT/analyze.json"
"$BIN" analyze --rom "$ROM" --config-out "$OUT/names.conf"
# The naming step a real session would do by hand: name the first
# discovered function, and switest's second function so the workflow
# shows a rejected candidate becoming a named region.
FIRST_ADDR=$(grep -m1 '"addr"' "$OUT/analyze.json" | tr -d ' ",' | cut -d: -f2)
printf 'arm_func %s example_func\n' "$FIRST_ADDR" >> "$OUT/names.conf"
case "$ROM" in
    *switest*) printf 'arm_func 0x02000100 div_helper\n' >> "$OUT/names.conf" ;;
esac

echo "== disasm (matching assembly)"
"$BIN" disasm --rom "$ROM" --config "$OUT/names.conf" --out "$OUT/example.s"
head -6 "$OUT/example.s"

echo "== disasm reassembly check"
if command -v arm-none-eabi-as >/dev/null 2>&1; then
    arm-none-eabi-as "$OUT/example.s" -o "$OUT/example.o"
    arm-none-eabi-ld -Ttext=$BASE -e 0 -o "$OUT/example.elf" "$OUT/example.o"
    arm-none-eabi-objcopy -O binary "$OUT/example.elf" "$OUT/example.bin"
    if cmp -s "$OUT/example.bin" "$ROM"; then
        echo "reassembly is byte-identical"
    else
        echo "example: reassembly differs from the input" >&2
        exit 1
    fi
else
    echo "skip: arm-none-eabi-as not found (install arm-none-eabi-binutils)"
fi

echo "== lift (C output with configured names)"
"$BIN" lift --rom "$ROM" --c --config "$OUT/names.conf" > "$OUT/example.c"
grep -m2 '^u32 ' "$OUT/example.c"
if command -v gcc >/dev/null 2>&1; then
    gcc -fsyntax-only "$OUT/example.c"
    echo "gcc -fsyntax-only: OK"
else
    echo "skip: gcc not found"
fi

echo "== recomp (host C translation unit)"
"$BIN" recomp --rom "$ROM" --config "$OUT/names.conf" --out "$OUT/recomp.c"
"$BIN" recomp --rom "$ROM" --json > "$OUT/recomp.json"
cat "$OUT/recomp.json"

echo "== run (headless frame)"
"$BIN" run --rom "$ROM" --headless --frames 1 --dump-frame "$OUT/frame.png"
ls -l "$OUT/frame.png"

echo "example: all steps green; artifacts in $OUT"
