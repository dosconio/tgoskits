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

use alloc::collections::VecDeque;
use core::{
    arch::naked_asm,
    fmt::{Debug, Formatter, Result},
    mem::size_of,
};

use ax_errno::AxResult;
use axaddrspace::{GuestPhysAddr, GuestVirtAddr, HostPhysAddr, NestedPageFaultInfo, device::Port};
use axvcpu::{AxArchVCpu, AxVCpuExitReason};
use axvisor_api::{
    memory::PhysFrame,
    vmm::{VCpuId, VMId},
};
use bit_field::BitField;
use x86::bits64::vmx;
use x86_64::registers::control::{Cr0Flags, Cr4Flags, EferFlags};
use x86_vlapic::EmulatedLocalApic;

use super::{
    VmxExitInfo, as_axerr,
    definitions::{VmxExitReason, VmxInterruptionType},
    structs::{IOBitmap, MsrBitmap, VmxRegion},
    vmcs::{self, VmcsControl32, VmcsGuest16, VmcsGuest32, VmcsGuest64, VmcsGuestNW, VmcsHostNW},
};
use crate::{
    boot_mode::{X86BootMode, X86VCpuSetupConfig},
    ept::GuestPageWalkInfo,
    msr::Msr,
    regs::GeneralRegisters,
    restore_host_interrupt_flag,
    xstate::XState,
};

/// ECAM (Enhanced Configuration Access Mechanism) MMIO range for QEMU Q35.
/// OVMF uses ECAM to access PCI config space via MMIO at 0xB000_0000.
pub(super) const ECAM_MMIO_BASE: usize = 0xB000_0000;
pub(super) const ECAM_MMIO_END: usize = 0xC000_0000; // 256 MB: 0xB000_0000 + 0x1000_0000

pub(super) struct PendingEvent {
    pub(super) vector: u8,
    pub(super) err_code: Option<u32>,
    pub(super) int_type: VmxInterruptionType,
}

pub(super) const VMX_PREEMPTION_TIMER_SET_VALUE: u32 = 1_000_000;

pub(super) const QEMU_EXIT_PORT: u16 = 0x604;
pub(super) const QEMU_EXIT_MAGIC: u64 = 0x2000;

#[derive(PartialEq, Eq, Debug)]
pub enum VmCpuMode {
    Real,
    Protected,
    Compatibility, // IA-32E mode (CS.L = 0)
    Mode64,        // IA-32E mode (CS.L = 1)
}

pub(super) const MSR_IA32_EFER_LMA_BIT: u64 = 1 << 10;
pub(super) const CR0_PE: usize = 1 << 0;

/// A virtual CPU within a guest.
#[repr(C)]
pub struct VmxVcpu {
    // The order of `guest_regs`, `host_stack_top`, and `host_rflags` is
    // mandatory. They must be the first three fields. If you want to change
    // the order or the type of these fields, you must also change the assembly
    // in this file.
    /// Guest general-purpose registers.
    pub(super) guest_regs: GeneralRegisters,
    /// The top of the host stack.
    host_stack_top: u64,
    /// Host RFLAGS captured immediately before VM entry.
    host_rflags: u64,

    // The order of the following fields is not mandatory.

    // VCpu states and configurations
    /// Whether the VMCS has been launched. Used to determine whether to `vmx_launch` or `vmx_resume`.
    launched: bool,
    /// The guest entry point.
    entry: Option<GuestPhysAddr>,
    /// The EPT root address.
    pub(super) ept_root: Option<HostPhysAddr>,
    // /// Whether this VCPU is a host VCpu. Used in type 1.5 hypervisor.
    // is_host: bool, temporary removed because we don't care about type 1.5 now

    // VMCS-related fields
    /// The VMCS region.
    pub(super) vmcs: VmxRegion,
    /// The shadow VMCS region for VMCS shadowing (required when VMCS_SHADOWING is mandatory1).
    pub(super) shadow_vmcs: VmxRegion,
    /// The I/O bitmap for the VMCS.
    pub(super) io_bitmap: IOBitmap,
    /// The MSR bitmap for the VMCS.
    pub(super) msr_bitmap: MsrBitmap,
    /// The VMREAD bitmap for VMCS shadowing.
    pub(super) vmread_bitmap: PhysFrame,
    /// The VMWRITE bitmap for VMCS shadowing.
    pub(super) vmwrite_bitmap: PhysFrame,
    /// The PML page for Page Modification Logging.
    pub(super) pml_page: PhysFrame,
    /// The EPTP-list page for EPTP switching (required when ENABLE_VM_FUNCTIONS is mandatory1).
    pub(super) eptp_list_page: PhysFrame,
    /// The XSS-exiting bitmap.
    pub(super) xss_bitmap: PhysFrame,
    /// The APIC-access page for APIC virtualization.
    pub(super) apic_access_page: PhysFrame,
    /// The posted-interrupt descriptor (required when PIN_CTRL bit6 is forced by MSR).
    pub(super) posted_interrupt_desc: PhysFrame,

    // Interrupt-related fields
    /// Pending events to be injected to the guest.
    pub(super) pending_events: VecDeque<PendingEvent>,
    /// Emulated Local APIC.
    pub(super) vlapic: EmulatedLocalApic,

    // Extra states
    /// The XState of the VCpu. Both host and guest.
    pub(super) xstate: XState,

    /// End of guest RAM (for EPT violation handling).
    pub(super) ram_end: usize,

    /// A 4 KB page filled with 0xFF bytes, used to map unmapped GPA regions
    /// during guest memory probing so that reads return all-ones (indicating
    /// non-existent memory) without requiring instruction emulation.
    pub(super) dummy_ff_page: PhysFrame,

    /// Trace VM-exits after CPUID 0x80000000 to diagnose why OVMF
    /// never calls CPUID 0x80000001.
    pub(super) trace_after_cpuid_8k: bool,
    pub(super) trace_after_8k_count: usize,

    // Tracing-related fields
    #[cfg(feature = "tracing")]
    /// The guest registers when the VM-exit happens.
    guest_regs_exiting: GeneralRegisters,
}

/// Ring buffer tracking the last 16 VM-exits (exit_reason, rip, cs_ar)
/// to diagnose VM-entry failures. When a VM-entry failure (reason 0x21)
/// occurs, we log the last N VM-exits to identify what caused the guest
/// state to become invalid.
pub(super) struct ExitHistory {
    pub(super) buf: [(u32, u64, u32); 16],
    pub(super) idx: usize,
    pub(super) count: u64,
}

pub(super) static EXIT_HISTORY: spin::Mutex<ExitHistory> = spin::Mutex::new(ExitHistory {
    buf: [(0, 0, 0); 16],
    idx: 0,
    count: 0,
});

impl VmxVcpu {
    /// Create a new [`VmxVcpu`].
    pub fn new(vm_id: VMId, vcpu_id: VCpuId) -> AxResult<Self> {
        let vmcs_revision_id = super::read_vmcs_revision_id();
        let vcpu = Self {
            guest_regs: GeneralRegisters::default(),
            host_stack_top: 0,
            host_rflags: 0,
            launched: false,
            entry: None,
            ept_root: None,
            // is_host: false,
            vmcs: VmxRegion::new(vmcs_revision_id, false)?,
            shadow_vmcs: VmxRegion::new(vmcs_revision_id, true)?,
            io_bitmap: IOBitmap::intercept_all()?,
            msr_bitmap: MsrBitmap::passthrough_all()?,
            vmread_bitmap: PhysFrame::alloc_zero()?,
            vmwrite_bitmap: PhysFrame::alloc_zero()?,
            pml_page: PhysFrame::alloc_zero()?,
            eptp_list_page: PhysFrame::alloc_zero()?,
            xss_bitmap: PhysFrame::alloc_zero()?,
            apic_access_page: PhysFrame::alloc_zero()?,
            posted_interrupt_desc: PhysFrame::alloc_zero()?,
            pending_events: VecDeque::with_capacity(8),
            vlapic: EmulatedLocalApic::new(vm_id, vcpu_id),
            xstate: XState::new(),
            ram_end: 0,
            dummy_ff_page: {
                let mut f = PhysFrame::alloc()?;
                f.fill(0xFF);
                f
            },
            trace_after_cpuid_8k: false,
            trace_after_8k_count: 0,
            #[cfg(feature = "tracing")]
            guest_regs_exiting: GeneralRegisters::default(),
        };
        info!("[HV] created VmxVcpu(vmcs: {:#x})", vcpu.vmcs.phys_addr());
        Ok(vcpu)
    }

    /// Set the new [`VmxVcpu`] context from guest OS.
    pub fn setup(
        &mut self,
        ept_root: HostPhysAddr,
        entry: GuestPhysAddr,
        boot_mode: X86BootMode,
        ram_size: usize,
    ) -> AxResult {
        self.ram_end = ram_size;
        self.setup_vmcs(entry, ept_root, boot_mode)?;
        Ok(())
    }

    // /// Get the identifier of this [`VmxVcpu`].
    // pub fn vcpu_id(&self) -> usize {
    //     get_current_vcpu::<Self>().unwrap().id()
    // }

    /// Bind this [`VmxVcpu`] to current logical processor.
    pub fn bind_to_current_processor(&self) -> AxResult {
        // debug!(
        //     "VmxVcpu bind to current processor vmcs @ {:#x}",
        //     self.vmcs.phys_addr()
        // );
        unsafe {
            vmx::vmptrld(self.vmcs.phys_addr().as_usize() as u64).map_err(as_axerr)?;
        }
        self.setup_vmcs_host()?;
        Ok(())
    }

    /// Unbind this [`VmxVcpu`] from current logical processor.
    pub fn unbind_from_current_processor(&self) -> AxResult {
        // debug!(
        //     "VmxVcpu unbind from current processor vmcs @ {:#x}",
        //     self.vmcs.phys_addr()
        // );

        unsafe {
            vmx::vmclear(self.vmcs.phys_addr().as_usize() as u64).map_err(as_axerr)?;
        }
        Ok(())
    }

    /// Get CPU mode of the guest.
    pub fn get_cpu_mode(&self) -> VmCpuMode {
        let ia32_efer = Msr::IA32_EFER.read();
        let cs_access_right = VmcsGuest32::CS_ACCESS_RIGHTS.read().unwrap();
        let cr0 = VmcsGuestNW::CR0.read().unwrap();
        if (ia32_efer & MSR_IA32_EFER_LMA_BIT) != 0 {
            if (cs_access_right & 0x1000) != 0 {
                // CS.L = 1 (bit 12 per Intel SDM 24.4.1)
                VmCpuMode::Mode64
            } else {
                VmCpuMode::Compatibility
            }
        } else if (cr0 & CR0_PE) != 0 {
            VmCpuMode::Protected
        } else {
            VmCpuMode::Real
        }
    }

    pub fn inner_run(&mut self) -> Option<VmxExitInfo> {
        self.inject_pending_events().unwrap();

        // Comprehensive exit reason statistics
        struct ExitStats {
            cpuid: core::sync::atomic::AtomicU64,
            io: core::sync::atomic::AtomicU64,
            msr_read: core::sync::atomic::AtomicU64,
            msr_write: core::sync::atomic::AtomicU64,
            ept_violation: core::sync::atomic::AtomicU64,
            preempt: core::sync::atomic::AtomicU64,
            ext_intr: core::sync::atomic::AtomicU64,
            hlt: core::sync::atomic::AtomicU64,
            intr_window: core::sync::atomic::AtomicU64,
            cr_access: core::sync::atomic::AtomicU64,
            other: core::sync::atomic::AtomicU64,
            total: core::sync::atomic::AtomicU64,
        }
        static STATS: ExitStats = ExitStats {
            cpuid: core::sync::atomic::AtomicU64::new(0),
            io: core::sync::atomic::AtomicU64::new(0),
            msr_read: core::sync::atomic::AtomicU64::new(0),
            msr_write: core::sync::atomic::AtomicU64::new(0),
            ept_violation: core::sync::atomic::AtomicU64::new(0),
            preempt: core::sync::atomic::AtomicU64::new(0),
            ext_intr: core::sync::atomic::AtomicU64::new(0),
            hlt: core::sync::atomic::AtomicU64::new(0),
            intr_window: core::sync::atomic::AtomicU64::new(0),
            cr_access: core::sync::atomic::AtomicU64::new(0),
            other: core::sync::atomic::AtomicU64::new(0),
            total: core::sync::atomic::AtomicU64::new(0),
        };

        // Run guest first, then count the exit reason after we get back
        self.load_guest_xstate();

        #[cfg(feature = "tracing")]
        {
            use crate::regs::GeneralRegistersDiff;
            // Tracing, do a diff of the guest registers before entering the guest
            let diff = GeneralRegistersDiff::new(self.guest_regs_exiting, self.guest_regs);
            if !diff.is_same() {
                debug!("VCpu registers changed during handling VM-exit: {diff:#x?}");
            } else {
                debug!("VCpu registers unchanged during handling VM-exit");
            }
        }

        unsafe {
            if self.launched {
                self.vmx_resume();
            } else {
                self.launched = true;
                VmcsHostNW::RSP
                    .write(&self.host_stack_top as *const _ as usize)
                    .unwrap();

                // RIP was already set correctly in setup_vmcs_guest().
                // For UEFI mode, RIP must be 0xFFF0 (the IP offset within CS),
                // not the linear address 0xFFFFFFF0.  Do not overwrite it here.

                self.dump_vmcs_state();

                self.vmx_launch();
            }
        }
        self.load_host_xstate();
        restore_host_interrupt_flag(self.host_rflags);

        #[cfg(feature = "tracing")]
        {
            self.guest_regs_exiting = self.guest_regs;
        }

        // Handle vm-exits
        let exit_info = self.exit_info().unwrap();

        // Track last VM-exits in a ring buffer to diagnose VM-entry failures.
        // When a VM-entry failure (reason 0x21) occurs, we log the last N
        // VM-exits to identify what caused the guest state to become invalid.
        {
            let cs_ar = VmcsGuest32::CS_ACCESS_RIGHTS.read().unwrap_or(0);
            let rip = self.rip() as u64;
            let reason = exit_info.exit_reason as u32;
            let mut hist = EXIT_HISTORY.lock();
            let idx = hist.idx;
            hist.buf[idx] = (reason, rip, cs_ar);
            hist.idx = (idx + 1) % 16;
            hist.count += 1;
        }

        // Update exit reason statistics
        STATS
            .total
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        match exit_info.exit_reason {
            VmxExitReason::CPUID => {
                STATS
                    .cpuid
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
            VmxExitReason::IO_INSTRUCTION => {
                STATS.io.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
            VmxExitReason::MSR_READ => {
                STATS
                    .msr_read
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
            VmxExitReason::MSR_WRITE => {
                STATS
                    .msr_write
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
            VmxExitReason::EPT_VIOLATION => {
                STATS
                    .ept_violation
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
            VmxExitReason::PREEMPTION_TIMER => {
                STATS
                    .preempt
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
            VmxExitReason::EXTERNAL_INTERRUPT => {
                STATS
                    .ext_intr
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
            VmxExitReason::HLT => {
                STATS
                    .hlt
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
            VmxExitReason::INTERRUPT_WINDOW => {
                STATS
                    .intr_window
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
            VmxExitReason::CR_ACCESS => {
                STATS
                    .cr_access
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
            _ => {
                STATS
                    .other
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
        }

        // Trace VM-exits after CPUID 0x80000000 to diagnose OVMF not calling 0x80000001
        // Disabled: the CPUID 0x80000001 issue has been resolved (OVMF now enters
        // long mode successfully). Keep the counter logic for potential future use.
        if self.trace_after_cpuid_8k && self.trace_after_8k_count < 200 {
            let reason = exit_info.exit_reason;
            let rax = self.regs().rax as u32;
            let short_reason = match reason {
                VmxExitReason::IO_INSTRUCTION => "IO",
                VmxExitReason::CPUID => "CPU",
                VmxExitReason::EXTERNAL_INTERRUPT => "EXT",
                VmxExitReason::PREEMPTION_TIMER => "PRM",
                _ => "OTH",
            };
            // Log only CPUID exits (the original diagnostic target), skip noisy
            // IO/EXT/PRM exits that flood the log during OVMF timer loops.
            if matches!(reason, VmxExitReason::CPUID) {
                debug!("[T]{}{}{:x}", self.trace_after_8k_count, short_reason, rax);
            }
            self.trace_after_8k_count += 1;
            if matches!(reason, VmxExitReason::CPUID) && rax == 0x80000001 {
                self.trace_after_cpuid_8k = false; // Found it, stop tracing
            }
            if self.trace_after_8k_count >= 200 {
                self.trace_after_cpuid_8k = false;
            }
        }

        match self.builtin_vmexit_handler(&exit_info) {
            Some(result) => match result {
                Ok(()) => None,
                Err(err) => {
                    panic!(
                        "VmxVcpu failed to handle a VM-exit that should be handled by itself: \
                         {:?}, error {:?}, vcpu: {:#x?}",
                        exit_info.exit_reason, err, self
                    );
                }
            },
            None => Some(exit_info),
        }
    }

    /// Basic information about VM exits.
    pub fn exit_info(&self) -> AxResult<vmcs::VmxExitInfo> {
        vmcs::exit_info()
    }

    /// Raw information for VM Exits Due to Vectored Events, See SDM 25.9.2
    pub fn raw_interrupt_exit_info(&self) -> AxResult<u32> {
        vmcs::raw_interrupt_exit_info()
    }

    /// Information for VM exits due to external interrupts.
    pub fn interrupt_exit_info(&self) -> AxResult<vmcs::VmxInterruptInfo> {
        vmcs::interrupt_exit_info()
    }

    /// Information for VM exits due to I/O instructions.
    pub fn io_exit_info(&self) -> AxResult<vmcs::VmxIoExitInfo> {
        vmcs::io_exit_info()
    }

    /// Information for VM exits due to nested page table faults (EPT violation).
    pub fn nested_page_fault_info(&self) -> AxResult<NestedPageFaultInfo> {
        vmcs::ept_violation_info()
    }

    /// Information for VM exits due to APIC access.
    pub fn apic_access_exit_info(&self) -> AxResult<vmcs::ApicAccessExitInfo> {
        vmcs::apic_access_exit_info()
    }

    /// Guest general-purpose registers.
    pub fn regs(&self) -> &GeneralRegisters {
        &self.guest_regs
    }

    /// Mutable reference of guest general-purpose registers.
    pub fn regs_mut(&mut self) -> &mut GeneralRegisters {
        &mut self.guest_regs
    }

    /// Guest stack pointer. (`RSP`)
    pub fn stack_pointer(&self) -> usize {
        VmcsGuestNW::RSP.read().unwrap()
    }

    /// Set guest stack pointer. (`RSP`)
    pub fn set_stack_pointer(&mut self, rsp: usize) {
        VmcsGuestNW::RSP.write(rsp).unwrap()
    }

    /// Translate guest virtual addr to linear addr
    pub fn gla2gva(&self, guest_rip: GuestVirtAddr) -> GuestVirtAddr {
        let cpu_mode = self.get_cpu_mode();
        let seg_base = if cpu_mode == VmCpuMode::Mode64 {
            0
        } else {
            VmcsGuestNW::CS_BASE.read().unwrap()
        };
        // debug!(
        //     "seg_base: {:#x}, guest_rip: {:#x} cpu mode:{:?}",
        //     seg_base, guest_rip, cpu_mode
        // );
        guest_rip + seg_base
    }

    /// Get Translate guest page table info
    pub fn get_ptw_info(&self) -> GuestPageWalkInfo {
        let top_entry = VmcsGuestNW::CR3.read().unwrap();
        let level = self.get_paging_level();
        let is_write_access = false;
        let is_inst_fetch = false;
        let is_user_mode_access = ((VmcsGuest32::SS_ACCESS_RIGHTS.read().unwrap() >> 5) & 0x3) == 3;
        let mut pse = true;
        let mut nxe =
            (VmcsGuest64::IA32_EFER.read().unwrap() & EferFlags::NO_EXECUTE_ENABLE.bits()) != 0;
        let wp = (VmcsGuestNW::CR0.read().unwrap() & Cr0Flags::WRITE_PROTECT.bits() as usize) != 0;
        let is_smap_on = (VmcsGuestNW::CR4.read().unwrap()
            & Cr4Flags::SUPERVISOR_MODE_ACCESS_PREVENTION.bits() as usize)
            != 0;
        let is_smep_on = (VmcsGuestNW::CR4.read().unwrap()
            & Cr4Flags::SUPERVISOR_MODE_EXECUTION_PROTECTION.bits() as usize)
            != 0;
        let width: u32;
        if level == 4 || level == 3 {
            width = 9;
        } else if level == 2 {
            width = 10;
            pse = VmcsGuestNW::CR4.read().unwrap() & Cr4Flags::PAGE_SIZE_EXTENSION.bits() as usize
                != 0;
            nxe = false;
        } else {
            width = 0;
        }
        GuestPageWalkInfo {
            top_entry,
            level,
            width,
            is_user_mode_access,
            is_write_access,
            is_inst_fetch,
            pse,
            wp,
            nxe,
            is_smap_on,
            is_smep_on,
        }
    }

    /// Guest rip. (`RIP`)
    pub fn rip(&self) -> usize {
        VmcsGuestNW::RIP.read().unwrap()
    }

    /// Guest cs. (`cs`)
    pub fn cs(&self) -> u16 {
        VmcsGuest16::CS_SELECTOR.read().unwrap()
    }

    /// Advance guest `RIP` by `instr_len` bytes.
    pub fn advance_rip(&mut self, instr_len: u8) -> AxResult {
        VmcsGuestNW::RIP.write(VmcsGuestNW::RIP.read()? + instr_len as usize)
    }

    /// Add a virtual interrupt or exception to the pending events list,
    /// and try to inject it before later VM entries.
    pub fn queue_event(&mut self, vector: u8, err_code: Option<u32>) {
        let int_type = VmxInterruptionType::from_vector(vector);
        self.pending_events.push_back(PendingEvent {
            vector,
            err_code,
            int_type,
        });
    }

    pub fn queue_external_interrupt(&mut self, vector: u8) {
        self.pending_events.push_back(PendingEvent {
            vector,
            err_code: None,
            int_type: VmxInterruptionType::External,
        });
    }

    /// If enable, a VM exit occurs at the beginning of any instruction if
    /// `RFLAGS.IF` = 1 and there are no other blocking of interrupts.
    /// (see SDM, Vol. 3C, Section 24.4.2)
    pub fn set_interrupt_window(&mut self, enable: bool) -> AxResult {
        let mut ctrl = VmcsControl32::PRIMARY_PROCBASED_EXEC_CONTROLS.read()?;
        let bits = vmcs::controls::PrimaryControls::INTERRUPT_WINDOW_EXITING.bits();
        if enable {
            ctrl |= bits
        } else {
            ctrl &= !bits
        }
        VmcsControl32::PRIMARY_PROCBASED_EXEC_CONTROLS.write(ctrl)?;
        Ok(())
    }

    /// Set I/O intercept by modifying I/O bitmap.
    pub fn set_io_intercept_of_range(&mut self, port_base: u32, count: u32, intercept: bool) {
        self.io_bitmap
            .set_intercept_of_range(port_base, count, intercept)
    }

    /// Set msr intercept by modifying msr bitmap.
    /// Todo: distinguish read and write.
    pub fn set_msr_intercept_of_range(&mut self, msr: u32, intercept: bool) {
        self.msr_bitmap.set_read_intercept(msr, intercept);
        self.msr_bitmap.set_write_intercept(msr, intercept);
    }
}

/// Get ready then vmlaunch or vmresume.
macro_rules! vmx_entry_with {
    ($instr:literal) => {
        naked_asm!(
            "pushfq",                                  // save host RFLAGS, including IF
            "pop    qword ptr [rdi + {host_rflags}]",
            save_regs_to_stack!(),                      // save host status
            "mov    [rdi + {host_stack_size}], rsp",    // save current RSP to Vcpu::host_stack_top
            "mov    rsp, rdi",                          // set RSP to guest regs area
            restore_regs_from_stack!(),                 // restore guest status
            $instr,                                     // let's go!
            "jmp    {failed}",
            host_stack_size = const size_of::<GeneralRegisters>(),
            host_rflags = const size_of::<GeneralRegisters>() + size_of::<u64>(),
            failed = sym Self::vmx_entry_failed,
            // options(noreturn),
        )
    }
}

impl VmxVcpu {
    #[unsafe(naked)]
    /// Enter guest with vmlaunch.
    ///
    /// `#[naked]` is essential here, without it the rust compiler will think `&mut self` is not used and won't give us correct %rdi.
    ///
    /// This function itself never returns, but [`Self::vmx_exit`] will do the return for this.
    ///
    /// The return value is a dummy value.
    unsafe extern "C" fn vmx_launch(&mut self) -> usize {
        vmx_entry_with!("vmlaunch")
    }

    #[unsafe(naked)]
    /// Enter guest with vmresume.
    ///
    /// See [`Self::vmx_launch`] for detail.
    unsafe extern "C" fn vmx_resume(&mut self) -> usize {
        vmx_entry_with!("vmresume")
    }

    #[unsafe(naked)]
    /// Return after vm-exit. This function is used only for returning from [`Self::vmx_launch`] or [`Self::vmx_resume`].
    ///
    /// NEVER call this function directly.
    ///
    /// The return value is a dummy value.
    pub(super) unsafe extern "C" fn vmx_exit(&mut self) -> usize {
        // it's not necessary to use another `unsafe` here, as Rust now do not require it in naked functions.
        naked_asm!(
            "cli",                                  // keep host IRQs off until host xstate is restored
            save_regs_to_stack!(),                  // save guest status, after this, rsp points to the `VmxVcpu`
            "mov    rsp, [rsp + {host_stack_top}]", // set RSP to Vcpu::host_stack_top
            restore_regs_from_stack!(),             // restore host status
            "ret",
            host_stack_top = const size_of::<GeneralRegisters>(),
        );
    }

    fn vmx_entry_failed() -> ! {
        let instr_err = vmcs::instruction_error();
        let exit_reason = super::vmcs::VmcsReadOnly32::EXIT_REASON.read().unwrap_or(0);
        let exit_qual = super::vmcs::VmcsReadOnlyNW::EXIT_QUALIFICATION
            .read()
            .unwrap_or(0);
        let idt_vec = super::vmcs::VmcsReadOnly32::IDT_VECTORING_INFO
            .read()
            .unwrap_or(0);
        let idt_err = super::vmcs::VmcsReadOnly32::IDT_VECTORING_ERR_CODE
            .read()
            .unwrap_or(0);
        let instr_len = super::vmcs::VmcsReadOnly32::VMEXIT_INSTRUCTION_LEN
            .read()
            .unwrap_or(0);
        let pin_ctrl = VmcsControl32::PINBASED_EXEC_CONTROLS.read().unwrap_or(0);
        let prim_ctrl = VmcsControl32::PRIMARY_PROCBASED_EXEC_CONTROLS
            .read()
            .unwrap_or(0);
        let sec_ctrl = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS
            .read()
            .unwrap_or(0);
        let entry_ctrl = VmcsControl32::VMENTRY_CONTROLS.read().unwrap_or(0);
        let exit_ctrl = VmcsControl32::VMEXIT_CONTROLS.read().unwrap_or(0);
        let eptp = super::vmcs::VmcsControl64::EPTP.read().unwrap_or(0);
        let guest_efer = VmcsGuest64::IA32_EFER.read().unwrap_or(0);
        let guest_cr0 = VmcsGuestNW::CR0.read().unwrap_or(0);
        let guest_cr4 = VmcsGuestNW::CR4.read().unwrap_or(0);
        let guest_rip = VmcsGuestNW::RIP.read().unwrap_or(0);
        let guest_cs = VmcsGuest16::CS_SELECTOR.read().unwrap_or(0);
        let guest_cs_base = VmcsGuestNW::CS_BASE.read().unwrap_or(0);
        let guest_cs_ar = VmcsGuest32::CS_ACCESS_RIGHTS.read().unwrap_or(0);
        let host_cr0 = VmcsHostNW::CR0.read().unwrap_or(0);
        let host_cr4 = VmcsHostNW::CR4.read().unwrap_or(0);
        let host_efer = super::vmcs::VmcsHost64::IA32_EFER.read().unwrap_or(0);
        panic!(
            "{}: exit_reason=0x{:x} exit_qual=0x{:x} idt_vec=0x{:x} idt_err=0x{:x} \
             instr_len={}\nPIN_CTRL=0x{:x} PRIM_CTRL=0x{:x} SEC_CTRL=0x{:x}\nENTRY_CTRL=0x{:x} \
             EXIT_CTRL=0x{:x} EPTP=0x{:x}\nG_CR0=0x{:x} G_CR4=0x{:x} G_RIP=0x{:x} \
             G_EFER=0x{:x}\nG_CS=0x{:x} G_CS_BASE=0x{:x} G_CS_AR=0x{:x}\nH_CR0=0x{:x} \
             H_CR4=0x{:x} H_EFER=0x{:x}",
            instr_err.as_str(),
            exit_reason,
            exit_qual,
            idt_vec,
            idt_err,
            instr_len,
            pin_ctrl,
            prim_ctrl,
            sec_ctrl,
            entry_ctrl,
            exit_ctrl,
            eptp,
            guest_cr0,
            guest_cr4,
            guest_rip,
            guest_efer,
            guest_cs,
            guest_cs_base,
            guest_cs_ar,
            host_cr0,
            host_cr4,
            host_efer,
        )
    }

    pub(super) fn load_guest_xstate(&mut self) {
        self.xstate.switch_to_guest();
    }

    pub(super) fn load_host_xstate(&mut self) {
        self.xstate.switch_to_host();
    }
}

impl Drop for VmxVcpu {
    fn drop(&mut self) {
        unsafe { vmx::vmclear(self.vmcs.phys_addr().as_usize() as u64).unwrap() };
        info!("[HV] dropped VmxVcpu(vmcs: {:#x})", self.vmcs.phys_addr());
    }
}

impl Debug for VmxVcpu {
    fn fmt(&self, f: &mut Formatter) -> Result {
        (|| -> AxResult<Result> {
            Ok(f.debug_struct("VmxVcpu")
                .field("guest_regs", &self.guest_regs)
                .field("rip", &VmcsGuestNW::RIP.read()?)
                .field("rsp", &VmcsGuestNW::RSP.read()?)
                .field("rflags", &VmcsGuestNW::RFLAGS.read()?)
                .field("cr0", &VmcsGuestNW::CR0.read()?)
                .field("cr3", &VmcsGuestNW::CR3.read()?)
                .field("cr4", &VmcsGuestNW::CR4.read()?)
                .field("cs", &VmcsGuest16::CS_SELECTOR.read()?)
                .field("fs_base", &VmcsGuestNW::FS_BASE.read()?)
                .field("gs_base", &VmcsGuestNW::GS_BASE.read()?)
                .field("tss", &VmcsGuest16::TR_SELECTOR.read()?)
                .finish())
        })()
        .unwrap()
    }
}

impl AxArchVCpu for VmxVcpu {
    type CreateConfig = ();

    type SetupConfig = X86VCpuSetupConfig;

    fn new(vm_id: VMId, vcpu_id: VCpuId, _config: Self::CreateConfig) -> AxResult<Self> {
        Self::new(vm_id, vcpu_id)
    }

    fn set_entry(&mut self, entry: GuestPhysAddr) -> AxResult {
        self.entry = Some(entry);
        Ok(())
    }

    fn set_ept_root(&mut self, ept_root: HostPhysAddr) -> AxResult {
        self.ept_root = Some(ept_root);
        Ok(())
    }

    fn setup(&mut self, config: Self::SetupConfig) -> AxResult {
        self.setup_vmcs(
            self.entry.unwrap(),
            self.ept_root.unwrap(),
            config.boot_mode,
        )?;
        self.ram_end = config.ram_size;
        Ok(())
    }

    fn run(&mut self) -> AxResult<AxVCpuExitReason> {
        let result = match self.inner_run() {
            Some(exit_info) => Ok(if exit_info.entry_failure {
                let exit_reason_raw = exit_info.exit_reason as u64;
                self.diag_entry_failure(exit_reason_raw)?
            } else {
                match exit_info.exit_reason {
                    VmxExitReason::VMCALL => {
                        self.advance_rip(exit_info.exit_instruction_length as _)?;
                        AxVCpuExitReason::Hypercall {
                            nr: self.regs().rax,
                            args: [
                                self.regs().rdi,
                                self.regs().rsi,
                                self.regs().rdx,
                                self.regs().rcx,
                                self.regs().r8,
                                self.regs().r9,
                            ],
                        }
                    }
                    VmxExitReason::IO_INSTRUCTION => {
                        let io_info = self.io_exit_info().unwrap();
                        self.advance_rip(exit_info.exit_instruction_length as _)?;

                        let port = io_info.port;

                        // Diagnostic: track hot IO ports to identify polling loops
                        {
                            static IO_PORT_COUNTS: core::sync::atomic::AtomicU64 =
                                core::sync::atomic::AtomicU64::new(0);
                            static LAST_HOT_PORT: core::sync::atomic::AtomicU16 =
                                core::sync::atomic::AtomicU16::new(0);
                            static LAST_HOT_PORT_COUNT: core::sync::atomic::AtomicU64 =
                                core::sync::atomic::AtomicU64::new(0);
                            let total =
                                IO_PORT_COUNTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                            let prev_port =
                                LAST_HOT_PORT.load(core::sync::atomic::Ordering::Relaxed);
                            if port != prev_port {
                                let prev_count = LAST_HOT_PORT_COUNT
                                    .swap(0, core::sync::atomic::Ordering::Relaxed);
                                if prev_count > 1000 {
                                    info!(
                                        "[IO-HOT] port={:#x} count={} (was hot for {} ops), now \
                                         switching to port={:#x} RIP={:#x}",
                                        prev_port,
                                        prev_count,
                                        prev_count,
                                        port,
                                        self.rip()
                                    );
                                }
                                LAST_HOT_PORT.store(port, core::sync::atomic::Ordering::Relaxed);
                            }
                            LAST_HOT_PORT_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                            if total.is_multiple_of(50000) && total > 0 {
                                let hot_port =
                                    LAST_HOT_PORT.load(core::sync::atomic::Ordering::Relaxed);
                                let hot_count =
                                    LAST_HOT_PORT_COUNT.load(core::sync::atomic::Ordering::Relaxed);
                                info!(
                                    "[IO-STAT] total_io={} hot_port={:#x} hot_count={} RIP={:#x}",
                                    total,
                                    hot_port,
                                    hot_count,
                                    self.rip()
                                );
                                // One-time code dump for the hot IO port location
                                // — commented out, too verbose.
                                // static IO_CODE_DUMPED: core::sync::atomic::AtomicBool =
                                //     core::sync::atomic::AtomicBool::new(false);
                                // if !IO_CODE_DUMPED.swap(true, core::sync::atomic::Ordering::Relaxed)
                                //     && let Some(ept_root) = self.ept_root
                                // {
                                //     let rip = self.rip() as u64;
                                //     let base_gpa = rip.saturating_sub(16) & !0x7u64;
                                //     let mut dump = [0u8; 64];
                                //     for (i, byte) in dump.iter_mut().enumerate() {
                                //         let gpa = base_gpa + i as u64;
                                //         if let Some(hpa) = self.gpa_to_hpa_via_ept(ept_root, gpa) {
                                //             const PHYS_VIRT_OFFSET: u64 = 0xffff_8000_0000_0000;
                                //             *byte = unsafe {
                                //                 core::ptr::read_volatile(
                                //                     ((hpa as u64) + PHYS_VIRT_OFFSET) as *const u8,
                                //                 )
                                //             };
                                //         }
                                //     }
                                //     let rax = self.regs().rax;
                                //     let rcx = self.regs().rcx;
                                //     let rdx = self.regs().rdx;
                                //     let rflags = VmcsGuestNW::RFLAGS.read().unwrap_or(0);
                                //     info!(
                                //         "[IO-CODE] RIP={:#x} RAX={:#x} RCX={:#x} RDX={:#x} \
                                //          RFLAGS={:#x} is_in={} access_size={} code={:02x?}",
                                //         rip, rax, rcx, rdx, rflags,
                                //         io_info.is_in, io_info.access_size, &dump[..64]
                                //     );
                                // }
                            }
                        }

                        // Log fw_cfg port accesses (0x510-0x51B) at debug level
                        // if (0x510..=0x51B).contains(&port) {
                        //     let dir = if io_info.is_in { "IN" } else { "OUT" };
                        //     let data = if io_info.is_in { 0 } else { self.regs().rax & 0xffffffff };
                        //     debug!(
                        //         "[IO-fw_cfg] port={:#x} dir={} width={} string={} rep={} data={:#x}",
                        //         port, dir, io_info.access_size, io_info.is_string, io_info.is_repeat, data
                        //     );
                        // }

                        // Diagnostic: log non-PM-TIMER, non-keyboard port
                        // accesses — commented out, too noisy.
                        // if !(0x600..=0x60B).contains(&port) && port != 0x60 && port != 0x64 {
                        //     let dir = if io_info.is_in { "IN" } else { "OUT" };
                        //     let data = if io_info.is_in { 0 } else { self.regs().rax & 0xffffffff };
                        //     debug!(
                        //         "[IO-ALL] port={:#x} dir={} size={} RIP={:#x} data={:#x}",
                        //         port, dir, io_info.access_size, self.rip(), data
                        //     );
                        // }

                        let width = match axaddrspace::device::AccessWidth::try_from(
                            io_info.access_size as usize,
                        ) {
                            Ok(width) => width,
                            Err(_) => {
                                warn!("VMX invalid IO-Exit: {io_info:#x?} of {exit_info:#x?}");
                                return Ok(AxVCpuExitReason::Halt);
                            }
                        };

                        if io_info.is_string {
                            // String I/O: ins/outs with optional rep prefix
                            let count = if io_info.is_repeat {
                                self.regs().rcx
                            } else {
                                1
                            };
                            let dir_down = VmcsGuestNW::RFLAGS.read().unwrap_or(0) & (1 << 10) != 0; // DF flag

                            if io_info.is_in {
                                // rep insb/w/d: read from port, write to [RDI]
                                AxVCpuExitReason::IoStringIn {
                                    port: Port(port),
                                    width,
                                    count,
                                    guest_addr: self.regs().rdi,
                                    dir_down,
                                }
                            } else {
                                // rep outsb/w/d: read from [RSI], write to port
                                AxVCpuExitReason::IoStringOut {
                                    port: Port(port),
                                    width,
                                    count,
                                    guest_addr: self.regs().rsi,
                                    dir_down,
                                }
                            }
                        } else if io_info.is_in {
                            AxVCpuExitReason::IoRead {
                                port: Port(port),
                                width,
                            }
                        } else if port == QEMU_EXIT_PORT
                            && width == axaddrspace::device::AccessWidth::Word
                            && self.regs().rax == QEMU_EXIT_MAGIC
                        {
                            AxVCpuExitReason::SystemDown
                        } else {
                            AxVCpuExitReason::IoWrite {
                                port: Port(port),
                                width,
                                data: self.regs().rax.get_bits(width.bits_range()),
                            }
                        }
                    }
                    VmxExitReason::EXTERNAL_INTERRUPT => {
                        let int_info = self.interrupt_exit_info()?;
                        assert!(int_info.valid);
                        AxVCpuExitReason::ExternalInterrupt {
                            vector: int_info.vector as _,
                        }
                    }
                    VmxExitReason::HLT => {
                        self.handle_hlt()?;
                        AxVCpuExitReason::Hlt
                    }
                    VmxExitReason::MSR_READ => {
                        // RDMSR is a 2-byte instruction (0F 32).  Advance RIP
                        // here so that the guest continues at the next
                        // instruction after the VMM emulates the read.
                        // Without this, the guest would re-execute the same
                        // RDMSR forever (the built-in handler advances RIP
                        // itself, but the propagated path did not).
                        self.advance_rip(2).ok();
                        // `reg` is unused here.
                        AxVCpuExitReason::SysRegRead {
                            addr: axaddrspace::device::SysRegAddr::new(self.regs().rcx as _),
                            reg: 0,
                        }
                    }
                    VmxExitReason::EPT_VIOLATION => {
                        let info = self.nested_page_fault_info()?;
                        let gpa = info.fault_guest_paddr.as_usize();

                        if gpa >= x86_vioapic::IOAPIC_MMIO_BASE as usize
                            && gpa
                                < (x86_vioapic::IOAPIC_MMIO_BASE + x86_vioapic::IOAPIC_MMIO_SIZE)
                                    as usize
                        {
                            let instr_len = super::vmcs::VmcsReadOnly32::VMEXIT_INSTRUCTION_LEN
                                .read()
                                .unwrap_or(0);
                            // Intel SDM: VMEXIT_INSTRUCTION_LEN is undefined for
                            // EPT violations. Decode the instruction bytes to get
                            // both the length and the destination/source register,
                            // falling back to RAX/Dword if decoding fails.
                            let (instr_len, reg, width) = if let Some((bytes, actual_len)) =
                                self.read_guest_instr_bytes(15)
                            {
                                if let Some((reg, width, decoded_len)) =
                                    Self::decode_mmio_mov_instr(&bytes[..actual_len])
                                {
                                    let len = if instr_len > 0 {
                                        instr_len as u8
                                    } else {
                                        decoded_len.max(1)
                                    };
                                    (len, reg, width)
                                } else {
                                    let len = if instr_len > 0 {
                                        instr_len as u8
                                    } else {
                                        Self::decode_x86_instruction_length(&bytes[..actual_len])
                                            .max(1)
                                    };
                                    (len, 0u8, axaddrspace::device::AccessWidth::Dword)
                                }
                            } else {
                                let len = if instr_len > 0 { instr_len as u8 } else { 2 };
                                (len, 0u8, axaddrspace::device::AccessWidth::Dword)
                            };
                            self.advance_rip(instr_len)?;
                            if info.access_flags.contains(axaddrspace::MappingFlags::WRITE) {
                                let data = self.regs().get_reg_of_index(reg);
                                return Ok(AxVCpuExitReason::MmioWrite {
                                    addr: info.fault_guest_paddr,
                                    width,
                                    data,
                                });
                            } else {
                                return Ok(AxVCpuExitReason::MmioRead {
                                    addr: info.fault_guest_paddr,
                                    width,
                                    reg: reg as usize,
                                    reg_width: width,
                                    signed_ext: false,
                                });
                            }
                        }

                        // ECAM MMIO: route to PCI host bridge's ECAM interface via
                        // the VMM's MMIO dispatch. OVMF accesses PCI config space
                        // via ECAM at 0xB000_0000 (MCFG ACPI table).
                        if (ECAM_MMIO_BASE..ECAM_MMIO_END).contains(&gpa) {
                            // Intel SDM: VMEXIT_INSTRUCTION_LEN is undefined for
                            // EPT violations. Always decode the instruction bytes
                            // to get both the length and the destination/source
                            // register.
                            let (instr_len, reg, width) = if let Some((bytes, actual_len)) =
                                self.read_guest_instr_bytes(15)
                            {
                                if let Some((reg, width, decoded_len)) =
                                    Self::decode_mmio_mov_instr(&bytes[..actual_len])
                                {
                                    (decoded_len.max(1), reg, width)
                                } else {
                                    // Not a MOV instruction; fall back to RAX
                                    let len =
                                        Self::decode_x86_instruction_length(&bytes[..actual_len])
                                            .max(1);
                                    (len, 0u8, axaddrspace::device::AccessWidth::Dword)
                                }
                            } else {
                                // Cannot read instruction bytes; fall back
                                (2u8, 0u8, axaddrspace::device::AccessWidth::Dword)
                            };

                            self.advance_rip(instr_len as _)?;
                            if info.access_flags.contains(axaddrspace::MappingFlags::WRITE) {
                                let data = self.regs().get_reg_of_index(reg);
                                return Ok(AxVCpuExitReason::MmioWrite {
                                    addr: info.fault_guest_paddr,
                                    width,
                                    data,
                                });
                            } else {
                                return Ok(AxVCpuExitReason::MmioRead {
                                    addr: info.fault_guest_paddr,
                                    width,
                                    reg: reg as usize,
                                    reg_width: width,
                                    signed_ext: false,
                                });
                            }
                        }

                        // Execute beyond ram_end: inject #PF
                        if self.ram_end > 0
                            && gpa >= self.ram_end
                            && info
                                .access_flags
                                .contains(axaddrspace::MappingFlags::EXECUTE)
                        {
                            // Intel SDM: VMEXIT_INSTRUCTION_LEN is undefined for EPT violations.
                            self.advance_rip(2)?;
                            self.queue_event(14, Some(1 << 4));
                            return Ok(AxVCpuExitReason::Nothing);
                        }

                        // info!(
                        //     "EPT_VIOLATION: GPA={:#x}, access={:?}, RIP={:#x}",
                        //     info.fault_guest_paddr, info.access_flags, self.rip()
                        // );
                        AxVCpuExitReason::NestedPageFault {
                            addr: info.fault_guest_paddr,
                            access_flags: info.access_flags,
                        }
                    }
                    VmxExitReason::MSR_WRITE => {
                        // WRMSR is a 2-byte instruction (0F 30).  Advance RIP
                        // here for the same reason as MSR_READ above.
                        self.advance_rip(2).ok();
                        let value = (self.regs().rax & 0xffff_ffff)
                            | ((self.regs().rdx & 0xffff_ffff) << 32);
                        AxVCpuExitReason::SysRegWrite {
                            addr: axaddrspace::device::SysRegAddr::new(self.regs().rcx as _),
                            value,
                        }
                    }
                    VmxExitReason::EXCEPTION_NMI => {
                        let int_info = self.interrupt_exit_info()?;
                        if !int_info.valid {
                            warn!("VMX EXCEPTION_NMI: invalid interrupt info");
                            return Ok(AxVCpuExitReason::Halt);
                        }
                        let vector = int_info.vector;
                        let int_type = int_info.int_type;
                        match int_type {
                            VmxInterruptionType::NMI => {
                                info!("VMX EXCEPTION_NMI: NMI received, injecting");
                                self.queue_event(vector, None);
                                AxVCpuExitReason::Nothing
                            }
                            VmxInterruptionType::HardException
                            | VmxInterruptionType::SoftException
                            | VmxInterruptionType::PrivSoftException => {
                                let rip = self.rip();
                                let cs_base = VmcsGuestNW::CS_BASE.read().unwrap_or(0);
                                let cs_selector = VmcsGuest16::CS_SELECTOR.read().unwrap_or(0);
                                let cs_ar = VmcsGuest32::CS_ACCESS_RIGHTS.read().unwrap_or(0);
                                let cr0 = VmcsGuestNW::CR0.read().unwrap_or(0);
                                let linear = cs_base + rip;
                                let idt_vec = vmcs::idt_vectoring_info().ok().flatten();
                                info!(
                                    "VMX EXCEPTION_NMI: inject exception vector={}, type={:?}, \
                                     err={:?}, RIP={:#x}, CS={:#x} BASE={:#x} AR={:#x}, \
                                     CR0={:#x}, linear={:#x}, IDT-vec={:?}",
                                    vector,
                                    int_type,
                                    int_info.err_code,
                                    rip,
                                    cs_selector,
                                    cs_base,
                                    cs_ar,
                                    cr0,
                                    linear,
                                    idt_vec
                                );
                                self.queue_event(vector, int_info.err_code);
                                AxVCpuExitReason::Nothing
                            }
                            _ => {
                                warn!(
                                    "VMX EXCEPTION_NMI: unhandled type={:?}, vector={}, RIP={:#x}",
                                    int_type,
                                    vector,
                                    self.rip()
                                );
                                AxVCpuExitReason::Halt
                            }
                        }
                    }
                    _ => {
                        if exit_info.exit_reason == VmxExitReason::TRIPLE_FAULT {
                            let idtr_base = VmcsGuestNW::IDTR_BASE.read().unwrap_or(0);
                            let idtr_limit = VmcsGuest32::IDTR_LIMIT.read().unwrap_or(0);
                            let gdtr_base = VmcsGuestNW::GDTR_BASE.read().unwrap_or(0);
                            let gdtr_limit = VmcsGuest32::GDTR_LIMIT.read().unwrap_or(0);
                            let rsp = VmcsGuestNW::RSP.read().unwrap_or(0);
                            let rflags = VmcsGuestNW::RFLAGS.read().unwrap_or(0);
                            let cr0 = VmcsGuestNW::CR0.read().unwrap_or(0);
                            let cr3 = VmcsGuestNW::CR3.read().unwrap_or(0);
                            let cr4 = VmcsGuestNW::CR4.read().unwrap_or(0);
                            let cs = VmcsGuest16::CS_SELECTOR.read().unwrap_or(0);
                            let cs_base = VmcsGuestNW::CS_BASE.read().unwrap_or(0);
                            let cs_ar = VmcsGuest32::CS_ACCESS_RIGHTS.read().unwrap_or(0);
                            let interruptibility =
                                VmcsGuest32::INTERRUPTIBILITY_STATE.read().unwrap_or(0);
                            let idt_vec = vmcs::idt_vectoring_info().ok().flatten();
                            error!(
                                "[TRIPLE_FAULT] RIP={:#x}, RSP={:#x}, RFLAGS={:#x}, \
                                 IDTR={:#x}:{:#x}, GDTR={:#x}:{:#x}, CR0={:#x}, CR3={:#x}, \
                                 CR4={:#x}, CS={:#x}:{:#x} AR={:#x}, INTBL={:#x}, IDT-vec={:?}",
                                self.rip(),
                                rsp,
                                rflags,
                                idtr_base,
                                idtr_limit,
                                gdtr_base,
                                gdtr_limit,
                                cr0,
                                cr3,
                                cr4,
                                cs,
                                cs_base,
                                cs_ar,
                                interruptibility,
                                idt_vec
                            );
                        } else if exit_info.exit_reason == VmxExitReason::EXCEPTION_NMI
                            && let Ok(int_info) = self.interrupt_exit_info()
                        {
                            warn!(
                                "VMX EXCEPTION_NMI: vector={}, int_type={:?}, err_code={:?}, \
                                 valid={}",
                                int_info.vector,
                                int_info.int_type,
                                int_info.err_code,
                                int_info.valid
                            );
                        }
                        warn!("VMX unsupported VM-Exit: {exit_info:#x?}");
                        warn!("VCpu {self:#x?}");
                        AxVCpuExitReason::Halt
                    }
                }
            }),
            None => Ok(AxVCpuExitReason::Nothing),
        };

        if let Some(reason) = self.take_smp_init_sipi() {
            return Ok(reason);
        }

        result
    }

    fn bind(&mut self) -> AxResult {
        self.bind_to_current_processor()
    }

    fn unbind(&mut self) -> AxResult {
        self.launched = false;
        self.unbind_from_current_processor()
    }

    fn set_gpr(&mut self, reg: usize, val: usize) {
        self.regs_mut().set_reg_of_index(reg as u8, val as u64);
    }

    fn inject_interrupt(&mut self, vector: usize) -> AxResult {
        if vector == 0 {
            warn!("inject_interrupt called with vector 0, ignoring");
            return Ok(());
        }
        self.queue_external_interrupt(vector as u8);
        Ok(())
    }

    fn handle_timer_expired(&mut self) -> AxResult {
        let vector = self.vlapic.timer_vector();
        let is_masked = self.vlapic.timer_is_masked();
        let is_periodic = self.vlapic.timer_is_periodic();

        // Rate-limited info logging for timer expirations
        static TIMER_EXPIRE_COUNT: core::sync::atomic::AtomicU64 =
            core::sync::atomic::AtomicU64::new(0);
        let count = TIMER_EXPIRE_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if count < 3 || count == 100 || count == 1000 || count == 10000 {
            info!(
                "[VLAPIC] timer expired #{}: vector={:#x}, masked={}, periodic={}",
                count, vector, is_masked, is_periodic
            );
        }

        // Per Intel SDM Vol. 3A Section 10.5.4: when the APIC timer expires
        // while LVT_TIMER is masked, the interrupt is held pending
        // (Delivery Status = SendPending). It will be delivered when the
        // guest unmasks LVT_TIMER.
        // When unmasked: set IRR and queue for VMCS injection.
        // When masked: do NOT set IRR; the pending state is tracked by
        // the timer hardware and will fire when unmasked.
        if vector > 0 && !is_masked {
            let vcpu_id = self.vlapic.timer_where_am_i().1 as u32;
            self.vlapic.set_intr(vcpu_id, vector as u32);
            self.queue_external_interrupt(vector);
        } else if is_masked {
            debug!("[VLAPIC] timer expired but masked, interrupt held pending");
        }

        if is_periodic {
            self.vlapic.timer_restart()?;
        }

        Ok(())
    }

    fn set_return_value(&mut self, val: usize) {
        self.regs_mut().rax = val as u64;
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use super::*;

    #[test]
    fn test_vm_cpu_mode_enum() {
        // Test VmCpuMode enum values
        assert_ne!(VmCpuMode::Real, VmCpuMode::Protected);
        assert_ne!(VmCpuMode::Protected, VmCpuMode::Compatibility);
        assert_ne!(VmCpuMode::Compatibility, VmCpuMode::Mode64);

        // Test Debug formatting
        let debug_str = format!("{:?}", VmCpuMode::Mode64);
        assert!(debug_str.contains("Mode64"));
    }

    #[test]
    fn test_general_registers_operations() {
        let mut regs = GeneralRegisters::default();

        // Test initial state
        assert_eq!(regs.rax, 0);
        assert_eq!(regs.rbx, 0);

        // Test setting and getting values
        regs.rax = 0x1234567890abcdef;
        regs.rbx = 0xfedcba0987654321;

        assert_eq!(regs.rax, 0x1234567890abcdef);
        assert_eq!(regs.rbx, 0xfedcba0987654321);

        // Test register access by index
        regs.set_reg_of_index(0, 0x1111111111111111); // RAX
        assert_eq!(regs.get_reg_of_index(0), 0x1111111111111111);

        regs.set_reg_of_index(1, 0x2222222222222222); // RCX
        assert_eq!(regs.get_reg_of_index(1), 0x2222222222222222);
    }

    #[test]
    fn test_constants() {
        // Test that constants have expected values
        assert_eq!(VMX_PREEMPTION_TIMER_SET_VALUE, 1_000_000);
        assert_eq!(QEMU_EXIT_PORT, 0x604);
        assert_eq!(QEMU_EXIT_MAGIC, 0x2000);
        assert_eq!(MSR_IA32_EFER_LMA_BIT, 1 << 10);
        assert_eq!(CR0_PE, 1 << 0);
    }

    #[test]
    fn test_bit_operations() {
        use bit_field::BitField;

        let mut value = 0u64;
        value.set_bits(0..32, 0x12345678);
        value.set_bits(32..64, 0xabcdef00);

        assert_eq!(value.get_bits(0..32), 0x12345678);
        assert_eq!(value.get_bits(32..64), 0xabcdef00);
    }

    // Mock tests for VmxVcpu (limited to safe operations)
    mod vmx_vcpu_tests {
        use super::*;

        // Helper function to create a test VmxVcpu (this would normally require VMX hardware)
        fn create_test_vcpu_regs() -> GeneralRegisters {
            let mut regs = GeneralRegisters::default();
            regs.rax = 0x1000;
            regs.rbx = 0x2000;
            regs.rcx = 0x3000;
            regs.rdx = 0x4000;
            regs
        }

        #[test]
        fn test_general_registers_clone() {
            let regs = create_test_vcpu_regs();
            let cloned_regs = regs.clone();

            assert_eq!(regs.rax, cloned_regs.rax);
            assert_eq!(regs.rbx, cloned_regs.rbx);
            assert_eq!(regs.rcx, cloned_regs.rcx);
            assert_eq!(regs.rdx, cloned_regs.rdx);
        }

        #[test]
        fn test_edx_eax_operations() {
            // Test the logic for combining EDX:EAX
            let rax = 0x12345678u64;
            let rdx = 0xabcdef00u64;

            // Simulate read_edx_eax logic
            let combined = ((rdx & 0xffff_ffff) << 32) | (rax & 0xffff_ffff);
            assert_eq!(combined, 0xabcdef0012345678);

            // Simulate write_edx_eax logic
            let val = 0xfedcba0987654321u64;
            let new_rax = val & 0xffff_ffff;
            let new_rdx = val >> 32;

            assert_eq!(new_rax, 0x87654321);
            assert_eq!(new_rdx, 0xfedcba09);
        }

        #[test]
        fn test_register_bit_operations() {
            let mut regs = GeneralRegisters::default();

            // Test setting specific bits in registers
            regs.rcx = 0;
            regs.rcx.set_bits(0..32, 0x12345678);
            assert_eq!(regs.rcx.get_bits(0..32), 0x12345678);

            regs.rdx = 0xffffffffffffffff;
            regs.rdx.set_bits(32..64, 0);
            assert_eq!(regs.rdx.get_bits(32..64), 0);
            assert_eq!(regs.rdx.get_bits(0..32), 0xffffffff);
        }

        #[test]
        fn test_gla2gva_logic() {
            // Test the address translation logic (without actual VMX hardware)
            let guest_rip = 0x1000usize;
            let seg_base_64bit = 0; // In 64-bit mode, segment base is 0
            let seg_base_other = 0x10000; // In other modes, segment base matters

            // 64-bit mode calculation
            let gva_64bit = guest_rip + seg_base_64bit;
            assert_eq!(gva_64bit, 0x1000);

            // Other mode calculation
            let gva_other = guest_rip + seg_base_other;
            assert_eq!(gva_other, 0x11000);
        }

        #[test]
        fn test_interrupt_vector_validation() {
            // Test interrupt vector validation logic
            let valid_exception = 6; // #UD exception
            let valid_interrupt = 0x20;
            let invalid_vector = 0;

            assert!(valid_exception < 32); // Exceptions are < 32
            assert!(valid_interrupt >= 32); // Interrupts are >= 32
            assert_eq!(invalid_vector, 0); // Vector 0 should be handled specially
        }

        #[test]
        fn test_page_walk_info_struct() {
            let ptw_info = GuestPageWalkInfo {
                top_entry: 0x1000,
                level: 4,
                width: 9,
                is_user_mode_access: false,
                is_write_access: false,
                is_inst_fetch: false,
                pse: true,
                wp: true,
                nxe: true,
                is_smap_on: false,
                is_smep_on: false,
            };

            assert_eq!(ptw_info.level, 4);
            assert_eq!(ptw_info.width, 9);
            assert_eq!(ptw_info.top_entry, 0x1000);
        }

        #[test]
        fn test_cpuid_constants() {
            // Test CPUID-related constants used in handle_cpuid
            const LEAF_FEATURE_INFO: u32 = 0x1;
            const LEAF_HYPERVISOR_INFO: u32 = 0x4000_0000;
            const FEATURE_VMX: u32 = 1 << 5;
            const FEATURE_HYPERVISOR: u32 = 1 << 31;

            assert_eq!(LEAF_FEATURE_INFO, 1);
            assert_eq!(LEAF_HYPERVISOR_INFO, 0x40000000);
            assert_eq!(FEATURE_VMX, 32);
            assert_eq!(FEATURE_HYPERVISOR, 0x80000000);
        }

        #[test]
        fn test_cr_flags_operations() {
            use x86_64::registers::control::{Cr0Flags, Cr4Flags};

            // Test CR0 flags
            let cr0_flags = Cr0Flags::PAGING | Cr0Flags::PROTECTED_MODE_ENABLE;
            assert!(cr0_flags.contains(Cr0Flags::PAGING));
            assert!(cr0_flags.contains(Cr0Flags::PROTECTED_MODE_ENABLE));
            assert!(!cr0_flags.contains(Cr0Flags::CACHE_DISABLE));

            // Test CR4 flags
            let cr4_flags = Cr4Flags::VIRTUAL_MACHINE_EXTENSIONS | Cr4Flags::PAGE_SIZE_EXTENSION;
            assert!(cr4_flags.contains(Cr4Flags::VIRTUAL_MACHINE_EXTENSIONS));
            assert!(cr4_flags.contains(Cr4Flags::PAGE_SIZE_EXTENSION));
        }

        #[test]
        fn test_access_width_operations() {
            // Test access width enumeration
            use axaddrspace::device::AccessWidth;

            assert_eq!(AccessWidth::Byte as usize, 0);
            assert_eq!(AccessWidth::Word as usize, 1);
            assert_eq!(AccessWidth::Dword as usize, 2);
            assert_eq!(AccessWidth::Qword as usize, 3);

            // Test conversion
            assert_eq!(AccessWidth::try_from(1), Ok(AccessWidth::Byte));
            assert_eq!(AccessWidth::try_from(2), Ok(AccessWidth::Word));
            assert_eq!(AccessWidth::try_from(4), Ok(AccessWidth::Dword));
            assert_eq!(AccessWidth::try_from(8), Ok(AccessWidth::Qword));
        }
    }

    // Tests for utility functions that don't require hardware
    #[test]
    fn test_vmx_exit_reason_enum() {
        // Test that VmxExitReason enum can be used in match statements
        let test_reason = VmxExitReason::VMCALL;
        match test_reason {
            VmxExitReason::VMCALL => assert!(true),
            _ => assert!(false),
        }
    }

    #[test]
    fn test_debug_implementations() {
        // Test Debug implementations for various types
        let cpu_mode = VmCpuMode::Mode64;
        let debug_str = format!("{:?}", cpu_mode);
        assert!(!debug_str.is_empty());

        let regs = GeneralRegisters::default();
        let debug_str = format!("{:?}", regs);
        assert!(!debug_str.is_empty());
    }

    // Note: Most VmxVcpu methods require actual VMX hardware support and cannot be unit tested
    // without either:
    // 1. Running on VMX-capable hardware with appropriate privileges
    // 2. Extensive mocking of the entire VMX infrastructure
    //
    // For comprehensive testing of VmxVcpu, integration tests on actual hardware
    // or hardware simulators would be more appropriate.
}
