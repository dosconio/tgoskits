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

//! VMCS control-domain setup, host/guest state initialization, and CR0/CR4/EFER
//! fixups. Extracted from `vcpu.rs` to keep the core VCPU definition focused on
//! lifecycle (`new`/`run`/`reset`).

use ax_errno::AxResult;
use axaddrspace::{GuestPhysAddr, HostPhysAddr};
use bit_field::BitField;
use raw_cpuid::CpuId;
use x86::{
    bits64::vmx,
    dtables::{self, DescriptorTablePointer},
    segmentation::SegmentSelector,
};
use x86_64::registers::control::{Cr0, Cr0Flags, Cr3, Cr4, Cr4Flags, EferFlags};

use super::{
    as_axerr,
    vcpu::{MSR_IA32_EFER_LMA_BIT, VMX_PREEMPTION_TIMER_SET_VALUE, VmxVcpu},
    vmcs::{
        self, VmcsControl16, VmcsControl32, VmcsControl64, VmcsControlNW, VmcsGuest16, VmcsGuest32,
        VmcsGuest64, VmcsGuestNW, VmcsHost16, VmcsHost32, VmcsHost64, VmcsHostNW,
    },
};
use crate::{boot_mode::X86BootMode, msr::Msr};

/// Compute the base address of the TSS descriptor referenced by `tr` in the
/// given GDT.
fn get_tr_base(tr: SegmentSelector, gdt: &DescriptorTablePointer<u64>) -> u64 {
    let index = tr.index() as usize;
    let table_len = (gdt.limit as usize + 1) / core::mem::size_of::<u64>();
    let table = unsafe { core::slice::from_raw_parts(gdt.base, table_len) };
    let entry = table[index];
    if entry & (1 << 47) != 0 {
        // present
        let base_low = entry.get_bits(16..40) | entry.get_bits(56..64) << 24;
        let base_high = table[index + 1] & 0xffff_ffff;
        base_low | base_high << 32
    } else {
        // no present
        0
    }
}

impl VmxVcpu {
    pub(super) fn setup_io_bitmap(&mut self) -> AxResult {
        // By default, I/O bitmap is set as `intercept_all`.
        // Todo: these should be combined with emulated pio device management,
        // in `modules/axvm/src/device/x86_64/mod.rs` somehow.
        let io_to_be_intercepted = super::vcpu::QEMU_EXIT_PORT..super::vcpu::QEMU_EXIT_PORT + 1; // QEMU exit port
        self.io_bitmap.set_intercept_of_range(
            io_to_be_intercepted.start as _,
            io_to_be_intercepted.count() as u32,
            true,
        );
        Ok(())
    }

    #[allow(dead_code)]
    pub(super) fn setup_msr_bitmap(&mut self) -> AxResult {
        // Intercept IA32_APIC_BASE MSR accesses
        // let msr = x86::msr::IA32_APIC_BASE;
        // self.msr_bitmap.set_read_intercept(msr, true);
        // self.msr_bitmap.set_write_intercept(msr, true);

        const IA32_APIC_BASE: u32 = 0x1B;
        self.msr_bitmap.set_read_intercept(IA32_APIC_BASE, true);
        self.msr_bitmap.set_write_intercept(IA32_APIC_BASE, true);

        const IA32_UMWAIT_CONTROL: u32 = 0xe1;
        self.msr_bitmap
            .set_write_intercept(IA32_UMWAIT_CONTROL, true);
        self.msr_bitmap
            .set_read_intercept(IA32_UMWAIT_CONTROL, true);

        // Intercept all x2APIC MSR accesses
        for msr in 0x800..=0x83f {
            self.msr_bitmap.set_read_intercept(msr, true);
            self.msr_bitmap.set_write_intercept(msr, true);
        }

        // Intercept IA32_TSC_DEADLINE MSR (0x6E0) for TSC-Deadline timer mode
        const IA32_TSC_DEADLINE: u32 = 0x6E0;
        self.msr_bitmap.set_read_intercept(IA32_TSC_DEADLINE, true);
        self.msr_bitmap.set_write_intercept(IA32_TSC_DEADLINE, true);

        // Intercept IA32_EFER MSR (0xC0000080) to track long mode transitions
        const IA32_EFER: u32 = 0xC0000080;
        self.msr_bitmap.set_read_intercept(IA32_EFER, true);
        self.msr_bitmap.set_write_intercept(IA32_EFER, true);

        // Intercept IA32_STAR/IA32_LSTAR/IA32_CSTAR/IA32_FMASK for syscall tracking
        const IA32_STAR: u32 = 0xC0000081;
        const IA32_LSTAR: u32 = 0xC0000082;
        const IA32_CSTAR: u32 = 0xC0000083;
        const IA32_FMASK: u32 = 0xC0000084;
        for msr in [IA32_STAR, IA32_LSTAR, IA32_CSTAR, IA32_FMASK] {
            self.msr_bitmap.set_read_intercept(msr, true);
            self.msr_bitmap.set_write_intercept(msr, true);
        }

        Ok(())
    }

    pub(super) fn setup_vmcs(
        &mut self,
        entry: GuestPhysAddr,
        ept_root: HostPhysAddr,
        boot_mode: X86BootMode,
    ) -> AxResult {
        let paddr = self.vmcs.phys_addr().as_usize() as u64;
        unsafe {
            vmx::vmclear(paddr).map_err(as_axerr)?;
        }
        self.bind_to_current_processor()?;
        self.setup_msr_bitmap()?;
        // Verify EFER MSR bitmap intercept is correctly set
        // Use the set_read_intercept/set_write_intercept API to verify by re-reading the bitmap
        {
            // Just log the physical address for now; the bitmap is set up correctly
            // since set_read_intercept/set_write_intercept were called
            let pa = self.msr_bitmap.phys_addr();
            info!("[MSR-BITMAP] EFER bitmap at phys_addr={:#x}", pa);
        }
        self.setup_vmcs_guest(entry, boot_mode)?;
        self.setup_vmcs_control(ept_root, true)?;
        self.fixup_ia32e_guest_cr_and_efer()?;
        // Update CR0/CR4 read shadows to match the fixed guest state after IA32E fixup.
        // If PE/PG are masked in CR0_GUEST_HOST_MASK, the shadow must match guest CR0.
        VmcsControlNW::CR0_READ_SHADOW.write(VmcsGuestNW::CR0.read()?)?;
        VmcsControlNW::CR4_READ_SHADOW.write(VmcsGuestNW::CR4.read()?)?;
        info!(
            "[VMX setup] Post-fixup shadows: CR0_SHADOW={:#x} CR4_SHADOW={:#x}",
            VmcsControlNW::CR0_READ_SHADOW.read().unwrap_or(0),
            VmcsControlNW::CR4_READ_SHADOW.read().unwrap_or(0),
        );
        self.unbind_from_current_processor()?;
        Ok(())
    }

    pub(super) fn setup_vmcs_host(&self) -> AxResult {
        VmcsHost64::IA32_PAT.write(Msr::IA32_PAT.read())?;
        VmcsHost64::IA32_EFER.write(Msr::IA32_EFER.read())?;
        VmcsHost64::IA32_PERF_GLOBAL_CTRL.write(0)?;

        VmcsHostNW::CR0.write(Cr0::read_raw() as _)?;
        VmcsHostNW::CR3.write(Cr3::read_raw().0.start_address().as_u64() as _)?;
        VmcsHostNW::CR4.write(Cr4::read_raw() as _)?;

        VmcsHost16::ES_SELECTOR.write(x86::segmentation::es().bits())?;
        VmcsHost16::CS_SELECTOR.write(x86::segmentation::cs().bits())?;
        VmcsHost16::SS_SELECTOR.write(x86::segmentation::ss().bits())?;
        VmcsHost16::DS_SELECTOR.write(x86::segmentation::ds().bits())?;
        VmcsHost16::FS_SELECTOR.write(x86::segmentation::fs().bits())?;
        VmcsHost16::GS_SELECTOR.write(x86::segmentation::gs().bits())?;
        VmcsHostNW::FS_BASE.write(Msr::IA32_FS_BASE.read() as _)?;
        VmcsHostNW::GS_BASE.write(Msr::IA32_GS_BASE.read() as _)?;

        let tr = unsafe { x86::task::tr() };
        let mut gdtp = DescriptorTablePointer::<u64>::default();
        let mut idtp = DescriptorTablePointer::<u64>::default();
        unsafe {
            dtables::sgdt(&mut gdtp);
            dtables::sidt(&mut idtp);
        }
        VmcsHost16::TR_SELECTOR.write(tr.bits())?;
        VmcsHostNW::TR_BASE.write(get_tr_base(tr, &gdtp) as _)?;
        VmcsHostNW::GDTR_BASE.write(gdtp.base as _)?;
        VmcsHostNW::IDTR_BASE.write(idtp.base as _)?;
        VmcsHostNW::RIP.write(Self::vmx_exit as *const () as usize)?;

        VmcsHostNW::IA32_SYSENTER_ESP.write(0)?;
        VmcsHostNW::IA32_SYSENTER_EIP.write(0)?;
        VmcsHost32::IA32_SYSENTER_CS.write(0)?;

        Ok(())
    }

    fn setup_vmcs_guest(&mut self, entry: GuestPhysAddr, boot_mode: X86BootMode) -> AxResult {
        let cr0_val: Cr0Flags =
            Cr0Flags::NOT_WRITE_THROUGH | Cr0Flags::CACHE_DISABLE | Cr0Flags::EXTENSION_TYPE;
        self.set_cr(0, cr0_val.bits());
        self.set_cr(4, 0);

        macro_rules! set_guest_segment {
            ($seg:ident, $access_rights:expr) => {{
                use VmcsGuest16::*;
                use VmcsGuest32::*;
                use VmcsGuestNW::*;
                paste::paste! {
                    [<$seg _SELECTOR>].write(0)?;
                    [<$seg _BASE>].write(0)?;
                    [<$seg _LIMIT>].write(0xffff)?;
                    [<$seg _ACCESS_RIGHTS>].write($access_rights)?;
                }
            }};
        }

        // Both trampoline and UEFI modes start in real mode.
        // UEFI mode starts at the x86 reset vector (0xFFFFFFF0);
        // OVMF firmware handles the mode transitions itself.
        set_guest_segment!(ES, 0x93); // 16-bit, present, data, read/write, accessed
        set_guest_segment!(CS, 0x9b); // 16-bit, present, code, exec/read, accessed
        set_guest_segment!(SS, 0x93);
        set_guest_segment!(DS, 0x93);
        set_guest_segment!(FS, 0x93);
        set_guest_segment!(GS, 0x93);
        set_guest_segment!(TR, 0x8b); // busy 32-bit TSS, present, usable
        set_guest_segment!(LDTR, 0x8082); // unusable (bit15=Unusable per SDM), LDT type

        // In UEFI mode, the OVMF firmware starts from the reset vector (0xFFFFFFF0).
        // The guest starts in real mode with CS=F000:FFF0 pointing to the reset vector.
        // OVMF will handle the mode transitions (real→protected→long) itself.
        info!("[VMX setup] boot_mode={:?}, setting CS for UEFI", boot_mode);
        if boot_mode == X86BootMode::Uefi {
            VmcsGuestNW::CS_BASE.write(0xFFFF0000)?;
            VmcsGuest16::CS_SELECTOR.write(0xF000)?;
            VmcsGuest32::CS_ACCESS_RIGHTS.write(0x9b)?;
            info!("[VMX setup] Set CS_SELECTOR=0xF000, CS_BASE=0xFFFF0000, CS_AR=0x9b");
        }

        VmcsGuestNW::GDTR_BASE.write(0)?;
        VmcsGuest32::GDTR_LIMIT.write(0xffff)?;
        VmcsGuestNW::IDTR_BASE.write(0)?;
        VmcsGuest32::IDTR_LIMIT.write(0xffff)?;

        VmcsGuestNW::CR3.write(0)?;
        VmcsGuestNW::DR7.write(0x400)?;
        VmcsGuestNW::RSP.write(0)?;

        {
            let cr0_fixed0 = Msr::IA32_VMX_CR0_FIXED0.read();
            let cr0_fixed1 = Msr::IA32_VMX_CR0_FIXED1.read();
            let cr0_must0 =
                cr0_fixed1 & !(Cr0Flags::NOT_WRITE_THROUGH | Cr0Flags::CACHE_DISABLE).bits();
            // When UNRESTRICTED_GUEST is set, CR0.PE and CR0.PG may be 0
            // regardless of CR0_FIXED0 (Intel SDM Vol 3, Section 26.3.1.1).
            // For UEFI boot, we want the guest to start in real mode (PE=0, PG=0)
            // so OVMF can handle mode transitions itself.
            // NOTE: We read the MSR directly instead of VMCS SEC_CTRL because
            // setup_vmcs_control() hasn't been called yet, so the VMCS field is 0.
            let sec2_cap = Msr::IA32_VMX_PROCBASED_CTLS2.read();
            // allowed1 is in the high 32 bits; bit 7 = UNRESTRICTED_GUEST
            let unrestricted_guest = ((sec2_cap >> 32) >> 7) & 1 != 0;
            let cr0_must1 = if unrestricted_guest {
                // UNRESTRICTED_GUEST is set: PE and PG can be 0
                cr0_fixed0 as usize
                    & !(Cr0Flags::PAGING
                        | Cr0Flags::PROTECTED_MODE_ENABLE
                        | Cr0Flags::NOT_WRITE_THROUGH
                        | Cr0Flags::CACHE_DISABLE)
                        .bits() as usize
            } else if (cr0_fixed0 & (Cr0Flags::PROTECTED_MODE_ENABLE | Cr0Flags::PAGING).bits())
                == (Cr0Flags::PROTECTED_MODE_ENABLE | Cr0Flags::PAGING).bits()
            {
                // CR0_FIXED0 forces PE+PG and no UNRESTRICTED_GUEST: include them
                cr0_fixed0 as usize
                    & !(Cr0Flags::NOT_WRITE_THROUGH | Cr0Flags::CACHE_DISABLE).bits() as usize
            } else {
                cr0_fixed0 as usize
                    & !(Cr0Flags::PAGING | Cr0Flags::PROTECTED_MODE_ENABLE).bits() as usize
            };
            VmcsGuestNW::CR0.write(cr0_must1)?;
            info!(
                "[VMX setup] GUEST_CR0={:#x} (must0={:#x}, must1={:#x}, fixed0={:#x}, \
                 unrestricted_guest={})",
                cr0_must1, cr0_must0, cr0_must1, cr0_fixed0, unrestricted_guest
            );
        }

        {
            let cr4_must0 = Msr::IA32_VMX_CR4_FIXED1.read();
            let cr4_must1 = Msr::IA32_VMX_CR4_FIXED0.read();
            let mut guest_cr4 = cr4_must1;
            guest_cr4 |= Cr4Flags::PHYSICAL_ADDRESS_EXTENSION.bits();
            VmcsGuestNW::CR4.write(guest_cr4 as usize)?;
            info!(
                "[VMX setup] GUEST_CR4={:#x} (must0={:#x}, must1={:#x})",
                guest_cr4, cr4_must0, cr4_must1
            );
        }
        // In UEFI mode, the guest starts in real mode at the reset vector.
        // In VMX, RIP stores the offset within the code segment; the linear
        // address is CS.base + RIP.  The x86 reset state has CS.base=0xFFFF0000
        // and IP=0xFFF0, so linear address = 0xFFFF0000 + 0xFFF0 = 0xFFFFFFF0.
        let rip_val = if boot_mode == X86BootMode::Uefi {
            0xFFF0usize
        } else {
            entry.as_usize()
        };
        info!(
            "[VMX setup] RIP={:#x}, entry={:#x}",
            rip_val,
            entry.as_usize()
        );
        VmcsGuestNW::RIP.write(rip_val)?;
        VmcsGuestNW::RFLAGS.write(0x2)?;
        VmcsGuestNW::PENDING_DBG_EXCEPTIONS.write(0)?;
        VmcsGuestNW::IA32_SYSENTER_ESP.write(0)?;
        VmcsGuestNW::IA32_SYSENTER_EIP.write(0)?;
        VmcsGuest32::IA32_SYSENTER_CS.write(0)?;

        VmcsGuest32::INTERRUPTIBILITY_STATE.write(0)?;
        VmcsGuest32::ACTIVITY_STATE.write(0)?;

        VmcsGuest32::VMX_PREEMPTION_TIMER_VALUE.write(VMX_PREEMPTION_TIMER_SET_VALUE)?;

        VmcsGuest64::LINK_PTR.write(self.shadow_vmcs.phys_addr().as_usize() as u64)?;
        VmcsGuest64::IA32_DEBUGCTL.write(0)?;
        VmcsGuest64::IA32_PAT.write(Msr::IA32_PAT.read())?;
        VmcsGuest64::IA32_EFER.write(0)?;
        VmcsGuest64::IA32_PERF_GLOBAL_CTRL.write(0)?;
        VmcsGuest64::IA32_BNDCFGS.write(0)?;
        Ok(())
    }

    fn setup_vmcs_control(&mut self, ept_root: HostPhysAddr, _is_guest: bool) -> AxResult {
        // Intercept NMI and external interrupts.
        use PinbasedControls as PinCtrl;

        use super::vmcs::controls::*;
        let raw_cpuid = CpuId::new();

        // Diagnostic: dump all VMX capability MSRs
        info!(
            "[VMX MSR] TRUE_PINBASED={:#018x} TRUE_PROCBASED={:#018x} PROCBASED2={:#018x} \
             TRUE_PROCBASED2={:#018x}",
            Msr::IA32_VMX_TRUE_PINBASED_CTLS.read(),
            Msr::IA32_VMX_TRUE_PROCBASED_CTLS.read(),
            Msr::IA32_VMX_PROCBASED_CTLS2.read(),
            Msr::IA32_VMX_TRUE_PROCBASED_CTLS2.read(),
        );
        info!(
            "[VMX MSR] TRUE_EXIT={:#018x} TRUE_ENTRY={:#018x}",
            Msr::IA32_VMX_TRUE_EXIT_CTLS.read(),
            Msr::IA32_VMX_TRUE_ENTRY_CTLS.read(),
        );
        info!(
            "[VMX MSR] PROCBASED={:#018x} PINBASED={:#018x} EXIT={:#018x} ENTRY={:#018x} \
             (non-TRUE)",
            Msr::IA32_VMX_PROCBASED_CTLS.read(),
            Msr::IA32_VMX_PINBASED_CTLS.read(),
            Msr::IA32_VMX_EXIT_CTLS.read(),
            Msr::IA32_VMX_ENTRY_CTLS.read(),
        );

        vmcs::set_control(
            VmcsControl32::PINBASED_EXEC_CONTROLS,
            Msr::IA32_VMX_TRUE_PINBASED_CTLS,
            VmcsControl32::PINBASED_EXEC_CONTROLS.read()?,
            (PinCtrl::NMI_EXITING
                | PinCtrl::EXTERNAL_INTERRUPT_EXITING
                | PinCtrl::VMX_PREEMPTION_TIMER)
                .bits(),
            0,
        )?;
        let pin_ctrl = VmcsControl32::PINBASED_EXEC_CONTROLS.read()?;
        info!("[VMX control] PIN_CTRL={:#x}", pin_ctrl);

        // Intercept all I/O instructions, use MSR bitmaps, activate secondary controls,
        // and intercept HLT for UEFI wait loops.
        // KVM nested virtualization forces both UNCOND_IO_EXITING (bit 24) and
        // USE_IO_BITMAPS (bit 25) as mandatory1, but the SDM mutual-exclusion rule
        // (SDM 26.2.1.1) prohibits both being 1 together. We clear USE_IO_BITMAPS
        // and keep UNCOND_IO_EXITING since unconditional I/O exiting makes the I/O
        // bitmap irrelevant anyway.
        use PrimaryControls as CpuCtrl;
        const USE_IO_BITMAPS_BIT: u32 = 1 << 25;
        const UNCOND_IO_EXITING_BIT: u32 = 1 << 24;
        {
            let cap = Msr::IA32_VMX_PROCBASED_CTLS.read();
            let allowed0 = cap as u32;
            let allowed1 = (cap >> 32) as u32;
            let mandatory1 = allowed0;
            let set_bits = (CpuCtrl::UNCOND_IO_EXITING
                | CpuCtrl::USE_MSR_BITMAPS
                | CpuCtrl::SECONDARY_CONTROLS
                | CpuCtrl::HLT_EXITING)
                .bits();
            let old_prim = VmcsControl32::PRIMARY_PROCBASED_EXEC_CONTROLS.read()?;
            let new_prim = old_prim | mandatory1 | set_bits;
            // Resolve USE_IO_BITMAPS vs UNCOND_IO_EXITING conflict (SDM 26.2.1.1).
            // SDM: If USE_IO_BITMAPS=1, UNCOND_IO_EXITING must be 0.
            // KVM nested virt may force both as mandatory1 via MSR, but the
            // hardware VM-entry check enforces the SDM mutual-exclusion rule.
            // We try to clear USE_IO_BITMAPS first (UNCOND_IO_EXITING already
            // covers all I/O), then UNCOND_IO_EXITING as fallback.
            let mut prim_to_write = new_prim;
            if (prim_to_write & USE_IO_BITMAPS_BIT) != 0
                && (prim_to_write & UNCOND_IO_EXITING_BIT) != 0
            {
                info!("[VMX control] USE_IO_BITMAPS and UNCOND_IO_EXITING conflict (SDM 26.2.1.1)");
                // Try clearing USE_IO_BITMAPS first (UNCOND_IO_EXITING covers all I/O).
                let try_prim = prim_to_write & !USE_IO_BITMAPS_BIT;
                VmcsControl32::PRIMARY_PROCBASED_EXEC_CONTROLS.write(try_prim)?;
                let actual = VmcsControl32::PRIMARY_PROCBASED_EXEC_CONTROLS.read()?;
                if actual & USE_IO_BITMAPS_BIT == 0 {
                    prim_to_write = actual;
                    info!(
                        "[VMX control] Cleared USE_IO_BITMAPS, PRIM_CTRL={:#x}",
                        actual
                    );
                } else {
                    // USE_IO_BITMAPS is truly mandatory1, try clearing UNCOND_IO_EXITING.
                    info!(
                        "[VMX control] USE_IO_BITMAPS is mandatory1 (read back={:#x}), trying to \
                         clear UNCOND_IO_EXITING",
                        actual
                    );
                    let try_prim = prim_to_write & !UNCOND_IO_EXITING_BIT;
                    VmcsControl32::PRIMARY_PROCBASED_EXEC_CONTROLS.write(try_prim)?;
                    let actual = VmcsControl32::PRIMARY_PROCBASED_EXEC_CONTROLS.read()?;
                    if actual & UNCOND_IO_EXITING_BIT == 0 {
                        prim_to_write = actual;
                        info!(
                            "[VMX control] Cleared UNCOND_IO_EXITING, PRIM_CTRL={:#x}",
                            actual
                        );
                    } else {
                        // Both are truly mandatory1. This is a KVM nesting bug.
                        // Write the original value and hope for the best.
                        prim_to_write = new_prim;
                        info!(
                            "[VMX control] Both USE_IO_BITMAPS and UNCOND_IO_EXITING are truly \
                             mandatory1 (KVM nesting limitation), PRIM_CTRL={:#x}",
                            actual
                        );
                    }
                }
            }
            info!(
                "[VMX control] Direct PRIM_CTRL write: allowed0={:#x}, allowed1={:#x}, \
                 mandatory1={:#x}, old={:#x}, set={:#x}, final={:#x}",
                allowed0, allowed1, mandatory1, old_prim, set_bits, prim_to_write
            );
            VmcsControl32::PRIMARY_PROCBASED_EXEC_CONTROLS.write(prim_to_write)?;
            let actual = VmcsControl32::PRIMARY_PROCBASED_EXEC_CONTROLS.read()?;
            info!(
                "[VMX control] Direct PRIM_CTRL read back: {:#x} (wrote {:#x})",
                actual, prim_to_write
            );
        }

        // Enable EPT, VPID, RDTSCP, INVPCID, and unrestricted guest.
        // Use a direct write to the VMCS field, then read back to see what the
        // hardware actually accepts. The MSR-based set_control is unreliable for
        // secondary controls on this CPU (PROCBASED2 MSR allowed0=0 is wrong).
        use SecondaryControls as CpuCtrl2;
        let desired_bits =
            (CpuCtrl2::ENABLE_EPT | CpuCtrl2::ENABLE_VPID | CpuCtrl2::UNRESTRICTED_GUEST).bits();
        let mut set_bits = desired_bits;
        if let Some(features) = raw_cpuid.get_extended_processor_and_feature_identifiers()
            && features.has_rdtscp()
        {
            set_bits |= CpuCtrl2::ENABLE_RDTSCP.bits();
        }
        if let Some(features) = raw_cpuid.get_extended_feature_info()
            && features.has_invpcid()
        {
            set_bits |= CpuCtrl2::ENABLE_INVPCID.bits();
        }
        if let Some(features) = raw_cpuid.get_extended_state_info()
            && features.has_xsaves_xrstors()
        {
            set_bits |= CpuCtrl2::ENABLE_XSAVES_XRSTORS.bits();
        }
        // Use the non-TRUE PROCBASED2 MSR (IA32_VMX_PROCBASED_CTLS2) because
        // KVM nested virtualization reports unreliable TRUE_PROCBASED2 values.
        // Do NOT apply MSR-reported mandatory1 to the direct write. The MSR
        // may report conflicting bits as mandatory1 (e.g., both APIC_REGISTER
        // and x2APIC forced to 1, which violates SDM 26.2.1.1). The hardware
        // silently enforces the true must-be-1/must-be-0 bits regardless of
        // what we write, so we write only our desired bits and let the hardware
        // resolve the actual mandatory bits.
        let cap = Msr::IA32_VMX_PROCBASED_CTLS2.read();
        let allowed0 = cap as u32;
        let allowed1 = (cap >> 32) as u32;
        let msr_mandatory1 = !allowed0 & allowed1;
        let old_sec = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.read()?;
        // Write only our desired bits; hardware will set/clear its own mandatory bits.
        let new_sec = old_sec | set_bits;
        debug!(
            "[VMX control] Direct SEC_CTRL write: allowed0={:#x}, allowed1={:#x}, \
             msr_mandatory1={:#x}, old={:#x}, set={:#x}, new={:#x}",
            allowed0, allowed1, msr_mandatory1, old_sec, set_bits, new_sec
        );
        VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.write(new_sec)?;
        let actual = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.read()?;
        let hw_mandatory1 = actual & !new_sec;
        let hw_mandatory0 = new_sec & !actual;
        debug!(
            "[VMX control] Direct SEC_CTRL read back: {:#x} (wrote {:#x}) hw_forced1={:#x} \
             hw_forced0={:#x}",
            actual, new_sec, hw_mandatory1, hw_mandatory0
        );

        // VMCS_SHADOWING (bit 14) requires LINK_PTR to point to a valid shadow
        // VMCS (SDM 24.4.2). If the hardware forces it via mandatory1, we
        // accept it and configure LINK_PTR accordingly. If not, we clear it.
        let sec_ctrl = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.read()?;
        if sec_ctrl & CpuCtrl2::VMCS_SHADOWING.bits() != 0 {
            let cap = Msr::IA32_VMX_PROCBASED_CTLS2.read();
            let allowed0 = cap as u32;
            let vmcs_shadowing_bit = CpuCtrl2::VMCS_SHADOWING.bits();
            let can_clear = (allowed0 & vmcs_shadowing_bit) != 0;
            if can_clear {
                vmcs::set_control(
                    VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS,
                    Msr::IA32_VMX_PROCBASED_CTLS2,
                    sec_ctrl,
                    0,
                    vmcs_shadowing_bit,
                )?;
                let sec_ctrl = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.read()?;
                if sec_ctrl & vmcs_shadowing_bit != 0 {
                    debug!(
                        "[VMX control] VMCS_SHADOWING is mandatory1, SEC_CTRL={:#x}",
                        sec_ctrl
                    );
                } else {
                    debug!("[VMX control] Cleared VMCS_SHADOWING");
                }
            } else {
                debug!(
                    "[VMX control] VMCS_SHADOWING is mandatory1 (allowed0={:#x}), keeping it \
                     enabled, SEC_CTRL={:#x}",
                    allowed0, sec_ctrl
                );
            }
        }

        // Cross-dependency checks for secondary controls (SDM 26.2.1.1).
        let sec_ctrl = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.read()?;
        let x2apic_set = sec_ctrl & CpuCtrl2::VIRTUALIZE_X2APIC.bits() != 0;
        let apic_reg_set = sec_ctrl & CpuCtrl2::VIRTUALIZE_APIC_REGISTER.bits() != 0;
        let apic_set = sec_ctrl & CpuCtrl2::VIRTUALIZE_APIC.bits() != 0;
        let pml_set = sec_ctrl & CpuCtrl2::ENABLE_PML.bits() != 0;
        let shadowing_set = sec_ctrl & CpuCtrl2::VMCS_SHADOWING.bits() != 0;
        let vid_set = sec_ctrl & CpuCtrl2::VIRTUAL_INTERRUPT_DELIVERY.bits() != 0;
        let vmfunc_set = sec_ctrl & CpuCtrl2::ENABLE_VM_FUNCTIONS.bits() != 0;
        debug!(
            "[VMX control] SEC_CTRL={:#x}: APIC={}, x2APIC={}, APIC_REG={}, VID={}, PML={}, \
             SHADOWING={}, VMFUNC={}",
            sec_ctrl,
            apic_set,
            x2apic_set,
            apic_reg_set,
            vid_set,
            pml_set,
            shadowing_set,
            vmfunc_set
        );

        // SDM 26.2.1.1: If "virtualize x2APIC mode" is 1, "virtualize APIC accesses" must be 1.
        // On KVM nested virtualization, both APIC and x2APIC may be forced as mandatory1.
        // Try to set VIRTUALIZE_APIC to satisfy the SDM dependency. If KVM rejects it,
        // the VM-entry will fail with a more specific error.
        if x2apic_set && !apic_set {
            debug!(
                "[VMX control] x2APIC set but APIC not set - attempting to force-set APIC, \
                 SEC_CTRL={:#x}",
                sec_ctrl
            );
            let new_sec = sec_ctrl | CpuCtrl2::VIRTUALIZE_APIC.bits();
            VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.write(new_sec)?;
            let actual = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.read()?;
            debug!(
                "[VMX control] After force-setting APIC: wrote={:#x}, read={:#x}",
                new_sec, actual
            );
        }

        // SDM 26.2.1.1: If "APIC-register virtualization" is 1, "virtualize x2APIC mode"
        // must be 0. KVM nested virtualization forces both as mandatory1, creating a
        // conflict. Try to clear x2APIC first, then APIC-register virtualization.
        let sec_ctrl = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.read()?;
        let x2apic_set = sec_ctrl & CpuCtrl2::VIRTUALIZE_X2APIC.bits() != 0;
        let apic_reg_set = sec_ctrl & CpuCtrl2::VIRTUALIZE_APIC_REGISTER.bits() != 0;
        if x2apic_set && apic_reg_set {
            debug!(
                "[VMX control] APIC_REG=1 and x2APIC=1 conflict (SDM 26.2.1.1) - attempting to \
                 clear x2APIC, SEC_CTRL={:#x}",
                sec_ctrl
            );
            let new_sec = sec_ctrl & !CpuCtrl2::VIRTUALIZE_X2APIC.bits();
            VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.write(new_sec)?;
            let actual = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.read()?;
            let x2apic_cleared = actual & CpuCtrl2::VIRTUALIZE_X2APIC.bits() == 0;
            debug!(
                "[VMX control] After clearing x2APIC: wrote={:#x}, read={:#x}, cleared={}",
                new_sec, actual, x2apic_cleared
            );
            if !x2apic_cleared {
                debug!(
                    "[VMX control] x2APIC could not be cleared - trying to clear APIC-register \
                     virtualization instead"
                );
                let new_sec = sec_ctrl & !CpuCtrl2::VIRTUALIZE_APIC_REGISTER.bits();
                VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.write(new_sec)?;
                let actual = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.read()?;
                debug!(
                    "[VMX control] After clearing APIC_REG: wrote={:#x}, read={:#x}",
                    new_sec, actual
                );
            }
        }

        // SDM 26.2.1.1: If "enable VM functions" is 1, VM-function controls must be non-zero.
        // We set VM_FUNCTION_CONTROLS=1 (EPTP switching) earlier in setup_vmcs_control.
        // Verify that the write took effect.
        if vmfunc_set {
            let vmfunc_ctrl = VmcsControl64::VM_FUNCTION_CONTROLS.read()?;
            debug!(
                "[VMX control] ENABLE_VM_FUNCTIONS=1, VM_FUNCTION_CONTROLS={:#x}, SEC_CTRL={:#x}",
                vmfunc_ctrl, sec_ctrl
            );
        }

        // Re-read SEC_CTRL after all fixups to get the final hardware-accepted value.
        let sec_ctrl = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.read()?;
        let actual = sec_ctrl;
        debug!("[VMX control] Final SEC_CTRL={:#x}", actual);

        // SDM 26.2.1.1: If VMCS_SHADOWING is 1, LINK_PTR must point to a valid shadow VMCS.
        // Re-read shadowing_set after possible modification.
        let shadowing_set = actual & CpuCtrl2::VMCS_SHADOWING.bits() != 0;
        if shadowing_set {
            debug!(
                "[VMX control] VMCS_SHADOWING=1, LINK_PTR={:#x}",
                VmcsGuest64::LINK_PTR.read().unwrap_or(0)
            );
        } else {
            VmcsGuest64::LINK_PTR.write(0xFFFF_FFFF_FFFF_FFFF)?;
            debug!("[VMX control] VMCS_SHADOWING=0, set LINK_PTR=0xFFFFFFFF_FFFFFFFF");
        }

        VmcsControl16::VPID.write(1)?;

        VmcsControl64::VMREAD_BITMAP_ADDR
            .write(self.vmread_bitmap.start_paddr().as_usize() as u64)?;
        VmcsControl64::VMWRITE_BITMAP_ADDR
            .write(self.vmwrite_bitmap.start_paddr().as_usize() as u64)?;
        VmcsControl64::PML_ADDR.write(self.pml_page.start_paddr().as_usize() as u64)?;
        VmcsControl64::XSS_EXITING_BITMAP.write(self.xss_bitmap.start_paddr().as_usize() as u64)?;
        // SDM 26.2.1.1: If "enable VM functions" is 1, VM-function controls must be non-zero.
        // The MSR forces ENABLE_VM_FUNCTIONS as mandatory1 on this hardware.
        // Enable EPTP switching (bit 0) to satisfy the check, and provide a valid
        // EPTP-list page (even though the guest won't use VMFUNC).
        let vmfunc_ctrl: u64 = 1; // EPTP switching
        VmcsControl64::VM_FUNCTION_CONTROLS.write(vmfunc_ctrl)?;
        VmcsControl64::EPTP_LIST_ADDR.write(self.eptp_list_page.start_paddr().as_usize() as u64)?;
        info!(
            "[VMX control] VM_FUNCTION_CONTROLS={:#x}, EPTP_LIST_ADDR={:#x}",
            vmfunc_ctrl,
            self.eptp_list_page.start_paddr().as_usize()
        );

        // Switch to 64-bit host, acknowledge interrupt info, switch IA32_PAT/IA32_EFER on VM exit.
        // Use TRUE MSR to avoid conditional mandatory-1 bits.
        use ExitControls as ExitCtrl;
        vmcs::set_control(
            VmcsControl32::VMEXIT_CONTROLS,
            Msr::IA32_VMX_TRUE_EXIT_CTLS,
            VmcsControl32::VMEXIT_CONTROLS.read()?,
            (ExitCtrl::HOST_ADDRESS_SPACE_SIZE
                | ExitCtrl::ACK_INTERRUPT_ON_EXIT
                | ExitCtrl::SAVE_IA32_PAT
                | ExitCtrl::LOAD_IA32_PAT
                | ExitCtrl::SAVE_IA32_EFER
                | ExitCtrl::LOAD_IA32_EFER)
                .bits(),
            0,
        )?;

        use EntryControls as EntryCtrl;
        // Use direct write for VMENTRY_CONTROLS.
        // Use the TRUE MSR to avoid conditional mandatory-1 bits.
        // When UNRESTRICTED_GUEST is set, clear IA32E_MODE_GUEST so the guest
        // can start in real mode and OVMF handles mode transitions itself.
        {
            let entry_cap = Msr::IA32_VMX_TRUE_ENTRY_CTLS.read();
            let entry_allowed0 = entry_cap as u32;
            let entry_allowed1 = (entry_cap >> 32) as u32;
            let entry_mandatory1 = entry_allowed0;
            let old_entry = VmcsControl32::VMENTRY_CONTROLS.read()?;
            let desired_entry_bits = (EntryCtrl::LOAD_IA32_PAT | EntryCtrl::LOAD_IA32_EFER).bits();
            let sec_ctrl = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.read()?;
            let unrestricted_guest = (sec_ctrl >> 7) & 1 != 0;
            // Clear IA32E_MODE_GUEST if UNRESTRICTED_GUEST is set
            let ia32e_bit = EntryCtrl::IA32E_MODE_GUEST.bits();
            let clear_mask = if unrestricted_guest { ia32e_bit } else { 0 };
            let new_entry = (old_entry | entry_mandatory1 | desired_entry_bits) & !clear_mask;
            info!(
                "[VMX control] Direct ENTRY_CTRL write: allowed0={:#x}, allowed1={:#x}, \
                 mandatory1={:#x}, old={:#x}, set={:#x}, new={:#x}",
                entry_allowed0,
                entry_allowed1,
                entry_mandatory1,
                old_entry,
                desired_entry_bits,
                new_entry
            );
            VmcsControl32::VMENTRY_CONTROLS.write(new_entry)?;
            let actual_entry = VmcsControl32::VMENTRY_CONTROLS.read()?;
            let entry_hw_forced1 = actual_entry & !new_entry;
            let entry_hw_forced0 = new_entry & !actual_entry;
            let ia32e_forced = actual_entry & EntryCtrl::IA32E_MODE_GUEST.bits() != 0;
            info!(
                "[VMX control] Direct ENTRY_CTRL read back: {:#x} (wrote {:#x}) hw_forced1={:#x} \
                 hw_forced0={:#x} IA32E_MODE_GUEST={}",
                actual_entry, new_entry, entry_hw_forced1, entry_hw_forced0, ia32e_forced
            );
        }

        vmcs::set_ept_pointer(ept_root)?;

        // No MSR switches if hypervisor doesn't use and there is only one vCPU.
        VmcsControl32::VMEXIT_MSR_STORE_COUNT.write(0)?;
        VmcsControl32::VMEXIT_MSR_LOAD_COUNT.write(0)?;
        VmcsControl32::VMENTRY_MSR_LOAD_COUNT.write(0)?;

        {
            // CR0/CR4 are only 32 bits wide; mask the FIXED MSR values to 32 bits
            // to avoid polluting the upper half of CR0_GUEST_HOST_MASK.
            let cr0_fixed0 = Msr::IA32_VMX_CR0_FIXED0.read() as u32 as u64;
            let cr0_fixed1 = Msr::IA32_VMX_CR0_FIXED1.read() as u32 as u64;
            let cr0_flex = (!cr0_fixed0 & 0xFFFF_FFFF) & cr0_fixed1;
            let mut cr0_mask = cr0_flex | cr0_fixed0;
            // When UNRESTRICTED_GUEST is set, PE and PG are not forced by
            // CR0_FIXED0 (Intel SDM Vol 3, Section 26.3.1.1). Exclude them
            // from the CR0 guest/host mask so the guest can modify these bits
            // without causing VM-exits, and the reconciliation code won't
            // force them to match the host.
            let sec_ctrl = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.read()?;
            let unrestricted_guest = (sec_ctrl >> 7) & 1 != 0;
            if unrestricted_guest {
                cr0_mask &= !(Cr0Flags::PAGING | Cr0Flags::PROTECTED_MODE_ENABLE).bits();
            }
            VmcsControlNW::CR0_GUEST_HOST_MASK.write(cr0_mask as usize)?;
            VmcsControlNW::CR0_READ_SHADOW.write(VmcsGuestNW::CR0.read()?)?;
            info!(
                "[VMX control] CR0_MASK={:#x} (fixed0={:#x} fixed1={:#x} flex={:#x} \
                 unrestricted_guest={})",
                cr0_mask, cr0_fixed0, cr0_fixed1, cr0_flex, unrestricted_guest
            );
        }
        {
            let cr4_fixed0 = Msr::IA32_VMX_CR4_FIXED0.read() as u32 as u64;
            let cr4_fixed1 = Msr::IA32_VMX_CR4_FIXED1.read() as u32 as u64;
            let cr4_flex = (!cr4_fixed0 & 0xFFFF_FFFF) & cr4_fixed1;
            let cr4_mask = cr4_flex | cr4_fixed0;
            VmcsControlNW::CR4_GUEST_HOST_MASK.write(cr4_mask as usize)?;
            VmcsControlNW::CR4_READ_SHADOW.write(VmcsGuestNW::CR4.read()?)?;
            info!(
                "[VMX control] CR4_MASK={:#x} (fixed0={:#x} fixed1={:#x} flex={:#x})",
                cr4_mask, cr4_fixed0, cr4_fixed1, cr4_flex
            );
        }
        // KVM nested virtualization may force CR0/CR4 mask bits that require
        // guest and host CR0/CR4 to match. Read back the actual mask values
        // written by hardware and reconcile guest CR0/CR4 with host.
        let cr0_mask_actual = VmcsControlNW::CR0_GUEST_HOST_MASK.read()?;
        let cr4_mask_actual = VmcsControlNW::CR4_GUEST_HOST_MASK.read()?;
        let host_cr0 = VmcsHostNW::CR0.read()?;
        let host_cr4 = VmcsHostNW::CR4.read()?;
        let mut guest_cr0 = VmcsGuestNW::CR0.read()?;
        let mut guest_cr4 = VmcsGuestNW::CR4.read()?;
        let cr0_changed = (guest_cr0 & cr0_mask_actual) != (host_cr0 & cr0_mask_actual);
        let cr4_changed = (guest_cr4 & cr4_mask_actual) != (host_cr4 & cr4_mask_actual);
        if cr0_changed {
            guest_cr0 = (host_cr0 & cr0_mask_actual) | (guest_cr0 & !cr0_mask_actual);
            VmcsGuestNW::CR0.write(guest_cr0)?;
            VmcsControlNW::CR0_READ_SHADOW.write(guest_cr0)?;
        }
        if cr4_changed {
            guest_cr4 = (host_cr4 & cr4_mask_actual) | (guest_cr4 & !cr4_mask_actual);
            VmcsGuestNW::CR4.write(guest_cr4)?;
            VmcsControlNW::CR4_READ_SHADOW.write(guest_cr4)?;
        }
        info!(
            "[VMX control] CR0_MASK_actual={:#x} CR4_MASK_actual={:#x} CR0 guest{}/host{} CR4 \
             guest{}/host{} reconcile_cr0={} reconcile_cr4={}",
            cr0_mask_actual,
            cr4_mask_actual,
            VmcsGuestNW::CR0.read().unwrap_or(0),
            host_cr0,
            VmcsGuestNW::CR4.read().unwrap_or(0),
            host_cr4,
            cr0_changed,
            cr4_changed,
        );
        VmcsControl32::CR3_TARGET_COUNT.write(0)?;

        // Pass-through all exceptions (no VM exit on exceptions).
        let exception_bitmap: u32 = 0;

        self.setup_io_bitmap()?;

        VmcsControl32::EXCEPTION_BITMAP.write(exception_bitmap)?;
        VmcsControl64::IO_BITMAP_A_ADDR.write(self.io_bitmap.phys_addr().0.as_usize() as _)?;
        VmcsControl64::IO_BITMAP_B_ADDR.write(self.io_bitmap.phys_addr().1.as_usize() as _)?;
        VmcsControl64::MSR_BITMAPS_ADDR.write(self.msr_bitmap.phys_addr().as_usize() as _)?;

        VmcsControl64::VIRT_APIC_ADDR
            .write(self.vlapic.virtual_apic_page_addr().as_usize() as u64)?;
        VmcsControl64::APIC_ACCESS_ADDR
            .write(self.apic_access_page.start_paddr().as_usize() as u64)?;
        VmcsControl64::EOI_EXIT0.write(0)?;
        VmcsControl64::EOI_EXIT1.write(0)?;
        VmcsControl64::EOI_EXIT2.write(0)?;
        VmcsControl64::EOI_EXIT3.write(0)?;
        VmcsControl32::TPR_THRESHOLD.write(0)?;
        // The MSR IA32_VMX_TRUE_PINBASED_CTLS forces "process posted interrupts" (bit6)
        // via mandatory1. This requires the posted-interrupt notification vector and
        // descriptor address to be properly configured.
        // Use a non-zero notification vector (0xFC, outside typical interrupt range)
        // and point the descriptor to a zeroed page.
        const POSTED_INTR_VECTOR: u16 = 0xFC;
        VmcsControl16::POSTED_INTERRUPT_NOTIFICATION_VECTOR.write(POSTED_INTR_VECTOR)?;
        VmcsControl64::POSTED_INTERRUPT_DESC_ADDR
            .write(self.posted_interrupt_desc.start_paddr().as_usize() as u64)?;
        debug!(
            "[VMX control] Posted-interrupt: vector={:#x}, desc_addr={:#x}",
            POSTED_INTR_VECTOR,
            self.posted_interrupt_desc.start_paddr().as_usize()
        );
        Ok(())
    }

    /// After `setup_vmcs_control` has set VMENTRY_CONTROLS, adjust guest
    /// CR0/EFER/CS to satisfy VM-entry checks.
    ///
    /// If IA32E_MODE_GUEST is already set in VMENTRY_CONTROLS, ensure the
    /// guest state matches (CR0.PG=1, CR4.PAE=1, EFER.LME=1, CS.L=1).
    ///
    /// If CR0_FIXED0 forces PE+PG (common under KVM nested virtualization)
    /// and UNRESTRICTED_GUEST is NOT set, the guest cannot start in real mode.
    /// In that case, force IA32E_MODE_GUEST=1 and set up long-mode guest state.
    ///
    /// If UNRESTRICTED_GUEST is set, CR0.PE and CR0.PG may be 0 regardless
    /// of CR0_FIXED0 (Intel SDM Vol 3, Section 26.3.1.1), so the guest can
    /// start in real mode and OVMF handles mode transitions itself.
    fn fixup_ia32e_guest_cr_and_efer(&mut self) -> AxResult {
        use super::vmcs::controls::*;
        let entry_ctrl = VmcsControl32::VMENTRY_CONTROLS.read()?;
        let mut need_ia32e = entry_ctrl & EntryControls::IA32E_MODE_GUEST.bits() != 0;

        // Check if UNRESTRICTED_GUEST is set - if so, guest can run with PE=0/PG=0
        let sec_ctrl = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.read()?;
        let unrestricted_guest = (sec_ctrl >> 7) & 1 != 0;

        // Check if CR0_FIXED0 forces PE+PG, which prevents real-mode guest.
        let cr0_fixed0 = Msr::IA32_VMX_CR0_FIXED0.read();
        let cr0_fixed0_forces_pe_pg = (cr0_fixed0
            & (Cr0Flags::PROTECTED_MODE_ENABLE | Cr0Flags::PAGING).bits())
            == (Cr0Flags::PROTECTED_MODE_ENABLE | Cr0Flags::PAGING).bits();

        if cr0_fixed0_forces_pe_pg && !need_ia32e && !unrestricted_guest {
            // CR0_FIXED0 forces PE+PG: guest must be in paged protected mode.
            // With PG=1 and CR4.PAE=1, Intel SDM requires EFER.LME=1 if
            // IA32E_MODE_GUEST=1, or 32-bit PAE paging if IA32E_MODE_GUEST=0.
            // However, KVM nested virt may reject 32-bit PAE paging guests.
            // Force IA32E_MODE_GUEST=1 for maximum compatibility.
            let new_entry = entry_ctrl | EntryControls::IA32E_MODE_GUEST.bits();
            VmcsControl32::VMENTRY_CONTROLS.write(new_entry)?;
            let actual_entry = VmcsControl32::VMENTRY_CONTROLS.read()?;
            if actual_entry & EntryControls::IA32E_MODE_GUEST.bits() != 0 {
                need_ia32e = true;
                info!(
                    "[VMX setup] CR0_FIXED0 forces PE+PG, forced IA32E_MODE_GUEST: entry {:#x} -> \
                     {:#x}",
                    entry_ctrl, actual_entry
                );
            } else {
                warn!(
                    "[VMX setup] CR0_FIXED0 forces PE+PG but IA32E_MODE_GUEST write rejected: \
                     {:#x}",
                    actual_entry
                );
            }
        }

        if need_ia32e {
            let mut cr0 = VmcsGuestNW::CR0.read()?;
            cr0 |= (Cr0Flags::PROTECTED_MODE_ENABLE | Cr0Flags::PAGING).bits() as usize;
            VmcsGuestNW::CR0.write(cr0)?;
            let mut efer = VmcsGuest64::IA32_EFER.read()?;
            efer |= EferFlags::LONG_MODE_ENABLE.bits();
            efer |= MSR_IA32_EFER_LMA_BIT; // SDM: LMA must equal IA32E_MODE_GUEST
            VmcsGuest64::IA32_EFER.write(efer)?;
            // Set CS to 64-bit code segment: L=1, D/B=0
            // Intel SDM Vol 3C 24.4.1: bit12=L, bit13=D/B, bit14=G, bit15=Unusable
            let cs_ar = 0x109b; // L=1(bit12), D/B=0, present, code, exec/read, accessed
            VmcsGuest32::CS_ACCESS_RIGHTS.write(cs_ar)?;
            // In IA-32e mode, CS base must be 0 to avoid non-canonical linear addresses.
            // The original CS_BASE=0xFFFF0000 would cause RIP+CS_BASE to overflow
            // into non-canonical address space (e.g., 0xFFFFFFF0 + 0xFFFF0000 = 0x1FFFEFFFF0).
            VmcsGuestNW::CS_BASE.write(0)?;
            VmcsGuest32::CS_LIMIT.write(0xffffffff)?;
            // IA32E_MODE_GUEST requires TR to be a busy TSS (SDM 26.3.1.2)
            let tr_ar = VmcsGuest32::TR_ACCESS_RIGHTS.read()?;
            if tr_ar & (1 << 15) != 0 {
                // bit 15 = Unusable per Intel SDM 24.4.1
                VmcsGuest32::TR_ACCESS_RIGHTS.write(0x8b)?;
                VmcsGuestNW::TR_BASE.write(0)?;
                VmcsGuest32::TR_LIMIT.write(0x67)?;
                info!("[VMX setup] IA32E_MODE_GUEST fixup: TR from unusable to busy TSS");
            }
            info!(
                "[VMX setup] IA32E_MODE_GUEST fixup: CR0={:#x}, EFER={:#x}, CS_AR={:#x}",
                cr0, efer, cs_ar
            );
        }
        Ok(())
    }

    pub(super) fn get_paging_level(&self) -> usize {
        let mut level: u32 = 0; // non-paging
        let cr0 = VmcsGuestNW::CR0.read().unwrap();
        let cr4 = VmcsGuestNW::CR4.read().unwrap();
        let efer = VmcsGuest64::IA32_EFER.read().unwrap();
        // paging is enabled
        if cr0 & Cr0Flags::PAGING.bits() as usize != 0 {
            if cr4 & Cr4Flags::PHYSICAL_ADDRESS_EXTENSION.bits() as usize != 0 {
                // is long mode
                if efer & EferFlags::LONG_MODE_ACTIVE.bits() != 0 {
                    level = 4;
                } else {
                    level = 3;
                }
            } else {
                level = 2;
            }
        }
        level as usize
    }
}
