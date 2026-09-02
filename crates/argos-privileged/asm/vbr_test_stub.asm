; A stand-in for the real FAT32 VBR (M6.4), used only by the boot-chain test.
;
; The MBR's job is to find the active partition, load its first sector to
; 0x7C00, and jump there with DL = drive and DS:SI = the partition entry.
; This stub occupies that first sector and reports, over the serial port,
; whether all of that actually happened -- so M6.3 can be tested for real
; before any of M6.4 exists.
;
; Serial rather than screen output because QEMU can redirect COM1 to a file,
; making the result something a test can assert on rather than a human
; watching a window.

BITS 16
ORG 0x7C00

COM1 equ 0x3F8

start:
    cli
    xor ax, ax
    mov ds, ax
    mov ss, ax
    mov sp, 0x7C00
    sti
    cld

    ; Keep what the MBR handed us before anything can clobber it.
    mov [handoff_drive], dl
    mov [handoff_si], si

    mov si, msg_reached
    call puts

    ; The BIOS drive number must be a hard disk (0x80 or above); a stale or
    ; zeroed DL would mean the MBR did not preserve it.
    mov al, [handoff_drive]
    cmp al, 0x80
    jb .bad_drive

    ; DS:SI must point at the active partition entry, whose first byte is
    ; the 0x80 active flag and whose type byte (offset 4) is FAT32 LBA.
    mov si, [handoff_si]
    cmp byte [si], 0x80
    jne .bad_entry
    cmp byte [si + 4], 0x0C
    jne .bad_entry

    mov si, msg_handoff_ok
    call puts
    jmp done

.bad_drive:
    mov si, msg_bad_drive
    call puts
    jmp done
.bad_entry:
    mov si, msg_bad_entry
    call puts

done:
    cli
    hlt
    jmp done

; Writes the NUL-terminated string at DS:SI to COM1.
puts:
    lodsb
    test al, al
    jz .ret
    mov dx, COM1
    out dx, al
    jmp puts
.ret:
    ret

handoff_drive: db 0
handoff_si:    dw 0

msg_reached:    db "VBR_REACHED ", 0
msg_handoff_ok: db "HANDOFF_OK ", 0
msg_bad_drive:  db "BAD_DRIVE ", 0
msg_bad_entry:  db "BAD_ENTRY ", 0

times 510 - ($ - $$) db 0
dw 0xAA55
