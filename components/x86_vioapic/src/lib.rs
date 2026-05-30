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

//! Emulated IOAPIC (I/O Advanced Programmable Interrupt Controller).
//!
//! Implements a minimal IOAPIC for interrupt delivery from virtual devices
//! to guest vCPUs. OVMF discovers the IOAPIC via the MADT ACPI table and
//! configures it via MMIO at 0xFEC00000.

#![no_std]

extern crate alloc;

use alloc::{sync::Arc, vec::Vec};

use ax_errno::AxResult;
use ax_memory_addr::AddrRange;
use axaddrspace::{GuestPhysAddr, device::AccessWidth};
use axdevice_base::{BaseDeviceOps, EmuDeviceType};
use log::{debug, warn};
use spin::{Mutex, Once};

/// Global singleton for the virtual IOAPIC, used to dispatch interrupts
/// from devices to vCPUs across crate boundaries.
pub static GLOBAL_VIOAPIC: Once<Arc<IoApic>> = Once::new();

pub const IOAPIC_MMIO_BASE: u64 = 0xFEC0_0000;
pub const IOAPIC_MMIO_SIZE: u64 = 0x1000;

const IOAPIC_NUM_PINS: usize = 24;

const REG_IOAPICID: u32 = 0x00;
const REG_IOAPICVER: u32 = 0x01;
const REG_IOAPICARB: u32 = 0x02;
const REG_REDTBL_BASE: u32 = 0x10;

const IOAPIC_VERSION: u32 = 0x11;
const MAX_REDIRECTION_ENTRIES: u32 = 23;

#[derive(Clone, Copy, Default)]
struct RedirectionTableEntry {
    lo: u32,
    hi: u32,
}

impl RedirectionTableEntry {
    fn vector(&self) -> u8 {
        (self.lo & 0xFF) as u8
    }

    fn delivery_mode(&self) -> u8 {
        ((self.lo >> 8) & 0x7) as u8
    }

    #[allow(dead_code)]
    fn dest_mode(&self) -> bool {
        (self.lo >> 11) & 1 != 0
    }

    fn is_masked(&self) -> bool {
        (self.lo >> 16) & 1 != 0
    }

    fn trigger_mode(&self) -> u8 {
        ((self.lo >> 15) & 1) as u8
    }

    fn destination(&self) -> u8 {
        ((self.hi >> 24) & 0xFF) as u8
    }

    fn set_remote_irr(&mut self, val: bool) {
        if val {
            self.lo |= 1 << 14;
        } else {
            self.lo &= !(1 << 14);
        }
    }
}

pub struct IoApic {
    ioregsel: Mutex<u32>,
    id: u8,
    rtels: [Mutex<RedirectionTableEntry>; IOAPIC_NUM_PINS],
    gsi_base: u32,
    pending_irqs: Mutex<Vec<(u32, u8)>>,
}

impl IoApic {
    pub fn new(id: u8, gsi_base: u32) -> Self {
        Self {
            ioregsel: Mutex::new(0),
            id,
            rtels: core::array::from_fn(|_| Mutex::new(RedirectionTableEntry::default())),
            gsi_base,
            pending_irqs: Mutex::new(Vec::new()),
        }
    }

    pub fn handle_mmio_read(&self, offset: u64, _width: u8) -> AxResult<usize> {
        match offset {
            0x00 => {
                let sel = *self.ioregsel.lock();
                debug!("[IOAPIC] read IOREGSEL: {:#x}", sel);
                Ok(sel as usize)
            }
            0x10 => {
                let sel = *self.ioregsel.lock();
                let val = self.read_reg(sel);
                debug!("[IOAPIC] read reg[{}] = {:#x}", sel, val);
                Ok(val as usize)
            }
            _ => {
                warn!("[IOAPIC] unhandled MMIO read at offset {:#x}", offset);
                Ok(0)
            }
        }
    }

    pub fn handle_mmio_write(&self, offset: u64, _width: u8, val: usize) -> AxResult {
        match offset {
            0x00 => {
                *self.ioregsel.lock() = val as u32;
                debug!("[IOAPIC] write IOREGSEL: {:#x}", val);
            }
            0x10 => {
                let sel = *self.ioregsel.lock();
                self.write_reg(sel, val as u32);
                debug!("[IOAPIC] write reg[{}] = {:#x}", sel, val);
            }
            _ => {
                warn!(
                    "[IOAPIC] unhandled MMIO write at offset {:#x}, val={:#x}",
                    offset, val
                );
            }
        }
        Ok(())
    }

    pub fn raise_irq(&self, gsi: u32) {
        if gsi >= IOAPIC_NUM_PINS as u32 {
            warn!(
                "[IOAPIC] IRQ {} out of range (max {})",
                gsi, IOAPIC_NUM_PINS
            );
            return;
        }
        let rte = self.rtels[gsi as usize].lock();
        if rte.is_masked() {
            debug!("[IOAPIC] IRQ {} masked, ignoring", gsi);
            return;
        }
        let vector = rte.vector();
        let dest = rte.destination();
        debug!(
            "[IOAPIC] IRQ {}: vector={}, dest={}, delivery_mode={}",
            gsi,
            vector,
            dest,
            rte.delivery_mode()
        );
        self.pending_irqs.lock().push((dest as u32, vector));
    }

    pub fn take_pending_irqs(&self, vcpu_id: u32) -> Vec<u8> {
        let mut pending = self.pending_irqs.lock();
        let mut result = Vec::new();
        pending.retain(|(vcpu, vector)| {
            if *vcpu == vcpu_id {
                result.push(*vector);
                false
            } else {
                true
            }
        });
        result
    }

    pub fn get_irq_rte(&self, gsi: u32) -> Option<(u8, u8, bool)> {
        if gsi >= IOAPIC_NUM_PINS as u32 {
            return None;
        }
        let rte = self.rtels[gsi as usize].lock();
        Some((rte.vector(), rte.destination(), rte.is_masked()))
    }

    pub fn eoi(&self, vector: u8) {
        for (gsi, rte_cell) in self.rtels.iter().enumerate() {
            let mut rte = rte_cell.lock();
            if rte.vector() == vector && rte.trigger_mode() == 1 {
                rte.set_remote_irr(false);
            }
            let _ = gsi;
        }
    }

    fn read_reg(&self, index: u32) -> u32 {
        match index {
            REG_IOAPICID => self.id as u32,
            REG_IOAPICVER => IOAPIC_VERSION | (MAX_REDIRECTION_ENTRIES << 16),
            REG_IOAPICARB => self.id as u32,
            idx @ REG_REDTBL_BASE..=0x3F => {
                let pin = (idx - REG_REDTBL_BASE) as usize;
                if pin >= IOAPIC_NUM_PINS {
                    warn!("[IOAPIC] read invalid RTE index {}", idx);
                    return 0;
                }
                let rte = self.rtels[pin / 2].lock();
                if pin.is_multiple_of(2) {
                    rte.lo
                } else {
                    rte.hi
                }
            }
            _ => {
                warn!("[IOAPIC] read unknown register {}", index);
                0
            }
        }
    }

    fn write_reg(&self, index: u32, val: u32) {
        match index {
            REG_IOAPICID => {
                warn!("[IOAPIC] attempt to write IOAPICID (read-only), ignoring");
            }
            REG_IOAPICVER => {
                warn!("[IOAPIC] attempt to write IOAPICVER (read-only), ignoring");
            }
            idx @ REG_REDTBL_BASE..=0x3F => {
                let pin = (idx - REG_REDTBL_BASE) as usize;
                if pin >= IOAPIC_NUM_PINS {
                    warn!("[IOAPIC] write invalid RTE index {}", idx);
                    return;
                }
                let mut rte = self.rtels[pin / 2].lock();
                if pin.is_multiple_of(2) {
                    rte.lo = val & 0xFFFE_FFFF;
                    rte.lo &= !(1 << 12);
                } else {
                    rte.hi = val & 0xFF00_0000;
                }
            }
            _ => {
                warn!("[IOAPIC] write unknown register {} = {:#x}", index, val);
            }
        }
    }

    pub fn get_num_pins(&self) -> usize {
        IOAPIC_NUM_PINS
    }

    pub fn get_gsi_base(&self) -> u32 {
        self.gsi_base
    }
}

impl BaseDeviceOps<AddrRange<GuestPhysAddr>> for IoApic {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::InterruptController
    }

    fn address_range(&self) -> AddrRange<GuestPhysAddr> {
        AddrRange::new(
            GuestPhysAddr::from(IOAPIC_MMIO_BASE as usize),
            GuestPhysAddr::from((IOAPIC_MMIO_BASE + IOAPIC_MMIO_SIZE) as usize),
        )
    }

    fn handle_read(&self, addr: GuestPhysAddr, _width: AccessWidth) -> AxResult<usize> {
        let offset = addr.as_usize() as u64 - IOAPIC_MMIO_BASE;
        self.handle_mmio_read(offset, 4)
    }

    fn handle_write(&self, addr: GuestPhysAddr, _width: AccessWidth, val: usize) -> AxResult {
        let offset = addr.as_usize() as u64 - IOAPIC_MMIO_BASE;
        self.handle_mmio_write(offset, 4, val)
    }
}
