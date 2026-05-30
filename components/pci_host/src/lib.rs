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
use axaddrspace::device::{AccessWidth, Port, PortRange};
use axdevice_base::{BaseDeviceOps, EmuDeviceType};
use log::{trace, warn};
use spin::Mutex;

mod config_space;
pub use config_space::{BarInfo, PciConfigSpace, PciDeviceConfig};

pub mod consts;
pub use consts::*;

const CONFIG_ADDR_RANGE_END: u16 = PCI_CONFIG_ADDRESS + 3;
const CONFIG_DATA_RANGE_END: u16 = PCI_CONFIG_DATA + 3;

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
        trace!(
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

        trace!(
            "PCI config read: Bus={} Dev={} Func={} Reg={:#x} offset={:#x} width={}",
            bus,
            dev,
            func,
            reg,
            port_offset,
            width.size()
        );

        let full_reg = reg;
        if bus == 0 && dev == 0 && func == 0 {
            let full_val = self.host_bridge_config.read(full_reg, AccessWidth::Dword);
            let val = Self::extract_bytes(full_val, port_offset, width);
            trace!("PCI host bridge read: reg={:#x} val={:#x}", full_reg, val);
            return Ok(val);
        }

        if bus == 0 && dev == 31 && func == 0 {
            let full_val = self.lpc_bridge_config.read(full_reg, AccessWidth::Dword);
            let val = Self::extract_bytes(full_val, port_offset, width);
            trace!("PCI LPC bridge read: reg={:#x} val={:#x}", full_reg, val);
            return Ok(val);
        }

        let devices = self.devices.lock();
        if let Some(device) = devices.get(&(bus, dev, func)) {
            let full_val = device.config_space().read(full_reg, AccessWidth::Dword);
            let val = Self::extract_bytes(full_val, port_offset, width);
            trace!(
                "PCI device ({},{},{}) read: reg={:#x} val={:#x}",
                bus, dev, func, full_reg, val
            );
            Ok(val)
        } else {
            trace!("PCI no device at Bus={} Dev={} Func={}", bus, dev, func);
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

        trace!(
            "PCI config write: Bus={} Dev={} Func={} Reg={:#x} offset={:#x} width={} val={:#x}",
            bus,
            dev,
            func,
            reg,
            port_offset,
            width.size(),
            val
        );

        let full_reg = reg;
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
            trace!(
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
            trace!(
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
                trace!(
                    "PCI BAR{} write for device ({},{},{}): val={:#x}",
                    bar_idx, bus, dev, func, new_val
                );
                device.on_bar_write(bar_idx);
            }
        } else {
            trace!(
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
                *self.config_address.lock() = val as u32;
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
