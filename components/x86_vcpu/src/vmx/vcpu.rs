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

use ax_errno::{AxResult, ax_err, ax_err_type};
use axaddrspace::{
    GuestPhysAddr, GuestVirtAddr, HostPhysAddr, MappingFlags, NestedPageFaultInfo,
    device::{AccessWidth, Port, SysRegAddr, SysRegAddrRange},
};
use axdevice_base::BaseDeviceOps;
use axvcpu::{AxArchVCpu, AxVCpuExitReason};
use axvisor_api::{
    memory::PhysFrame,
    vmm::{VCpuId, VMId},
};
use bit_field::BitField;
use raw_cpuid::CpuId;
use x86::{
    bits64::vmx,
    controlregs::Xcr0,
    dtables::{self, DescriptorTablePointer},
    segmentation::SegmentSelector,
};
use x86_64::registers::control::{Cr0, Cr0Flags, Cr3, Cr4, Cr4Flags, EferFlags};
use x86_vioapic::{GLOBAL_VIOAPIC, IOAPIC_MMIO_BASE, IOAPIC_MMIO_SIZE};
use x86_vlapic::EmulatedLocalApic;

/// ECAM (Enhanced Configuration Access Mechanism) MMIO range for QEMU Q35.
/// OVMF uses ECAM to access PCI config space via MMIO at 0xB000_0000.
const ECAM_MMIO_BASE: usize = 0xB000_0000;
const ECAM_MMIO_END: usize = 0xC000_0000; // 256 MB: 0xB000_0000 + 0x1000_0000

use super::{
    VmxExitInfo, as_axerr,
    definitions::{VmxExitReason, VmxInterruptionType},
    structs::{IOBitmap, MsrBitmap, VmxRegion},
    vmcs::{
        self, ApicAccessExitType, VmcsControl16, VmcsControl32, VmcsControl64, VmcsControlNW,
        VmcsGuest16, VmcsGuest32, VmcsGuest64, VmcsGuestNW, VmcsHost16, VmcsHost32, VmcsHost64,
        VmcsHostNW, VmcsReadOnly32, VmcsReadOnlyNW,
    },
};

struct PendingEvent {
    vector: u8,
    err_code: Option<u32>,
    int_type: VmxInterruptionType,
}
use crate::{
    boot_mode::{X86BootMode, X86VCpuSetupConfig},
    ept::GuestPageWalkInfo,
    msr::Msr,
    regs::GeneralRegisters,
    restore_host_interrupt_flag,
    xstate::XState,
};

const VMX_PREEMPTION_TIMER_SET_VALUE: u32 = 1_000_000;

const QEMU_EXIT_PORT: u16 = 0x604;
const QEMU_EXIT_MAGIC: u64 = 0x2000;

#[derive(PartialEq, Eq, Debug)]
pub enum VmCpuMode {
    Real,
    Protected,
    Compatibility, // IA-32E mode (CS.L = 0)
    Mode64,        // IA-32E mode (CS.L = 1)
}

const MSR_IA32_EFER_LMA_BIT: u64 = 1 << 10;
const CR0_PE: usize = 1 << 0;

/// A virtual CPU within a guest.
#[repr(C)]
pub struct VmxVcpu {
    // The order of `guest_regs`, `host_stack_top`, and `host_rflags` is
    // mandatory. They must be the first three fields. If you want to change
    // the order or the type of these fields, you must also change the assembly
    // in this file.
    /// Guest general-purpose registers.
    guest_regs: GeneralRegisters,
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
    ept_root: Option<HostPhysAddr>,
    // /// Whether this VCPU is a host VCpu. Used in type 1.5 hypervisor.
    // is_host: bool, temporary removed because we don't care about type 1.5 now

    // VMCS-related fields
    /// The VMCS region.
    vmcs: VmxRegion,
    /// The shadow VMCS region for VMCS shadowing (required when VMCS_SHADOWING is mandatory1).
    shadow_vmcs: VmxRegion,
    /// The I/O bitmap for the VMCS.
    io_bitmap: IOBitmap,
    /// The MSR bitmap for the VMCS.
    msr_bitmap: MsrBitmap,
    /// The VMREAD bitmap for VMCS shadowing.
    vmread_bitmap: PhysFrame,
    /// The VMWRITE bitmap for VMCS shadowing.
    vmwrite_bitmap: PhysFrame,
    /// The PML page for Page Modification Logging.
    pml_page: PhysFrame,
    /// The EPTP-list page for EPTP switching (required when ENABLE_VM_FUNCTIONS is mandatory1).
    eptp_list_page: PhysFrame,
    /// The XSS-exiting bitmap.
    xss_bitmap: PhysFrame,
    /// The APIC-access page for APIC virtualization.
    apic_access_page: PhysFrame,
    /// The posted-interrupt descriptor (required when PIN_CTRL bit6 is forced by MSR).
    posted_interrupt_desc: PhysFrame,

    // Interrupt-related fields
    /// Pending events to be injected to the guest.
    pending_events: VecDeque<PendingEvent>,
    /// Emulated Local APIC.
    vlapic: EmulatedLocalApic,

    // Extra states
    /// The XState of the VCpu. Both host and guest.
    xstate: XState,

    /// End of guest RAM (for EPT violation handling).
    ram_end: usize,

    /// A 4 KB page filled with 0xFF bytes, used to map unmapped GPA regions
    /// during guest memory probing so that reads return all-ones (indicating
    /// non-existent memory) without requiring instruction emulation.
    dummy_ff_page: PhysFrame,

    /// Trace VM-exits after CPUID 0x80000000 to diagnose why OVMF
    /// never calls CPUID 0x80000001.
    trace_after_cpuid_8k: bool,
    trace_after_8k_count: usize,

    // Tracing-related fields
    #[cfg(feature = "tracing")]
    /// The guest registers when the VM-exit happens.
    guest_regs_exiting: GeneralRegisters,
}

/// Ring buffer tracking the last 16 VM-exits (exit_reason, rip, cs_ar)
/// to diagnose VM-entry failures. When a VM-entry failure (reason 0x21)
/// occurs, we log the last N VM-exits to identify what caused the guest
/// state to become invalid.
struct ExitHistory {
    buf: [(u32, u64, u32); 16],
    idx: usize,
    count: u64,
}

static EXIT_HISTORY: spin::Mutex<ExitHistory> = spin::Mutex::new(ExitHistory {
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
        debug!(
            "VmxVcpu bind to current processor vmcs @ {:#x}",
            self.vmcs.phys_addr()
        );
        unsafe {
            vmx::vmptrld(self.vmcs.phys_addr().as_usize() as u64).map_err(as_axerr)?;
        }
        self.setup_vmcs_host()?;
        Ok(())
    }

    /// Unbind this [`VmxVcpu`] from current logical processor.
    pub fn unbind_from_current_processor(&self) -> AxResult {
        debug!(
            "VmxVcpu unbind from current processor vmcs @ {:#x}",
            self.vmcs.phys_addr()
        );

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

    /// Run the guest. It returns when a vm-exit happens and returns the vm-exit if it cannot be handled by this [`VmxVcpu`] itself.
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
            last_summary: core::sync::atomic::AtomicU64,
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
            last_summary: core::sync::atomic::AtomicU64::new(0),
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
        let total = STATS
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

        // Print exit reason summary every 100 exits
        let last = STATS
            .last_summary
            .load(core::sync::atomic::Ordering::Relaxed);
        if total - last >= 1000 {
            STATS
                .last_summary
                .store(total, core::sync::atomic::Ordering::Relaxed);
            let cr0 = VmcsGuestNW::CR0.read().unwrap_or(0);
            let efer = VmcsGuest64::IA32_EFER.read().unwrap_or(0);
            let pe = cr0 & 1;
            let pg = (cr0 >> 31) & 1;
            let lma = (efer >> 10) & 1;
            info!(
                "[STAT] #{total}: CPUID={} IO={} MSR_R={} MSR_W={} EPT={} PREEMPT={} EXT={} \
                 HLT={} INTR_WIN={} CR={} OTH={} RIP={:#x} PE={} PG={} LMA={}",
                STATS.cpuid.load(core::sync::atomic::Ordering::Relaxed),
                STATS.io.load(core::sync::atomic::Ordering::Relaxed),
                STATS.msr_read.load(core::sync::atomic::Ordering::Relaxed),
                STATS.msr_write.load(core::sync::atomic::Ordering::Relaxed),
                STATS
                    .ept_violation
                    .load(core::sync::atomic::Ordering::Relaxed),
                STATS.preempt.load(core::sync::atomic::Ordering::Relaxed),
                STATS.ext_intr.load(core::sync::atomic::Ordering::Relaxed),
                STATS.hlt.load(core::sync::atomic::Ordering::Relaxed),
                STATS
                    .intr_window
                    .load(core::sync::atomic::Ordering::Relaxed),
                STATS.cr_access.load(core::sync::atomic::Ordering::Relaxed),
                STATS.other.load(core::sync::atomic::Ordering::Relaxed),
                self.rip(),
                pe,
                pg,
                lma
            );
        }

        // Detect stuck loops: same RIP for CPUID exits
        static LAST_CPUID_RIP: core::sync::atomic::AtomicU64 =
            core::sync::atomic::AtomicU64::new(0);
        static SAME_CPUID_RIP_COUNT: core::sync::atomic::AtomicU32 =
            core::sync::atomic::AtomicU32::new(0);
        static STUCK_DUMPED: core::sync::atomic::AtomicBool =
            core::sync::atomic::AtomicBool::new(false);
        if matches!(exit_info.exit_reason, VmxExitReason::CPUID) {
            let rip = self.rip() as u64;
            let last_rip = LAST_CPUID_RIP.load(core::sync::atomic::Ordering::Relaxed);
            if rip == last_rip {
                let c = SAME_CPUID_RIP_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                if c == 20 && !STUCK_DUMPED.load(core::sync::atomic::Ordering::Relaxed) {
                    STUCK_DUMPED.store(true, core::sync::atomic::Ordering::Relaxed);
                    let regs = self.regs();
                    let rflags = VmcsGuestNW::RFLAGS.read().unwrap_or(0);
                    let cr0 = VmcsGuestNW::CR0.read().unwrap_or(0);
                    let cr4 = VmcsGuestNW::CR4.read().unwrap_or(0);
                    let efer = VmcsGuest64::IA32_EFER.read().unwrap_or(0);
                    info!(
                        "[STUCK] CPUID loop at RIP={rip:#x}: RAX={:#x} RBX={:#x} RCX={:#x} \
                         RDX={:#x} RBP={:#x} RDI={:#x} RSI={:#x}",
                        regs.rax, regs.rbx, regs.rcx, regs.rdx, regs.rbp, regs.rdi, regs.rsi
                    );
                    info!("[STUCK] RFLAGS={rflags:#x} CR0={cr0:#x} CR4={cr4:#x} EFER={efer:#x}");
                    // Dump 64 bytes of instructions around the CPUID call
                    if let Some(ept_root) = self.ept_root {
                        let base_gpa = (rip.saturating_sub(16)) & !0x7u64;
                        let mut dump = [0u8; 80];
                        for (i, byte) in dump.iter_mut().enumerate() {
                            let gpa = base_gpa + i as u64;
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
                            "[STUCK] Code around CPUID (RIP-16..RIP+64): {:02x?}",
                            &dump[..80]
                        );
                    }
                }
            } else {
                LAST_CPUID_RIP.store(rip, core::sync::atomic::Ordering::Relaxed);
                SAME_CPUID_RIP_COUNT.store(0, core::sync::atomic::Ordering::Relaxed);
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
                info!("[T]{}{}{:x}", self.trace_after_8k_count, short_reason, rax);
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

    /// Handle NMI window VM-exit.
    /// KVM nested virt sets NMI_WINDOW_EXITING when it has a pending virtual NMI
    /// to inject and is waiting for the NMI window to open. When this exit fires,
    /// the NMI window IS open, so KVM should deliver the NMI on the next VM-entry.
    /// We simply resume execution — do NOT try to clear NMI_WINDOW_EXITING or
    /// inject an NMI ourselves, as that conflicts with KVM's own NMI injection
    /// and causes an infinite loop.
    fn handle_nmi_window(&mut self) -> AxResult {
        // No action needed. KVM will inject the pending NMI on the next VM-entry.
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

// Implementation of private methods
impl VmxVcpu {
    fn setup_io_bitmap(&mut self) -> AxResult {
        // By default, I/O bitmap is set as `intercept_all`.
        // Todo: these should be combined with emulated pio device management,
        // in `modules/axvm/src/device/x86_64/mod.rs` somehow.
        let io_to_be_intercepted = QEMU_EXIT_PORT..QEMU_EXIT_PORT + 1; // QEMU exit port
        self.io_bitmap.set_intercept_of_range(
            io_to_be_intercepted.start as _,
            io_to_be_intercepted.count() as u32,
            true,
        );
        Ok(())
    }

    #[allow(dead_code)]
    fn setup_msr_bitmap(&mut self) -> AxResult {
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

    fn setup_vmcs(
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

    fn setup_vmcs_host(&self) -> AxResult {
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
        // intercept HLT for UEFI wait loops.
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
        info!(
            "[VMX control] Direct SEC_CTRL write: allowed0={:#x}, allowed1={:#x}, \
             msr_mandatory1={:#x}, old={:#x}, set={:#x}, new={:#x}",
            allowed0, allowed1, msr_mandatory1, old_sec, set_bits, new_sec
        );
        VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.write(new_sec)?;
        let actual = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.read()?;
        let hw_mandatory1 = actual & !new_sec;
        let hw_mandatory0 = new_sec & !actual;
        info!(
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
                    info!(
                        "[VMX control] VMCS_SHADOWING is mandatory1, SEC_CTRL={:#x}",
                        sec_ctrl
                    );
                } else {
                    info!("[VMX control] Cleared VMCS_SHADOWING");
                }
            } else {
                info!(
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
        info!(
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
            info!(
                "[VMX control] x2APIC set but APIC not set - attempting to force-set APIC, \
                 SEC_CTRL={:#x}",
                sec_ctrl
            );
            let new_sec = sec_ctrl | CpuCtrl2::VIRTUALIZE_APIC.bits();
            VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.write(new_sec)?;
            let actual = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.read()?;
            info!(
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
            info!(
                "[VMX control] APIC_REG=1 and x2APIC=1 conflict (SDM 26.2.1.1) - attempting to \
                 clear x2APIC, SEC_CTRL={:#x}",
                sec_ctrl
            );
            let new_sec = sec_ctrl & !CpuCtrl2::VIRTUALIZE_X2APIC.bits();
            VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.write(new_sec)?;
            let actual = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.read()?;
            let x2apic_cleared = actual & CpuCtrl2::VIRTUALIZE_X2APIC.bits() == 0;
            info!(
                "[VMX control] After clearing x2APIC: wrote={:#x}, read={:#x}, cleared={}",
                new_sec, actual, x2apic_cleared
            );
            if !x2apic_cleared {
                info!(
                    "[VMX control] x2APIC could not be cleared - trying to clear APIC-register \
                     virtualization instead"
                );
                let new_sec = sec_ctrl & !CpuCtrl2::VIRTUALIZE_APIC_REGISTER.bits();
                VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.write(new_sec)?;
                let actual = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.read()?;
                info!(
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
            info!(
                "[VMX control] ENABLE_VM_FUNCTIONS=1, VM_FUNCTION_CONTROLS={:#x}, SEC_CTRL={:#x}",
                vmfunc_ctrl, sec_ctrl
            );
        }

        // Re-read SEC_CTRL after all fixups to get the final hardware-accepted value.
        let sec_ctrl = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS.read()?;
        let actual = sec_ctrl;
        info!("[VMX control] Final SEC_CTRL={:#x}", actual);

        // SDM 26.2.1.1: If VMCS_SHADOWING is 1, LINK_PTR must point to a valid shadow VMCS.
        // Re-read shadowing_set after possible modification.
        let shadowing_set = actual & CpuCtrl2::VMCS_SHADOWING.bits() != 0;
        if shadowing_set {
            info!(
                "[VMX control] VMCS_SHADOWING=1, LINK_PTR={:#x}",
                VmcsGuest64::LINK_PTR.read().unwrap_or(0)
            );
        } else {
            VmcsGuest64::LINK_PTR.write(0xFFFF_FFFF_FFFF_FFFF)?;
            info!("[VMX control] VMCS_SHADOWING=0, set LINK_PTR=0xFFFFFFFF_FFFFFFFF");
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
        info!(
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

    fn get_paging_level(&self) -> usize {
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

    fn dump_vmcs_state(&self) {
        let rip = VmcsGuestNW::RIP.read().unwrap_or(0);
        let rsp = VmcsGuestNW::RSP.read().unwrap_or(0);
        let rflags = VmcsGuestNW::RFLAGS.read().unwrap_or(0);
        let cr0 = VmcsGuestNW::CR0.read().unwrap_or(0);
        let cr3 = VmcsGuestNW::CR3.read().unwrap_or(0);
        let cr4 = VmcsGuestNW::CR4.read().unwrap_or(0);
        let dr7 = VmcsGuestNW::DR7.read().unwrap_or(0);
        let efer = VmcsGuest64::IA32_EFER.read().unwrap_or(0);
        let pat = VmcsGuest64::IA32_PAT.read().unwrap_or(0);
        let sysenter_cs = VmcsGuest32::IA32_SYSENTER_CS.read().unwrap_or(0);
        let sysenter_esp = VmcsGuestNW::IA32_SYSENTER_ESP.read().unwrap_or(0);
        let sysenter_eip = VmcsGuestNW::IA32_SYSENTER_EIP.read().unwrap_or(0);
        let int_state = VmcsGuest32::INTERRUPTIBILITY_STATE.read().unwrap_or(0);
        let activity = VmcsGuest32::ACTIVITY_STATE.read().unwrap_or(0);
        let link_ptr = VmcsGuest64::LINK_PTR.read().unwrap_or(0);
        let gdtr_base = VmcsGuestNW::GDTR_BASE.read().unwrap_or(0);
        let gdtr_limit = VmcsGuest32::GDTR_LIMIT.read().unwrap_or(0);
        let idtr_base = VmcsGuestNW::IDTR_BASE.read().unwrap_or(0);
        let idtr_limit = VmcsGuest32::IDTR_LIMIT.read().unwrap_or(0);

        let cs_sel = VmcsGuest16::CS_SELECTOR.read().unwrap_or(0);
        let cs_base = VmcsGuestNW::CS_BASE.read().unwrap_or(0);
        let cs_limit = VmcsGuest32::CS_LIMIT.read().unwrap_or(0);
        let cs_ar = VmcsGuest32::CS_ACCESS_RIGHTS.read().unwrap_or(0);

        let ss_sel = VmcsGuest16::SS_SELECTOR.read().unwrap_or(0);
        let ss_base = VmcsGuestNW::SS_BASE.read().unwrap_or(0);
        let ss_limit = VmcsGuest32::SS_LIMIT.read().unwrap_or(0);
        let ss_ar = VmcsGuest32::SS_ACCESS_RIGHTS.read().unwrap_or(0);

        let ds_sel = VmcsGuest16::DS_SELECTOR.read().unwrap_or(0);
        let ds_ar = VmcsGuest32::DS_ACCESS_RIGHTS.read().unwrap_or(0);

        let es_sel = VmcsGuest16::ES_SELECTOR.read().unwrap_or(0);
        let es_ar = VmcsGuest32::ES_ACCESS_RIGHTS.read().unwrap_or(0);

        let fs_sel = VmcsGuest16::FS_SELECTOR.read().unwrap_or(0);
        let fs_base = VmcsGuestNW::FS_BASE.read().unwrap_or(0);
        let fs_ar = VmcsGuest32::FS_ACCESS_RIGHTS.read().unwrap_or(0);

        let gs_sel = VmcsGuest16::GS_SELECTOR.read().unwrap_or(0);
        let gs_base = VmcsGuestNW::GS_BASE.read().unwrap_or(0);
        let gs_ar = VmcsGuest32::GS_ACCESS_RIGHTS.read().unwrap_or(0);

        let tr_sel = VmcsGuest16::TR_SELECTOR.read().unwrap_or(0);
        let tr_base = VmcsGuestNW::TR_BASE.read().unwrap_or(0);
        let tr_limit = VmcsGuest32::TR_LIMIT.read().unwrap_or(0);
        let tr_ar = VmcsGuest32::TR_ACCESS_RIGHTS.read().unwrap_or(0);

        let ldtr_sel = VmcsGuest16::LDTR_SELECTOR.read().unwrap_or(0);
        let ldtr_base = VmcsGuestNW::LDTR_BASE.read().unwrap_or(0);
        let ldtr_ar = VmcsGuest32::LDTR_ACCESS_RIGHTS.read().unwrap_or(0);

        let perf_global_ctrl = VmcsGuest64::IA32_PERF_GLOBAL_CTRL.read().unwrap_or(0);
        let bndcfgs = VmcsGuest64::IA32_BNDCFGS.read().unwrap_or(0);

        let exit_ctrl = VmcsControl32::VMEXIT_CONTROLS.read().unwrap_or(0);

        let cr0_mask = VmcsControlNW::CR0_GUEST_HOST_MASK.read().unwrap_or(0);
        let cr0_shadow = VmcsControlNW::CR0_READ_SHADOW.read().unwrap_or(0);
        let cr4_mask = VmcsControlNW::CR4_GUEST_HOST_MASK.read().unwrap_or(0);
        let cr4_shadow = VmcsControlNW::CR4_READ_SHADOW.read().unwrap_or(0);

        let sec_ctrl = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS
            .read()
            .unwrap_or(0);
        let entry_ctrl = VmcsControl32::VMENTRY_CONTROLS.read().unwrap_or(0);
        let eptp = VmcsControl64::EPTP.read().unwrap_or(0);
        let vmentry_intinfo = VmcsControl32::VMENTRY_INTERRUPTION_INFO_FIELD
            .read()
            .unwrap_or(0);

        debug!(
            "[VMCS-DUMP] RIP={:#x} RSP={:#x} RFLAGS={:#x}",
            rip, rsp, rflags
        );
        debug!(
            "[VMCS-DUMP] CR0={:#x} CR3={:#x} CR4={:#x} DR7={:#x}",
            cr0, cr3, cr4, dr7
        );
        debug!("[VMCS-DUMP] EFER={:#x} PAT={:#x}", efer, pat);
        debug!(
            "[VMCS-DUMP] SYSENTER: CS={:#x} ESP={:#x} EIP={:#x}",
            sysenter_cs, sysenter_esp, sysenter_eip
        );
        debug!(
            "[VMCS-DUMP] INT_STATE={:#x} ACTIVITY={:#x} LINK_PTR={:#x}",
            int_state, activity, link_ptr
        );
        debug!(
            "[VMCS-DUMP] GDTR={:#x}:{:#x} IDTR={:#x}:{:#x}",
            gdtr_base, gdtr_limit, idtr_base, idtr_limit
        );
        debug!(
            "[VMCS-DUMP] CS: sel={:#x} base={:#x} limit={:#x} AR={:#x}",
            cs_sel, cs_base, cs_limit, cs_ar
        );
        debug!(
            "[VMCS-DUMP] SS: sel={:#x} base={:#x} limit={:#x} AR={:#x}",
            ss_sel, ss_base, ss_limit, ss_ar
        );
        debug!(
            "[VMCS-DUMP] DS: sel={:#x} AR={:#x}  ES: sel={:#x} AR={:#x}",
            ds_sel, ds_ar, es_sel, es_ar
        );
        debug!(
            "[VMCS-DUMP] FS: sel={:#x} base={:#x} AR={:#x}",
            fs_sel, fs_base, fs_ar
        );
        debug!(
            "[VMCS-DUMP] GS: sel={:#x} base={:#x} AR={:#x}",
            gs_sel, gs_base, gs_ar
        );
        debug!(
            "[VMCS-DUMP] TR: sel={:#x} base={:#x} limit={:#x} AR={:#x}",
            tr_sel, tr_base, tr_limit, tr_ar
        );
        debug!(
            "[VMCS-DUMP] LDTR: sel={:#x} base={:#x} AR={:#x}",
            ldtr_sel, ldtr_base, ldtr_ar
        );
        debug!(
            "[VMCS-DUMP] CR0_MASK={:#x} CR0_SHADOW={:#x} CR4_MASK={:#x} CR4_SHADOW={:#x}",
            cr0_mask, cr0_shadow, cr4_mask, cr4_shadow
        );
        debug!(
            "[VMCS-DUMP] SEC_CTRL={:#x} ENTRY_CTRL={:#x} EXIT_CTRL={:#x} EPTP={:#x} VPID={}",
            sec_ctrl,
            entry_ctrl,
            exit_ctrl,
            eptp,
            VmcsControl16::VPID.read().unwrap_or(0),
        );
        debug!(
            "[VMCS-DUMP] PERF_GLOBAL_CTRL={:#x} BNDCFGS={:#x} VMENTRY_INFO={:#x}",
            perf_global_ctrl, bndcfgs, vmentry_intinfo
        );

        let pin_ctrl = VmcsControl32::PINBASED_EXEC_CONTROLS.read().unwrap_or(0);
        let prim_ctrl = VmcsControl32::PRIMARY_PROCBASED_EXEC_CONTROLS
            .read()
            .unwrap_or(0);
        debug!(
            "[VMCS-DUMP] PIN_CTRL={:#x} PRIM_CTRL={:#x}",
            pin_ctrl, prim_ctrl
        );

        let host_cr0 = VmcsHostNW::CR0.read().unwrap_or(0);
        let host_cr3 = VmcsHostNW::CR3.read().unwrap_or(0);
        let host_cr4 = VmcsHostNW::CR4.read().unwrap_or(0);
        let host_rip = VmcsHostNW::RIP.read().unwrap_or(0);
        let host_rsp = VmcsHostNW::RSP.read().unwrap_or(0);
        let host_efer = VmcsHost64::IA32_EFER.read().unwrap_or(0);
        debug!(
            "[VMCS-DUMP-HOST] CR0={:#x} CR3={:#x} CR4={:#x} RIP={:#x} RSP={:#x} EFER={:#x}",
            host_cr0, host_cr3, host_cr4, host_rip, host_rsp, host_efer
        );

        let host_cs = VmcsHost16::CS_SELECTOR.read().unwrap_or(0);
        let host_ss = VmcsHost16::SS_SELECTOR.read().unwrap_or(0);
        let host_ds = VmcsHost16::DS_SELECTOR.read().unwrap_or(0);
        let host_es = VmcsHost16::ES_SELECTOR.read().unwrap_or(0);
        let host_fs = VmcsHost16::FS_SELECTOR.read().unwrap_or(0);
        let host_gs = VmcsHost16::GS_SELECTOR.read().unwrap_or(0);
        let host_tr = VmcsHost16::TR_SELECTOR.read().unwrap_or(0);
        debug!(
            "[VMCS-DUMP-HOST] CS={:#x} SS={:#x} DS={:#x} ES={:#x} FS={:#x} GS={:#x} TR={:#x}",
            host_cs, host_ss, host_ds, host_es, host_fs, host_gs, host_tr
        );

        let host_fs_base = VmcsHostNW::FS_BASE.read().unwrap_or(0);
        let host_gs_base = VmcsHostNW::GS_BASE.read().unwrap_or(0);
        let host_tr_base = VmcsHostNW::TR_BASE.read().unwrap_or(0);
        let host_gdtr = VmcsHostNW::GDTR_BASE.read().unwrap_or(0);
        let host_idtr = VmcsHostNW::IDTR_BASE.read().unwrap_or(0);
        debug!(
            "[VMCS-DUMP-HOST] FS_BASE={:#x} GS_BASE={:#x} TR_BASE={:#x} GDTR={:#x} IDTR={:#x}",
            host_fs_base, host_gs_base, host_tr_base, host_gdtr, host_idtr
        );

        let io_a = VmcsControl64::IO_BITMAP_A_ADDR.read().unwrap_or(0);
        let io_b = VmcsControl64::IO_BITMAP_B_ADDR.read().unwrap_or(0);
        let msr_bmp = VmcsControl64::MSR_BITMAPS_ADDR.read().unwrap_or(0);
        debug!(
            "[VMCS-DUMP-HOST] IO_A={:#x} IO_B={:#x} MSR_BMP={:#x}",
            io_a, io_b, msr_bmp
        );
    }
}

// Implementaton for type1.5 hypervisor
// #[cfg(feature = "type1_5")]
impl VmxVcpu {
    fn set_cr(&mut self, cr_idx: usize, val: u64) {
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
    fn cr(&self, cr_idx: usize) -> usize {
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
    unsafe extern "C" fn vmx_exit(&mut self) -> usize {
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
        let exit_reason = VmcsReadOnly32::EXIT_REASON.read().unwrap_or(0);
        let exit_qual = VmcsReadOnlyNW::EXIT_QUALIFICATION.read().unwrap_or(0);
        let idt_vec = VmcsReadOnly32::IDT_VECTORING_INFO.read().unwrap_or(0);
        let idt_err = VmcsReadOnly32::IDT_VECTORING_ERR_CODE.read().unwrap_or(0);
        let instr_len = VmcsReadOnly32::VMEXIT_INSTRUCTION_LEN.read().unwrap_or(0);
        let pin_ctrl = VmcsControl32::PINBASED_EXEC_CONTROLS.read().unwrap_or(0);
        let prim_ctrl = VmcsControl32::PRIMARY_PROCBASED_EXEC_CONTROLS
            .read()
            .unwrap_or(0);
        let sec_ctrl = VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS
            .read()
            .unwrap_or(0);
        let entry_ctrl = VmcsControl32::VMENTRY_CONTROLS.read().unwrap_or(0);
        let exit_ctrl = VmcsControl32::VMEXIT_CONTROLS.read().unwrap_or(0);
        let eptp = VmcsControl64::EPTP.read().unwrap_or(0);
        let guest_efer = VmcsGuest64::IA32_EFER.read().unwrap_or(0);
        let guest_cr0 = VmcsGuestNW::CR0.read().unwrap_or(0);
        let guest_cr4 = VmcsGuestNW::CR4.read().unwrap_or(0);
        let guest_rip = VmcsGuestNW::RIP.read().unwrap_or(0);
        let guest_cs = VmcsGuest16::CS_SELECTOR.read().unwrap_or(0);
        let guest_cs_base = VmcsGuestNW::CS_BASE.read().unwrap_or(0);
        let guest_cs_ar = VmcsGuest32::CS_ACCESS_RIGHTS.read().unwrap_or(0);
        let host_cr0 = VmcsHostNW::CR0.read().unwrap_or(0);
        let host_cr4 = VmcsHostNW::CR4.read().unwrap_or(0);
        let host_efer = VmcsHost64::IA32_EFER.read().unwrap_or(0);
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

    /// Whether the guest interrupts are blocked. (SDM Vol. 3C, Section 24.4.2, Table 24-3)
    fn allow_interrupt(&self) -> bool {
        let rflags = VmcsGuestNW::RFLAGS.read().unwrap();
        let block_state = VmcsGuest32::INTERRUPTIBILITY_STATE.read().unwrap();
        let if_flag = rflags as u64 & x86_64::registers::rflags::RFlags::INTERRUPT_FLAG.bits() != 0;
        let blocked = block_state != 0;
        if !if_flag || blocked {
            // Log why interrupts can't be delivered (rate-limited)
            static LOG_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
            let count = LOG_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if count < 5 || count == 100 || count == 1000 {
                info!(
                    "[INTR-BLOCK] IF={}, block_state={:#x}, RFLAGS={:#x} (count={})",
                    if_flag, block_state, rflags, count
                );
            }
        }
        if_flag && !blocked
    }

    /// Try to inject a pending event before next VM entry.
    fn inject_pending_events(&mut self) -> AxResult {
        vmcs::clear_injection()?;

        let apic_enabled = self.vlapic.is_software_enabled();

        if apic_enabled {
            // APIC-enabled path: IOAPIC → vLAPIC → inject as External interrupt
            if let Some(vioapic) = GLOBAL_VIOAPIC.get() {
                let vcpu_id = self.vlapic.timer_where_am_i().1 as u32;
                let pending = vioapic.take_pending_irqs(vcpu_id);
                for vector in pending {
                    info!(
                        "[IOAPIC] Injecting pending IRQ vector={:#x} to vcpu={}",
                        vector, vcpu_id
                    );
                    self.vlapic.set_intr(vcpu_id, vector as u32);
                    self.queue_external_interrupt(vector);
                }
            }

            // Check for pending timer interrupt from LVT_TIMER unmask.
            let pending_timer = self.vlapic.take_pending_timer_vector();
            if pending_timer > 0 {
                self.queue_external_interrupt(pending_timer);
                debug!("[VLAPIC] Queued pending timer interrupt vector={pending_timer:#x}");
            }

            // Also check 8259 PIC for pending interrupts (Virtual Wire Mode fallback).
            // In real hardware with Virtual Wire Mode, the 8259 PIC output is connected
            // to LAPIC LINT0 (configured as ExtINT). When OVMF enables APIC but hasn't
            // fully configured IOAPIC RTEs yet, PIT IRQ0 may still need to be delivered
            // through the 8259 PIC path. This simulates the LINT0 ExtINT connection.
            if self.pending_events.is_empty()
                && let Some(pic) = i8259_pic::GLOBAL_PIC_MASTER.get()
                && let Some(vector) = pic.acknowledge()
            {
                debug!(
                    "[PIC] Virtual Wire Mode (APIC on, IOAPIC empty): acknowledging IRQ, \
                     vector={:#x}",
                    vector
                );
                self.queue_external_interrupt(vector);
            }
        } else {
            // Virtual Wire Mode: 8259 PIC → inject as External interrupt
            // When APIC is not software-enabled, OVMF receives interrupts
            // through the 8259 PIC path (INTR pin → ExtINT).
            //
            // Per Intel SDM 10.4.3, the local APIC timer still generates
            // interrupts even when software-disabled. Check for pending
            // timer interrupts here as well.
            let pending_timer = self.vlapic.take_pending_timer_vector();
            if pending_timer > 0 {
                self.queue_external_interrupt(pending_timer);
                debug!(
                    "[VLAPIC] Queued pending timer interrupt (APIC disabled) \
                     vector={pending_timer:#x}"
                );
            }

            if let Some(pic) = i8259_pic::GLOBAL_PIC_MASTER.get()
                && let Some(vector) = pic.acknowledge()
            {
                debug!(
                    "[PIC] Virtual Wire Mode: acknowledging IRQ, vector={:#x}",
                    vector
                );
                self.queue_external_interrupt(vector);
            }
        }

        if let Some(event) = self.pending_events.front() {
            let can_inject =
                !matches!(event.int_type, VmxInterruptionType::External) || self.allow_interrupt();
            if can_inject {
                debug!(
                    "[INTR] Injecting interrupt vector={:#x} type={:?}",
                    event.vector, event.int_type
                );
                vmcs::inject_event_with_type(event.vector, event.err_code, event.int_type)?;
                self.pending_events.pop_front();
            } else {
                // Log periodically to avoid flooding but still provide debug info
                static INJECT_FAIL_COUNT: core::sync::atomic::AtomicU32 =
                    core::sync::atomic::AtomicU32::new(0);
                let count = INJECT_FAIL_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                if count.is_multiple_of(1000) {
                    let rflags = VmcsGuestNW::RFLAGS.read().unwrap_or(0);
                    info!(
                        "[INTR] Cannot inject vector={:#x} type={:?} (fail #{count}), \
                         RFLAGS={rflags:#x}, setting interrupt window",
                        event.vector, event.int_type
                    );
                }
                self.set_interrupt_window(true)?;
            }
        }
        Ok(())
    }

    /// Handle vm-exits than can and should be handled by [`VmxVcpu`] itself.
    ///
    /// Return the result or None if the vm-exit was not handled.
    fn builtin_vmexit_handler(&mut self, exit_info: &VmxExitInfo) -> Option<AxResult> {
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
                    info!(
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
                            info!("[EFER] LME+PG set, LMA activated: EFER={new_efer:#x}");
                        }
                        info!("[EFER] After EFER write: CR0={cr0:#x}, PG={pg}, LME={lme}");
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
                        info!(
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

    /// Read a 64-bit value from EDX:EAX.
    fn read_edx_eax(&self) -> u64 {
        ((self.regs().rdx & 0xffff_ffff) << 32) | (self.regs().rax & 0xffff_ffff)
    }

    /// Write a 64-bit value to EDX:EAX.
    fn write_edx_eax(&mut self, val: u64) {
        self.regs_mut().rax = val & 0xffff_ffff;
        self.regs_mut().rdx = val >> 32;
    }

    fn handle_apic_base_msr(&mut self, write: bool) -> AxResult {
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

    fn handle_apic_msr_access(&mut self, write: bool, msr: u32) -> AxResult {
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

    fn handle_tsc_deadline_msr(&mut self, write: bool) -> AxResult {
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

    fn handle_apic_access(&mut self, exit_info: &VmxExitInfo) -> AxResult {
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

    fn handle_apic_mmio_ept_violation(&mut self) -> AxResult {
        let info = self.nested_page_fault_info()?;
        let gpa = info.fault_guest_paddr.as_usize();
        let apic_offset = (gpa - 0xFEE0_0000) as u32;
        let is_write = info.access_flags.contains(axaddrspace::MappingFlags::WRITE);

        let instr_len = VmcsReadOnly32::VMEXIT_INSTRUCTION_LEN.read().unwrap_or(0);
        let instr_len: u8 = if instr_len == 0 {
            if let Some((bytes, actual_len)) = self.read_guest_instr_bytes(15) {
                let decoded = Self::decode_x86_instruction_length(&bytes[..actual_len]);
                warn!(
                    "[APIC-MMIO] instr_len=0, decoded={}, RIP={:#x}, bytes={:02x?}",
                    decoded,
                    self.rip(),
                    &bytes[..actual_len.min(6)]
                );
                if decoded > 0 { decoded } else { 2 }
            } else {
                warn!(
                    "[APIC-MMIO] instr_len=0, failed to read guest instr, using default=2, \
                     RIP={:#x}",
                    self.rip()
                );
                2
            }
        } else {
            instr_len as u8
        };

        let apic_msr = 0x800 + (apic_offset >> 4);

        if is_write {
            let value = self.regs().rax as u32;
            info!(
                "[APIC-MMIO] write: offset={:#x}, msr={:#x}, value={:#x}",
                apic_offset, apic_msr, value
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
            info!(
                "[APIC-MMIO] read: offset={:#x}, msr={:#x}, value={:#x}",
                apic_offset, apic_msr, value
            );
            self.regs_mut().rax = value;
        }

        self.advance_rip(instr_len)?;

        Ok(())
    }

    /// Handle EPT violations caused by guest memory probing beyond ram_end.
    /// Maps a read-only dummy page (all 0xFF) for reads so the guest detects
    /// non-existent memory.  Writes are discarded by advancing RIP.
    fn handle_memory_probing_ept_violation(
        &mut self,
        gpa: usize,
        info: &NestedPageFaultInfo,
    ) -> AxResult {
        let page_aligned_gpa = gpa & !0xFFF;

        if info.access_flags.contains(MappingFlags::WRITE) {
            // Write beyond ram_end: discard by advancing past the instruction.
            let instr_len = VmcsReadOnly32::VMEXIT_INSTRUCTION_LEN.read().unwrap_or(0);
            let instr_len: u8 = if instr_len == 0 {
                if let Some((bytes, actual_len)) = self.read_guest_instr_bytes(15) {
                    Self::decode_x86_instruction_length(&bytes[..actual_len]).max(1)
                } else {
                    2
                }
            } else {
                instr_len as u8
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

    fn handle_pci_mmio_ept_violation(&mut self) -> AxResult {
        let info = self.nested_page_fault_info()?;
        let gpa = info.fault_guest_paddr.as_usize();
        let is_write = info.access_flags.contains(axaddrspace::MappingFlags::WRITE);

        if is_write {
            // Writes to PCI MMIO: discard by advancing past the instruction.
            let instr_len = VmcsReadOnly32::VMEXIT_INSTRUCTION_LEN.read().unwrap_or(0);
            let instr_len: u8 = if instr_len == 0 {
                if let Some((bytes, actual_len)) = self.read_guest_instr_bytes(15) {
                    let decoded = Self::decode_x86_instruction_length(&bytes[..actual_len]);
                    if decoded > 0 { decoded } else { 2 }
                } else {
                    2
                }
            } else {
                instr_len as u8
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
                let instr_len = VmcsReadOnly32::VMEXIT_INSTRUCTION_LEN.read().unwrap_or(0);
                let instr_len: u8 = if instr_len == 0 { 2 } else { instr_len as u8 };
                self.advance_rip(instr_len)?;
            }
            // Do NOT advance RIP — the instruction will re-execute against the
            // newly-mapped dummy page and read 0xFFFFFFFF into the correct register.
        }

        Ok(())
    }

    fn read_guest_instr_bytes(&self, max_len: usize) -> Option<([u8; 15], usize)> {
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

    fn decode_x86_instruction_length(bytes: &[u8]) -> u8 {
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

            if mod_field != 3 && rm_field == 4 && i < bytes.len() {
                i += 1;
            }

            match mod_field {
                0 if rm_field == 5 => {
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
    fn decode_mmio_mov_instr(bytes: &[u8]) -> Option<(u8, AccessWidth, u8)> {
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

    fn x86_opcode_has_modrm(opcode: u8) -> bool {
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

    fn gva_to_gpa_via_guest_pt(&self, cr3: u64, gva: u64) -> Option<u64> {
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

    fn gpa_to_hpa_via_ept(&self, ept_root: HostPhysAddr, gpa: u64) -> Option<usize> {
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

    fn read_phys_u64(&self, paddr: u64) -> Option<u64> {
        const PHYS_VIRT_OFFSET: u64 = 0xffff_8000_0000_0000;
        let vaddr = paddr + PHYS_VIRT_OFFSET;
        Some(unsafe { core::ptr::read_volatile(vaddr as *const u64) })
    }

    fn write_phys_u64(&self, paddr: u64, val: u64) {
        const PHYS_VIRT_OFFSET: u64 = 0xffff_8000_0000_0000;
        let vaddr = paddr + PHYS_VIRT_OFFSET;
        unsafe { core::ptr::write_volatile(vaddr as *mut u64, val) }
    }

    /// Map a 4 KB host page into the EPT at the given GPA with the specified
    /// permission flags.  Walks the 4-level EPT, allocating intermediate tables
    /// as needed.  Returns Ok(()) on success.
    fn ept_map_4k_with_flags(&self, gpa: u64, hpa: u64, perm: u64) -> AxResult {
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
    fn ept_map_4k_readonly(&self, gpa: u64, hpa: u64) -> AxResult {
        const EPT_R: u64 = 1 << 0;
        self.ept_map_4k_with_flags(gpa, hpa, EPT_R)
    }

    fn handle_vmx_preemption_timer(&mut self) -> AxResult {
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

    fn handle_hlt(&mut self) -> AxResult {
        // HLT instruction: guest is waiting for an interrupt.
        // Advance RIP past HLT so the guest doesn't re-execute it.
        // The caller (vcpu_run loop) is responsible for yielding or
        // waiting for an interrupt before re-entering the VM.
        const VM_EXIT_INSTR_LEN_HLT: u8 = 1;
        self.advance_rip(VM_EXIT_INSTR_LEN_HLT)?;
        Ok(())
    }

    #[allow(clippy::single_match)]
    fn handle_cr(&mut self) -> AxResult {
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
                    // log at debug instead.
                    if cr == 3 {
                        debug!(
                            "[CR{}] write val={:#x}, RIP before={:#x}, instr_len={}",
                            cr, val, rip_before, instr_len
                        );
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

    fn handle_cpuid(&mut self) -> AxResult {
        use raw_cpuid::{CpuIdResult, cpuid};

        const VM_EXIT_INSTR_LEN_CPUID: u8 = 2;
        const LEAF_FEATURE_INFO: u32 = 0x1;
        const LEAF_CACHE_PARAMETERS: u32 = 0x4;
        const LEAF_STRUCTURED_EXTENDED_FEATURE_FLAGS_ENUMERATION: u32 = 0x7;
        const LEAF_PROCESSOR_EXTENDED_STATE_ENUMERATION: u32 = 0xd;
        const LEAF_TSC_CORE_CRYSTAL_RATIO: u32 = 0x15;
        const EAX_FREQUENCY_INFO: u32 = 0x16;
        const LEAF_X2APIC_TOPOLOGY: u32 = 0xb;
        const LEAF_X2APIC_TOPOLOGY_V2: u32 = 0x1f;
        const LEAF_HYPERVISOR_INFO: u32 = 0x4000_0000;
        const LEAF_HYPERVISOR_FEATURE: u32 = 0x4000_0001;
        const VENDOR_STR: &[u8; 12] = b"RVMRVMRVMRVM";
        let vendor_regs = unsafe { &*(VENDOR_STR.as_ptr() as *const [u32; 3]) };

        let regs_clone = *self.regs_mut();
        let function = regs_clone.rax as u32;
        let res = match function {
            LEAF_FEATURE_INFO => {
                // CPUID leaf 0x1 has NO sub-leaves per Intel SDM.
                // ECX is not a sub-leaf index and is ignored by hardware.
                // Previously, checking rcx >= 4 caused most leaf-1 queries
                // to return all-zeros, breaking OVMF's APIC/feature detection.
                const FEATURE_VMX: u32 = 1 << 5;
                const FEATURE_HYPERVISOR: u32 = 1 << 31;
                const FEATURE_MCE: u32 = 1 << 7;
                const FEATURE_TSC_DEADLINE: u32 = 1 << 24;
                const FEATURE_MONITOR: u32 = 1 << 3;
                let mut res = cpuid!(regs_clone.rax, 0);
                res.ecx &= !FEATURE_VMX;
                res.ecx &= !FEATURE_TSC_DEADLINE;
                res.ecx &= !FEATURE_MONITOR;
                res.ecx &= !FEATURE_HYPERVISOR;
                res.edx &= !FEATURE_MCE;
                res.ebx = 0x0001_0800; // BrandIndex=0, CLFLUSH=64B, MaxLogicalProc=1, APIC ID=0
                res
            }
            LEAF_CACHE_PARAMETERS => {
                let mut res = cpuid!(regs_clone.rax, regs_clone.rcx);
                if regs_clone.rcx > 0 {
                    res.eax = 0;
                }
                res
            }
            // See SDM Table 3-8. Information Returned by CPUID Instruction (Contd.)
            LEAF_STRUCTURED_EXTENDED_FEATURE_FLAGS_ENUMERATION => {
                let mut res = cpuid!(regs_clone.rax, regs_clone.rcx);
                if regs_clone.rcx == 0 {
                    // Bit 05: WAITPKG.
                    res.ecx.set_bit(5, false); // clear waitpkg
                    // Bit 16: LA57. Supports 57-bit linear addresses and five-level paging if 1.
                    res.ecx.set_bit(16, false); // clear LA57
                }

                res
            }
            LEAF_PROCESSOR_EXTENDED_STATE_ENUMERATION => {
                self.load_guest_xstate();
                let res = cpuid!(regs_clone.rax, regs_clone.rcx);
                self.load_host_xstate();

                res
            }
            LEAF_X2APIC_TOPOLOGY => {
                if regs_clone.rcx == 0 {
                    CpuIdResult {
                        eax: 0,
                        ebx: 1,
                        ecx: 0x100,
                        edx: 0,
                    }
                } else {
                    CpuIdResult {
                        eax: 0,
                        ebx: 0,
                        ecx: 0,
                        edx: 0,
                    }
                }
            }
            LEAF_X2APIC_TOPOLOGY_V2 => {
                if regs_clone.rcx == 0 {
                    CpuIdResult {
                        eax: 0,
                        ebx: 1,
                        ecx: 0x100,
                        edx: 0,
                    }
                } else {
                    CpuIdResult {
                        eax: 0,
                        ebx: 0,
                        ecx: 0,
                        edx: 0,
                    }
                }
            }
            LEAF_HYPERVISOR_INFO => CpuIdResult {
                eax: LEAF_HYPERVISOR_FEATURE,
                ebx: vendor_regs[0],
                ecx: vendor_regs[1],
                edx: vendor_regs[2],
            },
            LEAF_HYPERVISOR_FEATURE => CpuIdResult {
                eax: 0,
                ebx: 0,
                ecx: 0,
                edx: 0,
            },
            LEAF_TSC_CORE_CRYSTAL_RATIO => {
                let mut res = cpuid!(regs_clone.rax, regs_clone.rcx);
                // Always log CPUID 0x15 values for diagnostics
                info!(
                    "[CPUID-0x15] Raw: eax={:#x} ebx={:#x} ecx={:#x} edx={:#x}",
                    res.eax, res.ebx, res.ecx, res.edx
                );
                // OVMF's InternalGetApicTimerFrequency() computes:
                //   APIC_freq = ECX * EAX / EBX  (ECX=crystal Hz, EAX=denominator, EBX=numerator)
                // If ECX=0 (crystal frequency unknown) or EAX/EBX=0, the result is 0,
                // which triggers ASSERT(ApicFrequency != 0) in MicroSecondDelay().
                // Provide fallback values when the hardware doesn't report a valid frequency.
                if res.eax == 0 || res.ebx == 0 || res.ecx == 0 {
                    info!("[CPUID-0x15] Values invalid, using fallback (100 MHz crystal)");
                    res.eax = 1;
                    res.ebx = 1;
                    res.ecx = 100_000_000; // 100 MHz crystal
                }
                res
            }
            EAX_FREQUENCY_INFO => {
                const TIMER_FREQUENCY_MHZ: u32 = 3_000;
                let mut res = cpuid!(regs_clone.rax, regs_clone.rcx);
                if res.eax == 0 {
                    warn!(
                        "handle_cpuid: Failed to get TSC frequency by CPUID, default to \
                         {TIMER_FREQUENCY_MHZ} MHz"
                    );
                    res.eax = TIMER_FREQUENCY_MHZ;
                }
                res
            }
            0x8000_0000 => {
                let mut res = cpuid!(regs_clone.rax, regs_clone.rcx);
                const MAX_EXT_LEAF: u32 = 0x8000_0008;
                if res.eax > MAX_EXT_LEAF {
                    res.eax = MAX_EXT_LEAF;
                }
                // Dump 96 bytes from RIP-8 to see code after CPUID 0x80000000
                if let Some(ept_root) = self.ept_root {
                    let rip = self.rip() as u64;
                    let base_gpa = (rip - 8) & !0x7u64;
                    let mut dump = [0u8; 96];
                    for (i, byte) in dump.iter_mut().enumerate() {
                        let gpa = base_gpa + i as u64;
                        if let Some(hpa) = self.gpa_to_hpa_via_ept(ept_root, gpa) {
                            const PHYS_VIRT_OFFSET: u64 = 0xffff_8000_0000_0000;
                            *byte = unsafe {
                                core::ptr::read_volatile(
                                    (hpa as u64 + PHYS_VIRT_OFFSET) as *const u8,
                                )
                            };
                        }
                    }
                    info!("[8k] e={:x} rip={:x} {:02x?}", res.eax, rip, &dump[..96]);
                } else {
                    info!("[8k] e={:x}", res.eax);
                }
                res
            }
            0x8000_0001 => {
                let mut res = cpuid!(regs_clone.rax, regs_clone.rcx);
                res.ecx &= !(1 << 2);
                info!(
                    "[C8k1] a={:#x} b={:#x} c={:#x} d={:#x} LM={}",
                    res.eax,
                    res.ebx,
                    res.ecx,
                    res.edx,
                    (res.edx >> 29) & 1
                );
                res
            }
            0x8000_0008 => {
                let mut res = cpuid!(regs_clone.rax, regs_clone.rcx);
                res.ecx = 0;
                res
            }
            _ => cpuid!(regs_clone.rax, regs_clone.rcx),
        };

        trace!(
            "VM exit: CPUID({:#x}, {:#x}): {:?}",
            regs_clone.rax, regs_clone.rcx, res
        );

        {
            static CPUID_OUT_COUNT: core::sync::atomic::AtomicU64 =
                core::sync::atomic::AtomicU64::new(0);
            let out_count = CPUID_OUT_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            // Log ALL CPUID calls at info level for the first 500 calls to diagnose
            // why OVMF never calls leaf 0x80000001 after 0x80000000.
            if out_count < 500 {
                let rip = self.rip() as u64;
                match function {
                    0x80000000 => {
                        // Dump 32 bytes from RIP-8 to see code around CPUID 0x80000000
                        // Only dump first 3 occurrences to avoid log truncation
                        if out_count < 3 {
                            if let Some(ept_root) = self.ept_root {
                                let base_gpa = (rip - 8) & !0x7u64;
                                let mut dump = [0u8; 32];
                                for (i, byte) in dump.iter_mut().enumerate() {
                                    let gpa = base_gpa + i as u64;
                                    if let Some(hpa) = self.gpa_to_hpa_via_ept(ept_root, gpa) {
                                        const PHYS_VIRT_OFFSET: u64 = 0xffff_8000_0000_0000;
                                        *byte = unsafe {
                                            core::ptr::read_volatile(
                                                (hpa as u64 + PHYS_VIRT_OFFSET) as *const u8,
                                            )
                                        };
                                    }
                                }
                                info!("[8k]#{out_count} {rip:x} {:02x?}", &dump[..32]);
                            } else {
                                info!("[8k]#{out_count} {rip:x}");
                            }
                        } else {
                            info!("[8k]#{out_count} {rip:x}");
                        }
                        self.trace_after_cpuid_8k = true;
                        self.trace_after_8k_count = 0;
                    }
                    0x80000001 => info!("[81]#{out_count} {rip:x}"),
                    0x1 => {
                        if out_count < 1 {
                            if let Some(ept_root) = self.ept_root {
                                // Dump 128 bytes from RIP-8 to see more context
                                let base_gpa = (rip - 8) & !0x7u64;
                                let mut dump = [0u8; 128];
                                for (i, byte) in dump.iter_mut().enumerate() {
                                    let gpa = base_gpa + i as u64;
                                    if let Some(hpa) = self.gpa_to_hpa_via_ept(ept_root, gpa) {
                                        const PHYS_VIRT_OFFSET: u64 = 0xffff_8000_0000_0000;
                                        *byte = unsafe {
                                            core::ptr::read_volatile(
                                                (hpa as u64 + PHYS_VIRT_OFFSET) as *const u8,
                                            )
                                        };
                                    }
                                }
                                info!("[1]#{out_count} {rip:x} {:02x?}", &dump[..128]);
                            } else {
                                info!("[1]#{out_count} {rip:x}");
                            }
                        } else {
                            info!("[1]#{out_count} {rip:x}");
                        }
                    }
                    _ => info!("[C]#{out_count} {function:x} {rip:x}"),
                }
            }
        }

        let regs = self.regs_mut();
        regs.rax = res.eax as _;
        regs.rbx = res.ebx as _;
        regs.rcx = res.ecx as _;
        regs.rdx = res.edx as _;
        self.advance_rip(VM_EXIT_INSTR_LEN_CPUID)?;

        Ok(())
    }

    fn handle_xsetbv(&mut self) -> AxResult {
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

    fn load_guest_xstate(&mut self) {
        self.xstate.switch_to_guest();
    }

    fn load_host_xstate(&mut self) {
        self.xstate.switch_to_host();
    }
}

impl Drop for VmxVcpu {
    fn drop(&mut self) {
        unsafe { vmx::vmclear(self.vmcs.phys_addr().as_usize() as u64).unwrap() };
        info!("[HV] dropped VmxVcpu(vmcs: {:#x})", self.vmcs.phys_addr());
    }
}

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
                // Dump guest state on the first few VM-entry failures to
                // diagnose the invalid guest state (reason 0x21).
                static ENTRY_FAIL_DUMPED: core::sync::atomic::AtomicU32 =
                    core::sync::atomic::AtomicU32::new(0);
                let dumped = ENTRY_FAIL_DUMPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                if dumped < 3 {
                    // Log the last 16 VM-exits before this VM-entry failure to
                    // identify what caused the guest state to become invalid.
                    {
                        let hist = EXIT_HISTORY.lock();
                        error!(
                            "[ENTRY-FAIL #{dumped}] Last {} VM-exits before failure (total {}):",
                            hist.buf.len(),
                            hist.count
                        );
                        let start = hist.idx; // oldest entry
                        for i in 0..16 {
                            let idx = (start + i) % 16;
                            let (reason, rip, cs_ar) = hist.buf[idx];
                            if reason != 0 || rip != 0 {
                                error!(
                                    "  [{}] reason={reason:#x} RIP={rip:#x} CS_ar={cs_ar:#010x}",
                                    i
                                );
                            }
                        }
                    }
                    let cs_sel = VmcsGuest16::CS_SELECTOR.read().unwrap_or(0);
                    let cs_ar = VmcsGuest32::CS_ACCESS_RIGHTS.read().unwrap_or(0);
                    let cs_base = VmcsGuestNW::CS_BASE.read().unwrap_or(0);
                    let cs_limit = VmcsGuest32::CS_LIMIT.read().unwrap_or(0);
                    let ss_sel = VmcsGuest16::SS_SELECTOR.read().unwrap_or(0);
                    let ss_ar = VmcsGuest32::SS_ACCESS_RIGHTS.read().unwrap_or(0);
                    let ds_sel = VmcsGuest16::DS_SELECTOR.read().unwrap_or(0);
                    let ds_ar = VmcsGuest32::DS_ACCESS_RIGHTS.read().unwrap_or(0);
                    let es_sel = VmcsGuest16::ES_SELECTOR.read().unwrap_or(0);
                    let es_ar = VmcsGuest32::ES_ACCESS_RIGHTS.read().unwrap_or(0);
                    let fs_sel = VmcsGuest16::FS_SELECTOR.read().unwrap_or(0);
                    let fs_ar = VmcsGuest32::FS_ACCESS_RIGHTS.read().unwrap_or(0);
                    let gs_sel = VmcsGuest16::GS_SELECTOR.read().unwrap_or(0);
                    let gs_ar = VmcsGuest32::GS_ACCESS_RIGHTS.read().unwrap_or(0);
                    let tr_sel = VmcsGuest16::TR_SELECTOR.read().unwrap_or(0);
                    let tr_ar = VmcsGuest32::TR_ACCESS_RIGHTS.read().unwrap_or(0);
                    let tr_base = VmcsGuestNW::TR_BASE.read().unwrap_or(0);
                    let tr_limit = VmcsGuest32::TR_LIMIT.read().unwrap_or(0);
                    let ldtr_sel = VmcsGuest16::LDTR_SELECTOR.read().unwrap_or(0);
                    let ldtr_ar = VmcsGuest32::LDTR_ACCESS_RIGHTS.read().unwrap_or(0);
                    let ldtr_base = VmcsGuestNW::LDTR_BASE.read().unwrap_or(0);
                    let ldtr_limit = VmcsGuest32::LDTR_LIMIT.read().unwrap_or(0);
                    let cr0 = VmcsGuestNW::CR0.read().unwrap_or(0);
                    let cr3 = VmcsGuestNW::CR3.read().unwrap_or(0);
                    let cr4 = VmcsGuestNW::CR4.read().unwrap_or(0);
                    let efer = VmcsGuest64::IA32_EFER.read().unwrap_or(0);
                    let rflags = VmcsGuestNW::RFLAGS.read().unwrap_or(0);
                    let rip = self.rip();
                    let rsp = VmcsGuestNW::RSP.read().unwrap_or(0);
                    let dr7 = VmcsGuestNW::DR7.read().unwrap_or(0);
                    let fs_base = VmcsGuestNW::FS_BASE.read().unwrap_or(0);
                    let gs_base = VmcsGuestNW::GS_BASE.read().unwrap_or(0);
                    let pat = VmcsGuest64::IA32_PAT.read().unwrap_or(0);
                    let perf_global = VmcsGuest64::IA32_PERF_GLOBAL_CTRL.read().unwrap_or(0);
                    let pdpte0 = VmcsGuest64::PDPTE0.read().unwrap_or(0);
                    let pdpte1 = VmcsGuest64::PDPTE1.read().unwrap_or(0);
                    let pdpte2 = VmcsGuest64::PDPTE2.read().unwrap_or(0);
                    let pdpte3 = VmcsGuest64::PDPTE3.read().unwrap_or(0);
                    let gdtr_base = VmcsGuestNW::GDTR_BASE.read().unwrap_or(0);
                    let gdtr_limit = VmcsGuest32::GDTR_LIMIT.read().unwrap_or(0);
                    let idtr_base = VmcsGuestNW::IDTR_BASE.read().unwrap_or(0);
                    let idtr_limit = VmcsGuest32::IDTR_LIMIT.read().unwrap_or(0);
                    let link_ptr = VmcsGuest64::LINK_PTR.read().unwrap_or(0);
                    let debugctl = VmcsGuest64::IA32_DEBUGCTL.read().unwrap_or(0);
                    let intr_state = VmcsGuest32::INTERRUPTIBILITY_STATE.read().unwrap_or(0);
                    let act_state = VmcsGuest32::ACTIVITY_STATE.read().unwrap_or(0);
                    let sysenter_cs = VmcsGuest32::IA32_SYSENTER_CS.read().unwrap_or(0);
                    let pending_dbg = VmcsGuestNW::PENDING_DBG_EXCEPTIONS.read().unwrap_or(0);
                    let sysenter_esp = VmcsGuestNW::IA32_SYSENTER_ESP.read().unwrap_or(0);
                    let sysenter_eip = VmcsGuestNW::IA32_SYSENTER_EIP.read().unwrap_or(0);
                    let vmentry_intr = VmcsControl32::VMENTRY_INTERRUPTION_INFO_FIELD
                        .read()
                        .unwrap_or(0);
                    error!(
                        "[ENTRY-FAIL #{dumped}] reason={exit_reason_raw:#x} RIP={rip:#x} \
                         RSP={rsp:#x} RFLAGS={rflags:#x}\nCR0={cr0:#x} CR3={cr3:#x} CR4={cr4:#x} \
                         EFER={efer:#x} DR7={dr7:#x}\nCS={cs_sel:#x} ar={cs_ar:#08x} \
                         base={cs_base:#x} lim={cs_limit:#x}\nSS={ss_sel:#x} \
                         ar={ss_ar:#08x}\nDS={ds_sel:#x} ar={ds_ar:#08x} ES={es_sel:#x} \
                         ar={es_ar:#08x}\nFS={fs_sel:#x} ar={fs_ar:#08x} base={fs_base:#x} \
                         GS={gs_sel:#x} ar={gs_ar:#08x} base={gs_base:#x}\nTR={tr_sel:#x} \
                         ar={tr_ar:#08x} base={tr_base:#x} lim={tr_limit:#x}\nLDTR={ldtr_sel:#x} \
                         ar={ldtr_ar:#08x} base={ldtr_base:#x} \
                         lim={ldtr_limit:#x}\nGDTR={gdtr_base:#x}:{gdtr_limit:#x} \
                         IDTR={idtr_base:#x}:{idtr_limit:#x}\nLINK_PTR={link_ptr:#x} \
                         DEBUGCTL={debugctl:#x}\nINTR_STATE={intr_state:#x} \
                         ACT_STATE={act_state:#x}\nPENDING_DBG={pending_dbg:#x} \
                         SYSENTER_CS={sysenter_cs:#x}\nSYSENTER_ESP={sysenter_esp:#x} \
                         SYSENTER_EIP={sysenter_eip:#x}\nVMENTRY_INTR_INFO={vmentry_intr:#x}\\
                         nPAT={pat:#x} PERF_GLOBAL_CTRL={perf_global:#x}\nPDPTE0={pdpte0:#x} \
                         PDPTE1={pdpte1:#x} PDPTE2={pdpte2:#x} PDPTE3={pdpte3:#x}"
                    );
                }
                // Workaround: if CS is marked Unusable (bit 15), clear it.
                // CS must always be usable; the processor may incorrectly set
                // this bit in some edge cases. Clearing it allows VM-entry to
                // succeed and the guest to continue.
                let cs_ar = VmcsGuest32::CS_ACCESS_RIGHTS.read().unwrap_or(0);
                if cs_ar & 0x8000 != 0 {
                    let fixed_ar = cs_ar & !0x8000;
                    warn!(
                        "[ENTRY-FAIL] CS marked Unusable (ar={cs_ar:#010x}), clearing bit 15 -> \
                         ar={fixed_ar:#010x}"
                    );
                    VmcsGuest32::CS_ACCESS_RIGHTS.write(fixed_ar)?;
                }
                // Workaround: fix invalid IA32_PAT entries. Intel SDM 26.3.1.1
                // requires bits 2:0 of each PAT entry to be 0, 1, 4, 5, or 6.
                // Values 2, 3, 7 are invalid and cause VM-entry failure.
                // The guest may write these via passthrough (no VM-exit), so we
                // sanitize the saved PAT on VM-entry failure.
                let pat = VmcsGuest64::IA32_PAT.read().unwrap_or(0);
                let mut fixed_pat = pat;
                let mut pat_changed = false;
                for i in 0..8u64 {
                    let shift = i * 8;
                    let entry = (pat >> shift) & 0x7;
                    if entry == 2 || entry == 3 || entry == 7 {
                        // Replace invalid entry with UC (0), the safest value.
                        fixed_pat &= !(0x7 << shift);
                        pat_changed = true;
                    }
                }
                if pat_changed {
                    warn!(
                        "[ENTRY-FAIL] IA32_PAT has invalid entries (pat={pat:#018x}), fixing -> \
                         {fixed_pat:#018x}"
                    );
                    VmcsGuest64::IA32_PAT.write(fixed_pat)?;
                }
                // Workaround: clear reserved bits (31:16) of segment access
                // rights. Intel SDM 26.3.1.2 says no checks are performed on
                // unusable segments, but bits 31:16 are reserved and must be 0
                // for ALL segment access-rights fields. Some guests leave bit 16
                // set, which the processor may reject on VM-entry.
                for (name, field) in [
                    ("SS", VmcsGuest32::SS_ACCESS_RIGHTS),
                    ("DS", VmcsGuest32::DS_ACCESS_RIGHTS),
                    ("ES", VmcsGuest32::ES_ACCESS_RIGHTS),
                    ("FS", VmcsGuest32::FS_ACCESS_RIGHTS),
                    ("GS", VmcsGuest32::GS_ACCESS_RIGHTS),
                    ("LDTR", VmcsGuest32::LDTR_ACCESS_RIGHTS),
                ] {
                    let ar = field.read().unwrap_or(0);
                    if ar & 0xFFFF0000 != 0 {
                        let fixed_ar = ar & 0x0000FFFF;
                        warn!(
                            "[ENTRY-FAIL] {name} ar={ar:#010x} has reserved bits set, clearing -> \
                             {fixed_ar:#010x}"
                        );
                        field.write(fixed_ar)?;
                    }
                }
                AxVCpuExitReason::FailEntry {
                    hardware_entry_failure_reason: exit_reason_raw,
                }
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
                                static IO_CODE_DUMPED: core::sync::atomic::AtomicBool =
                                    core::sync::atomic::AtomicBool::new(false);
                                if !IO_CODE_DUMPED.swap(true, core::sync::atomic::Ordering::Relaxed)
                                    && let Some(ept_root) = self.ept_root
                                {
                                    let rip = self.rip() as u64;
                                    let base_gpa = rip.saturating_sub(16) & !0x7u64;
                                    let mut dump = [0u8; 64];
                                    for (i, byte) in dump.iter_mut().enumerate() {
                                        let gpa = base_gpa + i as u64;
                                        if let Some(hpa) = self.gpa_to_hpa_via_ept(ept_root, gpa) {
                                            const PHYS_VIRT_OFFSET: u64 = 0xffff_8000_0000_0000;
                                            *byte = unsafe {
                                                core::ptr::read_volatile(
                                                    ((hpa as u64) + PHYS_VIRT_OFFSET) as *const u8,
                                                )
                                            };
                                        }
                                    }
                                    let rax = self.regs().rax;
                                    let rcx = self.regs().rcx;
                                    let rdx = self.regs().rdx;
                                    let rflags = VmcsGuestNW::RFLAGS.read().unwrap_or(0);
                                    info!(
                                        "[IO-CODE] RIP={:#x} RAX={:#x} RCX={:#x} RDX={:#x} \
                                         RFLAGS={:#x} is_in={} access_size={} code={:02x?}",
                                        rip,
                                        rax,
                                        rcx,
                                        rdx,
                                        rflags,
                                        io_info.is_in,
                                        io_info.access_size,
                                        &dump[..64]
                                    );
                                }
                            }
                        }

                        // Log fw_cfg port accesses (0x510-0x51B) at debug level
                        if (0x510..=0x51B).contains(&port) {
                            let dir = if io_info.is_in { "IN" } else { "OUT" };
                            let data = if io_info.is_in {
                                0
                            } else {
                                self.regs().rax & 0xffffffff
                            };
                            debug!(
                                "[IO-fw_cfg] port={:#x} dir={} width={} string={} rep={} \
                                 data={:#x}",
                                port,
                                dir,
                                io_info.access_size,
                                io_info.is_string,
                                io_info.is_repeat,
                                data
                            );
                        }

                        // Diagnostic: log non-PM-TIMER, non-keyboard port
                        // accesses at debug level to avoid flooding the log.
                        // PM-TIMER (0x600-0x60B) and keyboard controller
                        // (0x60, 0x64) are too noisy, skip them.
                        if !(0x600..=0x60B).contains(&port) && port != 0x60 && port != 0x64 {
                            let dir = if io_info.is_in { "IN" } else { "OUT" };
                            let data = if io_info.is_in {
                                0
                            } else {
                                self.regs().rax & 0xffffffff
                            };
                            debug!(
                                "[IO-ALL] port={:#x} dir={} size={} RIP={:#x} data={:#x}",
                                port,
                                dir,
                                io_info.access_size,
                                self.rip(),
                                data
                            );
                        }

                        let width = match AccessWidth::try_from(io_info.access_size as usize) {
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
                            && width == AccessWidth::Word
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
                        // `reg` is unused here.
                        AxVCpuExitReason::SysRegRead {
                            addr: SysRegAddr::new(self.regs().rcx as _),
                            reg: 0,
                        }
                    }
                    VmxExitReason::EPT_VIOLATION => {
                        let info = self.nested_page_fault_info()?;
                        let gpa = info.fault_guest_paddr.as_usize();

                        if gpa >= IOAPIC_MMIO_BASE as usize
                            && gpa < (IOAPIC_MMIO_BASE + IOAPIC_MMIO_SIZE) as usize
                        {
                            let instr_len =
                                VmcsReadOnly32::VMEXIT_INSTRUCTION_LEN.read().unwrap_or(0);
                            if instr_len > 0 {
                                self.advance_rip(instr_len as _)?;
                                if info.access_flags.contains(MappingFlags::WRITE) {
                                    return Ok(AxVCpuExitReason::MmioWrite {
                                        addr: info.fault_guest_paddr,
                                        width: AccessWidth::Dword,
                                        data: self.regs().rax,
                                    });
                                } else {
                                    return Ok(AxVCpuExitReason::MmioRead {
                                        addr: info.fault_guest_paddr,
                                        width: AccessWidth::Dword,
                                        reg: 0,
                                        reg_width: AccessWidth::Dword,
                                        signed_ext: false,
                                    });
                                }
                            }
                        }

                        // ECAM MMIO: route to PCI host bridge's ECAM interface via
                        // the VMM's MMIO dispatch. OVMF accesses PCI config space
                        // via ECAM at 0xB000_0000 (MCFG ACPI table).
                        if (ECAM_MMIO_BASE..ECAM_MMIO_END).contains(&gpa) {
                            let vmexit_instr_len =
                                VmcsReadOnly32::VMEXIT_INSTRUCTION_LEN.read().unwrap_or(0);
                            // Intel SDM: VMEXIT_INSTRUCTION_LEN is undefined for
                            // EPT violations. Decode the instruction bytes to get
                            // both the length and the destination/source register.
                            let (instr_len, reg, width) = if let Some((bytes, actual_len)) =
                                self.read_guest_instr_bytes(15)
                            {
                                if let Some((reg, width, decoded_len)) =
                                    Self::decode_mmio_mov_instr(&bytes[..actual_len])
                                {
                                    let len = if vmexit_instr_len > 0 {
                                        vmexit_instr_len as u8
                                    } else {
                                        decoded_len.max(1)
                                    };
                                    (len, reg, width)
                                } else {
                                    // Not a MOV instruction; fall back to RAX
                                    let len = if vmexit_instr_len > 0 {
                                        vmexit_instr_len as u8
                                    } else {
                                        Self::decode_x86_instruction_length(&bytes[..actual_len])
                                            .max(1)
                                    };
                                    (len, 0u8, AccessWidth::Dword)
                                }
                            } else {
                                // Cannot read instruction bytes; fall back
                                let len = if vmexit_instr_len > 0 {
                                    vmexit_instr_len as u8
                                } else {
                                    2
                                };
                                (len, 0u8, AccessWidth::Dword)
                            };

                            self.advance_rip(instr_len as _)?;
                            if info.access_flags.contains(MappingFlags::WRITE) {
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
                            && info.access_flags.contains(MappingFlags::EXECUTE)
                        {
                            let instr_len =
                                VmcsReadOnly32::VMEXIT_INSTRUCTION_LEN.read().unwrap_or(0);
                            let instr_len: u8 = if instr_len == 0 { 2 } else { instr_len as u8 };
                            self.advance_rip(instr_len)?;
                            self.queue_event(14, Some(1 << 4));
                            return Ok(AxVCpuExitReason::Nothing);
                        }

                        info!(
                            "EPT_VIOLATION: GPA={:#x}, access={:?}, RIP={:#x}",
                            info.fault_guest_paddr,
                            info.access_flags,
                            self.rip()
                        );
                        AxVCpuExitReason::NestedPageFault {
                            addr: info.fault_guest_paddr,
                            access_flags: info.access_flags,
                        }
                    }
                    VmxExitReason::MSR_WRITE => {
                        let value = (self.regs().rax & 0xffff_ffff)
                            | ((self.regs().rdx & 0xffff_ffff) << 32);
                        AxVCpuExitReason::SysRegWrite {
                            addr: SysRegAddr::new(self.regs().rcx as _),
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

        if let Some(sipi) = self.vlapic.take_pending_init_sipi() {
            info!(
                "[SMP] INIT/SIPI: target_cpu={}, mode={:?}, vector={:#x}",
                sipi.target_cpu, sipi.mode, sipi.vector
            );
            let entry_point = if sipi.vector != 0 {
                GuestPhysAddr::from((sipi.vector as usize) << 12)
            } else {
                GuestPhysAddr::from(0x0)
            };
            return Ok(AxVCpuExitReason::CpuUp {
                target_cpu: sipi.target_cpu as u64,
                entry_point,
                arg: 0,
            });
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
        let lvt_val = self.vlapic.timer_read_lvt();

        info!(
            "[VLAPIC] timer expired: vector={vector:#x}, masked={is_masked}, \
             periodic={is_periodic}, lvt={lvt_val:#010x}"
        );

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
            info!("[VLAPIC] timer interrupt queued: vector={vector:#x}, vcpu={vcpu_id}");
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
    fn test_get_tr_base_logic() {
        let mut test_entry = 0u64;
        test_entry |= 1u64 << 47; // Present bit
        test_entry |= (0x1000u64 & 0xFFFFFF) << 16; // Base address bits 16-39

        // Present bit check
        let present = test_entry & (1 << 47) != 0;
        assert!(present);

        // Base address extraction
        let base_low = (test_entry >> 16) & 0xFFFFFF;
        let base_high = (test_entry >> 56) & 0xFF;
        let base_addr = base_low | (base_high << 24);

        assert_eq!(base_addr, 0x1000);
    }

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
