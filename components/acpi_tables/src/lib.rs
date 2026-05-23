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
        let fadt = self.build_fadt();
        let madt = self.build_madt();
        let mcfg = self.build_mcfg();
        let xsdt = self.build_xsdt(&fadt, &madt, &mcfg);
        let rsdp = self.build_rsdp(&xsdt);

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

        AcpiTables {
            rsdp,
            tables,
            table_offsets,
        }
    }

    /// Build RSDP (Root System Description Pointer)
    fn build_rsdp(&self, _xsdt: &[u8]) -> Vec<u8> {
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
        let xsdt_addr = self.config.rsdp_gpa + 64; // XSDT follows RSDP
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
    fn build_xsdt(&self, fadt: &[u8], madt: &[u8], _mcfg: &[u8]) -> Vec<u8> {
        // XSDT contains 64-bit pointers to FADT, MADT, MCFG
        let entry_count = 3usize;
        let header_len = 36;
        let body_len = entry_count * 8; // 64-bit entries
        let total_len = header_len + body_len;

        let mut builder = SdtBuilder::new(*b"XSDT", total_len as u32);

        // Calculate physical addresses of each table
        // RSDP (64 bytes) + XSDT + FADT + MADT + MCFG
        let xsdt_addr = self.config.rsdp_gpa + 64;
        let fadt_addr = xsdt_addr + total_len as u64;
        let madt_addr = fadt_addr + fadt.len() as u64;
        let mcfg_addr = madt_addr + madt.len() as u64;

        // Add 64-bit entry pointers
        builder.append_u64(fadt_addr);
        builder.append_u64(madt_addr);
        builder.append_u64(mcfg_addr);

        builder.build()
    }

    /// Build FADT (Fixed ACPI Description Table)
    fn build_fadt(&self) -> Vec<u8> {
        // FADT is typically 244+ bytes for ACPI 2.0+
        let total_len = 244;
        let mut builder = SdtBuilder::new(*b"FACP", total_len as u32);

        // FIRMWARE_CTRL (32-bit FACS address) - we don't provide FACS, set to 0
        builder.append_u32(0);
        // DSDT (32-bit DSDT address) - we don't provide DSDT, set to 0
        builder.append_u32(0);

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
        builder.append_u64(0);

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
            builder.append_u8((cpu_id as u8) << 1 | 1); // APIC ID (even) + Enabled flag
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
}
