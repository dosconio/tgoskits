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

//! virtio-blk PCI device emulation (legacy interface).
//!
//! This module implements a minimal virtio-blk device using the legacy
//! virtio-pci I/O BAR interface. OVMF discovers the device via PCI
//! enumeration and uses the legacy I/O registers to configure and
//! interact with it.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

use ax_errno::AxResult;
use axaddrspace::device::{AccessWidth, Port, PortRange};
use axdevice_base::{BaseDeviceOps, EmuDeviceType};
use log::{info, trace, warn};
use pci_host::{PciConfigSpace, PciDevice};
use spin::Mutex;

const VIRTIO_BLK_IO_SIZE: u32 = 0x40;

const VIRTIO_LEGIO_DEVICE_FEATURES: u16 = 0x00;
const VIRTIO_LEGIO_DRIVER_FEATURES: u16 = 0x04;
const VIRTIO_LEGIO_QUEUE_ADDR: u16 = 0x08;
const VIRTIO_LEGIO_QUEUE_SIZE: u16 = 0x0C;
const VIRTIO_LEGIO_QUEUE_SELECT: u16 = 0x0E;
const VIRTIO_LEGIO_QUEUE_NOTIFY: u16 = 0x10;
const VIRTIO_LEGIO_DEVICE_STATUS: u16 = 0x12;
const VIRTIO_LEGIO_ISR_STATUS: u16 = 0x13;
const VIRTIO_LEGIO_CONFIG_OFFSET: u16 = 0x14;

const VIRTIO_BLK_F_SIZE_MAX: u32 = 1;
const VIRTIO_BLK_F_SEG_MAX: u32 = 2;
const VIRTIO_BLK_F_BLK_SIZE: u32 = 6;
const VIRTIO_BLK_F_FLUSH: u32 = 9;

const VIRTIO_BLK_SUPPORTED_FEATURES: u32 = (1 << VIRTIO_BLK_F_SIZE_MAX)
    | (1 << VIRTIO_BLK_F_SEG_MAX)
    | (1 << VIRTIO_BLK_F_BLK_SIZE)
    | (1 << VIRTIO_BLK_F_FLUSH);

const VIRTIO_BLK_SECTOR_SIZE: u64 = 512;

const VIRTQ_SIZE: u16 = 128;

struct VirtioBlkConfig {
    capacity: u64,
    size_max: u32,
    seg_max: u32,
    blk_size: u32,
}

impl VirtioBlkConfig {
    fn new(disk_size: u64) -> Self {
        let capacity = disk_size / VIRTIO_BLK_SECTOR_SIZE;
        Self {
            capacity,
            size_max: 4096,
            seg_max: 1,
            blk_size: 512,
        }
    }
}

struct VirtQueueState {
    queue_addr: u32,
    _queue_size: u16,
    _queue_enable: bool,
}

impl Default for VirtQueueState {
    fn default() -> Self {
        Self {
            queue_addr: 0,
            _queue_size: VIRTQ_SIZE,
            _queue_enable: false,
        }
    }
}

pub struct VirtioBlkPci {
    config_space: PciConfigSpace,
    bdf: (u8, u8, u8),
    device_features: u32,
    driver_features: Mutex<u32>,
    device_status: Mutex<u8>,
    isr_status: Mutex<u8>,
    queue_select: Mutex<u16>,
    queues: Mutex<[VirtQueueState; 1]>,
    blk_config: VirtioBlkConfig,
    _disk_size: u64,
}

impl VirtioBlkPci {
    pub fn new(disk_size: u64, bdf: (u8, u8, u8)) -> Self {
        let config_space = PciConfigSpace::new_virtio_blk_legacy();

        Self {
            config_space,
            bdf,
            device_features: VIRTIO_BLK_SUPPORTED_FEATURES,
            driver_features: Mutex::new(0),
            device_status: Mutex::new(0),
            isr_status: Mutex::new(0),
            queue_select: Mutex::new(0),
            queues: Mutex::new([VirtQueueState::default()]),
            blk_config: VirtioBlkConfig::new(disk_size),
            _disk_size: disk_size,
        }
    }

    pub fn new_with_disk(disk_data: Vec<u8>, bdf: (u8, u8, u8)) -> Self {
        let disk_size = disk_data.len() as u64;
        let config_space = PciConfigSpace::new_virtio_blk_legacy();
        drop(disk_data);

        Self {
            config_space,
            bdf,
            device_features: VIRTIO_BLK_SUPPORTED_FEATURES,
            driver_features: Mutex::new(0),
            device_status: Mutex::new(0),
            isr_status: Mutex::new(0),
            queue_select: Mutex::new(0),
            queues: Mutex::new([VirtQueueState::default()]),
            blk_config: VirtioBlkConfig::new(disk_size),
            _disk_size: disk_size,
        }
    }

    fn current_bar_addr(&self) -> u16 {
        self.config_space.get_bar_addr(0) as u16
    }

    fn handle_legacy_read(&self, offset: u16, width: AccessWidth) -> AxResult<usize> {
        match offset {
            VIRTIO_LEGIO_DEVICE_FEATURES => Ok(self.device_features as usize),
            VIRTIO_LEGIO_QUEUE_SIZE => {
                let queue_sel = *self.queue_select.lock();
                if queue_sel == 0 {
                    Ok(VIRTQ_SIZE as usize)
                } else {
                    Ok(0)
                }
            }
            VIRTIO_LEGIO_QUEUE_SELECT => Ok(*self.queue_select.lock() as usize),
            VIRTIO_LEGIO_DEVICE_STATUS => Ok(*self.device_status.lock() as usize),
            VIRTIO_LEGIO_ISR_STATUS => {
                let val = *self.isr_status.lock();
                *self.isr_status.lock() = 0;
                Ok(val as usize)
            }
            VIRTIO_LEGIO_CONFIG_OFFSET.. => {
                self.read_blk_config(offset - VIRTIO_LEGIO_CONFIG_OFFSET, width)
            }
            _ => {
                trace!("virtio-blk: unhandled I/O read at offset {:#x}", offset);
                Ok(0)
            }
        }
    }

    fn read_blk_config(&self, offset: u16, width: AccessWidth) -> AxResult<usize> {
        let config_bytes: [u8; 28] = {
            let capacity_bytes = self.blk_config.capacity.to_le_bytes();
            let size_max_bytes = self.blk_config.size_max.to_le_bytes();
            let seg_max_bytes = self.blk_config.seg_max.to_le_bytes();
            let blk_size_bytes = self.blk_config.blk_size.to_le_bytes();
            let mut buf = [0u8; 28];
            buf[0..8].copy_from_slice(&capacity_bytes);
            buf[8..12].copy_from_slice(&size_max_bytes);
            buf[12..16].copy_from_slice(&seg_max_bytes);
            buf[20..24].copy_from_slice(&blk_size_bytes);
            buf
        };

        let offset = offset as usize;
        let bytes = width.size();
        let mut val = 0usize;
        for i in 0..bytes {
            if offset + i < config_bytes.len() {
                val |= (config_bytes[offset + i] as usize) << (i * 8);
            }
        }
        Ok(val)
    }

    fn handle_legacy_write(&self, offset: u16, width: AccessWidth, val: usize) -> AxResult {
        match offset {
            VIRTIO_LEGIO_DRIVER_FEATURES => {
                if width == AccessWidth::Dword {
                    *self.driver_features.lock() = val as u32;
                    trace!("virtio-blk: driver_features={:#x}", val);
                }
            }
            VIRTIO_LEGIO_QUEUE_ADDR => {
                if width == AccessWidth::Dword {
                    let queue_sel = *self.queue_select.lock();
                    if queue_sel == 0 {
                        self.queues.lock()[0].queue_addr = val as u32;
                        trace!("virtio-blk: queue_addr={:#x}", val);
                    }
                }
            }
            VIRTIO_LEGIO_QUEUE_SELECT => {
                *self.queue_select.lock() = val as u16;
                trace!("virtio-blk: queue_select={}", val);
            }
            VIRTIO_LEGIO_QUEUE_NOTIFY => {
                let queue_idx = val as u16;
                trace!("virtio-blk: queue_notify={}", queue_idx);
                self.process_virtqueue(queue_idx as usize);
            }
            VIRTIO_LEGIO_DEVICE_STATUS => {
                *self.device_status.lock() = val as u8;
                trace!("virtio-blk: device_status={:#x}", val);
                if val == 0 {
                    info!("virtio-blk: device reset");
                } else if (val & 0x80) != 0 {
                    warn!("virtio-blk: device feature negotiation failed");
                }
            }
            _ => {
                trace!(
                    "virtio-blk: unhandled I/O write at offset {:#x} val={:#x}",
                    offset, val
                );
            }
        }
        Ok(())
    }

    fn process_virtqueue(&self, _queue_idx: usize) {
        let status = *self.device_status.lock();
        if (status & 0x04) == 0 {
            trace!("virtio-blk: queue notify but driver not OK");
            return;
        }

        let queues = self.queues.lock();
        let queue = &queues[0];
        if queue.queue_addr == 0 {
            trace!("virtio-blk: queue not initialized");
            return;
        }

        let _desc_addr = (queue.queue_addr as u64) << 12;
        trace!(
            "virtio-blk: would process queue at desc_addr={:#x}",
            _desc_addr
        );
    }
}

impl PciDevice for VirtioBlkPci {
    fn config_space(&self) -> &PciConfigSpace {
        &self.config_space
    }

    fn bdf(&self) -> (u8, u8, u8) {
        self.bdf
    }

    fn on_bar_write(&self, bar_index: usize) {
        if bar_index == 0 {
            let addr = self.config_space.get_bar_addr(0);
            trace!("virtio-blk: BAR0 updated to {:#x}", addr);
        }
    }
}

impl BaseDeviceOps<PortRange> for VirtioBlkPci {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::VirtioBlk
    }

    fn address_range(&self) -> PortRange {
        let base = self.current_bar_addr();
        if base == 0 {
            return PortRange::new(Port(0xFFFF), Port(0));
        }
        let end = base + (VIRTIO_BLK_IO_SIZE - 1) as u16;
        PortRange::new(Port(base), Port(end))
    }

    fn handle_read(&self, addr: Port, width: AccessWidth) -> AxResult<usize> {
        let base = self.current_bar_addr();
        if addr.0 < base {
            return Ok(0);
        }
        let offset = addr.0 - base;
        self.handle_legacy_read(offset, width)
    }

    fn handle_write(&self, addr: Port, width: AccessWidth, val: usize) -> AxResult {
        let base = self.current_bar_addr();
        if addr.0 < base {
            return Ok(());
        }
        let offset = addr.0 - base;
        self.handle_legacy_write(offset, width, val)
    }
}
