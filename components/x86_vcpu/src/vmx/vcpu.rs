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
use axvisor_api::vmm::{VCpuId, VMId};
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

use super::{
    VmxExitInfo, as_axerr,
    definitions::{VmxExitReason, VmxInterruptionType},
    structs::{IOBitmap, MsrBitmap, VmxRegion},
    vmcs::{
        self, ApicAccessExitType, VmcsControl32, VmcsControl64, VmcsControlNW, VmcsGuest16,
        VmcsGuest32, VmcsGuest64, VmcsGuestNW, VmcsHost16, VmcsHost32, VmcsHost64, VmcsHostNW,
        VmcsReadOnly32,
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
,
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
    /// The I/O bitmap for the VMCS.
    io_bitmap: IOBitmap,
    /// The MSR bitmap for the VMCS.
    msr_bitmap: MsrBitmap,

    // Interrupt-related fields
    /// Pending events to be injected to the guest.
    pending_events: VecDeque<PendingEvent>,
    /// Emulated Local APIC.
    vlapic: EmulatedLocalApic,

    // Extra states
    /// The XState of the VCpu. Both host and guest.
    xstate: XState,

    // Tracing-related fields
    #[cfg(feature = "tracing")]
    /// The guest registers when the VM-exit happens.
    guest_regs_exiting: GeneralRegisters,
}

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
            io_bitmap: IOBitmap::intercept_all()?,
            msr_bitmap: MsrBitmap::passthrough_all()?,
            pending_events: VecDeque::with_capacity(8),
            vlapic: EmulatedLocalApic::new(vm_id, vcpu_id),
            xstate: XState::new(),
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
    ) -> AxResult {
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
            if (cs_access_right & 0x2000) != 0 {
                // CS.L = 1
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

        // Run guest
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
        // debug!("VM exit: {:#x?}", exit_info);

        // Log non-I/O, non-external-interrupt exits for diagnostics
        match exit_info.exit_reason {
            VmxExitReason::IO_INSTRUCTION | VmxExitReason::EXTERNAL_INTERRUPT => {}
            VmxExitReason::CPUID => {
                static CPUID_COUNT: core::sync::atomic::AtomicU64 =
                    core::sync::atomic::AtomicU64::new(0);
                let count = CPUID_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                if count < 50 || count == 100 || count == 1000 || count == 10000 || count == 100000
                {
                    let leaf = self.regs().rax as u32;
                    let rflags = VmcsGuestNW::RFLAGS.read().unwrap_or(0);
                    let if_flag = (rflags >> 9) & 1;
                    let regs = self.regs();
                    info!(
                        "[CPUID-IN] #{count}: leaf={leaf:#x}, sub={:#x}, RIP={:#x}, IF={if_flag}",
                        regs.rcx as u32,
                        self.rip()
                    );
                }
            }
            reason => {
                static EXIT_COUNT: core::sync::atomic::AtomicU64 =
                    core::sync::atomic::AtomicU64::new(0);
                let count = EXIT_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                if count < 20 || count == 100 || count == 1000 || count == 10000 {
                    let extra = match reason {
                        VmxExitReason::MSR_READ => {
                            alloc::format!(", MSR={:#x}", self.regs().rcx as u32)
                        }
                        VmxExitReason::MSR_WRITE => {
                            alloc::format!(", MSR={:#x}", self.regs().rcx as u32)
                        }
                        _ => alloc::string::String::new(),
                    };
                    info!(
                        "[VMX-DEBUG] Non-IO exit #{count}: reason={reason:?}, RIP={:#x}{}",
                        self.rip(),
                        extra
                    );
                }
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
        self.setup_vmcs_guest(entry, boot_mode)?;
        self.setup_vmcs_control(ept_root, true)?;
        self.unbind_from_current_processor()?;
        Ok(())
    }

    fn setup_vmcs_host(&self) -> AxResult {
        VmcsHost64::IA32_PAT.write(Msr::IA32_PAT.read())?;
        VmcsHost64::IA32_EFER.write(Msr::IA32_EFER.read())?;

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
        set_guest_segment!(TR, 0x8b); // present, system, 32-bit TSS busy
        set_guest_segment!(LDTR, 0x82); // present, system, LDT

        // In UEFI mode, the reset vector is at 0xFFFFFFF0 which is near the top
        // of the 4GB address space. CS base must be set to 0xFFFF0000 so that
        // the linear address (CS_base + IP) = 0xFFFF0000 + 0xFFF0 = 0xFFFFFFF0.
        // This matches the x86 hardware reset state: CS=0xF000, IP=0xFFF0.
        info!("[VMX setup] boot_mode={:?}, setting CS for UEFI", boot_mode);
        if boot_mode == X86BootMode::Uefi {
            VmcsGuestNW::CS_BASE.write(0xFFFF0000)?;
            VmcsGuest16::CS_SELECTOR.write(0xF000)?;
            info!("[VMX setup] Set CS_SELECTOR=0xF000, CS_BASE=0xFFFF0000");
        }

        VmcsGuestNW::GDTR_BASE.write(0)?;
        VmcsGuest32::GDTR_LIMIT.write(0xffff)?;
        VmcsGuestNW::IDTR_BASE.write(0)?;
        VmcsGuest32::IDTR_LIMIT.write(0xffff)?;

        VmcsGuestNW::CR3.write(0)?;
        VmcsGuestNW::DR7.write(0x400)?;
        VmcsGuestNW::RSP.write(0)?;
        // In UEFI mode, RIP should be 0xFFF0 (the offset within the CS segment),
        // not the full linear address 0xFFFFFFF0. The linear address is computed
        // by the processor as CS.base + RIP = 0xFFFF0000 + 0xFFF0 = 0xFFFFFFF0.
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

        VmcsGuest64::LINK_PTR.write(u64::MAX)?; // SDM Vol. 3C, Section 24.4.2
        VmcsGuest64::IA32_DEBUGCTL.write(0)?;
        VmcsGuest64::IA32_PAT.write(Msr::IA32_PAT.read())?;
        VmcsGuest64::IA32_EFER.write(0)?;
        Ok(())
    }

    fn setup_vmcs_control(&mut self, ept_root: HostPhysAddr, is_guest: bool) -> AxResult {
        // Intercept NMI and external interrupts.
        use PinbasedControls as PinCtrl;

        use super::vmcs::controls::*;
        let raw_cpuid = CpuId::new();

        vmcs::set_control(
            VmcsControl32::PINBASED_EXEC_CONTROLS,
            Msr::IA32_VMX_TRUE_PINBASED_CTLS,
            Msr::IA32_VMX_PINBASED_CTLS.read() as u32,
            (PinCtrl::NMI_EXITING | PinCtrl::EXTERNAL_INTERRUPT_EXITING).bits(),
            // (PinCtrl::NMI_EXITING | PinCtrl::VMX_PREEMPTION_TIMER).bits(),
            // PinCtrl::NMI_EXITING.bits(),
            0,
        )?;

        // Intercept all I/O instructions, use MSR bitmaps, activate secondary controls,
        // disable CR3 load/store interception, intercept HLT for UEFI wait loops.
        use PrimaryControls as CpuCtrl;
        vmcs::set_control(
            VmcsControl32::PRIMARY_PROCBASED_EXEC_CONTROLS,
            Msr::IA32_VMX_TRUE_PROCBASED_CTLS,
            Msr::IA32_VMX_PROCBASED_CTLS.read() as u32,
            (CpuCtrl::USE_IO_BITMAPS
                | CpuCtrl::USE_MSR_BITMAPS
                | CpuCtrl::SECONDARY_CONTROLS
                | CpuCtrl::HLT_EXITING)
                .bits(),
            (CpuCtrl::CR3_LOAD_EXITING
                | CpuCtrl::CR3_STORE_EXITING
                | CpuCtrl::CR8_LOAD_EXITING
                | CpuCtrl::CR8_STORE_EXITING)
                .bits(),
        )?;

        // Enable EPT, RDTSCP, INVPCID, and unrestricted guest.
        use SecondaryControls as CpuCtrl2;
        let mut val =
            // CpuCtrl2::VIRTUALIZE_APIC | 
            CpuCtrl2::ENABLE_EPT | CpuCtrl2::UNRESTRICTED_GUEST;
        if let Some(features) = raw_cpuid.get_extended_processor_and_feature_identifiers()
            && features.has_rdtscp()
        {
            val |= CpuCtrl2::ENABLE_RDTSCP;
        }
        if let Some(features) = raw_cpuid.get_extended_feature_info()
            && features.has_invpcid()
        {
            val |= CpuCtrl2::ENABLE_INVPCID;
        }
        if let Some(features) = raw_cpuid.get_extended_state_info()
            && features.has_xsaves_xrstors()
        {
            val |= CpuCtrl2::ENABLE_XSAVES_XRSTORS;
        }
        vmcs::set_control(
            VmcsControl32::SECONDARY_PROCBASED_EXEC_CONTROLS,
            Msr::IA32_VMX_PROCBASED_CTLS2,
            Msr::IA32_VMX_PROCBASED_CTLS2.read() as u32,
            val.bits(),
            0,
        )?;

        // Switch to 64-bit host, acknowledge interrupt info, switch IA32_PAT/IA32_EFER on VM exit.
        use ExitControls as ExitCtrl;
        vmcs::set_control(
            VmcsControl32::VMEXIT_CONTROLS,
            Msr::IA32_VMX_TRUE_EXIT_CTLS,
            Msr::IA32_VMX_EXIT_CTLS.read() as u32,
            (ExitCtrl::HOST_ADDRESS_SPACE_SIZE
                | ExitCtrl::ACK_INTERRUPT_ON_EXIT
                | ExitCtrl::SAVE_IA32_PAT
                | ExitCtrl::LOAD_IA32_PAT
                | ExitCtrl::SAVE_IA32_EFER
                | ExitCtrl::LOAD_IA32_EFER)
                .bits(),
            0,
        )?;

        let mut val = EntryCtrl::LOAD_IA32_PAT | EntryCtrl::LOAD_IA32_EFER;

        if !is_guest {
            // IA-32e mode guest
            // On processors that support Intel 64 architecture, this control determines whether the logical processor is in IA-32e mode after VM entry.
            // Its value is loaded into IA32_EFER.LMA as part of VM entry.
            val |= EntryCtrl::IA32E_MODE_GUEST;
        }

        // Load guest IA32_PAT/IA32_EFER on VM entry.
        use EntryControls as EntryCtrl;
        vmcs::set_control(
            VmcsControl32::VMENTRY_CONTROLS,
            Msr::IA32_VMX_TRUE_ENTRY_CTLS,
            Msr::IA32_VMX_ENTRY_CTLS.read() as u32,
            val.bits(),
            0,
        )?;

        vmcs::set_ept_pointer(ept_root)?;

        // No MSR switches if hypervisor doesn't use and there is only one vCPU.
        VmcsControl32::VMEXIT_MSR_STORE_COUNT.write(0)?;
        VmcsControl32::VMEXIT_MSR_LOAD_COUNT.write(0)?;
        VmcsControl32::VMENTRY_MSR_LOAD_COUNT.write(0)?;

        // VmcsControlNW::CR4_GUEST_HOST_MASK.write(0)?;
        VmcsControl32::CR3_TARGET_COUNT.write(0)?;

        // Pass-through exceptions (except #UD(6)), don't use I/O bitmap, set MSR bitmaps.
        let exception_bitmap: u32 = 1 << 6;

        self.setup_io_bitmap()?;

        VmcsControl32::EXCEPTION_BITMAP.write(exception_bitmap)?;
        VmcsControl64::IO_BITMAP_A_ADDR.write(self.io_bitmap.phys_addr().0.as_usize() as _)?;
        VmcsControl64::IO_BITMAP_B_ADDR.write(self.io_bitmap.phys_addr().1.as_usize() as _)?;
        VmcsControl64::MSR_BITMAPS_ADDR.write(self.msr_bitmap.phys_addr().as_usize() as _)?;

        // VmcsControl64::APIC_ACCESS_ADDR.write(
        //     EmulatedLocalApic::<H::MmHal, DummyHal>::virtual_apic_access_addr().as_usize() as _,
        // )?;
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
                    let must0 = Msr::IA32_VMX_CR0_FIXED1.read()
                        & !(Cr0Flags::NOT_WRITE_THROUGH | Cr0Flags::CACHE_DISABLE).bits();
                    let must1 = Msr::IA32_VMX_CR0_FIXED0.read()
                        & !(Cr0Flags::PAGING | Cr0Flags::PROTECTED_MODE_ENABLE).bits();
                    VmcsGuestNW::CR0.write(((val & must0) | must1) as _)?;
                    VmcsControlNW::CR0_READ_SHADOW.write(val as _)?;
                    VmcsControlNW::CR0_GUEST_HOST_MASK.write((must1 | !must0) as _)?;
                }
                3 => VmcsGuestNW::CR3.write(val as _)?,
                4 => {
                    // Retrieve/validate restrictions on CR4
                    let must0 = Msr::IA32_VMX_CR4_FIXED1.read();
                    let must1 = Msr::IA32_VMX_CR4_FIXED0.read();
                    let val = val | Cr4Flags::VIRTUAL_MACHINE_EXTENSIONS.bits();
                    VmcsGuestNW::CR4.write(((val & must0) | must1) as _)?;
                    VmcsControlNW::CR4_READ_SHADOW.write(val as _)?;
                    VmcsControlNW::CR4_GUEST_HOST_MASK.write((must1 | !must0) as _)?;
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
        panic!("{}", vmcs::instruction_error().as_str())
    }

    /// Whether the guest interrupts are blocked. (SDM Vol. 3C, Section 24.4.2, Table 24-3)
    fn allow_interrupt(&self) -> bool {
        let rflags = VmcsGuestNW::RFLAGS.read().unwrap();
        let block_state = VmcsGuest32::INTERRUPTIBILITY_STATE.read().unwrap();
        rflags as u64 & x86_64::registers::rflags::RFlags::INTERRUPT_FLAG.bits() != 0
            && block_state == 0
    }

    /// Try to inject a pending event before next VM entry.
    fn inject_pending_events(&mut self) -> AxResult {
        vmcs::clear_injection()?;

        if let Some(vioapic) = GLOBAL_VIOAPIC.get() {
            let vcpu_id = self.vlapic.timer_where_am_i().1 as u32;
            let pending = vioapic.take_pending_irqs(vcpu_id);
            for vector in pending {
                debug!(
                    "[IOAPIC] Injecting pending IRQ vector={:#x} to vcpu={}",
                    vector, vcpu_id
                );
                self.vlapic.set_intr(vcpu_id, vector as u32);
                self.queue_external_interrupt(vector);
            }
        }

        if let Some(event) = self.pending_events.front() {
            let can_inject =
                !matches!(event.int_type, VmxInterruptionType::External) || self.allow_interrupt();
            if can_inject {
                info!(
                    "[INTR] Injecting interrupt vector={:#x} type={:?}",
                    event.vector, event.int_type
                );
                vmcs::inject_event_with_type(event.vector, event.err_code, event.int_type)?;
                self.pending_events.pop_front();
            } else {
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
            VmxExitReason::INTERRUPT_WINDOW => Some(self.set_interrupt_window(false)),
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
            VmxExitReason::APIC_ACCESS => Some(self.handle_apic_access(exit_info)),
            VmxExitReason::EPT_VIOLATION => {
                if let Ok(info) = self.nested_page_fault_info() {
                    let gpa = info.fault_guest_paddr.as_usize();
                    let flags = info.access_flags;
                    static EPT_VIOL_COUNT: core::sync::atomic::AtomicU64 =
                        core::sync::atomic::AtomicU64::new(0);
                    let vc = EPT_VIOL_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    if vc < 20 || vc == 100 || vc == 1000 {
                        info!(
                            "[EPT-VIOL] #{vc}: GPA={gpa:#x}, RIP={:#x}, flags={:?}",
                            self.rip(),
                            flags
                        );
                    }
                    if (0xFEE0_0000..0xFEE0_1000).contains(&gpa) {
                        Some(self.handle_apic_mmio_ept_violation())
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

        if write {
            let value = self.read_edx_eax();
            let new_base = value & 0xFFFF_F000;
            let x2apic = (value & X2APIC_ENABLE) != 0;
            let enabled = (value & APIC_GLOBAL_ENABLE) != 0;
            info!(
                "[APIC-BASE] write: value={value:#x}, base={new_base:#x}, x2apic={x2apic}, \
                 enabled={enabled}"
            );
            if new_base != APIC_BASE_ADDR {
                warn!("[APIC-BASE] guest tried to change APIC base to {new_base:#x}, ignoring");
            }
        } else {
            let value = APIC_BASE_ADDR | APIC_GLOBAL_ENABLE | X2APIC_ENABLE;
            info!("[APIC-BASE] read: returning {value:#x}");
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
            debug!("[TSC-DEADLINE] write: value={value:#x}");
            if value != 0 {
                let current_tsc = unsafe { core::arch::x86_64::_rdtsc() };
                if value > current_tsc {
                    let delta = value - current_tsc;
                    let (_vm_id, _vcpu_id) = self.vlapic.timer_where_am_i();
                    let vector = self.vlapic.timer_vector();
                    let is_masked = self.vlapic.timer_is_masked();
                    let timer_val = self.vlapic.timer_read_lvt();

                    let _ = self.vlapic.timer_stop();

                    debug!(
                        "[TSC-DEADLINE] Setting deadline: current_tsc={current_tsc:#x}, \
                         deadline={value:#x}, delta={delta:#x}, vector={vector}, \
                         masked={is_masked}, lvt={timer_val:#x}"
                    );

                    self.vlapic.set_tsc_deadline(value);
                }
            } else {
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
        let apic_access_exit_info = self.apic_access_exit_info()?;

        let write = match apic_access_exit_info.access_type {
            ApicAccessExitType::LinearDataWrite => true,
            ApicAccessExitType::LinearDataRead => false,
            _ => {
                warn!(
                    "Unsupported APIC access type: {:?}",
                    apic_access_exit_info.access_type
                );
                return ax_err!(BadState, "Unsupported APIC access type");
            }
        };

        // TODO: handle APIC access.
        let _ = write;

        self.advance_rip(exit_info.exit_instruction_length as _)?;

        unimplemented!("apic access");
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

    fn read_guest_instr_bytes(&self, max_len: usize) -> Option<([u8; 15], usize)> {
        let guest_rip = self.rip() as u64;
        let cr3 = VmcsGuestNW::CR3.read().ok()? as u64;
        let ept_root = self.ept_root?;

        let gpa = self.gva_to_gpa_via_guest_pt(cr3, guest_rip)?;
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

    fn handle_vmx_preemption_timer(&mut self) -> AxResult {
        // The VMX-preemption timer counts down at rate proportional to that of the timestamp counter (TSC).
        // Specifically, the timer counts down by 1 every time bit X in the TSC changes due to a TSC increment.
        // The value of X is in the range 0–31 and can be determined by consulting the VMX capability MSR IA32_VMX_MISC (see Appendix A.6).
        VmcsGuest32::VMX_PREEMPTION_TIMER_VALUE.write(VMX_PREEMPTION_TIMER_SET_VALUE)?;
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
        const VM_EXIT_INSTR_LEN_MV_TO_CR: u8 = 3;

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
                if cr == 0 || cr == 4 {
                    self.advance_rip(VM_EXIT_INSTR_LEN_MV_TO_CR)?;
                    // TODO: check for #GP reasons
                    self.set_cr(cr as usize, val);

                    if cr == 0 && Cr0Flags::from_bits_truncate(val).contains(Cr0Flags::PAGING) {
                        vmcs::update_efer()?;
                    }
                    return Ok(());
                }
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
                const FEATURE_VMX: u32 = 1 << 5;
                const FEATURE_HYPERVISOR: u32 = 1 << 31;
                const FEATURE_MCE: u32 = 1 << 7;
                const FEATURE_TSC_DEADLINE: u32 = 1 << 24;
                const FEATURE_MONITOR: u32 = 1 << 3;
                let mut res = cpuid!(regs_clone.rax, regs_clone.rcx);
                res.ecx &= !FEATURE_VMX;
                res.ecx &= !FEATURE_TSC_DEADLINE;
                res.ecx &= !FEATURE_MONITOR;
                res.ecx |= FEATURE_HYPERVISOR;
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
                if res.eax == 0 {
                    res.eax = 1;
                    res.ebx = 1;
                    res.ecx = 100_000_000;
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
                res
            }
            0x8000_0001 => {
                let mut res = cpuid!(regs_clone.rax, regs_clone.rcx);
                res.ecx &= !(1 << 2);
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
            if out_count < 50 || out_count == 100 || out_count == 1000 || out_count == 10000 {
                info!(
                    "[CPUID-OUT] #{out_count}: leaf={:#x} => EAX={:#x}, EBX={:#x}, ECX={:#x}, \
                     EDX={:#x}",
                    function, res.eax, res.ebx, res.ecx, res.edx
                );
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

        let index = self.guest_regs.rcx.get_bits(0..32);
        let value = self.guest_regs.rdx.get_bits(0..32) << 32 | self.guest_regs.rax.get_bits(0..32);

        // TODO: get host-supported xcr0 mask by cpuid and reject any guest-xsetbv violating that
        if index == XCR_XCR0 {
            Xcr0::from_bits(value)
                .and_then(|x| {
                    if !x.contains(Xcr0::XCR0_FPU_MMX_STATE) {
                        return None;
                    }

                    if x.contains(Xcr0::XCR0_AVX_STATE) && !x.contains(Xcr0::XCR0_SSE_STATE) {
                        return None;
                    }

                    if x.contains(Xcr0::XCR0_BNDCSR_STATE) ^ x.contains(Xcr0::XCR0_BNDREG_STATE) {
                        return None;
                    }

                    if x.contains(Xcr0::XCR0_OPMASK_STATE)
                        || x.contains(Xcr0::XCR0_ZMM_HI256_STATE)
                        || x.contains(Xcr0::XCR0_HI16_ZMM_STATE)
                        || !x.contains(Xcr0::XCR0_AVX_STATE)
                        || !x.contains(Xcr0::XCR0_OPMASK_STATE)
                        || !x.contains(Xcr0::XCR0_ZMM_HI256_STATE)
                        || !x.contains(Xcr0::XCR0_HI16_ZMM_STATE)
                    {
                        return None;
                    }

                    Some(x)
                })
                .ok_or(ax_err_type!(InvalidInput))
                .and_then(|x| {
                    self.xstate.guest_xcr0 = x.bits();
                    self.advance_rip(VM_EXIT_INSTR_LEN_XSETBV)
                })
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
        )
    }

    fn run(&mut self) -> AxResult<AxVCpuExitReason> {
        match self.inner_run() {
            Some(exit_info) => Ok(if exit_info.entry_failure {
                let exit_reason_raw = exit_info.exit_reason as u64;
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
                                info!(
                                    "VMX EXCEPTION_NMI: inject exception vector={}, type={:?}, \
                                     err={:?}",
                                    vector, int_type, int_info.err_code
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
                            let rsp = VmcsGuestNW::RSP.read().unwrap_or(0);
                            let rflags = VmcsGuestNW::RFLAGS.read().unwrap_or(0);
                            let cr0 = VmcsGuestNW::CR0.read().unwrap_or(0);
                            let cr3 = VmcsGuestNW::CR3.read().unwrap_or(0);
                            let cr4 = VmcsGuestNW::CR4.read().unwrap_or(0);
                            let cs = VmcsGuest16::CS_SELECTOR.read().unwrap_or(0);
                            let cs_base = VmcsGuestNW::CS_BASE.read().unwrap_or(0);
                            let interruptibility =
                                VmcsGuest32::INTERRUPTIBILITY_STATE.read().unwrap_or(0);
                            let idt_vec = vmcs::idt_vectoring_info().ok().flatten();
                            error!(
                                "[TRIPLE_FAULT] RIP={:#x}, RSP={:#x}, RFLAGS={:#x}, \
                                 IDTR={:#x}:{:#x}, CR0={:#x}, CR3={:#x}, CR4={:#x}, \
                                 CS={:#x}:{:#x}, INTBL={:#x}, IDT-vec={:?}",
                                self.rip(),
                                rsp,
                                rflags,
                                idtr_base,
                                idtr_limit,
                                cr0,
                                cr3,
                                cr4,
                                cs,
                                cs_base,
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
        }
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

        info!(
            "[VLAPIC] timer expired: vector={vector:#x}, masked={is_masked}, \
             periodic={is_periodic}"
        );

        if !is_masked && vector > 0 {
            self.queue_external_interrupt(vector);
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
