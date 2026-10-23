@ BIOS-HLE micro-ROM: CpuFastSet and LZ77 with no BIOS.
@
@ Assembled by scripts/build-test-roms.sh into target/test-roms/switest.gba
@ (cart image, boots at 0x08000000). Runs entirely through the SWI HLE
@ path: no BIOS image is mapped.
@
@ Protocol (IWRAM scratch at 0x03000000):
@   +0x00 u8  done flag, C1h when finished
@   +0x04 32b LZ77 output capture (16 decompressed bytes)
@   +0x40 32b CpuFastSet fill capture (8 filled words)
@
@ Steps: LZ77-decompress lz_data into EWRAM 0x02000000, CpuFastSet-fill
@ 8 words of fill_word into EWRAM 0x02000100 (count 8 is already the
@ rounding target), capture both into IWRAM, set the flag, park.

    .syntax unified
    .cpu arm7tdmi
    .arm
    .section .text, "ax"

    .global start

    .org 0x000
entry:
    b   start

    .org 0x004
logo_fill:
    .rept 0x9C
    .byte 0x5A
    .endr

    .org 0x0A0
title:
    .ascii "SWI01"
    .org 0x0AC
game_code:
    .ascii "ASWI"
    .org 0x0B0
maker_code:
    .ascii "00"
    .org 0x0B2
    .byte 0x96
    .org 0x0B3
    .byte 0x00
    .org 0x0BC
    .byte 0x00
    .org 0x0BD
checksum:
    .byte 0x00

    .org 0x0C0
    b   start

    .org 0x100
start:
    msr cpsr_c, #0xDF
    ldr sp, =0x03007F00

    @ LZ77UnCompWram: r0 = source stream, r1 = destination.
    ldr r0, =lz_data
    ldr r1, =0x02000000
    swi 0x110000

    @ CpuFastSet fill: r0 = source word, r1 = destination, r2 = fill
    @ mode + 8 words.
    ldr r0, =fill_word
    ldr r1, =0x02000100
    mov r2, #0x01000000
    orr r2, r2, #8
    swi 0x0C0000

    @ Capture the 16 decompressed bytes (4 words; CpuFastSet rounds up
    @ to 8 words, the tail reads zeroed EWRAM).
    ldr r0, =0x02000000
    ldr r1, =0x03000004
    mov r2, #4
    swi 0x0C0000

    @ Capture the 8 filled words.
    ldr r0, =0x02000100
    ldr r1, =0x03000040
    mov r2, #8
    swi 0x0C0000

    ldr r3, =0x03000000
    mov r4, #0xC1
    strb r4, [r3]

park:
    b   park

    .balign 4
lz_data:
    @ Header: type 1 (LZ77), 16 bytes out. Two literal flag groups of
    @ 0x10..0x1F, padded to the 4-byte bound.
    .word 0x00001010
    .byte 0x00
    .byte 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17
    .byte 0x00
    .byte 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F
    .byte 0x00, 0x00

    .balign 4
fill_word:
    .word 0x5A5AA5A5

    .pool
