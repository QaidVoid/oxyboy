@ IRQ micro-ROM: VBlank interrupt through the BIOS-HLE
@ dispatch path.
@
@ Assembled by scripts/build-test-roms.sh into target/test-roms/irqtest.gba
@ (same header shape as mode3/thumbmix; boots at 0x08000000 in ARM state,
@ program at 0x0C0).
@
@ Setup: system and IRQ stacks, handler pointer at 0x03007FFC, DISPSTAT
@ V-Blank IRQ enable (bit 3), IE bit 0. IME is deliberately left 0: the
@ Rust driver verifies the IME=0 masking case over the first frames, then
@ flips IME itself and counts dispatches.
@
@ The main loop parks on a heartbeat counter, keeping the interrupted
@ registers (r0..r3, r12) at constant canary values. The handler checks
@ the BIOS-HLE contract (r0 = IE & IF == 1 for the single VBlank source)
@ and canary preservation, counts entries, acknowledges IF (write 1 to
@ clear), and returns with bx lr through the HLE sentinel.
@
@ IWRAM scratch (0x03000000):
@   +0x00 u32 handler entry count
@   +0x04 u32 unused (zero)
@   +0x08 u32 contract flags: bit0 r0 == IE&IF, bit1 canaries preserved,
@                             bit15 set on any mismatch
@   +0x0C u32 unused (zero)
@   +0x10 u32 main-loop heartbeat

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
    .ascii "IRQ01"
    .org 0x0AC
game_code:
    .ascii "IQ"
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
    ldr sp, =0x03003F00             @ sp_irq (crt0 convention; HLE does
                                    @ not use it, but set it anyway)
    msr cpsr_c, #0xDF               @ System mode, IRQ+FIQ disabled
    ldr sp, =0x03007F00             @ system stack for the handler

    ldr r0, =irq_handler
    ldr r1, =0x03007FFC             @ [0x03007FFC] = handler (the
    str r0, [r1]                    @ BIOS dispatch target)

    ldr r0, =0x04000004             @ DISPSTAT: V-Blank IRQ enable
    mov r1, #0x08                   @ (bit 3) only
    strh r1, [r0]

    ldr r0, =0x04000200             @ IE: VBlank source (bit 0) only
    mov r1, #0x01
    strh r1, [r0]

    @ IME stays 0 (reset default): the driver tests masking first.

    ldr r1, =0x03000000             @ zero the scratch block
    mov r0, #0
    str r0, [r1]
    str r0, [r1, #4]
    str r0, [r1, #8]
    str r0, [r1, #12]
    str r0, [r1, #16]

    msr cpsr_c, #0x1F               @ unmask CPU IRQs (I clear), stay
                                    @ in System mode
    mov r0, #0x11                   @ interrupted-context canaries
    mov r1, #0x22
    mov r2, #0x33
    mov r3, #0x44
    mov r12, #0x55

park:
    ldr r4, =0x03000010             @ heartbeat: proof the interrupted
    ldr r5, [r4]                    @ loop keeps running after returns
    add r5, r5, #1
    str r5, [r4]
    b   park

    .pool

@ The BIOS-HLE dispatcher enters here in ARM System state with IRQs
@ disabled and r0 = IE & IF. Interrupted r1..r3 and r12 are still live.
irq_handler:
    cmp r1, #0x22                   @ canaries intact?
    bne mismatch
    cmp r2, #0x33
    bne mismatch
    cmp r3, #0x44
    bne mismatch
    cmp r12, #0x55
    bne mismatch
    cmp r0, #0x01                   @ IE&IF: exactly the VBlank bit
    bne mismatch

    ldr r0, =0x03000008             @ contract flags: both checks pass
    ldr r2, [r0]
    orr r2, r2, #0x03
    str r2, [r0]

    ldr r0, =0x03000000             @ entries++
    ldr r1, [r0]
    add r1, r1, #1
    str r1, [r0]
    b   acknowledge

mismatch:
    ldr r0, =0x03000008             @ flag the failure
    ldr r1, [r0]
    orr r1, r1, #0x8000
    str r1, [r0]

acknowledge:
    ldr r0, =0x04000202             @ IF acknowledge: write 1 to clear
    mov r1, #0x01                   @ the VBlank bit
    strh r1, [r0]
    bx  lr                          @ back through the HLE sentinel

    .pool
