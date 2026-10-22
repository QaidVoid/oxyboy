@ Full-image source for the THUMB-mix test ROM.
@
@ Assembled by scripts/build-test-roms.sh with arm-none-eabi-as/ld into
@ target/test-roms/thumbmix.gba. Same header shape as mode3: the runtime
@ boots at 0x08000000 in ARM state, the entry branch carries execution to
@ the program at 0x0C0.
@
@ The program switches into THUMB state, enables mode 3 with BG2 on,
@ paints the first two VRAM pixels white, then idles in THUMB. It exists
@ so emission exercises both assembler dialects, THUMB literal pools, and
@ the ARM-to-THUMB state switch in one image.
@
@ The logo region 0x004..0x0A0 holds the 0x5A stand-in pattern (mode3
@ uses 0xA5; a different byte keeps the two corpora distinguishable in
@ hex dumps). The loader does not validate the logo.

    .syntax unified
    .cpu arm7tdmi
    .arm
    .section .text, "ax"

    .global start

    .org 0x000
entry:
    b   start                       @ branch over the header to 0x0C0

    .org 0x004
logo_fill:
    .rept 0x9C                      @ logo stand-in, 0x004..0x0A0
    .byte 0x5A
    .endr

    .org 0x0A0
title:
    .ascii "THMIX"
    .org 0x0AC
game_code:
    .ascii "TX"
    .org 0x0B0
maker_code:
    .ascii "00"
    .org 0x0B2
    .byte 0x96                      @ fixed unit code
    .org 0x0B3
    .byte 0x00                      @ device type
    .org 0x0BC
    .byte 0x00                      @ software version
    .org 0x0BD
checksum:
    .byte 0x00                      @ complement checksum, patched by
                                    @ the build script

    .org 0x0C0
start:
    ldr r0, =thumb_start+1          @ THUMB entry, bit 0 set for BX
    bx  r0

thumb_start:
    .thumb
    ldr r1, =0x04000000             @ DISPCNT
    movs r2, #3                     @ mode 3, BG2 on
    strh r2, [r1]

    ldr r3, =0x06000000             @ VRAM, first two pixels
    ldr r0, =0x03FF03FF             @ white, white
    str r0, [r3]

loop:
    b   loop

    .pool

    .org 0x100                      @ keep the image at header size
