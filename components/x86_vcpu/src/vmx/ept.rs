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

//! EPT-violation handling, MMIO forwarding, guest instruction decoding, and
//! guest page-table / EPT walking helpers. Extracted from `vcpu.rs`.

use ax_errno::{AxResult, ax_err_type};
use axaddrspace::{
    HostPhysAddr, MappingFlags, NestedPageFaultInfo,
    device::{AccessWidth, SysRegAddr, SysRegAddrRange},
};
use axdevice_base::BaseDeviceOps;
use axvisor_api::memory::PhysFrame;
use x86_vlapic::EmulatedLocalApic;

use super::{vcpu::VmxVcpu, vmcs::VmcsGuestNW};

impl VmxVcpu {
    pub(super) fn handle_apic_mmio_ept_violation(&mut self) -> AxResult {
        let info = self.nested_page_fault_info()?;
        let gpa = info.fault_guest_paddr.as_usize();
        let apic_offset = (gpa - 0xFEE0_0000) as u32;
        let is_write = info.access_flags.contains(axaddrspace::MappingFlags::WRITE);

        // Intel SDM: VMEXIT_INSTRUCTION_LEN is undefined for EPT violations.
        // Always decode the instruction bytes to get the length and the
        // destination/source register, falling back to RAX/Dword if decoding
        // fails (as the old code assumed RAX for all MMIO writes).
        let (instr_len, reg, _width) =
            if let Some((bytes, actual_len)) = self.read_guest_instr_bytes(15) {
                if let Some((reg, width, decoded_len)) =
                    Self::decode_mmio_mov_instr(&bytes[..actual_len])
                {
                    (decoded_len.max(1), reg, width)
                } else {
                    let len = Self::decode_x86_instruction_length(&bytes[..actual_len]).max(1);
                    (len, 0u8, AccessWidth::Dword)
                }
            } else {
                (2u8, 0u8, AccessWidth::Dword)
            };

        let apic_msr = 0x800 + (apic_offset >> 4);

        if is_write {
            let value = self.regs().get_reg_of_index(reg) as u32;
            trace!(
                "[APIC-MMIO] write: offset={:#x}, msr={:#x}, value={:#x}, reg={}",
                apic_offset, apic_msr, value, reg
            );
            <EmulatedLocalApic as BaseDeviceOps<SysRegAddrRange>>::handle_write(
                &self.vlapic,
                SysRegAddr::new(apic_msr as _),
                AccessWidth::Dword,
                value as usize,
            )?;
        } else {
            let value = <EmulatedLocalApic as BaseDeviceOps<SysRegAddrRange>>::handle_read(
                &self.vlapic,
                SysRegAddr::new(apic_msr as _),
                AccessWidth::Dword,
            )? as u64;
            trace!(
                "[APIC-MMIO] read: offset={:#x}, msr={:#x}, value={:#x}, reg={}",
                apic_offset, apic_msr, value, reg
            );
            // Write the read value to the correct destination register
            self.regs_mut().set_reg_of_index(reg, value);
        }

        self.advance_rip(instr_len)?;

        Ok(())
    }

    /// Handle EPT violations caused by guest memory probing beyond ram_end.
    /// Maps a read-only dummy page (all 0xFF) for reads so the guest detects
    /// non-existent memory.  Writes are discarded by advancing RIP.
    pub(super) fn handle_memory_probing_ept_violation(
        &mut self,
        gpa: usize,
        info: &NestedPageFaultInfo,
    ) -> AxResult {
        let page_aligned_gpa = gpa & !0xFFF;

        if info.access_flags.contains(MappingFlags::WRITE) {
            // Write beyond ram_end: discard by advancing past the instruction.
            // Intel SDM: VMEXIT_INSTRUCTION_LEN is undefined for EPT violations.
            let instr_len: u8 = if let Some((bytes, actual_len)) = self.read_guest_instr_bytes(15) {
                Self::decode_x86_instruction_length(&bytes[..actual_len]).max(1)
            } else {
                2
            };
            self.advance_rip(instr_len)?;
            info!(
                "[EPT-VIOL] write beyond ram_end discarded: GPA={gpa:#x}, ram_end={:#x}",
                self.ram_end
            );
        } else {
            // Read beyond ram_end: map a read-only dummy page so the guest reads 0xFF.
            if let Err(e) = self.ept_map_4k_readonly(
                page_aligned_gpa as u64,
                self.dummy_ff_page.start_paddr().as_usize() as u64,
            ) {
                info!("[EPT-VIOL] FAILED to map dummy page at GPA={page_aligned_gpa:#x}: {e:?}");
            } else {
                info!(
                    "[EPT-VIOL] mapped read-only dummy page at GPA={page_aligned_gpa:#x} \
                     (ram_end={:#x})",
                    self.ram_end
                );
            }
            // Do NOT advance RIP — the instruction will re-execute against the
            // newly-mapped dummy page and read 0xFF.
        }

        Ok(())
    }

    pub(super) fn handle_pci_mmio_ept_violation(&mut self) -> AxResult {
        let info = self.nested_page_fault_info()?;
        let gpa = info.fault_guest_paddr.as_usize();
        let is_write = info.access_flags.contains(axaddrspace::MappingFlags::WRITE);

        if is_write {
            // Writes to PCI MMIO: discard by advancing past the instruction.
            // Intel SDM: VMEXIT_INSTRUCTION_LEN is undefined for EPT violations.
            let instr_len: u8 = if let Some((bytes, actual_len)) = self.read_guest_instr_bytes(15) {
                let decoded = Self::decode_x86_instruction_length(&bytes[..actual_len]);
                if decoded > 0 { decoded } else { 2 }
            } else {
                2
            };
            debug!(
                "[PCI-MMIO] write ignored: GPA={:#x}, RIP={:#x}",
                gpa,
                self.rip()
            );
            self.advance_rip(instr_len)?;
        } else {
            // Reads from PCI MMIO: map a dummy page filled with 0xFF so the
            // instruction re-executes and reads 0xFFFFFFFF from the mapped page.
            // This is correct regardless of which register the instruction targets
            // (unlike setting RAX directly which only works for mov rax, [addr]).
            let page_aligned_gpa = gpa & !0xFFF;
            if let Err(e) = self.ept_map_4k_readonly(
                page_aligned_gpa as u64,
                self.dummy_ff_page.start_paddr().as_usize() as u64,
            ) {
                warn!("[PCI-MMIO] failed to map dummy page at GPA={page_aligned_gpa:#x}: {e:?}");
                // Fallback: set RAX and advance RIP (incorrect for non-RAX targets)
                self.regs_mut().rax = 0xFFFF_FFFF;
                let instr_len: u8 =
                    if let Some((bytes, actual_len)) = self.read_guest_instr_bytes(15) {
                        let decoded = Self::decode_x86_instruction_length(&bytes[..actual_len]);
                        if decoded > 0 { decoded } else { 2 }
                    } else {
                        2
                    };
                self.advance_rip(instr_len)?;
            }
            // Do NOT advance RIP — the instruction will re-execute against the
            // newly-mapped dummy page and read 0xFFFFFFFF into the correct register.
        }

        Ok(())
    }

    pub(super) fn read_guest_instr_bytes(&self, max_len: usize) -> Option<([u8; 15], usize)> {
        let guest_rip = self.rip() as u64;
        let ept_root = self.ept_root?;

        // When paging is disabled (CR0.PG=0), GVA=GPA directly.
        let cr0 = VmcsGuestNW::CR0.read().ok()?;
        let gpa = if cr0 & (1 << 31) == 0 {
            // No paging: linear address = physical address
            guest_rip
        } else {
            let cr3 = VmcsGuestNW::CR3.read().ok()? as u64;
            self.gva_to_gpa_via_guest_pt(cr3, guest_rip)?
        };
        let hpa = self.gpa_to_hpa_via_ept(ept_root, gpa)?;

        const PHYS_VIRT_OFFSET: u64 = 0xffff_8000_0000_0000;
        let mut bytes = [0u8; 15];
        for (i, byte) in bytes.iter_mut().enumerate().take(max_len.min(15)) {
            let vaddr = (hpa + i) as u64 + PHYS_VIRT_OFFSET;
            *byte = unsafe { core::ptr::read_volatile(vaddr as *const u8) };
        }
        Some((bytes, max_len.min(15)))
    }

    pub(super) fn decode_x86_instruction_length(bytes: &[u8]) -> u8 {
        if bytes.is_empty() {
            return 0;
        }

        let mut i = 0;

        loop {
            if i >= bytes.len() {
                return i as u8;
            }
            match bytes[i] {
                0x26 | 0x2E | 0x36 | 0x3E | 0x64 | 0x65 | 0x66 | 0x67 | 0xF0 | 0xF2 | 0xF3 => {
                    i += 1;
                }
                _ => break,
            }
        }

        if bytes[i] >= 0x40 && bytes[i] <= 0x4F {
            i += 1;
            if i >= bytes.len() {
                return i as u8;
            }
        }

        let opcode = bytes[i];
        i += 1;

        let has_modrm = if opcode == 0x0F {
            if i >= bytes.len() {
                return i as u8;
            }
            i += 1;
            true
        } else {
            Self::x86_opcode_has_modrm(opcode)
        };

        if has_modrm && i < bytes.len() {
            let modrm = bytes[i];
            i += 1;

            let mod_field = (modrm >> 6) & 3;
            let rm_field = modrm & 7;

            // Handle SIB byte: when rm_field == 4 (and mod_field != 3),
            // a SIB byte follows.  Read it to check the base field,
            // because if mod_field == 0 and SIB base == 5, a 32-bit
            // displacement follows the SIB byte.
            let sib_base = if mod_field != 3 && rm_field == 4 && i < bytes.len() {
                let sib = bytes[i];
                i += 1;
                sib & 7
            } else {
                0
            };

            match mod_field {
                0 if rm_field == 5 || (rm_field == 4 && sib_base == 5) => {
                    i += 4;
                }
                1 => {
                    i += 1;
                }
                2 => {
                    i += 4;
                }
                _ => {}
            }
        }

        i as u8
    }

    /// Decode a MOV r, r/m (read) or MOV r/m, r (write) instruction to determine
    /// the register operand, access width, and full instruction length.
    /// Returns (reg_index, width, instr_len) or None if not a recognized MOV.
    /// - 0x8A: MOV r8, r/m8   (read, Byte)
    /// - 0x8B: MOV r32/64, r/m32/64 (read, Word/Dword/Qword)
    /// - 0x88: MOV r/m8, r8   (write, Byte)
    /// - 0x89: MOV r/m, r32/64 (write, Word/Dword/Qword)
    pub(super) fn decode_mmio_mov_instr(bytes: &[u8]) -> Option<(u8, AccessWidth, u8)> {
        if bytes.is_empty() {
            return None;
        }
        let mut i = 0;
        let mut operand_size_16 = false;

        // Skip legacy prefixes
        while i < bytes.len() {
            match bytes[i] {
                0x26 | 0x2E | 0x36 | 0x3E | 0x64 | 0x65 | 0xF0 | 0xF2 | 0xF3 => {
                    i += 1;
                }
                0x66 => {
                    operand_size_16 = true;
                    i += 1;
                }
                0x67 => {
                    i += 1;
                }
                _ => break,
            }
        }

        // REX prefix (0x40-0x4F)
        let mut rex_r = 0u8;
        let mut rex_w = false;
        if i < bytes.len() && (0x40..=0x4F).contains(&bytes[i]) {
            let rex = bytes[i];
            rex_r = (rex >> 2) & 1;
            rex_w = (rex & 0x08) != 0;
            i += 1;
        }

        if i >= bytes.len() {
            return None;
        }

        let opcode = bytes[i];
        i += 1;

        let width = match opcode {
            0x8A | 0x88 => AccessWidth::Byte,
            0x8B | 0x89 => {
                if operand_size_16 {
                    AccessWidth::Word
                } else if rex_w {
                    AccessWidth::Qword
                } else {
                    AccessWidth::Dword
                }
            }
            _ => return None,
        };

        if i >= bytes.len() {
            return None;
        }

        let modrm = bytes[i];
        let reg = ((modrm >> 3) & 7) | (rex_r << 3);

        // Use the existing length decoder for the full instruction length
        let instr_len = Self::decode_x86_instruction_length(bytes);

        Some((reg, width, instr_len))
    }

    pub(super) fn x86_opcode_has_modrm(opcode: u8) -> bool {
        !matches!(
            opcode,
            0x06 | 0x07 | 0x0E |
                0x16 | 0x17 | 0x1E | 0x1F |
                0x27 | 0x2F | 0x37 | 0x3F |
                0x6A |
                0x70..=0x7F |
                0x9A |
                0xA0..=0xA3 |
                0xA8..=0xA9 |
                0xB0..=0xBF |
                0xC2 | 0xC3 | 0xC8 | 0xCA | 0xCB | 0xCD |
                0xD4..=0xD6 |
                0xE0..=0xE7 |
                0xEB |
                0xEC..=0xEF |
                0xF4 | 0xF5
        )
    }

    pub(super) fn gva_to_gpa_via_guest_pt(&self, cr3: u64, gva: u64) -> Option<u64> {
        let ept_root = self.ept_root?;
        let pml4_index = ((gva >> 39) & 0x1FF) as usize;
        let pdpt_index = ((gva >> 30) & 0x1FF) as usize;
        let pd_index = ((gva >> 21) & 0x1FF) as usize;
        let pt_index = ((gva >> 12) & 0x1FF) as usize;
        let offset = (gva & 0xFFF) as usize;

        let pml4e_gpa = (cr3 & 0xF_FFFF_F000) + (pml4_index * 8) as u64;
        let pml4e_hpa = self.gpa_to_hpa_via_ept(ept_root, pml4e_gpa)? as u64;
        let pml4e = self.read_phys_u64(pml4e_hpa)?;
        if pml4e & 1 == 0 {
            return None;
        }

        let pdpte_gpa = (pml4e & 0xF_FFFF_F000) + (pdpt_index * 8) as u64;
        let pdpte_hpa = self.gpa_to_hpa_via_ept(ept_root, pdpte_gpa)? as u64;
        let pdpte = self.read_phys_u64(pdpte_hpa)?;
        if pdpte & 1 == 0 {
            return None;
        }
        if pdpte & 0x80 != 0 {
            return Some((pdpte & 0xF_FFFF_F000) + (gva & 0x3FFF_FFFF));
        }

        let pde_gpa = (pdpte & 0xF_FFFF_F000) + (pd_index * 8) as u64;
        let pde_hpa = self.gpa_to_hpa_via_ept(ept_root, pde_gpa)? as u64;
        let pde = self.read_phys_u64(pde_hpa)?;
        if pde & 1 == 0 {
            return None;
        }
        if pde & 0x80 != 0 {
            return Some((pde & 0xF_FFFF_F000) + (gva & 0x1F_FFFF));
        }

        let pte_gpa = (pde & 0xF_FFFF_F000) + (pt_index * 8) as u64;
        let pte_hpa = self.gpa_to_hpa_via_ept(ept_root, pte_gpa)? as u64;
        let pte = self.read_phys_u64(pte_hpa)?;
        if pte & 1 == 0 {
            return None;
        }

        Some((pte & 0xF_FFFF_F000) + offset as u64)
    }

    pub(super) fn gpa_to_hpa_via_ept(&self, ept_root: HostPhysAddr, gpa: u64) -> Option<usize> {
        let pml4_index = ((gpa >> 39) & 0x1FF) as usize;
        let pdpt_index = ((gpa >> 30) & 0x1FF) as usize;
        let pd_index = ((gpa >> 21) & 0x1FF) as usize;
        let pt_index = ((gpa >> 12) & 0x1FF) as usize;
        let offset = (gpa & 0xFFF) as usize;

        let pml4e = self.read_phys_u64(ept_root.as_usize() as u64 + (pml4_index * 8) as u64)?;
        if pml4e & 7 == 0 {
            return None;
        }

        let pdpte = self.read_phys_u64((pml4e & 0xF_FFFF_F000) + (pdpt_index * 8) as u64)?;
        if pdpte & 7 == 0 {
            return None;
        }
        if pdpte & 0x80 != 0 {
            return Some(((pdpte & 0xF_FFFF_F000) + offset as u64) as usize);
        }

        let pde = self.read_phys_u64((pdpte & 0xF_FFFF_F000) + (pd_index * 8) as u64)?;
        if pde & 7 == 0 {
            return None;
        }
        if pde & 0x80 != 0 {
            return Some(((pde & 0xF_FFFF_F000) + (gpa & 0x1F_FFFF)) as usize);
        }

        let pte = self.read_phys_u64((pde & 0xF_FFFF_F000) + (pt_index * 8) as u64)?;
        if pte & 7 == 0 {
            return None;
        }

        Some(((pte & 0xF_FFFF_F000) + offset as u64) as usize)
    }

    pub(super) fn read_phys_u64(&self, paddr: u64) -> Option<u64> {
        const PHYS_VIRT_OFFSET: u64 = 0xffff_8000_0000_0000;
        let vaddr = paddr + PHYS_VIRT_OFFSET;
        Some(unsafe { core::ptr::read_volatile(vaddr as *const u64) })
    }

    pub(super) fn write_phys_u64(&self, paddr: u64, val: u64) {
        const PHYS_VIRT_OFFSET: u64 = 0xffff_8000_0000_0000;
        let vaddr = paddr + PHYS_VIRT_OFFSET;
        unsafe { core::ptr::write_volatile(vaddr as *mut u64, val) }
    }

    /// Map a 4 KB host page into the EPT at the given GPA with the specified
    /// permission flags.  Walks the 4-level EPT, allocating intermediate tables
    /// as needed.  Returns Ok(()) on success.
    pub(super) fn ept_map_4k_with_flags(&self, gpa: u64, hpa: u64, perm: u64) -> AxResult {
        let ept_root = self.ept_root.ok_or(ax_err_type!(Unsupported))?.as_usize() as u64;

        let pml4_index = ((gpa >> 39) & 0x1FF) as usize;
        let pdpt_index = ((gpa >> 30) & 0x1FF) as usize;
        let pd_index = ((gpa >> 21) & 0x1FF) as usize;
        let pt_index = ((gpa >> 12) & 0x1FF) as usize;

        // EPT entry flags: bit 0 = Read, bit 1 = Write, bit 2 = Execute
        const EPT_R: u64 = 1 << 0;
        const EPT_RWX: u64 = EPT_R | (1 << 1) | (1 << 2);

        // Walk PML4 → PDPT → PD → PT, allocating missing tables
        let pml4e_addr = ept_root + (pml4_index * 8) as u64;
        let pml4e = self
            .read_phys_u64(pml4e_addr)
            .ok_or(ax_err_type!(NotFound))?;
        let pdpt_base = if pml4e & EPT_R != 0 {
            pml4e & 0xF_FFFF_F000
        } else {
            let new_table = PhysFrame::alloc_zero()?.start_paddr().as_usize() as u64;
            self.write_phys_u64(pml4e_addr, new_table | EPT_RWX);
            new_table
        };

        let pdpte_addr = pdpt_base + (pdpt_index * 8) as u64;
        let pdpte = self
            .read_phys_u64(pdpte_addr)
            .ok_or(ax_err_type!(NotFound))?;
        let pd_base = if pdpte & EPT_R != 0 {
            pdpte & 0xF_FFFF_F000
        } else {
            let new_table = PhysFrame::alloc_zero()?.start_paddr().as_usize() as u64;
            self.write_phys_u64(pdpte_addr, new_table | EPT_RWX);
            new_table
        };

        let pde_addr = pd_base + (pd_index * 8) as u64;
        let pde = self.read_phys_u64(pde_addr).ok_or(ax_err_type!(NotFound))?;
        let pt_base = if pde & EPT_R != 0 {
            pde & 0xF_FFFF_F000
        } else {
            let new_table = PhysFrame::alloc_zero()?.start_paddr().as_usize() as u64;
            self.write_phys_u64(pde_addr, new_table | EPT_RWX);
            new_table
        };

        // Write the PTE
        let pte_addr = pt_base + (pt_index * 8) as u64;
        let pte = (hpa & 0xF_FFFF_F000) | perm;
        self.write_phys_u64(pte_addr, pte);

        Ok(())
    }

    /// Map a 4 KB page as read-only (EPT_R only) — used for dummy pages
    /// that should return 0xFF on read but trigger EPT violation on write.
    pub(super) fn ept_map_4k_readonly(&self, gpa: u64, hpa: u64) -> AxResult {
        const EPT_R: u64 = 1 << 0;
        self.ept_map_4k_with_flags(gpa, hpa, EPT_R)
    }
}
