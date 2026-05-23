// Copyright 2025 The Axvisor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS, ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! QEMU fw_cfg (Firmware Configuration) device emulation.
//!
//! This device provides a simple key-value interface for passing boot information
//! to UEFI firmware (OVMF). It uses two I/O ports:
//! - 0x510: Selector port (write to select an item)
//! - 0x511: Data port (read to get item data)

#![no_std]

extern crate alloc;

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    vec::Vec,
};
use core::sync::atomic::{AtomicU16, Ordering};

use ax_errno::AxResult;
use axaddrspace::device::{AccessWidth, Port, PortRange};
use axdevice_base::{BaseDeviceOps, EmuDeviceType};
use log::{trace, warn};
use spin::Mutex;

pub mod consts;
pub use consts::*;

/// A single fw_cfg item
#[derive(Debug, Clone)]
pub struct FwCfgItem {
    /// Item size in bytes
    pub size: usize,
    /// Item data
    pub data: Vec<u8>,
}

/// QEMU fw_cfg device
pub struct FwCfgDevice {
    /// Current selector (item being accessed)
    selector: AtomicU16,
    /// Current read offset within the selected item
    offset: Mutex<usize>,
    /// All items (built-in + file) indexed by selector key
    items: Mutex<BTreeMap<u16, FwCfgItem>>,
    /// File name -> selector mapping
    files: Mutex<BTreeMap<String, u16>>,
    /// Next file selector (starts at 0x8000)
    next_file_selector: AtomicU16,
}

impl FwCfgDevice {
    /// Creates a new fw_cfg device.
    ///
    /// # Arguments
    ///
    /// * `ram_size` - Guest RAM size in bytes
    /// * `cpu_num` - Number of vCPUs
    pub fn new(ram_size: usize, cpu_num: usize) -> Self {
        let device = Self {
            selector: AtomicU16::new(0),
            offset: Mutex::new(0),
            items: Mutex::new(BTreeMap::new()),
            files: Mutex::new(BTreeMap::new()),
            next_file_selector: AtomicU16::new(FW_CFG_FILE_START),
        };
        device.init_builtin_items(ram_size, cpu_num);
        device
    }

    /// Initialize built-in items
    fn init_builtin_items(&self, ram_size: usize, cpu_num: usize) {
        let mut items = self.items.lock();

        items.insert(
            FW_CFG_SIGNATURE,
            FwCfgItem {
                size: 4,
                data: b"QEMU".to_vec(),
            },
        );

        items.insert(
            FW_CFG_ID,
            FwCfgItem {
                size: 4,
                data: FW_CFG_VERSION.to_le_bytes().to_vec(),
            },
        );

        items.insert(
            FW_CFG_RAM_SIZE,
            FwCfgItem {
                size: 8,
                data: (ram_size as u64).to_le_bytes().to_vec(),
            },
        );

        items.insert(
            FW_CFG_NB_CPUS,
            FwCfgItem {
                size: 2,
                data: (cpu_num as u16).to_le_bytes().to_vec(),
            },
        );
    }

    /// Add a file item to fw_cfg.
    ///
    /// # Arguments
    ///
    /// * `name` - File name (e.g., "etc/acpi/tables")
    /// * `data` - File content
    pub fn add_file(&self, name: &str, data: &[u8]) {
        let selector = self.next_file_selector.fetch_add(1, Ordering::SeqCst);

        {
            let mut items = self.items.lock();
            items.insert(
                selector,
                FwCfgItem {
                    size: data.len(),
                    data: data.to_vec(),
                },
            );
        }

        {
            let mut files = self.files.lock();
            files.insert(name.to_string(), selector);
        }

        self.update_file_dir();
    }

    /// Update the FW_CFG_FILE_DIR item
    fn update_file_dir(&self) {
        let files = self.files.lock();
        let count = files.len() as u32;

        let mut data = Vec::with_capacity(4 + count as usize * 64);
        data.extend_from_slice(&count.to_le_bytes());

        for (name, &selector) in files.iter() {
            let items = self.items.lock();
            if let Some(item) = items.get(&selector) {
                // Each file entry: u32 size, u16 select, u16 reserved, [u8; 56] name
                data.extend_from_slice(&(item.size as u32).to_le_bytes());
                data.extend_from_slice(&selector.to_le_bytes());
                data.extend_from_slice(&0u16.to_le_bytes()); // reserved
                let mut name_buf = [0u8; 56];
                let name_bytes = name.as_bytes();
                let copy_len = name_bytes.len().min(55);
                name_buf[..copy_len].copy_from_slice(&name_bytes[..copy_len]);
                data.extend_from_slice(&name_buf);
            }
        }

        let mut items = self.items.lock();
        items.insert(
            FW_CFG_FILE_DIR,
            FwCfgItem {
                size: data.len(),
                data,
            },
        );
    }

    /// Handle selector write (port 0x510)
    fn handle_selector_write(&self, value: u16) {
        self.selector.store(value, Ordering::SeqCst);
        *self.offset.lock() = 0;
        trace!("fw_cfg: selector set to {:#x}", value);
    }

    /// Handle data read (port 0x511)
    fn handle_data_read(&self, width: AccessWidth) -> AxResult<usize> {
        let selector = self.selector.load(Ordering::SeqCst);
        let mut offset = self.offset.lock();

        let items = self.items.lock();
        if let Some(item) = items.get(&selector) {
            if *offset >= item.size {
                trace!("fw_cfg: read past end of item {:#x}", selector);
                return Ok(0);
            }

            let bytes_to_read = match width {
                AccessWidth::Byte => 1,
                AccessWidth::Word => 2,
                AccessWidth::Dword => 4,
                AccessWidth::Qword => 8,
            };

            let end = (*offset + bytes_to_read).min(item.size);
            let slice = &item.data[*offset..end];

            let mut result = 0usize;
            for (i, &byte) in slice.iter().enumerate() {
                result |= (byte as usize) << (i * 8);
            }

            *offset = end;
            trace!(
                "fw_cfg: read {} bytes from item {:#x} at offset {}, result={:#x}",
                slice.len(),
                selector,
                *offset - slice.len(),
                result
            );
            Ok(result)
        } else {
            trace!("fw_cfg: unknown selector {:#x}", selector);
            Ok(0)
        }
    }
}

impl BaseDeviceOps<PortRange> for FwCfgDevice {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::FwCfg
    }

    fn address_range(&self) -> PortRange {
        PortRange::new(Port(FW_CFG_IO_SELECTOR), Port(FW_CFG_IO_DATA))
    }

    fn handle_read(&self, addr: Port, width: AccessWidth) -> AxResult<usize> {
        match addr.0 {
            FW_CFG_IO_SELECTOR => Ok(self.selector.load(Ordering::SeqCst) as usize),
            FW_CFG_IO_DATA => self.handle_data_read(width),
            _ => {
                warn!("fw_cfg: read from unknown port {:#x}", addr.0);
                Ok(0)
            }
        }
    }

    fn handle_write(&self, addr: Port, _width: AccessWidth, val: usize) -> AxResult {
        match addr.0 {
            FW_CFG_IO_SELECTOR => {
                self.handle_selector_write(val as u16);
            }
            FW_CFG_IO_DATA => {
                trace!("fw_cfg: write to data port ignored");
            }
            _ => {
                warn!("fw_cfg: write to unknown port {:#x}", addr.0);
            }
        }
        Ok(())
    }
}
