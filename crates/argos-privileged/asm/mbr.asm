; Argos MBR boot code (phase 3 M6.3, backlog #45).
;
; The first link in the legacy BIOS boot chain for Windows install media:
; BIOS loads this sector at 0x7C00 and jumps to it; this finds the active
; partition, loads that partition's first sector (the VBR, M6.4) at 0x7C00,
; and hands control to it.
;
; Written from scratch under MIT/Apache rather than adapted from Rufus or
; ms-sys, which are GPL -- see docs/plan-phase3-self-contained.md, M6.1.
;
; Constraints this must respect:
;   * Fits in 440 bytes. Bytes 440..443 are the disk signature and 446..509
;     the partition table, both written by `mbrman`.
;   * Relocates itself before loading anything, because the sector it loads
;     goes to 0x7C00 -- where this code is currently running.
;   * Preserves the handoff convention every VBR expects: DL = BIOS drive
;     number, DS:SI = the active partition's 16-byte table entry.
;
; Deliberately uses INT 13h extensions (LBA) rather than CHS: the partition
; starts at 1 MiB, and CHS addressing cannot describe modern media without a
; fictional geometry. Anything that can boot from USB has the extensions;
; if not, this says so rather than reading the wrong sector.

BITS 16
ORG 0x0600

RELOC_BASE    equ 0x0600      ; where this code moves itself
LOAD_ADDR     equ 0x7C00      ; where BIOS put us, and where the VBR goes
PTABLE        equ RELOC_BASE + 0x1BE
BOOT_FLAG     equ 0x80
BOOT_SIG      equ 0xAA55      ; 0x55,0xAA read as a little-endian word

start:
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, LOAD_ADDR         ; stack grows down from just below our load address
    sti
    cld

    ; Move all 512 bytes to RELOC_BASE, then continue executing there, so
    ; that loading the VBR to 0x7C00 does not overwrite the running code.
    mov cx, 512
    mov si, LOAD_ADDR
    mov di, RELOC_BASE
    rep movsb
    jmp 0:relocated

relocated:
    mov [drive], dl           ; BIOS passes the boot drive in DL

    ; Find the one partition entry marked active.
    mov si, PTABLE
    mov cx, 4
.scan:
    cmp byte [si], BOOT_FLAG
    je .found
    add si, 16
    loop .scan
    mov si, msg_no_active
    jmp fail

.found:
    mov [part_entry], si
    mov eax, [si + 8]         ; entry offset 8: starting LBA (32-bit)
    mov [dap_lba], eax

    ; Confirm INT 13h extensions before relying on them.
    mov ah, 0x41
    mov bx, 0x55AA
    mov dl, [drive]
    int 0x13
    jc .no_extensions
    cmp bx, 0xAA55
    jne .no_extensions

    ; Read one sector (the VBR) to 0x7C00.
    mov ah, 0x42
    mov dl, [drive]
    mov si, dap
    int 0x13
    jc .read_failed

    cmp word [LOAD_ADDR + 510], BOOT_SIG
    jne .not_bootable

    ; Handoff: DL = drive, DS:SI = active partition entry.
    mov dl, [drive]
    mov si, [part_entry]
    jmp 0:LOAD_ADDR

.no_extensions:
    mov si, msg_no_lba
    jmp fail
.read_failed:
    mov si, msg_read
    jmp fail
.not_bootable:
    mov si, msg_no_sig
    jmp fail

; Prints the NUL-terminated string at DS:SI and halts. Printing rather than
; hanging silently is the whole difference between a diagnosable failure and
; a machine that just sits there.
fail:
    lodsb
    test al, al
    jz .halt
    mov ah, 0x0E              ; BIOS teletype output
    mov bx, 0x0007
    int 0x10
    jmp fail
.halt:
    cli
    hlt
    jmp .halt

; INT 13h Disk Address Packet.
align 4
dap:
    db 0x10                   ; packet size
    db 0
    dw 1                      ; sectors to read
    dw LOAD_ADDR              ; buffer offset
    dw 0                      ; buffer segment
dap_lba:
    dd 0                      ; LBA low 32 bits
    dd 0                      ; LBA high 32 bits

drive:      db 0
part_entry: dw 0

msg_no_active: db "Argos: no active partition", 0
msg_no_lba:    db "Argos: BIOS lacks LBA support", 0
msg_read:      db "Argos: read error", 0
msg_no_sig:    db "Argos: partition not bootable", 0

; Must fit before the disk signature at offset 440.
times 440 - ($ - $$) db 0
