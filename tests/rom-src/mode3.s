@ Full-image source for the mode-3 test ROM.
@
@ Assembled by scripts/build-test-roms.sh with arm-none-eabi-as/ld into
@ target/test-roms/mode3.gba. The runtime boots at 0x08000000 in ARM state
@ with no BIOS reset, so the entry branch at 0x000 carries execution to the
@ program at 0x0C0, the layout real cartridges use.
@
@ The program enables mode 3 with BG2 on, paints the first six VRAM pixels
@ (two white, two red, two blue), then idles.
@
@ The logo region 0x004..0x0A0 holds the fixed 0xA5 pattern, the same
@ synthetic stand-in testutil::synthetic uses: the copyrighted Nintendo
@ logo bitmap is never committed. The loader does not validate it.

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
    .byte 0xA5
    .endr

    .org 0x0A0
title:
    .ascii "OXYBOY"
    .org 0x0AC
game_code:
    .ascii "M3"
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
    ldr r0, =0x04000000             @ DISPCNT
    ldr r1, =0x0403                 @ mode 3, BG2 on
    str r1, [r0]

    ldr r0, =0x06000000             @ VRAM, first six pixels
    ldr r1, =0x7FFF7FFF             @ white, white
    str r1, [r0]
    ldr r1, =0x001F001F             @ red, red
    str r1, [r0, #4]
    ldr r1, =0x7C007C00             @ blue, blue
    str r1, [r0, #8]

idle:
    b   idle

    .pool

    .org 0x100                       @ keep the image at header size
