@ Keypad micro-ROM: polled KEYINPUT and the KEYCNT IRQ.
@
@ Assembled by scripts/build-test-roms.sh into target/test-roms/inputtest.gba
@ (same header shape as irqtest; boots at 0x08000000 in ARM state,
@ program at 0x0C0).
@
@ Setup: system and IRQ stacks, handler pointer at 0x03007FFC, IE bit 12
@ (keypad) only, IME = 1, KEYCNT = select button A, OR condition, IRQ
@ enable (0x4001). The driver presses and releases keys between frames.
@
@ The main loop polls KEYINPUT once per iteration and stores the halfword
@ to IWRAM (+0x00), so the driver can read what the ROM actually saw.
@ The handler checks the BIOS dispatcher contract (r0 = 0x04000000, the
@ IO base the BIOS leaves there), counts entries at +0x04, records the check
@ at +0x08, acknowledges IF, and returns through the HLE sentinel.
@
@ IWRAM scratch (0x03000000):
@   +0x00 u16 polled KEYINPUT snapshot (overwritten every iteration)
@   +0x04 u32 handler entry count
@   +0x08 u32 contract flags: bit0 r0 == 0x04000000, bit15 on any mismatch

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
    .ascii "INP01"
    .org 0x0AC
game_code:
    .ascii "IP"
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
    msr cpsr_c, #0xD2               @ IRQ mode, IRQ+FIQ disabled
    ldr sp, =0x03003F00             @ sp_irq (crt0 convention)
    msr cpsr_c, #0xDF               @ System mode, IRQ+FIQ disabled
    ldr sp, =0x03007F00             @ system stack for the handler

    ldr r0, =irq_handler
    ldr r1, =0x03007FFC             @ [0x03007FFC] = handler (the
    str r0, [r1]                    @ BIOS dispatch target)

    ldr r0, =0x04000200             @ IE: keypad source (bit 12) only
    ldr r1, =0x1000
    strh r1, [r0]

    ldr r0, =0x04000208             @ IME: master IRQ enable
    mov r1, #1
    strb r1, [r0]

    ldr r0, =0x04000132             @ KEYCNT: select A, OR condition,
    ldr r1, =0x4001                 @ IRQ enable (bit 14)
    strh r1, [r0]

    ldr r1, =0x03000000             @ zero the scratch block
    mov r0, #0
    str r0, [r1]
    str r0, [r1, #4]
    str r0, [r1, #8]

    msr cpsr_c, #0x1F               @ System mode, IRQs unmasked

poll:
    ldr r0, =0x04000130             @ KEYINPUT: the polled read
    ldrh r1, [r0]
    ldr r0, =0x03000000
    strh r1, [r0]
    b   poll

    .pool

@ The BIOS dispatcher enters here in ARM IRQ mode with IRQs disabled
@ and r0 = 0x04000000.
irq_handler:
    ldr r2, =0x03000004
    ldr r3, [r2]
    add r3, r3, #1
    str r3, [r2]

    ldr r2, =0x03000008
    ldr r3, [r2]
    cmp r0, #0x04000000             @ the BIOS leaves the IO base in r0
    bne mismatch
    orr r3, r3, #1                  @ bit0: contract held
    b   store_flags
mismatch:
    ldr r12, =0x8000
    orr r3, r3, r12                 @ bit 15: contract violation
store_flags:
    str r3, [r2]

    ldr r0, =0x04000202             @ acknowledge the keypad request
    ldr r1, =0x1000                 @ (write 1 to clear)
    strh r1, [r0]
    bx  lr

    .pool
