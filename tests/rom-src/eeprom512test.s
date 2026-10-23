@ EEPROM 512B micro-ROM: DMA3 bitstream write and read.
@
@ Assembled by scripts/build-test-roms.sh into target/test-roms/eeprom512test.gba
@ (cart image, boots at 0x08000000). The EEPROM_V ID string sits at
@ 0x0C4, word aligned; it does not distinguish the chip size, so the
@ width is settled by the DMA stream lengths (GBATEK: the ROM knows
@ its chip; see runtime::save::Eeprom).
@
@ Protocol (IWRAM scratch at 0x03000000):
@   +0x000 u8  phase: 0 = write then read-capture, 1 = read-capture
@   +0x004 8b  the 64 data bits read back from page 0Ah, MSB first
@   +0x040 73 halfwords: write-request stream buffer
@   +0x100 9  halfwords: read-request stream buffer
@   +0x140 68 halfwords: reply buffer
@
@ Streams per GBATEK "GBA Cart Backup EEPROM": a write request is
@ "10" + addr(6) + data(64) + "0"; a read request is "11" + addr(6) +
@ "0"; the reply is 4 ignored bits, 64 data bits MSB first, then the
@ ready bit. Each halfword carries one bit in bit 0. Transfers go
@ through DMA3 (DMA0-2 cannot reach the cart): requests with a fixed
@ destination, the reply fetch with a fixed source.

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
    .ascii "EP5101"
    .org 0x0AC
game_code:
    .ascii "AEP5"
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
    .ascii "EEPROM_V"
    .byte 0x31, 0x30, 0x30

    .org 0x100
start:
    msr cpsr_c, #0xDF               @ System mode, IRQ+FIQ disabled
    ldr sp, =0x03007F00             @ stack for the call/push sequences

    ldr r2, =0x03000000
    ldrb r3, [r2]
    mov r5, #10              @ page 0x0A
    cmp r3, #0
    beq write_phase
    b   read_page

write_phase:
    @ Program page 0Ah with 123456789ABCDEF0h.
    ldr r6, =0x12345678
    ldr r7, =0x9ABCDEF0
    bl  write_page

read_page:
    @ Read request: 11 + addr(6) + 0, then fetch and pack the reply.
    ldr r0, =0x03000100
    mov r1, #0b11
    mov r2, #2
    bl  emit_bits
    mov r1, r5
    mov r2, #6
    bl  emit_bits
    mov r1, #0
    mov r2, #1
    bl  emit_bits
    @ DMA3: shift the 9-bit request out to 0x0D000000.
    ldr r0, =0x040000D4
    ldr r1, =0x03000100
    str r1, [r0]
    ldr r1, =0x0D000000
    str r1, [r0, #4]
    mov r1, #9
    strh r1, [r0, #8]
    @ CNT_H: enable, dst fixed (bits 0-1 = 10), 16-bit units,
    @ immediate timing (the runtime's CNT_H encoding).
    ldr r1, =0x8002
    strh r1, [r0, #10]
    @ Reply fetch: DMA3 reads 68 halfwords from 0x0D000000 with a
    @ fixed source into the reply buffer.
    ldr r1, =0x0D000000
    str r1, [r0]
    ldr r1, =0x03000140
    str r1, [r0, #4]
    mov r1, #68
    strh r1, [r0, #8]
    @ CNT_H: enable, src fixed (bits 2-3 = 10), 16-bit units,
    @ immediate timing.
    ldr r1, =0x8008
    strh r1, [r0, #10]
    @ Pack reply bits 4..67 (64 data bits, MSB first) into 8 bytes
    @ at 0x03000004.
    ldr r0, =0x03000140
    ldr r5, =0x03000004
    mov r6, #8               @ first data bit byte offset (4 * 2)
    mov r7, #8               @ bytes remaining
byte_loop:
    mov r1, #0
    mov r8, #8               @ bits remaining in this byte
bit_loop:
    mov r1, r1, lsl #1
    ldrh r2, [r0, r6]
    and r2, r2, #1
    orr r1, r1, r2
    add r6, r6, #2
    subs r8, r8, #1
    bne bit_loop
    strb r1, [r5], #1
    subs r7, r7, #1
    bne byte_loop

park:
    b   park

    @ Sends a write request for the page in r5 with the 64-bit data
    @ r6:r7 (r6 high word, r7 low word).
write_page:
    push {r4, r5, r6, r7, lr}
    ldr r0, =0x03000040
    mov r1, #0b10            @ write request prefix
    mov r2, #2
    bl  emit_bits
    mov r1, r5
    mov r2, #6
    bl  emit_bits
    mov r1, r6
    mov r2, #32
    bl  emit_bits
    mov r1, r7
    mov r2, #32
    bl  emit_bits
    mov r1, #0               @ stream terminator
    mov r2, #1
    bl  emit_bits
    @ DMA3: shift the 73-bit stream out to 0x0D000000.
    ldr r0, =0x040000D4
    ldr r1, =0x03000040
    str r1, [r0]
    ldr r1, =0x0D000000
    str r1, [r0, #4]
    mov r1, #73
    strh r1, [r0, #8]
    @ CNT_H: enable, dst fixed (bits 0-1 = 10), 16-bit units,
    @ immediate timing (the runtime's CNT_H encoding).
    ldr r1, =0x8002
    strh r1, [r0, #10]
    pop {r4, r5, r6, r7, lr}
    bx  lr

    @ Emits the low r2 bits of r1 MSB-first as halfwords at [r0],
    @ advancing r0 past the stream. Trashes r0, r1, r2, r3, r12.
emit_bits:
    push {lr}
    mov r3, #1
    sub r12, r2, #1          @ shifts needed; zero for a 1-bit field
mask_shift:
    cmp r12, #0
    beq emit_loop
    mov r3, r3, lsl #1
    sub r12, r12, #1
    b   mask_shift
emit_loop:
    ands r2, r1, r3
    movne r2, #1
    moveq r2, #0
    strh r2, [r0], #2
    movs r3, r3, lsr #1
    bne emit_loop
    pop {lr}
    bx  lr

    .pool
