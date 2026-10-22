@ Adversarial multiboot test ROM for the safety fallbacks.
@
@ Assembled by scripts/build-test-roms.sh with arm-none-eabi-as/ld into
@ target/test-roms/mb-thumbptr.gba. The mb- prefix links the image at
@ 0x02000000 so canonical addresses, pool literals, and the analysis
@ base agree: pool words that point at THUMB code canonicalize onto the
@ analyzed image, which is exactly the case the word-reference
@ fallback reasons about.
@
@ Intentional wrong-analysis hazards, each exercising one emitter
@ fallback while the round trip must stay byte-identical:
@
@ - `bl 0x02400000` calls far outside the image: the target cannot be
@   canonicalized, so no label can exist (branch-without-label).
@ - `.2byte 0xB400` is `push {}`, UNPREDICTABLE on the ARM7TDMI
@   (GBATEK THUMB.15); the renderer's text would not re-assemble to the
@   same halfword (unpredictable-encoding).
@ - `ldr r1, =thumb_func+1` puts a bit-0 THUMB pointer in the pool that
@   lands on a labeled function: emitted symbolically as `label+1`.
@ - `ldr r2, =thumb_func+3` puts a bit-0 pointer one byte into an
@   instruction: executed THUMB code with no label, so the literal is
@   kept (word-reference-not-provable).
@
@ thumb_mid is reached only as the fallthrough of a taken-both-ways
@ conditional branch, so the analysis marks it executed without giving
@ it a label.

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
    .byte 0xC3
    .endr

    .org 0x0A0
title:
    .ascii "MBPTR"
    .org 0x0AC
game_code:
    .ascii "MB"
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
    bl  0x02400000                  @ call outside the image; the walk
                                    @ continues past a call site
    ldr r0, =thumb_start+1          @ THUMB entry, bit 0 set for BX
    bx  r0

thumb_start:                        @ 0x0D4
    .thumb
    ldr r0, =0x04000000             @ DISPCNT
    ldr r1, =thumb_func+1           @ labeled THUMB pointer
    ldr r2, =thumb_func+3           @ mid-instruction THUMB pointer
    movs r3, #3
    .2byte 0xB400                   @ push {}: UNPREDICTABLE
    movs r0, #1
    bl  thumb_func
    bne loop                        @ walk marks loop and falls through
thumb_mid:                          @ executed, but never labeled
    movs r4, #4
    bx  lr
loop:
    b   loop
thumb_func:
    movs r0, #2
    bx  lr

    .pool

    .org 0x100                      @ keep the image at header size
