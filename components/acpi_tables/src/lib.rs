// Copyright 2025 The Axvisor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! ACPI table generation for UEFI guest VMs.
//!
//! Generates a minimal set of ACPI tables required by OVMF:
//! - RSDP (Root System Description Pointer)
//! - XSDT (Extended System Description Table)
//! - FADT (Fixed ACPI Description Table)
//! - MADT (Multiple APIC Description Table)
//! - MCFG (Memory-mapped Configuration Space)

#![no_std]

extern crate alloc;

use alloc::{string::String, vec, vec::Vec};

mod sdt;

use sdt::SdtBuilder;

/// ACPI OEM ID (shared with sdt module)
const OEM_ID: [u8; 6] = *b"AXVSOR";

/// Configuration for ACPI table generation.
#[derive(Debug, Clone)]
pub struct AcpiConfig {
    /// Number of vCPUs
    pub cpu_num: usize,
    /// Guest RAM size in bytes
    pub ram_size: usize,
    /// Local APIC base address (typically 0xFEE00000)
    pub lapic_addr: u64,
    /// IO APIC base address (typically 0xFEC00000)
    pub ioapic_addr: u64,
    /// IO APIC ID
    pub ioapic_id: u8,
    /// IO APIC global system interrupt base
    pub ioapic_gsi_base: u32,
    /// PCI ECAM base address (typically 0xB0000000)
    pub ecam_base_addr: u64,
    /// PCI ECAM segment group number
    pub ecam_segment: u16,
    /// PCI ECAM start bus number
    pub ecam_bus_start: u8,
    /// PCI ECAM end bus number
    pub ecam_bus_end: u8,
    /// RSDP physical address in guest memory
    pub rsdp_gpa: u64,
}

impl Default for AcpiConfig {
    fn default() -> Self {
        Self {
            cpu_num: 1,
            ram_size: 0x1000_0000, // 256MB
            lapic_addr: 0xFEE0_0000,
            ioapic_addr: 0xFEC0_0000,
            ioapic_id: 0,
            ioapic_gsi_base: 0,
            ecam_base_addr: 0xB000_0000,
            ecam_segment: 0,
            ecam_bus_start: 0,
            ecam_bus_end: 0xFF,
            rsdp_gpa: 0x000F_0000,
        }
    }
}

/// Generated ACPI tables
#[derive(Debug, Clone)]
pub struct AcpiTables {
    /// RSDP table data
    pub rsdp: Vec<u8>,
    /// All other tables (XSDT + FADT + MADT + MCFG) as contiguous data
    pub tables: Vec<u8>,
    /// Offsets and sizes of individual tables within `tables`
    pub table_offsets: Vec<(String, usize, usize)>,
}

/// Build ACPI tables from configuration
pub struct AcpiTableBuilder {
    config: AcpiConfig,
}

impl AcpiTableBuilder {
    /// Create a new builder with the given configuration
    pub fn new(config: AcpiConfig) -> Self {
        Self { config }
    }

    /// Build all ACPI tables
    pub fn build(&self) -> AcpiTables {
        // Build DSDT first so we can reference its address in the FADT.
        let dsdt = self.build_dsdt();
        let madt = self.build_madt();
        let mcfg = self.build_mcfg();

        // Calculate table layout to determine physical addresses.
        // Layout: RSDP | XSDT | FADT | MADT | MCFG | DSDT
        let rsdp_size = 64u64;
        let xsdt_entry_count = 3usize; // FADT, MADT, MCFG
        let xsdt_size = (36 + xsdt_entry_count * 8) as u64;
        let fadt_size = 244u64;

        let xsdt_addr = self.config.rsdp_gpa + rsdp_size;
        let fadt_addr = xsdt_addr + xsdt_size;
        let madt_addr = fadt_addr + fadt_size;
        let mcfg_addr = madt_addr + madt.len() as u64;
        let dsdt_addr = mcfg_addr + mcfg.len() as u64;

        let fadt = self.build_fadt(dsdt_addr);
        let xsdt = self.build_xsdt(fadt_addr, madt_addr, mcfg_addr);
        let rsdp = self.build_rsdp(xsdt_addr);

        // Combine all tables into a contiguous buffer
        let mut tables = Vec::new();
        let mut table_offsets = Vec::new();

        let xsdt_offset = tables.len();
        tables.extend_from_slice(&xsdt);
        table_offsets.push(("XSDT".into(), xsdt_offset, xsdt.len()));

        let fadt_offset = tables.len();
        tables.extend_from_slice(&fadt);
        table_offsets.push(("FADT".into(), fadt_offset, fadt.len()));

        let madt_offset = tables.len();
        tables.extend_from_slice(&madt);
        table_offsets.push(("MADT".into(), madt_offset, madt.len()));

        let mcfg_offset = tables.len();
        tables.extend_from_slice(&mcfg);
        table_offsets.push(("MCFG".into(), mcfg_offset, mcfg.len()));

        let dsdt_offset = tables.len();
        tables.extend_from_slice(&dsdt);
        table_offsets.push(("DSDT".into(), dsdt_offset, dsdt.len()));

        AcpiTables {
            rsdp,
            tables,
            table_offsets,
        }
    }

    /// Build RSDP (Root System Description Pointer)
    fn build_rsdp(&self, xsdt_addr: u64) -> Vec<u8> {
        // RSDP is 36 bytes (ACPI 1.0) + extension to 64 bytes (ACPI 2.0+)
        let mut rsdp = vec![0u8; 64];

        // Signature: "RSD PTR "
        rsdp[0..8].copy_from_slice(b"RSD PTR ");
        // Checksum of first 20 bytes (filled later)
        // OEM ID
        rsdp[9..15].copy_from_slice(&OEM_ID);
        // Revision (2 for ACPI 2.0+)
        rsdp[15] = 2;
        // XSDT address (64-bit) at offset 24
        rsdp[24..32].copy_from_slice(&xsdt_addr.to_le_bytes());
        // Extended checksum (for entire 64 bytes, filled later)
        // Reserved
        rsdp[63] = 0;

        // Compute first checksum (bytes 0..20)
        let sum1 = rsdp[0..20].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        rsdp[8] = (0u8).wrapping_sub(sum1);

        // Compute extended checksum (bytes 0..64)
        let sum2 = rsdp[0..64].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        rsdp[32] = (0u8).wrapping_sub(sum2);

        rsdp
    }

    /// Build XSDT (Extended System Description Table)
    fn build_xsdt(&self, fadt_addr: u64, madt_addr: u64, mcfg_addr: u64) -> Vec<u8> {
        // XSDT contains 64-bit pointers to FADT, MADT, MCFG
        let entry_count = 3usize;
        let header_len = 36;
        let body_len = entry_count * 8; // 64-bit entries
        let total_len = header_len + body_len;

        let mut builder = SdtBuilder::new(*b"XSDT", total_len as u32);

        // Add 64-bit entry pointers
        builder.append_u64(fadt_addr);
        builder.append_u64(madt_addr);
        builder.append_u64(mcfg_addr);

        builder.build()
    }

    /// Build FADT (Fixed ACPI Description Table)
    fn build_fadt(&self, dsdt_addr: u64) -> Vec<u8> {
        // FADT is typically 244+ bytes for ACPI 2.0+
        let total_len = 244;
        let mut builder = SdtBuilder::new(*b"FACP", total_len as u32);

        // FIRMWARE_CTRL (32-bit FACS address) - we don't provide FACS, set to 0
        builder.append_u32(0);
        // DSDT (32-bit DSDT address)
        builder.append_u32(dsdt_addr as u32);

        // Reserved (byte)
        builder.append_u8(0);
        // Preferred_PM_Profile
        builder.append_u8(0); // Unspecified

        // SCI_INT (IRQ for ACPI SCI, typically 9)
        builder.append_u16(9);
        // SMI_CMD (System Management port, 0 = not supported)
        builder.append_u32(0);
        // ACPI_ENABLE
        builder.append_u8(0);
        // ACPI_DISABLE
        builder.append_u8(0);
        // S4BIOS_REQ
        builder.append_u8(0);
        // PSTATE_CNT
        builder.append_u8(0);
        // PM1a_EVT_BLK (Power management event register)
        // Use a placeholder I/O port address
        builder.append_u32(0x600);
        // PM1b_EVT_BLK
        builder.append_u32(0);
        // PM1a_CNT_BLK
        builder.append_u32(0x604);
        // PM1b_CNT_BLK
        builder.append_u32(0);
        // PM2_CNT_BLK
        builder.append_u32(0);
        // PM_TMR_BLK
        builder.append_u32(0x608);
        // GPE0_BLK
        builder.append_u32(0);
        // GPE1_BLK
        builder.append_u32(0);

        // PM1_EVT_LEN
        builder.append_u8(4);
        // PM1_CNT_LEN
        builder.append_u8(2);
        // PM2_CNT_LEN
        builder.append_u8(0);
        // PM_TMR_LEN
        builder.append_u8(4);
        // GPE0_BLK_LEN
        builder.append_u8(0);
        // GPE1_BLK_LEN
        builder.append_u8(0);
        // GPE1_BASE
        builder.append_u8(0);
        // CST_CNT
        builder.append_u8(0);
        // P_LVL2_LAT
        builder.append_u16(0x0065); // >100 means no C2
        // P_LVL3_LAT
        builder.append_u16(0x03E9); // >1000 means no C3
        // FLUSH_SIZE
        builder.append_u16(0);
        // FLUSH_STRIDE
        builder.append_u16(0);
        // DUTY_OFFSET
        builder.append_u8(0);
        // DUTY_WIDTH
        builder.append_u8(0);
        // DAY_ALRM
        builder.append_u8(0);
        // MON_ALRM
        builder.append_u8(0);
        // CENTURY
        builder.append_u8(0);
        // IAPC_BOOT_ARCH
        // Bit 0: Legacy devices, Bit 1: 8042, Bit 2: VGA, Bit 7: MSI not supported
        builder.append_u16(0x0002); // 8042 present
        // Reserved
        builder.append_u8(0);
        // Flags
        // Bit 0: WBINVD, Bit 1: WBINVD_FLUSH, Bit 2: PROC_C1, Bit 4: SLP_BUTTON
        builder.append_u32(0x0000_0015); // WBINVD + PROC_C1 + SLP_BUTTON

        // RESET_REG (12-byte GAS structure)
        builder.append_u8(0); // AddressSpaceId: SystemMemory
        builder.append_u8(0); // RegisterBitWidth
        builder.append_u8(0); // RegisterBitOffset
        builder.append_u8(0); // AccessSize
        builder.append_u64(0); // Address

        // RESET_VALUE
        builder.append_u8(0);

        // Reserved (3 bytes)
        builder.append_u8(0);
        builder.append_u8(0);
        builder.append_u8(0);

        // X_FIRMWARE_CTRL (64-bit FACS address)
        builder.append_u64(0);
        // X_DSDT (64-bit DSDT address)
        builder.append_u64(dsdt_addr);

        // X_PM1a_EVT_BLK (64-bit GAS)
        builder.append_gas(1, 0x600, 4); // SystemIO, port 0x600
        // X_PM1b_EVT_BLK
        builder.append_gas(0, 0, 0);
        // X_PM1a_CNT_BLK
        builder.append_gas(1, 0x604, 2);
        // X_PM1b_CNT_BLK
        builder.append_gas(0, 0, 0);
        // X_PM2_CNT_BLK
        builder.append_gas(0, 0, 0);
        // X_PM_TMR_BLK
        builder.append_gas(1, 0x608, 4);
        // X_GPE0_BLK
        builder.append_gas(0, 0, 0);
        // X_GPE1_BLK
        builder.append_gas(0, 0, 0);

        builder.build()
    }

    /// Build MADT (Multiple APIC Description Table)
    fn build_madt(&self) -> Vec<u8> {
        // Header (36) + Local APIC Address (4) + Flags (4) +
        // IO APIC Entry (12) + LAPIC entries (8 each) +
        // Interrupt Source Override (10)
        let header_and_fixed = 36 + 4 + 4;
        let ioapic_entry_size = 12;
        let lapic_entry_size = 8;
        let irq_override_size = 10;
        // ISA IRQ 0 -> GSI 2 override (standard PC)
        let num_irq_overrides = 1;

        let total_len = header_and_fixed
            + ioapic_entry_size
            + self.config.cpu_num * lapic_entry_size
            + num_irq_overrides * irq_override_size;

        let mut builder = SdtBuilder::new(*b"APIC", total_len as u32);

        // Local APIC Address (32-bit)
        builder.append_u32(self.config.lapic_addr as u32);
        // Flags (1 = PCAT_COMPAT)
        builder.append_u32(1);

        // IO APIC Entry (Type = 1)
        builder.append_u8(1); // Type: IO APIC
        builder.append_u8(ioapic_entry_size as u8); // Length
        builder.append_u8(self.config.ioapic_id); // IO APIC ID
        builder.append_u8(0); // Reserved
        builder.append_u32(self.config.ioapic_addr as u32); // IO APIC Address
        builder.append_u32(self.config.ioapic_gsi_base); // Global System Interrupt Base

        // Local APIC entries (Type = 0)
        for cpu_id in 0..self.config.cpu_num {
            builder.append_u8(0); // Type: Processor Local APIC
            builder.append_u8(lapic_entry_size as u8); // Length
            builder.append_u8(cpu_id as u8); // ACPI Processor UID
            builder.append_u8(cpu_id as u8); // APIC ID
            builder.append_u32(1); // Flags: Enabled
        }

        // Interrupt Source Override (Type = 2)
        // ISA IRQ 0 -> GSI 2 (standard PC convention)
        builder.append_u8(2); // Type: Interrupt Source Override
        builder.append_u8(irq_override_size as u8); // Length
        builder.append_u8(0); // Bus: ISA
        builder.append_u8(0); // Source: IRQ 0
        builder.append_u32(2); // Global System Interrupt: GSI 2
        builder.append_u16(0); // Flags: Active High, Edge Triggered

        builder.build()
    }

    /// Build MCFG (Memory-mapped Configuration Space)
    fn build_mcfg(&self) -> Vec<u8> {
        // Header (36) + Reserved (8) + 1 MCFG allocation entry (16)
        let total_len = 36 + 8 + 16;

        let mut builder = SdtBuilder::new(*b"MCFG", total_len as u32);

        // Reserved (8 bytes)
        builder.append_u64(0);

        // MCFG Allocation Entry
        builder.append_u64(self.config.ecam_base_addr); // Base Address
        builder.append_u16(self.config.ecam_segment); // PCI Segment Group Number
        builder.append_u8(self.config.ecam_bus_start); // Start Bus Number
        builder.append_u8(self.config.ecam_bus_end); // End Bus Number
        builder.append_u32(0); // Reserved

        builder.build()
    }

    /// Build DSDT (Differentiated System Description Table)
    ///
    /// Creates a DSDT with a PCI host bridge device (\_SB.PCI0) containing:
    /// - _HID PNP0A08 (PCI Express host bridge)
    /// - _CID PNP0A03 (PCI host bridge, for backwards compatibility)
    /// - _STA 0x0F (present, enabled, functioning, visible)
    /// - _BBN 0 (base bus number 0)
    /// - _SEG 0 (segment group 0)
    /// - _CRS (Current Resource Settings: bus, I/O, memory)
    /// - _PRT (PCI Routing Table: dev 1 INTA# -> GSI 16)
    fn build_dsdt(&self) -> Vec<u8> {
        // AML body:
        //   Scope(\_SB) {
        //       Device(PCI0) {
        //           Name(_HID, EISAID("PNP0A08"))
        //           Name(_CID, EISAID("PNP0A03"))
        //           Name(_STA, 0x0F)
        //           Name(_BBN, 0)
        //           Name(_SEG, 0)
        //           Name(_CRS, ResourceTemplate() {
        //               WordBusNumber(0, 0, 0xFF, 0, 0x100)
        //               WordIO(0, 0, 0x0CF7, 0, 0x0CF8)
        //               WordIO(0, 0x0D00, 0xFFFF, 0, 0xF300)
        //               DWordMemory(0, 0x80000000, 0xDFFFFFFF, 0, 0x60000000)
        //           })
        //           Method(_PRT, 0, NotSerialized) {
        //               Return (Package(1) {
        //                   Package(4) { 0x0001FFFF, 0, 0, 16 }
        //               })
        //           }
        //       }
        //   }
        //
        // EISAID("PNP0A08") = 0x41D00A08  (PCI Express Host Bridge)
        // EISAID("PNP0A03") = 0x41D00A03  (PCI Host Bridge, for backwards compat)
        //
        // The _CID is REQUIRED: Linux's PCI root bridge driver
        // (drivers/acpi/pci_root.c) only matches PNP0A03 in its
        // root_device_ids table.  A PNP0A08 device without a _CID of
        // PNP0A03 will NOT be recognised as a PCI root bridge, so _PRT
        // will never be evaluated and PCI interrupt routing will fail
        // with "can't derive routing for PCI INT A: no GSI".
        //
        // _CRS is REQUIRED: Without _CRS, the PCI root bridge driver
        // cannot determine bus resources (bus numbers, I/O, memory).
        // The previous crash when _CRS was present was caused by the
        // PkgLength encoding bug (values > 63 encoded as 1 byte), which
        // is now fixed.
        //
        // PkgLength encoding (ACPICA psargs.c — the reference decoder):
        //   Top 2 bits of byte0 = number of additional bytes (0-3).
        //   Values 0-63: 1-byte.  Values 64-4095: 2-byte.
        //   For 2-byte: byte0 = (1<<6)|(V&0x0F), byte1 = V>>4
        //   NOTE: byte0 uses a 4-bit mask (0x0F) for multi-byte PkgLengths,
        //   NOT 6-bit (0x3F) as a literal reading of the ACPI spec text
        //   suggests.  Verified against host DSDT (Method STRC at offset
        //   0x44 uses 0x40,0x05 for PkgLength=80) and ACPICA source.
        //
        // Resource descriptors (ACPI spec 6.4 / ACPICA aclocal.h):
        //   0x85 = Memory32 (fixed-length 9)        — NOT DWord Address Space!
        //   0x86 = FixedMemory32 (fixed-length 9)   — NOT Word Address Space!
        //   0x87 = DWord Address Space (var-len)
        //   0x88 = Word  Address Space (var-len)
        //   0x79 = End Tag (small item, 2 bytes)
        // ACPICA validates descriptor lengths strictly (utresrc.c):
        //   0x86/0x85 expect resource_length==9; we pass 13/23 → AE_AML_BAD_RESOURCE_LENGTH.
        //   WordBusNumber/WordIO use 0x88; DWordMemory uses 0x87.
        //   WordBusNumber (0x88): 3 header + 13 data = 16 bytes
        //   WordIO        (0x88): 3 header + 13 data = 16 bytes
        //   DWordMemory   (0x87): 3 header + 23 data = 26 bytes
        //   EndTag        (0x79): 2 bytes (checksum byte is NOT validated by ACPICA)
        //
        // Size calculations:
        //   _CRS buffer contents = 16+16+16+26+2 = 76 bytes
        //   _CRS PkgLength = 2+2+76 = 80 → 0x40,0x05 (2-byte)
        //   _CRS total = 1+4+1+2+2+76 = 86 bytes
        //   Device contents = 4+10+10+7+6+6+86+23 = 148
        //   Device PkgLength = 2+4+148 = 154 → 0x4A,0x09 (2-byte)
        //   Scope PkgLength = 2+1+4+156 = 163 → 0x43,0x0A (2-byte)
        //   Total AML body = 1+2+1+4+2+2+4+148 = 164
        let aml_body: [u8; 164] = [
            // Scope(\_SB) — PkgLength=163 (2-byte: 0x43, 0x0A)
            0x10, 0x43, 0x0A, // ScopeOp, PkgLength (2-byte)
            0x5C, // RootChar
            0x5F, 0x53, 0x42, 0x5F, // NameSeg "_SB_"
            // Device(PCI0) — PkgLength=154 (2-byte: 0x4A, 0x09)
            0x5B, 0x82, // ExtOpPrefix + DeviceOp
            0x4A, 0x09, // PkgLength=154 (2-byte)
            0x50, 0x43, 0x49, 0x30, // NameSeg "PCI0"
            // Name(_HID, EISAID("PNP0A08")) — 10 bytes
            // EISAID bytes are stored big-endian in the DWordConst:
            // spec EISA ID 0x41D00A08 → AML bytes 41 D0 0A 08
            0x08, // NameOp
            0x5F, 0x48, 0x49, 0x44, // NameSeg "_HID"
            0x0C, 0x41, 0xD0, 0x0A, 0x08, // Dword 0x080AD041 (byte-swapped EISA ID)
            // Name(_CID, EISAID("PNP0A03")) — 10 bytes
            0x08, // NameOp
            0x5F, 0x43, 0x49, 0x44, // NameSeg "_CID"
            0x0C, 0x41, 0xD0, 0x0A, 0x03, // Dword 0x030AD041 (byte-swapped EISA ID)
            // Name(_STA, 0x0F) — 7 bytes
            0x08, // NameOp
            0x5F, 0x53, 0x54, 0x41, // NameSeg "_STA"
            0x0A, 0x0F, // Byte 0x0F (present|enabled|visible|functioning)
            // Name(_BBN, 0) — 6 bytes
            0x08, // NameOp
            0x5F, 0x42, 0x42, 0x4E, // NameSeg "_BBN"
            0x00, // Zero (bus 0)
            // Name(_SEG, 0) — 6 bytes
            0x08, // NameOp
            0x5F, 0x53, 0x45, 0x47, // NameSeg "_SEG"
            0x00, // Zero (segment 0)
            // Name(_CRS, ResourceTemplate) — 86 bytes total
            0x08, // NameOp
            0x5F, 0x43, 0x52, 0x53, // NameSeg "_CRS"
            0x11, // BufferOp
            0x40, 0x05, // PkgLength=80 (2-byte, since 80 > 63)
            0x0A, 0x4C, // ByteConst=76 (buffer size, TermArg)
            // WordBusNumber: bus 0-255 (16 bytes)
            0x88, 0x0D, 0x00, // WORD Address Space, length=13
            0x02, // Resource Type: Bus Number
            0x0B, // General Flags: MaxFixed|MinFixed|PosDecode
            0x00, // Type-Specific Flags
            0x00, 0x00, // Granularity=0
            0x00, 0x00, // Min=0
            0xFF, 0x00, // Max=255
            0x00, 0x00, // Translation Offset=0
            0x00, 0x01, // Length=256
            // WordIO #1: 0x0000-0x0CF7 (legacy ISA, 16 bytes)
            0x88, 0x0D, 0x00, // WORD Address Space, length=13
            0x01, // Resource Type: I/O
            0x0B, // General Flags: MaxFixed|MinFixed|PosDecode
            0x03, // Type-Specific Flags: EntireRange
            0x00, 0x00, // Granularity=0
            0x00, 0x00, // Min=0
            0xF7, 0x0C, // Max=0x0CF7
            0x00, 0x00, // Translation Offset=0
            0xF8, 0x0C, // Length=0x0CF8
            // WordIO #2: 0x0D00-0xFFFF (PCI I/O, 16 bytes)
            0x88, 0x0D, 0x00, // WORD Address Space, length=13
            0x01, // Resource Type: I/O
            0x0B, // General Flags: MaxFixed|MinFixed|PosDecode
            0x03, // Type-Specific Flags: EntireRange
            0x00, 0x00, // Granularity=0
            0x00, 0x0D, // Min=0x0D00
            0xFF, 0xFF, // Max=0xFFFF
            0x00, 0x00, // Translation Offset=0
            0x00, 0xF3, // Length=0xF300
            // DWordMemory: 0x80000000-0xDFFFFFFF (PCI MMIO, 26 bytes)
            0x87, 0x17, 0x00, // DWORD Address Space, length=23
            0x00, // Resource Type: Memory
            0x0B, // General Flags: MaxFixed|MinFixed|PosDecode
            0x01, // Type-Specific Flags: NonCacheable, ReadWrite
            0x00, 0x00, 0x00, 0x00, // Granularity=0
            0x00, 0x00, 0x00, 0x80, // Min=0x80000000
            0xFF, 0xFF, 0xFF, 0xDF, // Max=0xDFFFFFFF
            0x00, 0x00, 0x00, 0x00, // Translation Offset=0
            0x00, 0x00, 0x00, 0x60, // Length=0x60000000
            // EndTag (2 bytes)
            0x79, 0x00, // EndTag + checksum
            // Method(_PRT, 0, NotSerialized) — 23 bytes
            0x14, // MethodOp
            0x16, // PkgLength=22
            0x5F, 0x50, 0x52, 0x54, // NameSeg "_PRT"
            0x00, // MethodFlags (0 args, NotSerialized, SyncLevel 0)
            0xA4, // ReturnOp
            // Package(1) { Package(4) { ... } } — 15 bytes
            0x12, 0x0E, 0x01, // PackageOp, PkgLength=14, NumElements=1
            // Entry: dev 1, INTA# -> GSI 16
            0x12, 0x0B, 0x04, // PackageOp, PkgLength=11, NumElements=4
            0x0C, 0xFF, 0xFF, 0x01, 0x00, // Dword 0x0001FFFF (dev 1, any function)
            0x00, // Zero (pin 0 = INTA#)
            0x00, // Zero (source = 0, meaning GSI)
            0x0A, 0x10, // Byte 16 (GSI 16)
        ];
        let total_len = 36 + aml_body.len();

        let mut builder = SdtBuilder::new(*b"DSDT", total_len as u32);

        // AML body
        for &byte in &aml_body {
            builder.append_u8(byte);
        }

        builder.build()
    }
}
