@ SRAM micro-ROM: plain byte writes and reads.
@
@ Assembled by scripts/build-test-roms.sh into target/test-roms/sramtest.gba
@ (cart image, boots at 0x08000000). The SRAM_V ID string sits at
@ 0x0C4, word aligned, so SaveKind::detect finds it.
@
@ Protocol (IWRAM scratch at 0x03000000):
@   +0x00 u8  phase: 0 = write then capture, 1 = capture only
@   +0x04 16b capture of 0x0E001000..0x0E001010
@
@ Phase 0 stores i ^ A5h at 0x0E001000+i (i = 0..15); both phases copy
@ the 16 bytes into the capture block. SRAM reads are supposed to run
@ from WRAM on hardware (GBATEK "GBA Cart Backup SRAM/FRAM"); the
@ emulator model has no such restriction, and the parking loop keeps
@ the code in ROM anyway.

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
    .ascii "SRAM01"
    .org 0x0AC
game_code:
    .ascii "ASAM"
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

    .org 0x0C4
save_id:
    .ascii "SRAM_V"
    .byte 0x31, 0x30

    .org 0x100
start:
    ldr r0, =0x0E001000
    ldr r2, =0x03000000
    ldrb r3, [r2]
    cmp r3, #0
    bne capture

    @ Phase 0: write the pattern bytes.
    mov r1, #0
write_loop:
    eor r4, r1, #0xA5
    strb r4, [r0, r1]
    add r1, r1, #1
    cmp r1, #16
    blt write_loop

capture:
    ldr r5, =0x03000004
    mov r1, #0
read_loop:
    ldrb r3, [r0, r1]
    strb r3, [r5, r1]
    add r1, r1, #1
    cmp r1, #16
    blt read_loop

park:
    b   park

    .pool
