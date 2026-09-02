; Argos FAT32 volume boot record (phase 3 M6.4, backlog #45).
;
; The second link in the legacy BIOS chain. The MBR (M6.3) loads this sector
; at 0x7C00 and jumps here with DL = BIOS drive. This locates `bootmgr` in
; the FAT32 root directory, loads it to linear 0x20000, and jumps to
; 0x2000:0000 -- the address and entry point Microsoft's own NT60 boot code
; uses, so the same `bootmgr` that ships on the install media takes over from
; there unchanged.
;
; Written from scratch under MIT/Apache rather than adapted from ms-sys or
; Rufus, which are GPL -- see docs/plan-phase3-self-contained.md, M6.1.
;
; Layout constraints, which are what make this tight:
;   * Bytes 0..2 are the jump to the code, 3..89 the BPB that `fatfs` wrote.
;     Only bytes 90..509 are ours -- 420 for everything below.
;   * The BPB must survive untouched: it describes the filesystem, and this
;     code reads its own geometry back out of it rather than assuming any.
;
; Scratch memory (all below where bootmgr lands, all free after the MBR):
;   0x0800  one FAT sector
;   0x0A00  one directory sector

BITS 16
ORG 0x7C00

; Emits one character to COM1, but only in a diagnostic build. Expands to
; nothing otherwise, so the shipped sector pays no bytes for it.
;
; Clobbers AX and DX without saving them -- a diagnostic build must still fit
; a 512-byte sector, and saving costs more than it is worth. Only valid at
; points where both registers are already dead; each use below is placed
; accordingly.
%macro DBG 1
%ifdef SERIAL_DIAG
    mov dx, 0x3F8
    mov al, %1
    out dx, al
%endif
%endmacro

BOOTMGR_SEG   equ 0x2000        ; linear 0x20000
FAT_BUF       equ 0x0800
DIR_BUF       equ 0x0A00
SECTOR_SIZE   equ 512

; --- Sector 0 header: jump, OEM name, and the BPB `fatfs` owns ------------
    jmp short start
    nop
    times 90 - ($ - $$) db 0    ; OEM name + BPB, overwritten on install

; --- Our code begins at offset 90 ----------------------------------------
start:
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7C00
    sti
    cld
    mov [drive], dl

    ; Geometry, read back out of the BPB rather than assumed.
    ;   0x0D byte  sectors per cluster
    ;   0x0E word  reserved sectors
    ;   0x10 byte  number of FATs
    ;   0x1C dword hidden sectors (this partition's start LBA)
    ;   0x24 dword sectors per FAT
    ;   0x2C dword root directory's first cluster
    mov eax, [0x7C00 + 0x1C]
    movzx ebx, word [0x7C00 + 0x0E]
    add eax, ebx                ; eax = first FAT sector (absolute LBA)
    mov [fat_start], eax

    movzx ecx, byte [0x7C00 + 0x10]
    mov ebx, [0x7C00 + 0x24]
.acc_fats:
    add eax, ebx
    loop .acc_fats
    mov [data_start], eax       ; first data sector; cluster 2 begins here

    mov eax, [0x7C00 + 0x2C]    ; root directory cluster
    mov [cur_cluster], eax
    DBG 'G'                     ; geometry computed

; --- Search the root directory's first cluster for BOOTMGR ---------------
; Only the first cluster: following the chain costs bytes this sector does
; not have, and the writer guarantees the entry is there (it verifies after
; copying and refuses the media otherwise -- see windows_fat32.rs).
    mov eax, [cur_cluster]
    call cluster_to_lba
    movzx ecx, byte [0x7C00 + 0x0D]
.dir_sector:
    push ecx
    push eax
    mov bx, DIR_BUF
    call read_sector

    ; 16 directory entries of 32 bytes each per sector.
    mov di, DIR_BUF
    mov cx, SECTOR_SIZE / 32
.dir_entry:
    cmp byte [di], 0
    je .not_found               ; 0x00: no further entries anywhere
    cmp byte [di], 0xE5
    je .next_entry              ; deleted
    ; 0x08 skips volume labels and long-filename fragments alike: an LFN
    ; entry's attribute is 0x0F, which has 0x08 set.
    test byte [di + 11], 0x08
    jnz .next_entry

    push cx
    push di
    mov si, name_bootmgr
    mov cx, 11
    repe cmpsb
    pop di
    pop cx
    je .found
.next_entry:
    add di, 32
    loop .dir_entry

    pop eax
    pop ecx
    inc eax
    loop .dir_sector
.not_found:
    mov si, msg_no_bootmgr
    jmp fail

; --- Load bootmgr, cluster by cluster, to 0x2000:0000 --------------------
.found:
    DBG 'F'                     ; bootmgr's entry found
    ; The two saved values from the directory scan are dead now.
    pop eax
    pop ecx
    ; First cluster: high word at +0x14, low word at +0x1A.
    xor eax, eax
    mov ax, [di + 0x14]         ; cluster number, high word
    shl eax, 16
    mov ax, [di + 0x1A]         ; low word
    mov [cur_cluster], eax
    mov word [load_seg], BOOTMGR_SEG

.load_cluster:
    mov eax, [cur_cluster]
    cmp eax, 0x0FFFFFF8
    jae .done
    call cluster_to_lba
    movzx ecx, byte [0x7C00 + 0x0D]
.load_sector:
    push ecx
    push eax
    mov es, [load_seg]
    xor bx, bx
    call read_sector
    ; Advance the destination by one sector without ever touching an offset:
    ; 512 bytes is exactly 32 paragraphs.
    add word [load_seg], SECTOR_SIZE / 16
    pop eax
    pop ecx
    inc eax
    loop .load_sector

    mov eax, [cur_cluster]
    call next_cluster
    mov [cur_cluster], eax
    jmp .load_cluster

.done:
    DBG 'L'                     ; whole file loaded, about to hand off
    xor ax, ax
    mov es, ax
    mov dl, [drive]
    jmp BOOTMGR_SEG:0x0000

; --- Helpers -------------------------------------------------------------

; eax = cluster -> eax = absolute LBA of its first sector.
cluster_to_lba:
    sub eax, 2
    movzx ebx, byte [0x7C00 + 0x0D]
    mul ebx
    add eax, [data_start]
    ret

; eax = cluster -> eax = next cluster in the chain.
next_cluster:
    push es
    shl eax, 2                  ; each FAT32 entry is 4 bytes
    xor edx, edx
    mov ebx, SECTOR_SIZE
    div ebx                     ; eax = sector within FAT, edx = byte offset
    add eax, [fat_start]
    push dx
    xor cx, cx
    mov es, cx
    mov bx, FAT_BUF
    call read_sector
    pop bx
    mov eax, [FAT_BUF + bx]
    and eax, 0x0FFFFFFF
    pop es
    ret

; Reads one sector: eax = absolute LBA, es:bx = destination.
read_sector:
    pushad
    mov [dap_lba], eax
    mov [dap_off], bx
    mov [dap_seg], es
    mov ah, 0x42
    mov dl, [drive]
    mov si, dap
    int 0x13
    jc .err
    popad
    ret
.err:
    mov si, msg_read
    ; falls through into `fail`

; Prints DS:SI and halts. A named reason beats a machine that just sits there.
;
; Assembling with -DSERIAL_DIAG sends these to COM1 instead of the screen, so
; the QEMU harness can assert on *why* a boot failed rather than only that it
; did. The shipped build uses INT 10h: on a real machine the screen is what a
; person can actually see.
fail:
    lodsb
    test al, al
    jz .halt
%ifdef SERIAL_DIAG
    mov dx, 0x3F8
    out dx, al
%else
    mov ah, 0x0E
    xor bx, bx
    int 0x10
%endif
    jmp fail
.halt:
    cli
    hlt
    jmp .halt

; --- Data ----------------------------------------------------------------
dap:
    db 0x10
    db 0
    dw 1
dap_off:  dw 0
dap_seg:  dw 0
dap_lba:  dd 0
          dd 0

fat_start:   dd 0
data_start:  dd 0
cur_cluster: dd 0
load_seg:    dw 0
drive:       db 0

name_bootmgr:   db "BOOTMGR    "
%ifdef SERIAL_DIAG
; One character each: the diagnostic build must still fit a 512-byte sector,
; and a letter is enough to say which path was taken.
msg_no_bootmgr: db "N", 0
msg_read:       db "R", 0
%else
msg_no_bootmgr: db "NO BOOTMGR", 0
msg_read:       db "DISK ERR", 0
%endif

%ifndef SERIAL_DIAG
times 510 - ($ - $$) db 0
dw 0xAA55
%endif
