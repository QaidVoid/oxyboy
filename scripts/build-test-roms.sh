#!/bin/sh
# Assembles the checked-in test ROM sources (tests/rom-src/*.s) into raw
# .gba images in target/test-roms/ using arm-none-eabi-binutils.
#
# Usage: scripts/build-test-roms.sh
#
# Tests never invoke this script: they locate built ROMs through
# testutil::testrom and skip loudly when none are found, so a machine
# without the toolchain still runs the full suite. Run this script once
# to enable the ROM-dependent tests.

set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
src_dir=$root/tests/rom-src
out_dir=$root/target/test-roms

for tool in arm-none-eabi-as arm-none-eabi-ld arm-none-eabi-objcopy; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "build-test-roms: missing $tool (install arm-none-eabi-binutils)" >&2
        exit 1
    fi
done

mkdir -p "$out_dir"

for src in "$src_dir"/*.s; do
    name=$(basename "${src%.s}")
    # ROMs prefixed mb- are linked at the multiboot execution base so
    # canonical addresses, pool literals, and the analysis base agree;
    # everything else is a cart image linked at the cart base.
    case "$name" in
        mb-*) text_base=0x02000000 ;;
        *)    text_base=0x08000000 ;;
    esac
    elf="$out_dir/$name.elf"
    bin="$out_dir/$name.gba"

    arm-none-eabi-as -o "$out_dir/$name.o" "$src"
    # Entry defaults to the .text start when -e cannot resolve; passing it
    # explicitly makes a misplaced .global fail the build instead of booting
    # from the wrong address.
    arm-none-eabi-ld -Ttext="$text_base" -e start -o "$elf" "$out_dir/$name.o"
    arm-none-eabi-objcopy -O binary "$elf" "$bin"
    rm -f "$out_dir/$name.o" "$elf"

    # Patch the header complement checksum at 0x0BD:
    # 0x100 - sum(header[0x0A0..0x0BD]) mod 256 (GBATEK, "The Cartridge
    # Header"). The assembler cannot compute it across the byte field, so
    # the source stores a placeholder.
    sum=$(od -An -tu1 -j 160 -N 29 "$bin" | awk '{for (i = 1; i <= NF; i++) s += $i} END {print s}')
    checksum=$(( (256 - sum % 256) % 256 ))
    printf "\\$(printf '%03o' "$checksum")" |
        dd of="$bin" bs=1 seek=189 conv=notrunc 2>/dev/null

    echo "built $bin"
done
