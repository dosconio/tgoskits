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

//! Emulated Local APIC.
#![no_std]
#![doc = include_str!("../README.md")]

extern crate alloc;

#[macro_use]
extern crate log;

mod consts;
mod regs;
mod timer;
mod utils;
mod vlapic;

use core::cell::UnsafeCell;

use ax_errno::AxResult;
use ax_memory_addr::{AddrRange, PAGE_SIZE_4K};
use axaddrspace::{
    GuestPhysAddr, HostPhysAddr, HostVirtAddr,
    device::{AccessWidth, SysRegAddr, SysRegAddrRange},
};
use axdevice_base::{BaseDeviceOps, EmuDeviceType};
use axvisor_api::{
    memory,
    vmm::{VCpuId, VMId},
};
pub use vlapic::PendingInitSipi;

use crate::{
    consts::{x2apic::x2apic_msr_access_reg, xapic::xapic_mmio_access_reg_offset},
    vlapic::VirtualApicRegs,
};

#[repr(align(4096))]
struct APICAccessPage([u8; PAGE_SIZE_4K]);

static VIRTUAL_APIC_ACCESS_PAGE: APICAccessPage = APICAccessPage([0; PAGE_SIZE_4K]);

/// A emulated local APIC device.
pub struct EmulatedLocalApic {
    vlapic_regs: UnsafeCell<VirtualApicRegs>,
}

impl EmulatedLocalApic {
    /// Create a new `EmulatedLocalApic`.
    pub fn new(vm_id: VMId, vcpu_id: VCpuId) -> Self {
        EmulatedLocalApic {
            vlapic_regs: UnsafeCell::new(VirtualApicRegs::new(vm_id, vcpu_id)),
        }
    }

    fn get_vlapic_regs(&self) -> &VirtualApicRegs {
        unsafe { &*self.vlapic_regs.get() }
    }

    #[allow(clippy::mut_from_ref)] // SAFETY: get_mut_vlapic_regs is never called concurrently.
    fn get_mut_vlapic_regs(&self) -> &mut VirtualApicRegs {
        unsafe { &mut *self.vlapic_regs.get() }
    }
}

impl EmulatedLocalApic {
    /// APIC-access address (64 bits).
    /// This field contains the physical address of the 4-KByte APIC-access page.
    /// If the “virtualize APIC accesses” VM-execution control is 1,
    /// access to this page may cause VM exits or be virtualized by the processor.
    /// See Section 30.4.
    pub fn virtual_apic_access_addr() -> HostPhysAddr {
        memory::virt_to_phys(HostVirtAddr::from_usize(
            VIRTUAL_APIC_ACCESS_PAGE.0.as_ptr() as usize,
        ))
    }

    /// Virtual-APIC address (64 bits).
    /// This field contains the physical address of the 4-KByte virtual-APIC page.
    /// The processor uses the virtual-APIC page to virtualize certain accesses to APIC registers and to manage virtual interrupts;
    /// see Chapter 30.
    pub fn virtual_apic_page_addr(&self) -> HostPhysAddr {
        self.get_vlapic_regs().virtual_apic_page_addr()
    }

    pub fn set_tsc_deadline(&self, deadline: u64) {
        self.get_mut_vlapic_regs().set_tsc_deadline(deadline);
    }

    pub fn get_tsc_deadline(&self) -> u64 {
        self.get_vlapic_regs().get_tsc_deadline()
    }

    pub fn timer_vector(&self) -> u8 {
        self.get_vlapic_regs().timer_vector()
    }

    pub fn timer_is_masked(&self) -> bool {
        self.get_vlapic_regs().timer_is_masked()
    }

    pub fn timer_read_lvt(&self) -> u32 {
        self.get_vlapic_regs().timer_read_lvt()
    }

    pub fn timer_stop(&self) -> AxResult {
        self.get_mut_vlapic_regs().timer_stop()
    }

    pub fn timer_where_am_i(&self) -> (VMId, VCpuId) {
        self.get_vlapic_regs().timer_where_am_i()
    }

    pub fn timer_is_periodic(&self) -> bool {
        self.get_vlapic_regs().timer_is_periodic()
    }

    pub fn timer_restart(&self) -> AxResult {
        self.get_mut_vlapic_regs().timer_restart()
    }

    /// Take the pending timer interrupt vector (if any) and clear it.
    pub fn take_pending_timer_vector(&self) -> u8 {
        self.get_mut_vlapic_regs().take_pending_timer_vector()
    }

    /// Start a TSC-Deadline timer.
    pub fn start_tsc_deadline_timer(&self, tsc_deadline: u64) -> AxResult {
        self.get_mut_vlapic_regs()
            .start_tsc_deadline_timer(tsc_deadline)
    }

    pub fn set_intr(&self, vcpu_id: u32, vector: u32) {
        self.get_mut_vlapic_regs().set_intr(vcpu_id, vector, false);
    }

    pub fn has_pending_interrupt(&self) -> bool {
        self.get_vlapic_regs().has_pending_interrupt()
    }

    /// Returns true if the APIC is software-enabled (SVR bit 8 set).
    pub fn is_software_enabled(&self) -> bool {
        self.get_vlapic_regs().is_software_enabled()
    }

    /// Returns the current IA32_APIC_BASE MSR value.
    pub fn apic_base(&self) -> u64 {
        self.get_vlapic_regs().apic_base()
    }

    /// Updates the IA32_APIC_BASE MSR value (enable bits honored, base forced).
    pub fn set_apic_base(&self, value: u64) {
        self.get_mut_vlapic_regs().set_apic_base(value);
    }

    /// Take the pending INIT/SIPI request, if any. Consumes the request.
    pub fn take_pending_init_sipi(&self) -> Option<crate::vlapic::PendingInitSipi> {
        self.get_mut_vlapic_regs().take_pending_init_sipi()
    }
}

impl BaseDeviceOps<AddrRange<GuestPhysAddr>> for EmulatedLocalApic {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::InterruptController
    }

    fn address_range(&self) -> AddrRange<GuestPhysAddr> {
        use crate::consts::xapic::{APIC_MMIO_SIZE, DEFAULT_APIC_BASE};
        AddrRange::new(
            GuestPhysAddr::from_usize(DEFAULT_APIC_BASE),
            GuestPhysAddr::from_usize(DEFAULT_APIC_BASE + APIC_MMIO_SIZE),
        )
    }

    fn handle_read(&self, addr: GuestPhysAddr, width: AccessWidth) -> AxResult<usize> {
        // debug!("EmulatedLocalApic::handle_read: addr={addr:?}, width={width:?}");
        match xapic_mmio_access_reg_offset(addr) {
            Some(reg_off) => self.get_vlapic_regs().handle_read(reg_off, width),
            None => {
                warn!("APIC MMIO read to reserved offset: {addr:#x?}");
                Ok(0)
            }
        }
    }

    fn handle_write(&self, addr: GuestPhysAddr, width: AccessWidth, val: usize) -> AxResult {
        // debug!("EmulatedLocalApic::handle_write: addr={addr:?}, width={width:?}, val={val:#x}");
        match xapic_mmio_access_reg_offset(addr) {
            Some(reg_off) => self.get_mut_vlapic_regs().handle_write(reg_off, val, width),
            None => {
                warn!("APIC MMIO write to reserved offset: {addr:#x?}, val={val:#x}");
                Ok(())
            }
        }
    }
}

impl BaseDeviceOps<SysRegAddrRange> for EmulatedLocalApic {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::InterruptController
    }

    fn address_range(&self) -> SysRegAddrRange {
        use crate::consts::x2apic::{X2APIC_MSE_REG_BASE, X2APIC_MSE_REG_SIZE};
        SysRegAddrRange::new(
            SysRegAddr(X2APIC_MSE_REG_BASE),
            SysRegAddr(X2APIC_MSE_REG_BASE + X2APIC_MSE_REG_SIZE),
        )
    }

    fn handle_read(&self, addr: SysRegAddr, width: AccessWidth) -> AxResult<usize> {
        // debug!("EmulatedLocalApic::handle_read: addr={addr:?}, width={width:?}");
        match x2apic_msr_access_reg(addr) {
            Some(reg_off) => self.get_vlapic_regs().handle_read(reg_off, width),
            None => {
                warn!("APIC MSR read to reserved offset: {addr:?}");
                Ok(0)
            }
        }
    }

    fn handle_write(&self, addr: SysRegAddr, width: AccessWidth, val: usize) -> AxResult {
        // debug!("EmulatedLocalApic::handle_write: addr={addr:?}, width={width:?}, val={val:#x}");
        match x2apic_msr_access_reg(addr) {
            Some(reg_off) => self.get_mut_vlapic_regs().handle_write(reg_off, val, width),
            None => {
                warn!("APIC MSR write to reserved offset: {addr:?}, val={val:#x}");
                Ok(())
            }
        }
    }
}
