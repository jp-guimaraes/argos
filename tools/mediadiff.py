#!/usr/bin/env python3
"""mediadiff.py -- dump estrutural e diff de midias de instalacao do Windows.

Le o dispositivo cru (sem montar), desce por todas as camadas -- MBR, GPT,
BPB, FAT, arvore de diretorios -- e emite JSON + texto legivel. O subcomando
`diff` compara dois dumps campo a campo.

Uso:
    sudo ./mediadiff.py dump /dev/diskN --label RUFUS --out rufus.json
    sudo ./mediadiff.py dump /dev/diskM --label ARGOS --out argos.json
    ./mediadiff.py diff rufus.json argos.json
"""

import sys, os, json, struct, zlib, stat, fcntl, argparse

SECTOR = 512

DKIOCGETBLOCKSIZE  = 0x40046418   # macOS
DKIOCGETBLOCKCOUNT = 0x40086419   # macOS
BLKGETSIZE64       = 0x80081272   # Linux

TYPE_GUIDS = {
    "C12A7328-F81F-11D2-BA4B-00A0C93EC93B": "EFI System Partition",
    "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7": "Microsoft Basic Data",
    "E3C9E316-0B5C-4DB8-817D-F92DF00215AE": "Microsoft Reserved",
    "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC": "Windows Recovery",
    "00000000-0000-0000-0000-000000000000": "(vazia)",
}

GPT_ATTR_BITS = {
    0:  "REQUIRED_PARTITION",
    1:  "NO_BLOCK_IO_PROTOCOL",
    2:  "LEGACY_BIOS_BOOTABLE",
    60: "READ_ONLY",
    61: "SHADOW_COPY",
    62: "HIDDEN",
    63: "NO_DRIVE_LETTER",
}

MBR_TYPES = {
    0x00: "vazia", 0x07: "NTFS/exFAT", 0x0B: "FAT32 CHS", 0x0C: "FAT32 LBA",
    0x0E: "FAT16 LBA", 0x17: "NTFS oculta", 0x1B: "FAT32 oculta",
    0x1C: "FAT32 LBA oculta", 0xEE: "GPT protective", 0xEF: "EFI System",
}

CHAVES = [
    "/bootmgr", "/bootmgr.efi", "/autorun.inf", "/setup.exe",
    "/boot/bcd", "/boot/boot.sdi", "/boot/bootfix.bin", "/boot/etfsboot.com",
    "/efi/boot/bootx64.efi", "/efi/boot/bootia32.efi",
    "/efi/microsoft/boot/bcd",
    "/sources/boot.wim", "/sources/install.wim", "/sources/install.esd",
    "/sources/install.swm", "/sources/install2.swm", "/sources/install3.swm",
    "/sources/setup.exe", "/sources/lang.ini",
]


# --------------------------------------------------------------------------
# acesso ao dispositivo
# --------------------------------------------------------------------------

class Dev:
    def __init__(self, path):
        self.path = path
        self.fd = os.open(path, os.O_RDONLY)
        self.bs, self.size = self._geometry()

    def _geometry(self):
        st = os.fstat(self.fd)
        if stat.S_ISREG(st.st_mode):
            return 512, st.st_size
        for req, fmt, mult in ((DKIOCGETBLOCKCOUNT, "Q", None),):
            try:
                bs = struct.unpack("I", fcntl.ioctl(
                    self.fd, DKIOCGETBLOCKSIZE, struct.pack("I", 0)))[0]
                cnt = struct.unpack("Q", fcntl.ioctl(
                    self.fd, DKIOCGETBLOCKCOUNT, struct.pack("Q", 0)))[0]
                return bs, bs * cnt
            except OSError:
                break
        try:
            n = struct.unpack("Q", fcntl.ioctl(
                self.fd, BLKGETSIZE64, struct.pack("Q", 0)))[0]
            return 512, n
        except OSError:
            return 512, 0

    def read(self, off, n):
        """Leitura alinhada ao bloco fisico -- obrigatorio em /dev/rdiskN."""
        if n <= 0:
            return b""
        a = (off // self.bs) * self.bs
        end = ((off + n + self.bs - 1) // self.bs) * self.bs
        os.lseek(self.fd, a, os.SEEK_SET)
        buf = bytearray()
        want = end - a
        while len(buf) < want:
            chunk = os.read(self.fd, min(1 << 20, want - len(buf)))
            if not chunk:
                break
            buf += chunk
        return bytes(buf[off - a: off - a + n])

    def close(self):
        os.close(self.fd)


def u16(b, o): return int.from_bytes(b[o:o + 2], "little")
def u32(b, o): return int.from_bytes(b[o:o + 4], "little")
def u64(b, o): return int.from_bytes(b[o:o + 8], "little")


def guid(b):
    return "%08X-%04X-%04X-%s-%s" % (
        u32(b, 0), u16(b, 4), u16(b, 6),
        b[8:10].hex().upper(), b[10:16].hex().upper())


def nonzero_len(b):
    i = len(b)
    while i > 0 and b[i - 1] == 0:
        i -= 1
    return i


def sha1(b):
    import hashlib
    return hashlib.sha1(b).hexdigest()[:16]


def human(n):
    if n is None:
        return "?"
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if abs(n) < 1024 or unit == "TiB":
            return f"{n:.1f} {unit}" if unit != "B" else f"{n} B"
        n /= 1024.0


# --------------------------------------------------------------------------
# camada 0: MBR
# --------------------------------------------------------------------------

def parse_mbr(dev, checks):
    s0 = dev.read(0, 512)
    out = {
        "assinatura": f"{s0[510]:02X}{s0[511]:02X}",
        "codigo_boot_bytes": nonzero_len(s0[:440]),
        "codigo_boot_sha1": sha1(s0[:440]),
        "codigo_boot_head": s0[:16].hex(),
        "disk_signature": s0[0x1B8:0x1BC].hex(),
        "particoes": [],
    }
    if out["assinatura"] != "55AA":
        checks.append(("ERRO", "setor 0 sem assinatura 0x55AA"))
    if out["disk_signature"] == "00000000":
        checks.append(("AVISO", "disk signature (0x1B8) zerada -- o Windows usa "
                                "esse campo para identificar o disco"))
    for i in range(4):
        e = s0[0x1BE + i * 16: 0x1BE + (i + 1) * 16]
        if not any(e):
            continue
        p = {
            "indice": i + 1,
            "flag_boot": f"0x{e[0]:02X}",
            "ativa": e[0] == 0x80,
            "tipo": f"0x{e[4]:02X}",
            "tipo_nome": MBR_TYPES.get(e[4], "?"),
            "chs_inicio": {"c": ((e[2] & 0xC0) << 2) | e[3], "h": e[1], "s": e[2] & 0x3F},
            "chs_fim":    {"c": ((e[6] & 0xC0) << 2) | e[7], "h": e[5], "s": e[6] & 0x3F},
            "lba_inicio": u32(e, 8),
            "setores": u32(e, 12),
        }
        p["bytes"] = p["setores"] * 512
        out["particoes"].append(p)
        if p["chs_inicio"]["s"] == 0 and p["tipo"] != "0xEE":
            checks.append(("AVISO", f"particao MBR {i+1}: CHS de inicio zerado"))
        fim = p["lba_inicio"] + p["setores"]
        # 0xEE com contagem saturada em 0xFFFFFFFF e o jeito padrao de
        # escrever o MBR protetivo de um disco maior que 2 TiB -- e o que
        # o Rufus faz. Nao e defeito.
        protetivo_saturado = e[4] == 0xEE and p["setores"] == 0xFFFFFFFF
        if dev.size and fim * 512 > dev.size and not protetivo_saturado:
            checks.append(("ERRO", f"particao MBR {i+1} termina em LBA {fim}, "
                                   f"alem do fim do disco ({dev.size // 512})"))
    ativas = [p for p in out["particoes"] if p["ativa"]]
    out["num_ativas"] = len(ativas)
    return out


# --------------------------------------------------------------------------
# camada 1: GPT
# --------------------------------------------------------------------------

def parse_gpt_header(raw, dev, checks, rotulo):
    if raw[:8] != b"EFI PART":
        return None
    hdr_size = u32(raw, 12)
    stored_crc = u32(raw, 16)
    calc = zlib.crc32(raw[:16] + b"\0\0\0\0" + raw[20:hdr_size]) & 0xFFFFFFFF
    h = {
        "revisao": f"0x{u32(raw, 8):08X}",
        "tamanho_cabecalho": hdr_size,
        "crc_cabecalho": f"{stored_crc:08X}",
        "crc_cabecalho_ok": stored_crc == calc,
        "my_lba": u64(raw, 24),
        "alternate_lba": u64(raw, 32),
        "primeiro_usavel": u64(raw, 40),
        "ultimo_usavel": u64(raw, 48),
        "disk_guid": guid(raw[56:72]),
        "lba_entradas": u64(raw, 72),
        "num_entradas": u32(raw, 80),
        "tamanho_entrada": u32(raw, 84),
        "crc_entradas": f"{u32(raw, 88):08X}",
    }
    if not h["crc_cabecalho_ok"]:
        checks.append(("ERRO", f"GPT {rotulo}: CRC do cabecalho invalido "
                               f"(gravado {stored_crc:08X}, calculado {calc:08X})"))
    tabela = dev.read(h["lba_entradas"] * 512,
                      h["num_entradas"] * h["tamanho_entrada"])
    calc_e = zlib.crc32(tabela) & 0xFFFFFFFF
    h["crc_entradas_ok"] = calc_e == u32(raw, 88)
    if not h["crc_entradas_ok"]:
        checks.append(("ERRO", f"GPT {rotulo}: CRC da tabela de entradas invalido"))
    return h, tabela


def parse_gpt(dev, checks):
    raw = dev.read(512, 512)
    if raw[:8] != b"EFI PART":
        return None
    primario = parse_gpt_header(raw, dev, checks, "primario")
    if primario is None:
        return None
    h, tabela = primario
    out = {"primario": h, "particoes": []}

    ultimo_lba = (dev.size // 512 - 1) if dev.size else None
    out["ultimo_lba_do_disco"] = ultimo_lba
    if ultimo_lba is not None:
        if h["alternate_lba"] != ultimo_lba:
            checks.append(("ERRO",
                f"GPT: alternate_lba = {h['alternate_lba']} mas o ultimo LBA do "
                f"disco e {ultimo_lba} -- o GPT de backup nao esta no fim do disco. "
                f"O Windows valida isso."))
        bkp_raw = dev.read(ultimo_lba * 512, 512)
        if bkp_raw[:8] != b"EFI PART":
            checks.append(("ERRO", "GPT: nao ha cabecalho de backup no ultimo "
                                   "setor do disco"))
            out["backup"] = None
        else:
            bkp = parse_gpt_header(bkp_raw, dev, checks, "backup")
            out["backup"] = bkp[0] if bkp else None
            if out["backup"] and out["backup"]["disk_guid"] != h["disk_guid"]:
                checks.append(("ERRO", "GPT: disk GUID do backup difere do primario"))

    for i in range(h["num_entradas"]):
        e = tabela[i * h["tamanho_entrada"]: (i + 1) * h["tamanho_entrada"]]
        if not any(e[:16]):
            continue
        tg = guid(e[:16])
        attrs = u64(e, 48)
        nomes = [n for b, n in GPT_ATTR_BITS.items() if attrs & (1 << b)]
        p = {
            "indice": i + 1,
            "tipo_guid": tg,
            "tipo_nome": TYPE_GUIDS.get(tg, "(desconhecido)"),
            "guid_unico": guid(e[16:32]),
            "lba_inicio": u64(e, 32),
            "lba_fim": u64(e, 40),
            "setores": u64(e, 40) - u64(e, 32) + 1,
            "attrs": f"0x{attrs:016X}",
            "attrs_nomes": nomes,
            "nome": e[56:128].decode("utf-16-le", "replace").rstrip("\0"),
        }
        p["bytes"] = p["setores"] * 512
        out["particoes"].append(p)
        if attrs & (1 << 63):
            checks.append(("ERRO", f"GPT particao {i+1}: bit 63 NO_DRIVE_LETTER "
                                   f"ligado -- o Windows nao atribui letra"))
        if attrs & (1 << 62):
            checks.append(("AVISO", f"GPT particao {i+1}: bit 62 HIDDEN ligado"))
        if attrs & (1 << 0):
            checks.append(("AVISO", f"GPT particao {i+1}: bit 0 REQUIRED_PARTITION "
                                    f"ligado"))
        if ultimo_lba is not None and p["lba_fim"] > ultimo_lba:
            checks.append(("ERRO", f"GPT particao {i+1} termina alem do disco"))
    return out


# --------------------------------------------------------------------------
# camada 2: sistema de arquivos
# --------------------------------------------------------------------------

def detect_fs(vbr):
    if vbr[0x52:0x5A] == b"FAT32   ":
        return "fat32"
    if vbr[3:11] == b"NTFS    ":
        return "ntfs"
    if vbr[3:11] == b"EXFAT   ":
        return "exfat"
    if vbr[0x36:0x3B] in (b"FAT16", b"FAT12", b"FAT  "):
        return "fat16"
    return "desconhecido"


def parse_ntfs(dev, part_off, checks):
    vbr = dev.read(part_off, 512)
    bps = u16(vbr, 0x0B)
    spc = vbr[0x0D]
    return {
        "tipo": "ntfs",
        "oem": vbr[3:11].decode("latin1"),
        "bytes_por_setor": bps,
        "setores_por_cluster": spc,
        "bytes_por_cluster": bps * spc,
        "setores_reservados": u16(vbr, 0x0E),
        "media_descriptor": f"0x{vbr[0x15]:02X}",
        "setores_por_trilha": u16(vbr, 0x18),
        "cabecas": u16(vbr, 0x1A),
        "hidden_sectors": u32(vbr, 0x1C),
        "total_setores": u64(vbr, 0x28),
        "mft_cluster": u64(vbr, 0x30),
        "mft_mirror_cluster": u64(vbr, 0x38),
        "drive_num": f"0x{vbr[0x24]:02X}",
        "volume_serial": vbr[0x48:0x50].hex(),
        "codigo_boot_bytes": nonzero_len(vbr[0x54:510]),
        "codigo_boot_sha1": sha1(vbr[0x54:510]),
        "assinatura": f"{vbr[510]:02X}{vbr[511]:02X}",
    }


class Fat32:
    def __init__(self, dev, part_lba, part_sectors, checks):
        self.dev = dev
        self.checks = checks
        self.part_off = part_lba * 512
        self.part_lba = part_lba
        self.part_sectors = part_sectors
        v = dev.read(self.part_off, 512)
        self.vbr = v
        self.bps = u16(v, 0x0B)
        self.spc = v[0x0D]
        self.rsvd = u16(v, 0x0E)
        self.nfats = v[0x10]
        self.tot16 = u16(v, 0x13)
        self.fatsz16 = u16(v, 0x16)
        self.tot32 = u32(v, 0x20)
        self.fatsz32 = u32(v, 0x24)
        self.extflags = u16(v, 0x28)
        self.rootclus = u32(v, 0x2C)
        self.fsinfo_sec = u16(v, 0x30)
        self.bkboot_sec = u16(v, 0x32)
        self.total = self.tot32 or self.tot16
        self.fatsz = self.fatsz32 or self.fatsz16
        self.data_start = self.rsvd + self.nfats * self.fatsz
        self.clusters = max(0, (self.total - self.data_start) // self.spc) if self.spc else 0
        self._fat_cache = {}

    def info(self):
        v = self.vbr
        d = {
            "tipo": "fat32",
            "jump": v[:3].hex(),
            "oem": v[3:11].decode("latin1"),
            "bytes_por_setor": self.bps,
            "setores_por_cluster": self.spc,
            "bytes_por_cluster": self.bps * self.spc,
            "setores_reservados": self.rsvd,
            "num_fats": self.nfats,
            "root_entries_16": u16(v, 0x11),
            "total_setores_16": self.tot16,
            "media_descriptor": f"0x{v[0x15]:02X}",
            "setores_por_fat_16": self.fatsz16,
            "setores_por_trilha": u16(v, 0x18),
            "cabecas": u16(v, 0x1A),
            "hidden_sectors": u32(v, 0x1C),
            "total_setores_32": self.tot32,
            "setores_por_fat_32": self.fatsz32,
            "ext_flags": f"0x{self.extflags:04X}",
            "versao_fs": f"0x{u16(v, 0x2A):04X}",
            "root_cluster": self.rootclus,
            "setor_fsinfo": self.fsinfo_sec,
            "setor_backup_boot": self.bkboot_sec,
            "drive_num": f"0x{v[0x40]:02X}",
            "reservado_0x41": f"0x{v[0x41]:02X}",
            "ext_boot_sig": f"0x{v[0x42]:02X}",
            "volume_id": v[0x43:0x47].hex(),
            "label_bpb": v[0x47:0x52].decode("latin1"),
            "fs_type": v[0x52:0x5A].decode("latin1"),
            "codigo_vbr_bytes": nonzero_len(v[0x5A:510]),
            "codigo_vbr_sha1": sha1(v[0x5A:510]),
            "codigo_vbr_head": v[0x5A:0x5A + 16].hex(),
            "assinatura": f"{v[510]:02X}{v[511]:02X}",
            "inicio_dados_setor": self.data_start,
            "num_clusters": self.clusters,
        }
        c = self.checks
        if d["assinatura"] != "55AA":
            c.append(("ERRO", "VBR sem assinatura 0x55AA"))
        if self.clusters < 65525:
            c.append(("ERRO", f"volume tem {self.clusters} clusters -- abaixo do "
                              f"minimo 65525 do FAT32"))
        if self.part_sectors and self.total > self.part_sectors:
            c.append(("ERRO", f"BPB declara {self.total} setores mas a particao "
                              f"tem {self.part_sectors} -- o volume nao cabe na "
                              f"particao. O Windows recusa volumes assim."))
        if self.part_sectors and self.total < self.part_sectors:
            folga = self.part_sectors - self.total
            sev = "AVISO" if folga <= self.spc else "INFO"
            c.append((sev, f"BPB declara {self.total} setores, particao tem "
                           f"{self.part_sectors} (sobram {folga})"))
        if u32(v, 0x1C) != self.part_lba:
            c.append(("AVISO", f"hidden_sectors = {u32(v, 0x1C)} mas a particao "
                               f"comeca no LBA {self.part_lba}"))
        if v[0x40] != 0x80:
            c.append(("AVISO", f"BS_DrvNum = 0x{v[0x40]:02X}; midia de boot "
                               f"costuma usar 0x80"))
        if v[0x41] != 0x00:
            c.append(("AVISO", f"byte reservado 0x41 = 0x{v[0x41]:02X} -- o "
                               f"chkdsk do Windows usa esse byte como flag de "
                               f"volume sujo"))
        if v[0x15] != 0xF8:
            c.append(("AVISO", f"media descriptor 0x{v[0x15]:02X}, esperado 0xF8"))
        if u16(v, 0x11) != 0:
            c.append(("ERRO", "root_entries deve ser 0 em FAT32"))

        # backup do boot sector
        if self.bkboot_sec:
            bkp = self.dev.read(self.part_off + self.bkboot_sec * 512, 512)
            d["backup_boot_igual"] = (bkp == v)
            if bkp != v:
                c.append(("ERRO", f"backup do boot sector (setor {self.bkboot_sec}) "
                                  f"difere do primario"))
        # FSInfo
        if self.fsinfo_sec:
            fsi = self.dev.read(self.part_off + self.fsinfo_sec * 512, 512)
            d["fsinfo"] = {
                "sig1": f"{u32(fsi, 0):08X}", "sig2": f"{u32(fsi, 484):08X}",
                "sig3": f"{u32(fsi, 508):08X}",
                "clusters_livres": f"0x{u32(fsi, 488):08X}",
                "proximo_livre": f"0x{u32(fsi, 492):08X}",
            }
            if u32(fsi, 0) != 0x41615252 or u32(fsi, 484) != 0x61417272 \
               or u32(fsi, 508) != 0xAA550000:
                c.append(("ERRO", "assinaturas do FSInfo invalidas"))

        # FAT[0] e FAT[1]
        f0, f1 = self.fat_entry(0), self.fat_entry(1)
        d["fat0"] = f"0x{f0:08X}"
        d["fat1"] = f"0x{f1:08X}"
        if (f0 & 0x0FFFFFFF) != (0x0FFFFF00 | v[0x15]):
            c.append(("AVISO", f"FAT[0] = 0x{f0:08X}, esperado 0x0FFFFF{v[0x15]:02X}"))
        if not (f1 & 0x08000000):
            c.append(("ERRO", "FAT[1] com o bit ClnShutBit (0x08000000) limpo -- "
                              "o volume esta marcado como nao desmontado "
                              "limpamente; o Windows trata como sujo"))
        if not (f1 & 0x04000000):
            c.append(("ERRO", "FAT[1] com o bit HrdErrBit (0x04000000) limpo -- "
                              "volume marcado com erro de I/O"))
        d["fat1_clean_shutdown"] = bool(f1 & 0x08000000)
        d["fat1_sem_erro_io"] = bool(f1 & 0x04000000)

        # as copias da FAT batem?
        if self.nfats > 1:
            n = min(self.fatsz * 512, 1 << 20)
            a = self.dev.read(self.part_off + self.rsvd * 512, n)
            b = self.dev.read(self.part_off + (self.rsvd + self.fatsz) * 512, n)
            d["fats_iguais_primeiro_mib"] = (a == b)
            if a != b:
                c.append(("AVISO", "as duas copias da FAT diferem no primeiro MiB"))
        return d

    # --- FAT ---------------------------------------------------------------
    def fat_entry(self, n):
        pagina = n // 32768
        if pagina not in self._fat_cache:
            off = self.part_off + self.rsvd * 512 + pagina * 32768 * 4
            self._fat_cache[pagina] = self.dev.read(off, 32768 * 4)
        buf = self._fat_cache[pagina]
        i = (n % 32768) * 4
        return u32(buf, i) if i + 4 <= len(buf) else 0

    def chain(self, start, limite=1 << 22):
        c = start
        vistos = 0
        while 2 <= c < 0x0FFFFFF8 and vistos < limite:
            yield c
            vistos += 1
            c = self.fat_entry(c) & 0x0FFFFFFF

    def cluster_off(self, n):
        return self.part_off + (self.data_start + (n - 2) * self.spc) * 512

    def read_chain(self, start, max_bytes=None):
        partes, total = [], 0
        for c in self.chain(start):
            partes.append(self.dev.read(self.cluster_off(c), self.spc * 512))
            total += self.spc * 512
            if max_bytes and total >= max_bytes:
                break
        return b"".join(partes)

    def chain_len(self, start):
        return sum(1 for _ in self.chain(start))


def sfn_checksum(name11):
    s = 0
    for ch in name11:
        s = (((s & 1) << 7) + (s >> 1) + ch) & 0xFF
    return s


def parse_dir(data):
    """Interpreta um diretorio cru em entradas, preservando a ordem no disco."""
    saida, lfn, lfn_chk = [], [], None
    for i in range(0, len(data) - 31, 32):
        e = data[i:i + 32]
        if e[0] == 0x00:
            break
        if e[0] == 0xE5:
            lfn, lfn_chk = [], None
            continue
        attr = e[11]
        if attr & 0x0F == 0x0F:
            seq = e[0] & 0x1F
            nome = (e[1:11] + e[14:26] + e[28:32]).decode("utf-16-le", "replace")
            nome = nome.split("￿")[0].split("\0")[0]
            lfn.append((seq, nome))
            lfn_chk = e[13]
            continue
        short = e[0:11].decode("latin1")
        nome_curto = short[:8].rstrip() + ("." + short[8:].rstrip()
                                           if short[8:].strip() else "")
        longo = "".join(n for _, n in sorted(lfn, key=lambda t: t[0])) if lfn else ""
        chk_ok = (lfn_chk == sfn_checksum(e[0:11])) if lfn else None
        saida.append({
            "nome": longo or nome_curto,
            "nome_curto": nome_curto,
            "raw_8_3": short,
            "attr": f"0x{attr:02X}",
            "dir": bool(attr & 0x10),
            "volume": bool(attr & 0x08) and not (attr & 0x10),
            "oculto": bool(attr & 0x02),
            "sistema": bool(attr & 0x04),
            "somente_leitura": bool(attr & 0x01),
            "cluster": (u16(e, 20) << 16) | u16(e, 26),
            "tamanho": u32(e, 28),
            "lfn_checksum_ok": chk_ok,
            "offset_no_dir": i,
        })
        lfn, lfn_chk = [], None
    return saida


def walk(fs, checks, max_entradas=20000):
    """Percorre a arvore inteira e devolve (lista de arquivos, raiz, estatisticas)."""
    arquivos = []
    raiz_entradas = None
    fila = [(fs.rootclus, "", 0)]
    dirs_vistos = set()
    problemas_dot = []
    while fila:
        cluster, caminho, prof = fila.pop(0)
        if cluster in dirs_vistos or prof > 12 or len(arquivos) > max_entradas:
            continue
        dirs_vistos.add(cluster)
        entradas = parse_dir(fs.read_chain(cluster))
        if caminho == "":
            raiz_entradas = entradas
        for ent in entradas:
            nome = ent["nome"]
            if nome in (".", ".."):
                if caminho == "":
                    problemas_dot.append(f"'{nome}' na raiz")
                continue
            p = f"{caminho}/{nome}".lower()
            if ent["volume"]:
                continue
            if ent["dir"]:
                sub = parse_dir(fs.read_chain(ent["cluster"])) if ent["cluster"] else []
                dot = [x for x in sub[:2]]
                if len(dot) < 2 or dot[0]["nome_curto"] != "." or dot[1]["nome_curto"] != "..":
                    problemas_dot.append(f"{p}: primeiras entradas nao sao '.' e '..'")
                else:
                    esperado_pai = 0 if caminho == "" else None
                    if esperado_pai == 0 and dot[1]["cluster"] != 0:
                        problemas_dot.append(
                            f"{p}: '..' aponta para cluster {dot[1]['cluster']}, "
                            f"deveria ser 0 (pai e a raiz)")
                fila.append((ent["cluster"], f"{caminho}/{nome}", prof + 1))
                arquivos.append({"caminho": p, "dir": True, "tamanho": 0,
                                 "cluster": ent["cluster"], "attr": ent["attr"]})
            else:
                arquivos.append({"caminho": p, "dir": False,
                                 "tamanho": ent["tamanho"],
                                 "cluster": ent["cluster"], "attr": ent["attr"]})
                if ent["lfn_checksum_ok"] is False:
                    checks.append(("ERRO", f"{p}: checksum do nome longo nao bate "
                                           f"com o nome 8.3 -- o Windows ignora "
                                           f"entradas assim"))
    for p in problemas_dot[:10]:
        checks.append(("ERRO", f"entradas de diretorio: {p}"))
    return arquivos, raiz_entradas


def checar_cadeias(fs, arquivos, checks, amostra=40):
    """Confere que a cadeia de clusters de cada arquivo cobre o tamanho declarado."""
    bpc = fs.bps * fs.spc
    alvos = [a for a in arquivos if not a["dir"] and a["tamanho"] > 0]
    alvos.sort(key=lambda a: -a["tamanho"])
    conferidos = alvos[:amostra]
    ruins = []
    for a in conferidos:
        precisa = (a["tamanho"] + bpc - 1) // bpc
        tem = fs.chain_len(a["cluster"])
        if tem != precisa:
            ruins.append((a["caminho"], a["tamanho"], precisa, tem))
    for c, t, p, g in ruins[:10]:
        checks.append(("ERRO", f"{c}: {t} bytes precisam de {p} clusters, "
                               f"a cadeia tem {g}"))
    return {"conferidos": len(conferidos), "inconsistentes": len(ruins)}


# --------------------------------------------------------------------------
# dump
# --------------------------------------------------------------------------

def dump(path, label):
    dev = Dev(path)
    checks = []
    d = {
        "rotulo": label,
        "dispositivo": path,
        "tamanho_bytes": dev.size,
        "tamanho_legivel": human(dev.size),
        "bloco_fisico": dev.bs,
    }
    d["mbr"] = parse_mbr(dev, checks)
    d["gpt"] = parse_gpt(dev, checks)
    d["esquema"] = "GPT" if d["gpt"] else "MBR"

    if d["gpt"]:
        pts = d["gpt"]["particoes"]
        prot = [p for p in d["mbr"]["particoes"] if p["tipo"] == "0xEE"]
        if not prot:
            checks.append(("AVISO", "GPT sem MBR protetivo tipo 0xEE no setor 0"))
        alvo = pts[0] if pts else None
        part_lba = alvo["lba_inicio"] if alvo else None
        part_sec = alvo["setores"] if alvo else None
    else:
        cands = [p for p in d["mbr"]["particoes"] if p["tipo"] != "0xEE"]
        alvo = cands[0] if cands else None
        part_lba = alvo["lba_inicio"] if alvo else None
        part_sec = alvo["setores"] if alvo else None
        if alvo and not alvo["ativa"]:
            checks.append(("ERRO", "nenhuma particao marcada como ativa (0x80) -- "
                                   "o MBR generico nao acha o que bootar"))

    d["particao_analisada"] = {"lba_inicio": part_lba, "setores": part_sec}

    if part_lba is None:
        d["volume"] = None
        d["checks"] = checks
        dev.close()
        return d

    vbr = dev.read(part_lba * 512, 512)
    fstype = detect_fs(vbr)
    d["fs_detectado"] = fstype

    if fstype == "fat32":
        fs = Fat32(dev, part_lba, part_sec, checks)
        d["volume"] = fs.info()
        arquivos, raiz = walk(fs, checks)
        d["arquivos"] = sorted(arquivos, key=lambda a: a["caminho"])
        d["raiz_ordem"] = [
            {"nome": e["nome"], "curto": e["nome_curto"], "attr": e["attr"],
             "dir": e["dir"], "volume": e["volume"], "tamanho": e["tamanho"]}
            for e in (raiz or [])
        ]
        rotulo_vol = [e["raw_8_3"] for e in (raiz or []) if e["volume"]]
        d["label_no_diretorio"] = rotulo_vol[0] if rotulo_vol else None
        if not rotulo_vol:
            checks.append(("AVISO", "raiz sem entrada de rotulo de volume "
                                    "(ATTR_VOLUME_ID)"))
        d["cadeias"] = checar_cadeias(fs, arquivos, checks)
        d["num_arquivos"] = sum(1 for a in arquivos if not a["dir"])
        d["num_dirs"] = sum(1 for a in arquivos if a["dir"])
        d["bytes_arquivos"] = sum(a["tamanho"] for a in arquivos if not a["dir"])
        mapa = {a["caminho"]: a for a in arquivos}
        d["chaves"] = {k: (mapa[k]["tamanho"] if k in mapa else None) for k in CHAVES}
        grandes = [a for a in arquivos if not a["dir"] and a["tamanho"] >= (1 << 32)]
        for g in grandes:
            checks.append(("ERRO", f"{g['caminho']}: {g['tamanho']} bytes, "
                                   f"impossivel em FAT32"))
    elif fstype == "ntfs":
        d["volume"] = parse_ntfs(dev, part_lba * 512, checks)
        d["arquivos"] = []
        d["chaves"] = {}
        checks.append(("INFO", "volume NTFS: a arvore de arquivos nao e percorrida "
                               "por esta ferramenta; compare as camadas de "
                               "particionamento e boot"))
    else:
        d["volume"] = {"tipo": fstype, "oem": vbr[3:11].decode("latin1", "replace"),
                       "assinatura": f"{vbr[510]:02X}{vbr[511]:02X}"}
        d["arquivos"] = []
        d["chaves"] = {}

    d["checks"] = checks
    dev.close()
    return d


# --------------------------------------------------------------------------
# saida legivel
# --------------------------------------------------------------------------

def imprime(d, listar=0):
    L = print
    L(f"{'=' * 74}")
    L(f"  {d['rotulo']}   {d['dispositivo']}   {d['tamanho_legivel']}   "
      f"esquema {d['esquema']}")
    L(f"{'=' * 74}")

    m = d["mbr"]
    L(f"\n--- SETOR 0 ---")
    L(f"  assinatura       {m['assinatura']}")
    L(f"  codigo de boot   {m['codigo_boot_bytes']} bytes  sha1:{m['codigo_boot_sha1']}")
    L(f"                   {m['codigo_boot_head']}")
    L(f"  disk signature   {m['disk_signature']}")
    for p in m["particoes"]:
        L(f"  part {p['indice']}  tipo {p['tipo']} ({p['tipo_nome']})  "
          f"ativa={p['ativa']}  lba={p['lba_inicio']}  setores={p['setores']} "
          f"({human(p['bytes'])})")
        L(f"          CHS ini c={p['chs_inicio']['c']} h={p['chs_inicio']['h']} "
          f"s={p['chs_inicio']['s']}   fim c={p['chs_fim']['c']} "
          f"h={p['chs_fim']['h']} s={p['chs_fim']['s']}")

    if d["gpt"]:
        g = d["gpt"]
        h = g["primario"]
        L(f"\n--- GPT ---")
        L(f"  crc cabecalho ok {h['crc_cabecalho_ok']}   crc entradas ok "
          f"{h['crc_entradas_ok']}")
        L(f"  my_lba {h['my_lba']}   alternate_lba {h['alternate_lba']}   "
          f"ultimo LBA do disco {g.get('ultimo_lba_do_disco')}")
        L(f"  usavel {h['primeiro_usavel']}..{h['ultimo_usavel']}   "
          f"entradas {h['num_entradas']}x{h['tamanho_entrada']}B")
        L(f"  disk GUID {h['disk_guid']}")
        L(f"  backup presente: {g.get('backup') is not None}")
        for p in g["particoes"]:
            L(f"  part {p['indice']}  {p['tipo_nome']}  lba {p['lba_inicio']}.."
              f"{p['lba_fim']} ({human(p['bytes'])})")
            L(f"          attrs {p['attrs']} {p['attrs_nomes'] or '(nenhum)'}   "
              f"nome {p['nome']!r}")
            L(f"          type GUID {p['tipo_guid']}")

    v = d.get("volume")
    if v and v.get("tipo") == "fat32":
        L(f"\n--- VOLUME FAT32 (LBA {d['particao_analisada']['lba_inicio']}) ---")
        L(f"  jump {v['jump']}  OEM {v['oem']!r}  fs_type {v['fs_type']!r}")
        L(f"  bytes/setor {v['bytes_por_setor']}  setores/cluster "
          f"{v['setores_por_cluster']}  -> cluster {human(v['bytes_por_cluster'])}")
        L(f"  reservados {v['setores_reservados']}  FATs {v['num_fats']}  "
          f"setores/FAT {v['setores_por_fat_32']}  inicio dados setor "
          f"{v['inicio_dados_setor']}")
        L(f"  total setores {v['total_setores_32']}  clusters {v['num_clusters']}  "
          f"root cluster {v['root_cluster']}")
        L(f"  hidden_sectors {v['hidden_sectors']}  media {v['media_descriptor']}  "
          f"CHS {v['setores_por_trilha']}spt/{v['cabecas']}h")
        L(f"  ext_flags {v['ext_flags']}  versao_fs {v['versao_fs']}  "
          f"fsinfo setor {v['setor_fsinfo']}  backup setor "
          f"{v['setor_backup_boot']}")
        L(f"  drive_num {v['drive_num']}  reservado41 {v['reservado_0x41']}  "
          f"ext_boot_sig {v['ext_boot_sig']}")
        L(f"  volume id {v['volume_id']}  label BPB {v['label_bpb']!r}  "
          f"label no diretorio {d.get('label_no_diretorio')!r}")
        L(f"  FAT[0] {v['fat0']}  FAT[1] {v['fat1']}  clean_shutdown "
          f"{v['fat1_clean_shutdown']}  sem_erro_io {v['fat1_sem_erro_io']}")
        L(f"  backup boot igual: {v.get('backup_boot_igual')}   "
          f"FATs iguais (1o MiB): {v.get('fats_iguais_primeiro_mib')}")
        if "fsinfo" in v:
            fi = v["fsinfo"]
            L(f"  FSInfo {fi['sig1']}/{fi['sig2']}/{fi['sig3']}  livres "
              f"{fi['clusters_livres']}  proximo {fi['proximo_livre']}")
        L(f"  codigo VBR {v['codigo_vbr_bytes']} bytes  sha1:{v['codigo_vbr_sha1']}")
        L(f"             {v['codigo_vbr_head']}")

        L(f"\n--- CONTEUDO ---")
        L(f"  {d['num_arquivos']} arquivos, {d['num_dirs']} diretorios, "
          f"{human(d['bytes_arquivos'])}")
        L(f"  cadeias de cluster conferidas: {d['cadeias']['conferidos']}, "
          f"inconsistentes: {d['cadeias']['inconsistentes']}")
        L(f"  ordem do diretorio raiz:")
        for e in d["raiz_ordem"][:25]:
            tag = "DIR " if e["dir"] else ("VOL " if e["volume"] else "    ")
            L(f"    {tag}{e['attr']}  {e['nome']:<28} {e['tamanho']}")
        L(f"  arquivos-chave:")
        for k, sz in d["chaves"].items():
            if sz is not None:
                L(f"    {k:<34} {human(sz)}")
        faltando = [k for k, s in d["chaves"].items() if s is None]
        if faltando:
            L(f"    ausentes: {', '.join(faltando)}")
    elif v and v.get("tipo") == "ntfs":
        L(f"\n--- VOLUME NTFS (LBA {d['particao_analisada']['lba_inicio']}) ---")
        for k, val in v.items():
            L(f"  {k:<24} {val}")
    elif v:
        L(f"\n--- VOLUME {v.get('tipo')} ---  {v}")

    if listar and d.get("arquivos"):
        L(f"\n--- LISTAGEM ({listar} primeiros) ---")
        for a in d["arquivos"][:listar]:
            L(f"  {'d' if a['dir'] else '-'} {a['tamanho']:>12}  {a['caminho']}")

    L(f"\n--- VERIFICACOES ---")
    if not d["checks"]:
        L("  nada a apontar")
    for sev, msg in d["checks"]:
        L(f"  [{sev}] {msg}")
    L("")


# --------------------------------------------------------------------------
# diff
# --------------------------------------------------------------------------

def flat(prefixo, obj, saida):
    if isinstance(obj, dict):
        for k, v in obj.items():
            flat(f"{prefixo}.{k}" if prefixo else k, v, saida)
    elif isinstance(obj, list):
        saida[prefixo] = f"[{len(obj)} itens]"
    else:
        saida[prefixo] = obj
    return saida


IGNORAR = ("arquivos", "checks", "raiz_ordem", "dispositivo", "rotulo", "chaves",
           "guid_unico", "disk_guid", "volume_id", "disk_signature",
           "volume_serial")


def diff(a, b):
    print(f"{'=' * 74}")
    print(f"  A = {a['rotulo']} ({a['dispositivo']})")
    print(f"  B = {b['rotulo']} ({b['dispositivo']})")
    print(f"{'=' * 74}")

    fa, fb = flat("", a, {}), flat("", b, {})
    chaves = sorted(set(fa) | set(fb))
    print(f"\n--- CAMPOS ESTRUTURAIS QUE DIFEREM ---")
    print(f"    (identificadores unicos como GUID e serial sao ignorados)")
    n = 0
    for k in chaves:
        if any(k.startswith(i) or f".{i}" in k for i in IGNORAR):
            continue
        va, vb = fa.get(k, "<ausente>"), fb.get(k, "<ausente>")
        if va != vb:
            n += 1
            print(f"  {k}")
            print(f"      A: {va}")
            print(f"      B: {vb}")
    if n == 0:
        print("  nenhum")

    # ordem da raiz
    ra = [e["nome"] for e in a.get("raiz_ordem", [])]
    rb = [e["nome"] for e in b.get("raiz_ordem", [])]
    if ra != rb:
        print(f"\n--- ORDEM DO DIRETORIO RAIZ DIFERE ---")
        print(f"  A: {ra}")
        print(f"  B: {rb}")

    # conjunto de arquivos
    ma = {x["caminho"]: x for x in a.get("arquivos", [])}
    mb = {x["caminho"]: x for x in b.get("arquivos", [])}
    if ma and mb:
        so_a = sorted(set(ma) - set(mb))
        so_b = sorted(set(mb) - set(ma))
        dif_tam = [(p, ma[p]["tamanho"], mb[p]["tamanho"])
                   for p in sorted(set(ma) & set(mb))
                   if ma[p]["tamanho"] != mb[p]["tamanho"]]
        print(f"\n--- CONTEUDO ---")
        print(f"  A: {len(ma)} entradas   B: {len(mb)} entradas")
        print(f"  so em A ({len(so_a)}):")
        for p in so_a[:40]:
            print(f"    {ma[p]['tamanho']:>12}  {p}")
        if len(so_a) > 40:
            print(f"    ... e mais {len(so_a) - 40}")
        print(f"  so em B ({len(so_b)}):")
        for p in so_b[:40]:
            print(f"    {mb[p]['tamanho']:>12}  {p}")
        if len(so_b) > 40:
            print(f"    ... e mais {len(so_b) - 40}")
        print(f"  tamanhos diferentes ({len(dif_tam)}):")
        for p, ta, tb in dif_tam[:40]:
            print(f"    {p}  A={ta}  B={tb}")

    # arquivos-chave lado a lado
    ka, kb = a.get("chaves") or {}, b.get("chaves") or {}
    if ka or kb:
        print(f"\n--- ARQUIVOS-CHAVE ---")
        for k in CHAVES:
            va, vb = ka.get(k), kb.get(k)
            if va is None and vb is None:
                continue
            marca = "  " if (va is not None) == (vb is not None) else "!!"
            print(f"  {marca} {k:<34} A={human(va) if va is not None else '-':>12}"
                  f"   B={human(vb) if vb is not None else '-':>12}")

    for rot, d in (("A", a), ("B", b)):
        print(f"\n--- VERIFICACOES DE {rot} ({d['rotulo']}) ---")
        if not d["checks"]:
            print("  nada a apontar")
        for sev, msg in d["checks"]:
            print(f"  [{sev}] {msg}")
    print("")


# --------------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("dump", help="analisa um dispositivo ou imagem")
    p.add_argument("dispositivo")
    p.add_argument("--label", default="MIDIA")
    p.add_argument("--out", help="grava o JSON aqui")
    p.add_argument("--listar", type=int, default=0,
                   help="imprime os N primeiros arquivos")

    p = sub.add_parser("diff", help="compara dois JSONs de dump")
    p.add_argument("a")
    p.add_argument("b")

    args = ap.parse_args()
    if args.cmd == "dump":
        d = dump(args.dispositivo, args.label)
        imprime(d, args.listar)
        if args.out:
            with open(args.out, "w") as f:
                json.dump(d, f, indent=1)
            print(f"JSON gravado em {args.out}")
    else:
        with open(args.a) as f:
            a = json.load(f)
        with open(args.b) as f:
            b = json.load(f)
        diff(a, b)


if __name__ == "__main__":
    main()
