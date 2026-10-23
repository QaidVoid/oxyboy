@ Flash 128K micro-ROM: bank switching, erase, program.
@
@ Assembled by scripts/build-test-roms.sh into target/test-roms/flash128test.gba
@ (cart image, boots at 0x08000000). The FLASH1M_V ID string sits at
@ 0x0C4, word aligned, so SaveKind::detect finds it.
@
@ Protocol (IWRAM scratch at 0x03000000):
@   +0x00 u8  phase: 0 = erase then program, both phases capture
@   +0x04 8b  capture of bank 0 at 0x0E000200..0x0E000208
@   +0x0C 8b  capture of bank 1 at the same window offset
@
@ Phase 0 erases sector 0 in each bank and programs 8 pattern bytes
@ per bank: bank 0 at window 0x0200, bank 1 at the same window offset
@ (GBATEK "Bank Switching": [5555]=AA, [2AAA]=55, [5555]=B0, [0000]=
@ bank; the upper 64K is visible only while bank 1 is selected).

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
    .ascii "FLM101"
    .org 0x0AC
game_code:
    .ascii "AFLM"
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
    .ascii "FLASH1M_V"
    .byte 0x30, 0x30, 0x30

    .org 0x100
start:
    msr cpsr_c, #0xDF               @ System mode, IRQ+FIQ disabled
    ldr sp, =0x03007F00             @ stack for the call/push sequences

    ldr r2, =0x03000000
    ldrb r3, [r2]
    cmp r3, #0
    bne capture

    @ Erase sector 0 of bank 0 and program 8 bytes at window 0x0200.
    bl  erase_sector0
    mov r4, #0
    bl  program_window

    @ Switch to bank 1, erase its sector 0, program the same window.
    mov r0, #1
    bl  select_bank
    bl  erase_sector0
    mov r4, #1
    bl  program_window
    @ Stay in bank 1 for the capture below.

capture:
    @ Bank 0 bytes first: switch, copy 8, switch back to bank 1.
    mov r0, #0
    bl  select_bank
    ldr r0, =0x0E000200
    ldr r5, =0x03000004
    mov r1, #0
read_bank0:
    ldrb r3, [r0, r1]
    strb r3, [r5, r1]
    add r1, r1, #1
    cmp r1, #8
    blt read_bank0
    mov r0, #1
    bl  select_bank
    ldr r0, =0x0E000200
    ldr r5, =0x0300000C
    mov r1, #0
read_bank1:
    ldrb r3, [r0, r1]
    strb r3, [r5, r1]
    add r1, r1, #1
    cmp r1, #8
    blt read_bank1

park:
    b   park

    @ Selects the 64K bank in r0 (GBATEK: [5555]=AA, [2AAA]=55,
    @ [5555]=B0, [0000]=bank).
select_bank:
    push {r1, r2}
    ldr r2, =0x0E005555
    mov r1, #0xAA
    strb r1, [r2]
    ldr r2, =0x0E002AAA
    mov r1, #0x55
    strb r1, [r2]
    ldr r2, =0x0E005555
    mov r1, #0xB0
    strb r1, [r2]
    ldr r2, =0x0E000000
    strb r0, [r2]
    pop {r1, r2}
    bx  lr

    @ Erases sector 0 of the selected bank: [5555]=AA, [2AAA]=55,
    @ [5555]=80, [5555]=AA, [2AAA]=55, [0000]=30.
erase_sector0:
    push {r1, r2, lr}
    ldr r2, =0x0E005555
    mov r1, #0xAA
    strb r1, [r2]
    ldr r2, =0x0E002AAA
    mov r1, #0x55
    strb r1, [r2]
    ldr r2, =0x0E005555
    mov r1, #0x80
    strb r1, [r2]
    mov r1, #0xAA
    strb r1, [r2]
    ldr r2, =0x0E002AAA
    mov r1, #0x55
    strb r1, [r2]
    ldr r2, =0x0E000000
    mov r1, #0x30
    strb r1, [r2]
    pop {r1, r2, lr}
    bx  lr

    @ Programs 8 pattern bytes (i xor 3Ch xor 10h*bank) at window
    @ 0x0200 of the selected bank; r4 is the bank tag.
program_window:
    push {r0, r1, r2, r3, lr}
    ldr r0, =0x0E000200
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
    cmp r4, #0
    addne r3, r3, #0x10
    strb r3, [r0, r1]
    pop {r0, r1}
    add r1, r1, #1
    cmp r1, #8
    blt program_loop
    pop {r0, r1, r2, r3, lr}
    bx  lr

    .pool
