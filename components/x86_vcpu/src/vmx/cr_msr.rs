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

//! CR-access and MSR/APIC-access VM-exit handling. Extracted from `vcpu.rs`.

use ax_errno::AxResult;
use axaddrspace::device::{AccessWidth, SysRegAddr, SysRegAddrRange};
use axdevice_base::BaseDeviceOps;
use x86_64::registers::control::{Cr0Flags, Cr4Flags};
use x86_vlapic::EmulatedLocalApic;

use super::{
    vcpu::{MSR_IA32_EFER_LMA_BIT, VmxVcpu},
    vmcs::{
        self, ApicAccessExitType, VmcsControlNW, VmcsGuest16, VmcsGuest32, VmcsGuest64,
        VmcsGuestNW, VmcsReadOnly32,
    },
};
use crate::msr::Msr;

impl VmxVcpu {
    pub(super) fn set_cr(&mut self, cr_idx: usize, val: u64) {
        (|| -> AxResult {
            // debug!("set guest CR{} to val {:#x}", cr_idx, val);
            match cr_idx {
                0 => {
                    // Retrieve/validate restrictions on CR0
                    //
                    // In addition to what the VMX MSRs tell us, make sure that
                    // - NW and CD are kept off as they are not updated on VM exit and we
                    //   don't want them enabled for performance reasons while in root mode
                    // - PE and PG can be freely chosen (by the guest) because we demand
                    //   unrestricted guest mode support anyway
                    // - ET is ignored
                    // CR0 is only 32 bits effective; mask the FIXED MSR values to 32 bits
                    // to avoid polluting the upper half of CR0_GUEST_HOST_MASK.
                    let must0 = Msr::IA32_VMX_CR0_FIXED1.read() as u32 as u64
                        & !(Cr0Flags::NOT_WRITE_THROUGH | Cr0Flags::CACHE_DISABLE).bits();
                    let must1 = Msr::IA32_VMX_CR0_FIXED0.read() as u32 as u64
                        & !(Cr0Flags::PAGING | Cr0Flags::PROTECTED_MODE_ENABLE).bits();
                    // PG and PE are guest-owned (excluded from must0/must1),
                    // so (val & must0) | must1 would clear them. Preserve the
                    // guest's intent by OR-ing PG and PE back in.
                    let guest_owned = Cr0Flags::PAGING | Cr0Flags::PROTECTED_MODE_ENABLE;
                    let cr0_val = ((val & must0) | must1) | (val & guest_owned.bits());
                    if (val & Cr0Flags::PAGING.bits()) != 0 {
                        info!(
                            "[CR0] PG=1 in val={val:#x}, must0={must0:#x}, must1={must1:#x}, \
                             cr0_val={cr0_val:#x}"
                        );
                    }
                    VmcsGuestNW::CR0.write(cr0_val as _)?;
                    VmcsControlNW::CR0_READ_SHADOW.write(val as _)?;
                    // Compute CR0_GUEST_HOST_MASK in u32 to avoid upper-bit pollution.
                    // Bits that must be 1 (must1) or must be 0 (!must0) are host-owned.
                    let not_must0: u32 = !(must0 as u32);
                    let cr0_mask: u32 = must1 as u32 | not_must0;
                    VmcsControlNW::CR0_GUEST_HOST_MASK.write(cr0_mask as _)?;
                    // If PG is being set, check if we need to activate long mode
                    if (val & Cr0Flags::PAGING.bits()) != 0 {
                        let efer = VmcsGuest64::IA32_EFER.read().unwrap_or(0);
                        let lme = (efer >> 8) & 1;
                        if lme != 0 && (efer & MSR_IA32_EFER_LMA_BIT) == 0 {
                            let new_efer = efer | MSR_IA32_EFER_LMA_BIT;
                            VmcsGuest64::IA32_EFER.write(new_efer)?;
                            info!("[CR0] PG set with LME, activated LMA: EFER={new_efer:#x}");
                        }
                    }
                }
                // Bit 63 of CR3 is the NOFLUSH hint for MOV to CR3, not part of
                // the actual CR3 value. Intel SDM 26.3.1.1 requires bit 63 to be 0
                // when CR4.PCIDE=1. Linux KPTI sets bit 63 on context switches;
                // mask it off before storing in VMCS guest CR3.
                3 => VmcsGuestNW::CR3.write((val & !(1u64 << 63)) as _)?,
                4 => {
                    // Retrieve/validate restrictions on CR4
                    // CR4 is only 32 bits wide; mask the FIXED MSR values to 32 bits
                    // to avoid polluting the upper half of CR4_GUEST_HOST_MASK.
                    let must0 = Msr::IA32_VMX_CR4_FIXED1.read() as u32 as u64;
                    let must1 = Msr::IA32_VMX_CR4_FIXED0.read() as u32 as u64;
                    let val = val | Cr4Flags::VIRTUAL_MACHINE_EXTENSIONS.bits();
                    VmcsGuestNW::CR4.write(((val & must0) | must1) as _)?;
                    VmcsControlNW::CR4_READ_SHADOW.write(val as _)?;
                    // Keep the mask within 32 bits: !must0 must also be masked
                    // to avoid setting bits 63:32 of CR4_GUEST_HOST_MASK.
                    let not_must0: u32 = !(must0 as u32);
                    let mask: u32 = must1 as u32 | not_must0;
                    info!("[CR4m] m0={:#x} m1={:#x} mk={:#x}", must0, must1, mask);
                    VmcsControlNW::CR4_GUEST_HOST_MASK.write(mask as usize)?;
                }
                _ => unreachable!(),
            };
            Ok(())
        })()
        .expect("Failed to write guest control register")
    }

    #[allow(dead_code)]
    pub(super) fn cr(&self, cr_idx: usize) -> usize {
        (|| -> AxResult<usize> {
            Ok(match cr_idx {
                0 => VmcsGuestNW::CR0.read()?,
                3 => VmcsGuestNW::CR3.read()?,
                4 => {
                    let host_mask = VmcsControlNW::CR4_GUEST_HOST_MASK.read()?;
                    (VmcsControlNW::CR4_READ_SHADOW.read()? & host_mask)
                        | (VmcsGuestNW::CR4.read()? & !host_mask)
                }
                _ => unreachable!(),
            })
        })()
        .expect("Failed to read guest control register")
    }

    /// Read a 64-bit value from EDX:EAX.
    pub(super) fn read_edx_eax(&self) -> u64 {
        ((self.regs().rdx & 0xffff_ffff) << 32) | (self.regs().rax & 0xffff_ffff)
    }

    /// Write a 64-bit value to EDX:EAX.
    pub(super) fn write_edx_eax(&mut self, val: u64) {
        self.regs_mut().rax = val & 0xffff_ffff;
        self.regs_mut().rdx = val >> 32;
    }

    pub(super) fn handle_apic_base_msr(&mut self, write: bool) -> AxResult {
        const VMEXIT_INSTR_LEN_RDMSR_WRMSR: u8 = 2;
        self.advance_rip(VMEXIT_INSTR_LEN_RDMSR_WRMSR)?;

        const APIC_BASE_ADDR: u64 = 0xFEE0_0000;
        const APIC_GLOBAL_ENABLE: u64 = 1 << 11;
        const X2APIC_ENABLE: u64 = 1 << 10;
        const BSP_FLAG: u64 = 1 << 8;

        if write {
            let value = self.read_edx_eax();
            let new_base = value & 0xFFFF_F000;
            let x2apic = (value & X2APIC_ENABLE) != 0;
            let enabled = (value & APIC_GLOBAL_ENABLE) != 0;
            let bsp = (value & BSP_FLAG) != 0;
            info!(
                "[APIC-BASE] write: value={value:#x}, base={new_base:#x}, x2apic={x2apic}, \
                 enabled={enabled}, bsp={bsp}"
            );
            if new_base != APIC_BASE_ADDR {
                warn!(
                    "[APIC-BASE] guest tried to change APIC base to {new_base:#x}, forcing to \
                     {APIC_BASE_ADDR:#x}"
                );
            }
            // Actually update the vLAPIC's apic_base state so that
            // is_software_enabled / is_x2apic_enabled reflect guest writes.
            // The base address is forced to the default inside set_apic_base.
            self.vlapic.set_apic_base(value);
        } else {
            // Return the vLAPIC's actual apic_base state. On first read
            // (before any write) this is 0; OVMF expects to see the BSP +
            // xAPIC-enabled bits set, so synthesize them if the guest has
            // never written the MSR yet.
            let current = self.vlapic.apic_base();
            let value = if current == 0 {
                // Power-on default: APIC enabled, BSP selected, base=FEE0_0000.
                APIC_BASE_ADDR | APIC_GLOBAL_ENABLE | BSP_FLAG
            } else {
                current
            };
            debug!("[APIC-BASE] read: returning {value:#x}");
            self.write_edx_eax(value);
        }
        Ok(())
    }

    pub(super) fn handle_apic_msr_access(&mut self, write: bool, msr: u32) -> AxResult {
        const VMEXIT_INSTR_LEN_RDMSR_WRMSR: u8 = 2;

        self.advance_rip(VMEXIT_INSTR_LEN_RDMSR_WRMSR)?;

        let msr = msr as _;
        if write {
            let value = self.read_edx_eax() as usize;

            info!("[APIC-MSR] write: msr={msr:#x}, value={value:#x}");

            <EmulatedLocalApic as BaseDeviceOps<SysRegAddrRange>>::handle_write(
                &self.vlapic,
                SysRegAddr::new(msr),
                AccessWidth::Qword,
                value,
            )
        } else {
            let value = <EmulatedLocalApic as BaseDeviceOps<SysRegAddrRange>>::handle_read(
                &self.vlapic,
                SysRegAddr::new(msr),
                AccessWidth::Qword,
            )? as u64;

            info!("[APIC-MSR] read: msr={msr:#x}, value={value:#x}");

            self.write_edx_eax(value);
            Ok(())
        }
    }

    pub(super) fn handle_tsc_deadline_msr(&mut self, write: bool) -> AxResult {
        const VMEXIT_INSTR_LEN_RDMSR_WRMSR: u8 = 2;
        self.advance_rip(VMEXIT_INSTR_LEN_RDMSR_WRMSR)?;

        if write {
            let value = self.read_edx_eax();
            info!("[TSC-DEADLINE] write: value={value:#x}");
            if value != 0 {
                let current_tsc = unsafe { core::arch::x86_64::_rdtsc() };
                if value > current_tsc {
                    let delta = value - current_tsc;
                    let (_vm_id, _vcpu_id) = self.vlapic.timer_where_am_i();
                    let vector = self.vlapic.timer_vector();
                    let is_masked = self.vlapic.timer_is_masked();
                    let timer_val = self.vlapic.timer_read_lvt();

                    // Cancel any existing timer before starting a new one
                    let _ = self.vlapic.timer_stop();

                    info!(
                        "[TSC-DEADLINE] Setting deadline: current_tsc={current_tsc:#x}, \
                         deadline={value:#x}, delta={delta:#x}, vector={vector}, \
                         masked={is_masked}, lvt={timer_val:#x}"
                    );

                    self.vlapic.set_tsc_deadline(value);
                    self.vlapic.start_tsc_deadline_timer(value)?;
                } else {
                    debug!(
                        "[TSC-DEADLINE] deadline {value:#x} <= current_tsc {current_tsc:#x}, \
                         ignoring"
                    );
                }
            } else {
                debug!("[TSC-DEADLINE] write: value=0, stopping timer");
                let _ = self.vlapic.timer_stop();
            }
        } else {
            let value = self.vlapic.get_tsc_deadline();
            debug!("[TSC-DEADLINE] read: value={value:#x}");
            self.write_edx_eax(value);
        }

        Ok(())
    }

    pub(super) fn handle_apic_access(&mut self, exit_info: &super::VmxExitInfo) -> AxResult {
        let apic_info = self.apic_access_exit_info()?;

        let apic_msr = 0x800u32 + (apic_info.offset as u32 >> 4);

        match apic_info.access_type {
            ApicAccessExitType::LinearDataWrite => {
                let value = self.regs().rax as u32;
                debug!(
                    "[APIC-ACCESS] write: offset={:#x}, msr={:#x}, value={:#x}",
                    apic_info.offset, apic_msr, value
                );
                <EmulatedLocalApic as BaseDeviceOps<SysRegAddrRange>>::handle_write(
                    &self.vlapic,
                    SysRegAddr::new(apic_msr as _),
                    AccessWidth::Dword,
                    value as usize,
                )?;
            }
            ApicAccessExitType::LinearDataRead => {
                let value = <EmulatedLocalApic as BaseDeviceOps<SysRegAddrRange>>::handle_read(
                    &self.vlapic,
                    SysRegAddr::new(apic_msr as _),
                    AccessWidth::Dword,
                )? as u64;
                debug!(
                    "[APIC-ACCESS] read: offset={:#x}, msr={:#x}, value={:#x}",
                    apic_info.offset, apic_msr, value
                );
                self.regs_mut().rax = value;
            }
            ref other => {
                warn!(
                    "[APIC-ACCESS] Unsupported access type: {:?}, offset={:#x}",
                    other, apic_info.offset
                );
                // Still advance RIP to avoid infinite loop
            }
        }

        self.advance_rip(exit_info.exit_instruction_length as _)?;

        Ok(())
    }

    #[allow(clippy::single_match)]
    pub(super) fn handle_cr(&mut self) -> AxResult {
        let instr_len = VmcsReadOnly32::VMEXIT_INSTRUCTION_LEN.read().unwrap_or(3) as u8;
        let instr_len = if instr_len == 0 { 3 } else { instr_len };

        let cr_access_info = vmcs::cr_access_info()?;

        let reg = cr_access_info.gpr;
        let cr = cr_access_info.cr_number;

        match cr_access_info.access_type {
            // move to cr
            0 => {
                let val = if reg == 4 {
                    self.stack_pointer() as u64
                } else {
                    self.guest_regs.get_reg_of_index(reg)
                };
                if cr == 0 || cr == 4 || cr == 3 {
                    let rip_before = self.rip();
                    // CR0/CR4 writes are significant (mode switches); log at info.
                    // CR3 writes happen on every context switch and flood the log;
                    // commented out to keep the log clean.
                    if cr == 3 {
                        // debug!(
                        //     "[CR{}] write val={:#x}, RIP before={:#x}, instr_len={}",
                        //     cr, val, rip_before, instr_len
                        // );
                    } else {
                        info!(
                            "[CR{}] write val={:#x}, RIP before={:#x}, instr_len={}",
                            cr, val, rip_before, instr_len
                        );
                    }
                    self.advance_rip(instr_len)?;
                    let rip_after = self.rip();
                    // TODO: check for #GP reasons
                    self.set_cr(cr as usize, val);
                    let cr0_host_mask = VmcsControlNW::CR0_GUEST_HOST_MASK.read().unwrap_or(0);
                    let cr4_host_mask = VmcsControlNW::CR4_GUEST_HOST_MASK.read().unwrap_or(0);
                    if cr != 3 {
                        info!(
                            "[CR{}] after advance RIP={:#x}, CR0_HOST_MASK={:#x}, \
                             CR4_HOST_MASK={:#x}",
                            cr, rip_after, cr0_host_mask, cr4_host_mask
                        );
                    }

                    if cr == 0 {
                        let cr0_flags = Cr0Flags::from_bits_truncate(val);
                        if cr0_flags.contains(Cr0Flags::PROTECTED_MODE_ENABLE) {
                            let gdtr_base = VmcsGuestNW::GDTR_BASE.read().unwrap_or(0);
                            let gdtr_limit = VmcsGuest32::GDTR_LIMIT.read().unwrap_or(0);
                            let cs_ar = VmcsGuest32::CS_ACCESS_RIGHTS.read().unwrap_or(0);
                            let cs_base = VmcsGuestNW::CS_BASE.read().unwrap_or(0);
                            let cs_sel = VmcsGuest16::CS_SELECTOR.read().unwrap_or(0);
                            info!(
                                "[CR0PE] v={:#x} GDTR={:#x}:{:#x} CS={:#x}:{:#x} ar={:#x}",
                                val, gdtr_base, gdtr_limit, cs_sel, cs_base, cs_ar
                            );
                        }
                        if cr0_flags.contains(Cr0Flags::PAGING) {
                            vmcs::update_efer()?;
                        }
                    }
                    if cr == 4 {
                        // Log CS state for debugging mode transitions.
                        // Do NOT modify CS access rights here: unconditionally
                        // setting D/B (bit 14) breaks long mode where L=1 requires
                        // D/B=0 (Intel SDM 26.3.1.2), causing VM-entry failure 0x21.
                        let cs_base = VmcsGuestNW::CS_BASE.read().unwrap_or(0);
                        let cs_ar = VmcsGuest32::CS_ACCESS_RIGHTS.read().unwrap_or(0);
                        let cs_sel = VmcsGuest16::CS_SELECTOR.read().unwrap_or(0);
                        info!(
                            "[CR4s] v={:#x} RIP={:#x} CS={:#x}:{:#x} ar={:#x}",
                            val,
                            self.rip(),
                            cs_sel,
                            cs_base,
                            cs_ar
                        );
                    }
                    return Ok(());
                }
            }
            // move from cr
            1 => {
                let val = self.cr(cr as usize) as u64;
                self.guest_regs.set_reg_of_index(reg, val);
                self.advance_rip(instr_len)?;
                return Ok(());
            }
            _ => {}
        };

        panic!(
            "Guest's access to cr not allowed: {:#x?}, {:#x?}",
            self, cr_access_info
        );
    }
}
