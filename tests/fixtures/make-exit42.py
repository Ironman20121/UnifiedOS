#!/usr/bin/env python3
"""Writes exit42.exe: the smallest 64-bit Windows console program that exits with code 42.

Used to check that a Windows program's exit code travels from Wine, through the
guest, back to the host. Usage: make-exit42.py OUTPUT
"""
import struct
import sys

FILE_ALIGNMENT = 0x200
SECTION_ALIGNMENT = 0x1000
IMAGE_BASE = 0x140000000
ENTRY_RVA = 0x1000
CODE = bytes([0xB8, 42, 0, 0, 0, 0xC3])  # mov eax, 42 ; ret

dos_header = b"MZ" + bytes(58) + struct.pack("<I", 0x40)
coff_header = struct.pack(
    "<4sHHIIIHH",
    b"PE\0\0",
    0x8664,  # x86-64
    1,  # sections
    0, 0, 0,
    0xF0,  # optional header size
    0x0022,  # executable, large-address-aware
)
optional_header = struct.pack(
    "<HBBIIIIIQIIHHHHHHIIIIHHQQQQII",
    0x20B,  # PE32+
    14, 0,
    FILE_ALIGNMENT,  # code size
    0, 0,
    ENTRY_RVA,
    ENTRY_RVA,  # base of code
    IMAGE_BASE,
    SECTION_ALIGNMENT,
    FILE_ALIGNMENT,
    6, 0,  # OS version
    0, 0,  # image version
    6, 0,  # subsystem version
    0,
    2 * SECTION_ALIGNMENT,  # image size: headers + one section
    FILE_ALIGNMENT,  # headers size
    0,
    3,  # console subsystem
    0x8100,  # NX-compatible, fixed base
    0x100000, 0x1000,  # stack reserve, commit
    0x100000, 0x1000,  # heap reserve, commit
    0,
    16,  # data directories
) + bytes(16 * 8)
section_header = struct.pack(
    "<8sIIIIIIHHI",
    b".text",
    len(CODE),
    ENTRY_RVA,
    FILE_ALIGNMENT,
    FILE_ALIGNMENT,  # file offset of the section
    0, 0, 0, 0,
    0x60000020,  # code, executable, readable
)

headers = dos_header + coff_header + optional_header + section_header
image = headers.ljust(FILE_ALIGNMENT, b"\0") + CODE.ljust(FILE_ALIGNMENT, b"\0")
with open(sys.argv[1], "wb") as output:
    output.write(image)
