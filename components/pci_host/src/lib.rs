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

//! PCI Host Bridge emulation for UEFI guest boot.
//!
//! This module provides PCI configuration space access via PIO (Port I/O):
//! - 0xCF8: Configuration Address Port (32-bit)
//! - 0xCFC: Configuration Data Port (32-bit)
//!
//! The PCI host bridge itself appears at Bus 0, Device 0, Function 0.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;

use ax_errno::AxResult;
use axaddrspace::{
    GuestPhysAddr, GuestPhysAddrRange,
    device::{AccessWidth, Port, PortRange},
};
use axdevice_base::{BaseDeviceOps, EmuDeviceType};
use log::{debug, info, warn};
use spin::Mutex;

mod config_space;
pub use config_space::{BarInfo, PciConfigSpace, PciDeviceConfig};

pub mod consts;
pub use consts::*;

const CONFIG_ADDR_RANGE_END: u16 = PCI_CONFIG_ADDRESS + 3;
const CONFIG_DATA_RANGE_END: u16 = PCI_CONFIG_DATA + 3;

/// ECAM (Enhanced Configuration Access Mechanism) base address.
/// QEMU Q35 uses 0xB000_0000 for the MCFG ACPI table.
pub const ECAM_BASE: u64 = 0xB000_0000;
/// ECAM size: 256 buses × 1MB per bus = 256 MB.
pub const ECAM_SIZE: u64 = 0x1000_0000;

type PciDeviceMap = alloc::collections::BTreeMap<(u8, u8, u8), Arc<dyn PciDevice>>;

pub trait PciDevice {
    fn config_space(&self) -> &PciConfigSpace;
    fn bdf(&self) -> (u8, u8, u8);
    fn on_bar_write(&self, _bar_index: usize) {}
}

pub struct PciHostBridge {
    config_address: Mutex<u32>,
    devices: Mutex<PciDeviceMap>,
    host_bridge_config: PciConfigSpace,
    lpc_bridge_config: PciConfigSpace,
}

impl PciHostBridge {
    pub fn new() -> Self {
        let host_bridge_config = PciConfigSpace::new_host_bridge();
        let lpc_bridge_config = PciConfigSpace::new_lpc_bridge();
        Self {
            config_address: Mutex::new(0),
            devices: Mutex::new(PciDeviceMap::new()),
            host_bridge_config,
            lpc_bridge_config,
        }
    }

    pub fn add_device(&self, device: Arc<dyn PciDevice>) {
        let bdf = device.bdf();
        self.devices.lock().insert(bdf, device);
        debug!(
            "PCI: Added device at Bus={}, Dev={}, Func={}",
            bdf.0, bdf.1, bdf.2
        );
    }

    fn parse_config_address(addr: u32) -> Option<(u8, u8, u8, u8)> {
        if (addr & PCI_CONFIG_ENABLE) == 0 {
            return None;
        }
        let bus = ((addr >> 16) & 0xFF) as u8;
        let dev = ((addr >> 11) & 0x1F) as u8;
        let func = ((addr >> 8) & 0x7) as u8;
        let reg = (addr & 0xFC) as u8;
        Some((bus, dev, func, reg))
    }

    fn handle_config_read(&self, port_offset: u8, width: AccessWidth) -> AxResult<usize> {
        let config_addr = *self.config_address.lock();

        let Some((bus, dev, func, reg)) = Self::parse_config_address(config_addr) else {
            return Ok(0xFFFF_FFFF_usize);
        };

        let full_reg = reg;
        if bus == 0 && dev == 0 && func == 0 {
            let full_val = self.host_bridge_config.read(full_reg, AccessWidth::Dword);
            let val = Self::extract_bytes(full_val, port_offset, width);
            debug!(
                "PCI host bridge read: reg={:#x} val={:#x} (bus={} dev={} func={} offset={} \
                 width={})",
                full_reg,
                val,
                bus,
                dev,
                func,
                port_offset,
                width.size()
            );
            return Ok(val);
        }

        if bus == 0 && dev == 31 && func == 0 {
            let full_val = self.lpc_bridge_config.read(full_reg, AccessWidth::Dword);
            let val = Self::extract_bytes(full_val, port_offset, width);
            // Log PM base (offset 0x40) and ACPI control (offset 0x44) reads at info level
            if full_reg == 0x40 || full_reg == 0x44 {
                info!(
                    "[LPC-READ] reg={:#x} full_val={:#x} val={:#x} offset={} width={}",
                    full_reg,
                    full_val,
                    val,
                    port_offset,
                    width.size()
                );
            }
            debug!(
                "PCI LPC bridge read: reg={:#x} val={:#x} (bus={} dev={} func={} offset={} \
                 width={})",
                full_reg,
                val,
                bus,
                dev,
                func,
                port_offset,
                width.size()
            );
            return Ok(val);
        }

        let devices = self.devices.lock();
        if let Some(device) = devices.get(&(bus, dev, func)) {
            let full_val = device.config_space().read(full_reg, AccessWidth::Dword);
            let val = Self::extract_bytes(full_val, port_offset, width);
            debug!(
                "PCI device ({},{},{}) read: reg={:#x} val={:#x}",
                bus, dev, func, full_reg, val
            );
            Ok(val)
        } else {
            debug!("PCI no device at Bus={} Dev={} Func={}", bus, dev, func);
            Ok(0xFFFF_FFFF_usize)
        }
    }

    fn extract_bytes(full_val: usize, port_offset: u8, width: AccessWidth) -> usize {
        let shift = port_offset * 8;
        let mask = match width {
            AccessWidth::Byte => 0xFF,
            AccessWidth::Word => 0xFFFF,
            AccessWidth::Dword => 0xFFFF_FFFF,
            AccessWidth::Qword => return full_val,
        };
        (full_val >> shift) & mask
    }

    fn handle_config_write(&self, port_offset: u8, width: AccessWidth, val: usize) -> AxResult {
        let config_addr = *self.config_address.lock();

        let Some((bus, dev, func, reg)) = Self::parse_config_address(config_addr) else {
            return Ok(());
        };

        let full_reg = reg;

        // Log all writes to host bridge and LPC bridge at debug level
        if bus == 0 && (dev == 0 || dev == 31) && func == 0 {
            debug!(
                "PCI config write: Bus={} Dev={} Func={} Reg={:#x} offset={:#x} width={} val={:#x}",
                bus,
                dev,
                func,
                full_reg,
                port_offset,
                width.size(),
                val
            );
        }
        if bus == 0 && dev == 0 && func == 0 {
            let old_val = self.host_bridge_config.read(full_reg, AccessWidth::Dword);
            let shift = port_offset * 8;
            let mask = match width {
                AccessWidth::Byte => 0xFF,
                AccessWidth::Word => 0xFFFF,
                AccessWidth::Dword => 0xFFFF_FFFF,
                AccessWidth::Qword => 0xFFFF_FFFF_FFFF_FFFF,
            };
            let new_val = (old_val & !(mask << shift)) | ((val & mask) << shift);
            self.host_bridge_config
                .write(full_reg, AccessWidth::Dword, new_val);
            debug!(
                "PCI host bridge write: reg={:#x} old={:#x} new={:#x}",
                full_reg, old_val, new_val
            );
            return Ok(());
        }

        if bus == 0 && dev == 31 && func == 0 {
            let old_val = self.lpc_bridge_config.read(full_reg, AccessWidth::Dword);
            let shift = port_offset * 8;
            let mask = match width {
                AccessWidth::Byte => 0xFF,
                AccessWidth::Word => 0xFFFF,
                AccessWidth::Dword => 0xFFFF_FFFF,
                AccessWidth::Qword => 0xFFFF_FFFF_FFFF_FFFF,
            };
            let new_val = (old_val & !(mask << shift)) | ((val & mask) << shift);
            self.lpc_bridge_config
                .write(full_reg, AccessWidth::Dword, new_val);
            debug!(
                "PCI LPC bridge write: reg={:#x} old={:#x} new={:#x}",
                full_reg, old_val, new_val
            );
            return Ok(());
        }

        let devices = self.devices.lock();
        if let Some(device) = devices.get(&(bus, dev, func)) {
            let old_val = device.config_space().read(full_reg, AccessWidth::Dword);
            let shift = port_offset * 8;
            let mask = match width {
                AccessWidth::Byte => 0xFF,
                AccessWidth::Word => 0xFFFF,
                AccessWidth::Dword => 0xFFFF_FFFF,
                AccessWidth::Qword => 0xFFFF_FFFF_FFFF_FFFF,
            };
            let new_val = (old_val & !(mask << shift)) | ((val & mask) << shift);
            device
                .config_space()
                .write(full_reg, AccessWidth::Dword, new_val);

            if let Some(bar_idx) = Self::bar_index_from_reg(full_reg) {
                debug!(
                    "PCI BAR{} write for device ({},{},{}): val={:#x}",
                    bar_idx, bus, dev, func, new_val
                );
                device.on_bar_write(bar_idx);
            }
        } else {
            debug!(
                "PCI no device at Bus={} Dev={} Func={}, write ignored",
                bus, dev, func
            );
        }
        Ok(())
    }

    fn bar_index_from_reg(reg: u8) -> Option<usize> {
        let reg = reg as usize;
        if reg >= PCI_BAR0 as usize && reg < PCI_BAR0 as usize + 6 * 4 {
            Some((reg - PCI_BAR0 as usize) / 4)
        } else {
            None
        }
    }

    /// Decode an ECAM MMIO address into (bus, dev, func, dword_reg, byte_offset).
    fn decode_ecam_addr(addr: GuestPhysAddr) -> (u8, u8, u8, u8, u8) {
        let offset = addr.as_usize() - ECAM_BASE as usize;
        let bus = ((offset >> 20) & 0xFF) as u8;
        let dev = ((offset >> 15) & 0x1F) as u8;
        let func = ((offset >> 12) & 0x7) as u8;
        let reg = (offset & 0xFFF) as u8;
        let dword_reg = reg & 0xFC;
        let byte_offset = reg & 0x3;
        (bus, dev, func, dword_reg, byte_offset)
    }

    /// Handle ECAM MMIO read: route to PCI config space using bus/dev/func/reg
    /// extracted from the GPA.
    fn handle_ecam_read(&self, addr: GuestPhysAddr, width: AccessWidth) -> AxResult<usize> {
        let (bus, dev, func, reg, byte_offset) = Self::decode_ecam_addr(addr);

        if bus == 0 && dev == 0 && func == 0 {
            let full_val = self.host_bridge_config.read(reg, AccessWidth::Dword);
            let val = Self::extract_bytes(full_val, byte_offset, width);
            debug!(
                "[ECAM] host bridge read: bus={} dev={} func={} reg={:#x} val={:#x} offset={} \
                 width={}",
                bus,
                dev,
                func,
                reg,
                val,
                byte_offset,
                width.size()
            );
            return Ok(val);
        }

        if bus == 0 && dev == 31 && func == 0 {
            let full_val = self.lpc_bridge_config.read(reg, AccessWidth::Dword);
            let val = Self::extract_bytes(full_val, byte_offset, width);
            if reg == 0x40 || reg == 0x44 {
                info!(
                    "[ECAM-LPC-READ] reg={:#x} full_val={:#x} val={:#x} offset={} width={}",
                    reg,
                    full_val,
                    val,
                    byte_offset,
                    width.size()
                );
            }
            return Ok(val);
        }

        let devices = self.devices.lock();
        if let Some(device) = devices.get(&(bus, dev, func)) {
            let full_val = device.config_space().read(reg, AccessWidth::Dword);
            let val = Self::extract_bytes(full_val, byte_offset, width);
            debug!(
                "[ECAM] device ({},{},{}) read: reg={:#x} val={:#x}",
                bus, dev, func, reg, val
            );
            Ok(val)
        } else {
            debug!(
                "[ECAM] no device at bus={} dev={} func={} reg={:#x}, returning 0xFFFFFFFF",
                bus, dev, func, reg
            );
            Ok(0xFFFF_FFFF_usize)
        }
    }

    /// Handle ECAM MMIO write: route to PCI config space using bus/dev/func/reg
    /// extracted from the GPA.
    fn handle_ecam_write(&self, addr: GuestPhysAddr, width: AccessWidth, val: usize) -> AxResult {
        let (bus, dev, func, reg, byte_offset) = Self::decode_ecam_addr(addr);

        if bus == 0 && (dev == 0 || dev == 31) && func == 0 {
            debug!(
                "[ECAM] config write: bus={} dev={} func={} reg={:#x} offset={} width={} val={:#x}",
                bus,
                dev,
                func,
                reg,
                byte_offset,
                width.size(),
                val
            );
        }

        if bus == 0 && dev == 0 && func == 0 {
            let old_val = self.host_bridge_config.read(reg, AccessWidth::Dword);
            let shift = byte_offset * 8;
            let mask = match width {
                AccessWidth::Byte => 0xFF,
                AccessWidth::Word => 0xFFFF,
                AccessWidth::Dword => 0xFFFF_FFFF,
                AccessWidth::Qword => 0xFFFF_FFFF_FFFF_FFFF,
            };
            let new_val = (old_val & !(mask << shift)) | ((val & mask) << shift);
            self.host_bridge_config
                .write(reg, AccessWidth::Dword, new_val);
            return Ok(());
        }

        if bus == 0 && dev == 31 && func == 0 {
            let old_val = self.lpc_bridge_config.read(reg, AccessWidth::Dword);
            let shift = byte_offset * 8;
            let mask = match width {
                AccessWidth::Byte => 0xFF,
                AccessWidth::Word => 0xFFFF,
                AccessWidth::Dword => 0xFFFF_FFFF,
                AccessWidth::Qword => 0xFFFF_FFFF_FFFF_FFFF,
            };
            let new_val = (old_val & !(mask << shift)) | ((val & mask) << shift);
            self.lpc_bridge_config
                .write(reg, AccessWidth::Dword, new_val);
            return Ok(());
        }

        let devices = self.devices.lock();
        if let Some(device) = devices.get(&(bus, dev, func)) {
            let old_val = device.config_space().read(reg, AccessWidth::Dword);
            let shift = byte_offset * 8;
            let mask = match width {
                AccessWidth::Byte => 0xFF,
                AccessWidth::Word => 0xFFFF,
                AccessWidth::Dword => 0xFFFF_FFFF,
                AccessWidth::Qword => 0xFFFF_FFFF_FFFF_FFFF,
            };
            let new_val = (old_val & !(mask << shift)) | ((val & mask) << shift);
            device
                .config_space()
                .write(reg, AccessWidth::Dword, new_val);

            if let Some(bar_idx) = Self::bar_index_from_reg(reg) {
                debug!(
                    "[ECAM] BAR{} write for device ({},{},{}): val={:#x}",
                    bar_idx, bus, dev, func, new_val
                );
                device.on_bar_write(bar_idx);
            }
        } else {
            debug!(
                "[ECAM] no device at bus={} dev={} func={} reg={:#x}, write ignored",
                bus, dev, func, reg
            );
        }
        Ok(())
    }
}

impl Default for PciHostBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl BaseDeviceOps<PortRange> for PciHostBridge {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::Dummy
    }

    fn address_range(&self) -> PortRange {
        PortRange::new(Port(PCI_CONFIG_ADDRESS), Port(PCI_CONFIG_DATA + 3))
    }

    fn handle_read(&self, addr: Port, width: AccessWidth) -> AxResult<usize> {
        match addr.0 {
            PCI_CONFIG_ADDRESS..=CONFIG_ADDR_RANGE_END => {
                let val = *self.config_address.lock();
                Ok(val as usize)
            }
            PCI_CONFIG_DATA..=CONFIG_DATA_RANGE_END => {
                let port_offset = (addr.0 - PCI_CONFIG_DATA) as u8;
                self.handle_config_read(port_offset, width)
            }
            _ => {
                warn!("PCI read from unknown port {:#x}", addr.0);
                Ok(0xFFFF_FFFF_usize)
            }
        }
    }

    fn handle_write(&self, addr: Port, width: AccessWidth, val: usize) -> AxResult {
        match addr.0 {
            PCI_CONFIG_ADDRESS..=CONFIG_ADDR_RANGE_END => {
                let old = *self.config_address.lock();
                let new = val as u32;
                if old != new {
                    debug!(
                        "[PCI-CF8] Config address write: {:#010x} -> {:#010x} (bus={} dev={} \
                         func={} reg={:#x})",
                        old,
                        new,
                        (new >> 16) & 0xFF,
                        (new >> 11) & 0x1F,
                        (new >> 8) & 0x7,
                        new & 0xFC
                    );
                    *self.config_address.lock() = new;
                }
            }
            PCI_CONFIG_DATA..=CONFIG_DATA_RANGE_END => {
                let port_offset = (addr.0 - PCI_CONFIG_DATA) as u8;
                let _ = self.handle_config_write(port_offset, width, val);
            }
            _ => {
                warn!("PCI write to unknown port {:#x} val={:#x}", addr.0, val);
            }
        }
        Ok(())
    }
}

/// MMIO interface for ECAM (Enhanced Configuration Access Mechanism).
/// This allows OVMF to access PCI config space via MMIO at 0xB000_0000,
/// which is required for UEFI firmware that uses MCFG ACPI table.
impl BaseDeviceOps<GuestPhysAddrRange> for PciHostBridge {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::Dummy
    }

    fn address_range(&self) -> GuestPhysAddrRange {
        GuestPhysAddrRange::from_start_size(
            GuestPhysAddr::from(ECAM_BASE as usize),
            ECAM_SIZE as usize,
        )
    }

    fn handle_read(&self, addr: GuestPhysAddr, width: AccessWidth) -> AxResult<usize> {
        self.handle_ecam_read(addr, width)
    }

    fn handle_write(&self, addr: GuestPhysAddr, width: AccessWidth, val: usize) -> AxResult {
        self.handle_ecam_write(addr, width, val)
    }
}
