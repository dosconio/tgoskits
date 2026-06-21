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

//! Built-in VM-exit dispatch and small per-reason handlers (NMI window,
//! VMX-preemption timer, HLT, XSETBV). Extracted from `vcpu.rs`.

use ax_errno::{AxResult, ax_err};
use axaddrspace::MappingFlags;
use bit_field::BitField;
use x86::controlregs::Xcr0;
use x86_vioapic::{IOAPIC_MMIO_BASE, IOAPIC_MMIO_SIZE};

use super::{
    VmxExitInfo,
    definitions::VmxExitReason,
    vcpu::{
        ECAM_MMIO_BASE, ECAM_MMIO_END, MSR_IA32_EFER_LMA_BIT, VMX_PREEMPTION_TIMER_SET_VALUE,
        VmxVcpu,
    },
    vmcs::{self, VmcsGuest16, VmcsGuest32, VmcsGuest64, VmcsGuestNW},
};

impl VmxVcpu {
    /// Handle vm-exits than can and should be handled by [`VmxVcpu`] itself.
    ///
    /// Return the result or None if the vm-exit was not handled.
    pub(super) fn builtin_vmexit_handler(&mut self, exit_info: &VmxExitInfo) -> Option<AxResult> {
        const X2APIC_MSR_BASE: u32 = 0x800;
        const X2APIC_MSR_END: u32 = 0x8ff; // SDM says 0x8ff, but actually 0x83f, we respect the SDM here.
        // Following vm-exits are handled here:
        // - interrupt window: turn off interrupt window;
        // - xsetbv: set guest xcr;
        // - cr access: just panic;
        match exit_info.exit_reason {
            VmxExitReason::INTERRUPT_WINDOW => {
                debug!(
                    "[INTR-WINDOW] Interrupt window VM-exit fired, RIP={:#x}",
                    self.rip()
                );
                // The interrupt window is now open (RFLAGS.IF=1, no blocking).
                // Try to inject any pending events before disabling the window.
                // inject_pending_events will re-enable the window if there are still
                // events that can't be injected.
                if let Err(e) = self.set_interrupt_window(false) {
                    warn!("[INTR-WINDOW] failed to disable interrupt window: {e:?}");
                }
                if let Err(e) = self.inject_pending_events() {
                    warn!("[INTR-WINDOW] failed to inject pending events: {e:?}");
                }
                // INTERRUPT_WINDOW is fully handled here; return Some(Ok(()))
                // so that inner_run() resumes the guest without propagating
                // the exit to the outer run() handler (which would treat it
                // as an unsupported VM-Exit and halt the vCPU).
                Some(Ok(()))
            }
            VmxExitReason::NMI_WINDOW => Some(self.handle_nmi_window()),
            VmxExitReason::PREEMPTION_TIMER => Some(self.handle_vmx_preemption_timer()),
            VmxExitReason::XSETBV => Some(self.handle_xsetbv()),
            VmxExitReason::CR_ACCESS => Some(self.handle_cr()),
            VmxExitReason::CPUID => Some(self.handle_cpuid()),
            msr_rw @ (VmxExitReason::MSR_READ | VmxExitReason::MSR_WRITE)
                if {
                    let msr = self.regs().rcx as u32;
                    msr == 0x1B // IA32_APIC_BASE
                } =>
            {
                Some(self.handle_apic_base_msr(msr_rw == VmxExitReason::MSR_WRITE))
            }
            msr_rw @ (VmxExitReason::MSR_READ | VmxExitReason::MSR_WRITE)
                if {
                    let msr = self.regs().rcx as u32;
                    (X2APIC_MSR_BASE..=X2APIC_MSR_END).contains(&msr)
                } =>
            {
                Some(self.handle_apic_msr_access(
                    msr_rw == VmxExitReason::MSR_WRITE,
                    self.regs().rcx as u32,
                ))
            }
            msr_rw @ (VmxExitReason::MSR_READ | VmxExitReason::MSR_WRITE)
                if {
                    let msr = self.regs().rcx as u32;
                    msr == 0x6E0 // IA32_TSC_DEADLINE
                } =>
            {
                Some(self.handle_tsc_deadline_msr(msr_rw == VmxExitReason::MSR_WRITE))
            }
            msr_rw @ (VmxExitReason::MSR_READ | VmxExitReason::MSR_WRITE)
                if {
                    let msr = self.regs().rcx as u32;
                    (0xC0000080..=0xC0000084).contains(&msr) // EFER, STAR, LSTAR, CSTAR, FMASK
                } =>
            {
                let is_write = msr_rw == VmxExitReason::MSR_WRITE;
                let msr = self.regs().rcx as u32;
                if is_write {
                    let value = self.read_edx_eax();
                    trace!(
                        "[EFER/SYS] write MSR {msr:#x} = {value:#x}, RIP={:#x}",
                        self.rip()
                    );
                    if msr == 0xC0000080 {
                        // Write to GUEST_EFER in VMCS, not physical MSR
                        if let Err(e) = VmcsGuest64::IA32_EFER.write(value) {
                            warn!("[EFER] failed to write GUEST_EFER: {e:?}");
                        }
                        // If LME is being set and CR0.PG is already set, set LMA too
                        let cr0 = VmcsGuestNW::CR0.read().unwrap_or(0);
                        let lme = (value >> 8) & 1;
                        let pg = (cr0 >> 31) & 1;
                        if lme != 0 && pg != 0 {
                            let new_efer = value | MSR_IA32_EFER_LMA_BIT;
                            if let Err(e) = VmcsGuest64::IA32_EFER.write(new_efer) {
                                warn!("[EFER] failed to write GUEST_EFER with LMA: {e:?}");
                            }
                            trace!("[EFER] LME+PG set, LMA activated: EFER={new_efer:#x}");
                        }
                        trace!("[EFER] After EFER write: CR0={cr0:#x}, PG={pg}, LME={lme}");
                    } else {
                        // For STAR/LSTAR/CSTAR/FMASK, pass through to hardware
                        unsafe {
                            match msr {
                                0xC0000081 => x86::msr::wrmsr(x86::msr::IA32_STAR, value),
                                0xC0000082 => x86::msr::wrmsr(x86::msr::IA32_LSTAR, value),
                                0xC0000083 => x86::msr::wrmsr(x86::msr::IA32_CSTAR, value),
                                0xC0000084 => x86::msr::wrmsr(x86::msr::IA32_FMASK, value),
                                _ => {}
                            }
                        }
                    }
                } else {
                    if msr == 0xC0000080 {
                        let value = VmcsGuest64::IA32_EFER.read().unwrap_or(0);
                        debug!(
                            "[EFER/SYS] read MSR {msr:#x} = {value:#x}, RIP={:#x}",
                            self.rip()
                        );
                        self.write_edx_eax(value);
                    } else {
                        let value = unsafe {
                            match msr {
                                0xC0000081 => x86::msr::rdmsr(x86::msr::IA32_STAR),
                                0xC0000082 => x86::msr::rdmsr(x86::msr::IA32_LSTAR),
                                0xC0000083 => x86::msr::rdmsr(x86::msr::IA32_CSTAR),
                                0xC0000084 => x86::msr::rdmsr(x86::msr::IA32_FMASK),
                                _ => 0,
                            }
                        };
                        info!(
                            "[EFER/SYS] read MSR {msr:#x} = {value:#x}, RIP={:#x}",
                            self.rip()
                        );
                        self.write_edx_eax(value);
                    }
                }
                self.advance_rip(2).ok()?;
                Some(Ok(()))
            }
            VmxExitReason::APIC_ACCESS => Some(self.handle_apic_access(exit_info)),
            VmxExitReason::EPT_VIOLATION => {
                if let Ok(info) = self.nested_page_fault_info() {
                    let gpa = info.fault_guest_paddr.as_usize();
                    let flags = info.access_flags;
                    static EPT_VIOL_COUNT: core::sync::atomic::AtomicU64 =
                        core::sync::atomic::AtomicU64::new(0);
                    let vc = EPT_VIOL_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    if vc < 20 || vc == 100 || vc == 1000 {
                        let _cr0 = VmcsGuestNW::CR0.read().unwrap_or(0);
                        let _cr4 = VmcsGuestNW::CR4.read().unwrap_or(0);
                        let _efer = VmcsGuest64::IA32_EFER.read().unwrap_or(0);
                        let _cs_sel = VmcsGuest16::CS_SELECTOR.read().unwrap_or(0);
                        let _cs_base = VmcsGuestNW::CS_BASE.read().unwrap_or(0);
                        let rip = self.rip();
                        info!(
                            "[EPT-VIOL] #{vc}: GPA={gpa:#x}, RIP={rip:#x}, flags={:?}",
                            flags
                        );
                        if vc < 5 {
                            let regs = self.regs();
                            info!(
                                "[EPT-VIOL] #{vc}: RAX={:#x} RBX={:#x} RCX={:#x} RDX={:#x}",
                                regs.rax, regs.rbx, regs.rcx, regs.rdx
                            );
                            if let Some((bytes, actual_len)) = self.read_guest_instr_bytes(8) {
                                info!(
                                    "[EPT-VIOL] #{vc}: instr bytes={:02x?}",
                                    &bytes[..actual_len.min(8)]
                                );
                            }
                        }
                    }

                    // Memory probing beyond ram_end must be handled before PCI MMIO
                    // because addresses between ram_end and PCI MMIO window start
                    // are RAM holes that OVMF probes for memory detection.
                    // PCI MMIO window (0xE0000000..0xFEC00000) matches QEMU Q35.
                    // APIC MMIO (0xFEE00000), IOAPIC MMIO, and PCI MMIO
                    // must be handled by their respective handlers instead.
                    // ECAM MMIO (0xB0000000..0xC0000000) is handled by the main
                    // EPT violation handler which returns MmioRead/MmioWrite so the
                    // VMM can dispatch to the PCI host bridge's ECAM interface.
                    let is_apic_mmio = (0xFEE0_0000..0xFEE0_1000).contains(&gpa);
                    let is_ioapic_mmio = gpa >= IOAPIC_MMIO_BASE as usize
                        && gpa < (IOAPIC_MMIO_BASE + IOAPIC_MMIO_SIZE) as usize;
                    let is_pci_mmio = (0xE000_0000..0xFEC0_0000).contains(&gpa);
                    let is_ecam_mmio = (ECAM_MMIO_BASE..ECAM_MMIO_END).contains(&gpa);
                    if self.ram_end > 0
                        && gpa >= self.ram_end
                        && !flags.contains(MappingFlags::EXECUTE)
                        && !is_apic_mmio
                        && !is_ioapic_mmio
                        && !is_pci_mmio
                        && !is_ecam_mmio
                    {
                        Some(self.handle_memory_probing_ept_violation(gpa, &info))
                    } else if is_apic_mmio {
                        Some(self.handle_apic_mmio_ept_violation())
                    } else if is_pci_mmio {
                        Some(self.handle_pci_mmio_ept_violation())
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Handle NMI window VM-exit.
    /// KVM nested virt sets NMI_WINDOW_EXITING when it has a pending virtual NMI
    /// to inject and is waiting for the NMI window to open. When this exit fires,
    /// the NMI window IS open, so KVM should deliver the NMI on the next VM-entry.
    /// We simply resume execution — do NOT try to clear NMI_WINDOW_EXITING or
    /// inject an NMI ourselves, as that conflicts with KVM's own NMI injection
    /// and causes an infinite loop.
    pub(super) fn handle_nmi_window(&mut self) -> AxResult {
        // No action needed. KVM will inject the pending NMI on the next VM-entry.
        Ok(())
    }

    pub(super) fn handle_vmx_preemption_timer(&mut self) -> AxResult {
        // The VMX-preemption timer counts down at rate proportional to that of the timestamp counter (TSC).
        // Specifically, the timer counts down by 1 every time bit X in the TSC changes due to a TSC increment.
        // The value of X is in the range 0–31 and can be determined by consulting the VMX capability MSR IA32_VMX_MISC (see Appendix A.6).
        VmcsGuest32::VMX_PREEMPTION_TIMER_VALUE.write(VMX_PREEMPTION_TIMER_SET_VALUE)?;

        // Debug: dump guest instruction at current RIP to diagnose loops
        static PREEMPTION_COUNT: core::sync::atomic::AtomicU32 =
            core::sync::atomic::AtomicU32::new(0);
        let count = PREEMPTION_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if count < 5 || count.is_multiple_of(100) {
            let rip = self.rip();
            let rflags = VmcsGuestNW::RFLAGS.read().unwrap_or(0);
            if let Some((bytes, actual_len)) = self.read_guest_instr_bytes(8) {
                info!(
                    "[PREEMPT] #{count}: RIP={rip:#x} RFLAGS={rflags:#x} bytes={:02x?}",
                    &bytes[..actual_len.min(8)]
                );
                // If this is a short jump (eb XX), dump bytes at the jump target
                if bytes.len() >= 2 && bytes[0] == 0xeb {
                    let offset = bytes[1] as i8 as i64;
                    let target = rip as i64 + 2 + offset;
                    if target > 0
                        && let Some(ept_root) = self.ept_root
                    {
                        let mut dump = [0u8; 48];
                        for (i, byte) in dump.iter_mut().enumerate() {
                            let gpa = target as u64 + i as u64;
                            if let Some(hpa) = self.gpa_to_hpa_via_ept(ept_root, gpa) {
                                const PHYS_VIRT_OFFSET: u64 = 0xffff_8000_0000_0000;
                                *byte = unsafe {
                                    core::ptr::read_volatile(
                                        (hpa as u64 + PHYS_VIRT_OFFSET) as *const u8,
                                    )
                                };
                            }
                        }
                        info!(
                            "[PREEMPT] #{count}: jump target {target:#x} bytes={:02x?}",
                            &dump[..48]
                        );
                    }
                }
            } else {
                info!("[PREEMPT] #{count}: RIP={rip:#x} RFLAGS={rflags:#x} (failed to read instr)");
            }
        }

        Ok(())
    }

    pub(super) fn handle_hlt(&mut self) -> AxResult {
        // HLT instruction: guest is waiting for an interrupt.
        // Advance RIP past HLT so the guest doesn't re-execute it.
        // The caller (vcpu_run loop) is responsible for yielding or
        // waiting for an interrupt before re-entering the VM.
        const VM_EXIT_INSTR_LEN_HLT: u8 = 1;
        self.advance_rip(VM_EXIT_INSTR_LEN_HLT)?;
        Ok(())
    }

    pub(super) fn handle_xsetbv(&mut self) -> AxResult {
        const XCR_XCR0: u64 = 0;
        const VM_EXIT_INSTR_LEN_XSETBV: u8 = 3;
        // #GP vector and error code for invalid XSETBV (Intel SDM Vol. 2A, XSETBV)
        const GP_VECTOR: u8 = 13;
        const GP_ERR_CODE: u32 = 0;

        let index = self.guest_regs.rcx.get_bits(0..32);
        let value = self.guest_regs.rdx.get_bits(0..32) << 32 | self.guest_regs.rax.get_bits(0..32);

        // TODO: get host-supported xcr0 mask by cpuid and reject any guest-xsetbv violating that
        if index == XCR_XCR0 {
            let validated = Xcr0::from_bits(value).and_then(|x| {
                if !x.contains(Xcr0::XCR0_FPU_MMX_STATE) {
                    return None;
                }

                if x.contains(Xcr0::XCR0_AVX_STATE) && !x.contains(Xcr0::XCR0_SSE_STATE) {
                    return None;
                }

                if x.contains(Xcr0::XCR0_BNDCSR_STATE) ^ x.contains(Xcr0::XCR0_BNDREG_STATE) {
                    return None;
                }

                // AVX-512 dependency: if any AVX-512 component is set,
                // all must be set AND AVX must be set.
                let has_any_avx512 = x.contains(Xcr0::XCR0_OPMASK_STATE)
                    || x.contains(Xcr0::XCR0_ZMM_HI256_STATE)
                    || x.contains(Xcr0::XCR0_HI16_ZMM_STATE);
                let has_all_avx512 = x.contains(Xcr0::XCR0_OPMASK_STATE)
                    && x.contains(Xcr0::XCR0_ZMM_HI256_STATE)
                    && x.contains(Xcr0::XCR0_HI16_ZMM_STATE);
                if has_any_avx512 && (!has_all_avx512 || !x.contains(Xcr0::XCR0_AVX_STATE)) {
                    return None;
                }

                Some(x)
            });

            match validated {
                Some(x) => {
                    self.xstate.guest_xcr0 = x.bits();
                    self.advance_rip(VM_EXIT_INSTR_LEN_XSETBV)
                }
                None => {
                    // Invalid XCR0 value: inject #GP(0) per Intel SDM.
                    // Do NOT advance RIP so the guest can handle the fault.
                    info!(
                        "[XSETBV] invalid XCR0={:#x}, injecting #GP at RIP={:#x}",
                        value,
                        self.rip()
                    );
                    vmcs::inject_event(GP_VECTOR, Some(GP_ERR_CODE))?;
                    Ok(())
                }
            }
        } else {
            // xcr0 only
            ax_err!(Unsupported, "only xcr0 is supported")
        }
    }
}
