@ Flash 64K micro-ROM: erase, program, and read back.
@
@ Assembled by scripts/build-test-roms.sh into target/test-roms/flash64test.gba
@ (cart image, boots at 0x08000000). The FLASH512_V ID string sits at
@ 0x0C4, word aligned, so SaveKind::detect finds it.
@
@ Protocol (IWRAM scratch at 0x03000000):
@   +0x00 u8  phase: 0 = erase then program, both phases capture
@   +0x04 16b capture of 0x0E000100..0x0E000110
@
@ Phase 0 erases sector 0 (0x0E000000..0x0E001000, GBATEK "Erase 4Kbyte
@ Sector") and programs 16 pattern bytes at 0x0E000100 (write byte
@ command A0h; erased flash is FFh-filled and writes clear bits only).
@ All flash accesses are 8-bit STRB/LDRB (GBATEK "Accessing FLASH
@ Memory").

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
    .ascii "FL6401"
    .org 0x0AC
game_code:
    .ascii "AFL6"
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
    .ascii "FLASH512_V"
    .byte 0x00

    .org 0x100
start:
    msr cpsr_c, #0xDF               @ System mode, IRQ+FIQ disabled
    ldr sp, =0x03007F00             @ stack for the call/push sequences

    ldr r2, =0x03000000
    ldrb r3, [r2]
    cmp r3, #0
    bne capture

    @ Erase sector 0: [5555]=AA, [2AAA]=55, [5555]=80, [5555]=AA,
    @ [2AAA]=55, [0000]=30 (GBATEK "Erase 4Kbyte Sector").
    ldr r0, =0x0E005555
    mov r1, #0xAA
    strb r1, [r0]
    ldr r0, =0x0E002AAA
    mov r1, #0x55
    strb r1, [r0]
    ldr r0, =0x0E005555
    mov r1, #0x80
    strb r1, [r0]
    @ Confirm: [5555]=AA, [2AAA]=55, [0000]=30.
    ldr r0, =0x0E005555
    mov r1, #0xAA
    strb r1, [r0]
    ldr r0, =0x0E002AAA
    mov r1, #0x55
    strb r1, [r0]
    ldr r0, =0x0E000000
    mov r1, #0x30
    strb r1, [r0]

    @ Program 16 bytes at 0x0E000100: [5555]=AA, [2AAA]=55, [5555]=A0,
    @ [addr]=data per byte (GBATEK "Write Single Data Byte").
    ldr r0, =0x0E000100
    mov r1, #0
program_loop:
    push {r0, r1}
    ldr r2, =0x0E005555
    mov r3, #0xAA
    strb r3, [r2]
    ldr r2, =0x0E002AAA
    mov r3, #0x55
    strb r3, [r2]
    ldr r2, =0x0E005555
    mov r3, #0xA0
    strb r3, [r2]
    eor r3, r1, #0x3C
    strb r3, [r0, r1]
    pop {r0, r1}
    add r1, r1, #1
    cmp r1, #16
    blt program_loop

capture:
    ldr r0, =0x0E000100
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
