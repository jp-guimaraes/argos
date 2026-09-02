; A stand-in for the real `bootmgr`, used only by the boot-chain test.
;
; The VBR (M6.4) must find `bootmgr` in the FAT32 root directory, load the
; *whole file* to linear 0x20000, and jump to 0x2000:0000 with DL = BIOS
; drive. This occupies that file and reports over the serial port whether all
; three actually happened -- so the VBR is testable without shipping a real
; Windows `bootmgr` into the test suite.
;
; The end-marker check is the part that matters most: loading only the first
; sector, or losing the cluster chain partway, would still print the first
; message. Only a complete, correctly-ordered load puts the marker where this
; expects it.

BITS 16
ORG 0x0000                      ; loaded at 0x2000:0000, entered there

COM1      equ 0x3F8
FILE_SIZE equ 40960             ; the test pads this file to exactly this size
MARKER    equ FILE_SIZE - 8     ; where the test writes "ARGOSEND"

start:
    cli
    ; The jump was to 0x2000:0000, so CS is already the load segment; point
    ; DS at it too, so every reference below is file-relative.
    mov ax, cs
    mov ds, ax
    ; ES too: the marker check below is a `cmpsb`, which reads DS:SI against
    ; ES:DI. The VBR leaves ES at 0, so without this the comparison would
    ; read the expected marker from address 0 instead of from this file --
    ; reporting TRUNCATED on a load that was in fact complete.
    mov es, ax
    xor ax, ax
    mov ss, ax
    mov sp, 0x7C00
    sti
    cld

    mov [handoff_drive], dl

    mov si, msg_loaded
    call puts

    ; DL must still be a hard disk number: the VBR is required to preserve
    ; what the BIOS and MBR handed down.
    mov al, [handoff_drive]
    cmp al, 0x80
    jb .bad_drive

    ; The last eight bytes of the file must be the marker, in the right
    ; place -- proof the entire file was loaded, contiguously and in order.
    mov si, MARKER
    mov di, expected_marker
    mov cx, 8
    repe cmpsb
    jne .truncated

    mov si, msg_full
    call puts
    jmp done

.bad_drive:
    mov si, msg_bad_drive
    call puts
    jmp done
.truncated:
    mov si, msg_truncated
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

handoff_drive:   db 0
expected_marker: db "ARGOSEND"

msg_loaded:    db "BOOTMGR_LOADED ", 0
msg_full:      db "FULL_LOAD_OK ", 0
msg_bad_drive: db "BAD_DRIVE ", 0
msg_truncated: db "TRUNCATED ", 0
