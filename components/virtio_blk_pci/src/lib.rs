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

//! virtio-blk PCI device emulation (legacy interface) with full VirtQueue I/O.
//!
//! Stage 8: Implements complete VirtQueue processing including descriptor chain
//! parsing, block I/O execution, used ring updates, and interrupt injection.

#![no_std]

extern crate alloc;

use alloc::{sync::Arc, vec::Vec};

use ax_errno::AxResult;
use axaddrspace::device::{AccessWidth, Port, PortRange};
use axdevice_base::{BaseDeviceOps, EmuDeviceType};
use log::{debug, info, trace, warn};
use pci_host::{PciConfigSpace, PciDevice};
use spin::Mutex;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const VIRTIO_BLK_IO_SIZE: u32 = 0x40;

// Legacy virtio-pci I/O register offsets
const VIRTIO_LEGIO_DEVICE_FEATURES: u16 = 0x00;
const VIRTIO_LEGIO_DRIVER_FEATURES: u16 = 0x04;
const VIRTIO_LEGIO_QUEUE_ADDR: u16 = 0x08;
const VIRTIO_LEGIO_QUEUE_SIZE: u16 = 0x0C;
const VIRTIO_LEGIO_QUEUE_SELECT: u16 = 0x0E;
const VIRTIO_LEGIO_QUEUE_NOTIFY: u16 = 0x10;
const VIRTIO_LEGIO_DEVICE_STATUS: u16 = 0x12;
const VIRTIO_LEGIO_ISR_STATUS: u16 = 0x13;
const VIRTIO_LEGIO_CONFIG_OFFSET: u16 = 0x14;

// Feature bits
const VIRTIO_BLK_F_SIZE_MAX: u32 = 1;
const VIRTIO_BLK_F_SEG_MAX: u32 = 2;
const VIRTIO_BLK_F_BLK_SIZE: u32 = 6;
const VIRTIO_BLK_F_FLUSH: u32 = 9;

const VIRTIO_BLK_SUPPORTED_FEATURES: u32 = (1 << VIRTIO_BLK_F_SIZE_MAX)
    | (1 << VIRTIO_BLK_F_SEG_MAX)
    | (1 << VIRTIO_BLK_F_BLK_SIZE)
    | (1 << VIRTIO_BLK_F_FLUSH);

const VIRTIO_BLK_SECTOR_SIZE: u64 = 512;

// Default virtqueue size
const VIRTQ_SIZE: u16 = 128;

// virtio-blk request types
const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;

// virtio-blk status codes
const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

// Descriptor flags
const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

// ISR status bits
const ISR_QUEUE_INTERRUPT: u8 = 0x01;
const _ISR_CONFIG_CHANGE: u8 = 0x02;

// ---------------------------------------------------------------------------
// InterruptCallback — abstracts interrupt injection back to the hypervisor
// ---------------------------------------------------------------------------

/// Trait for injecting interrupts back to the guest when the device completes
/// I/O. This decouples the virtio-blk device from the specific interrupt
/// controller implementation (vIOAPIC, 8259 PIC, etc.).
pub trait InterruptCallback: Send + Sync {
    /// Raise the device's interrupt. The implementation is responsible for
    /// routing to the correct GSI/IRQ based on PCI INTx configuration.
    fn raise_interrupt(&self);
}

// ---------------------------------------------------------------------------
// GuestMemoryAccessor — abstracts GPA→HVA translation for device access
// ---------------------------------------------------------------------------

/// Trait for reading/writing guest physical memory from a device.
///
/// This decouples the virtio-blk device from the VM implementation,
/// allowing the device to access guest memory without a direct dependency
/// on the `axvm` crate.
pub trait GuestMemoryAccessor: Send + Sync {
    /// Read `buf.len()` bytes from guest physical address `gpa` into `buf`.
    fn read_guest_memory(&self, gpa: u64, buf: &mut [u8]) -> AxResult;

    /// Write `buf.len()` bytes to guest physical address `gpa` from `buf`.
    fn write_guest_memory(&self, gpa: u64, buf: &[u8]) -> AxResult;
}

// ---------------------------------------------------------------------------
// BlockBackend — abstracts the disk storage backend
// ---------------------------------------------------------------------------

/// Trait for block device storage backends.
///
/// This decouples the virtio-blk device from the actual storage
/// implementation, allowing different backends (memory disk, file, etc.)
/// to be plugged in.
pub trait BlockBackend: Send + Sync {
    /// Read `count` sectors starting from `sector` into `buf`.
    /// Each sector is 512 bytes. Returns Ok(()) on success.
    fn read_sectors(&self, sector: u64, count: u32, buf: &mut [u8]) -> AxResult;

    /// Write `count` sectors starting from `sector` from `buf`.
    /// Each sector is 512 bytes. Returns Ok(()) on success.
    fn write_sectors(&self, sector: u64, count: u32, buf: &[u8]) -> AxResult;

    /// Flush any pending writes. Returns Ok(()) on success.
    fn flush(&self) -> AxResult;

    /// Returns the total number of sectors in the disk.
    fn sector_count(&self) -> u64;
}

// ---------------------------------------------------------------------------
// MemDisk — sparse in-memory block backend
// ---------------------------------------------------------------------------

/// A sparse in-memory disk backend.
///
/// Uses a `BTreeMap` keyed by sector number to store only sectors that
/// have been written to. Unwritten sectors are implicitly zero-filled.
/// This avoids pre-allocating the entire disk in memory, which can cause
/// OOM panics for large disk sizes.
pub struct MemDisk {
    sectors: Mutex<alloc::collections::BTreeMap<u64, [u8; 512]>>,
    sector_count: u64,
}

impl MemDisk {
    /// Create a new MemDisk of the given size in bytes, with lazy allocation.
    pub fn new(size: u64) -> Self {
        let sector_count = size / VIRTIO_BLK_SECTOR_SIZE;
        Self {
            sectors: Mutex::new(alloc::collections::BTreeMap::new()),
            sector_count,
        }
    }

    /// Create a new MemDisk from existing data.
    pub fn from_vec(data: Vec<u8>) -> Self {
        let sector_count = data.len() as u64 / VIRTIO_BLK_SECTOR_SIZE;
        let mut sectors = alloc::collections::BTreeMap::new();
        for (i, chunk) in data.chunks(512).enumerate() {
            let mut sector = [0u8; 512];
            let copy_len = chunk.len().min(512);
            sector[..copy_len].copy_from_slice(&chunk[..copy_len]);
            sectors.insert(i as u64, sector);
        }
        Self {
            sectors: Mutex::new(sectors),
            sector_count,
        }
    }
}

impl BlockBackend for MemDisk {
    fn read_sectors(&self, sector: u64, count: u32, buf: &mut [u8]) -> AxResult {
        let sectors = self.sectors.lock();
        let sector_size = VIRTIO_BLK_SECTOR_SIZE as usize;
        buf.fill(0); // default: all zeros for unwritten sectors
        for i in 0..count as u64 {
            let s = sector + i;
            if s >= self.sector_count {
                break;
            }
            if let Some(data) = sectors.get(&s) {
                let buf_offset = i as usize * sector_size;
                let copy_len = sector_size.min(buf.len() - buf_offset);
                buf[buf_offset..buf_offset + copy_len].copy_from_slice(&data[..copy_len]);
            }
        }
        Ok(())
    }

    fn write_sectors(&self, sector: u64, count: u32, buf: &[u8]) -> AxResult {
        let mut sectors = self.sectors.lock();
        let sector_size = VIRTIO_BLK_SECTOR_SIZE as usize;
        for i in 0..count as u64 {
            let s = sector + i;
            if s >= self.sector_count {
                break;
            }
            let mut data = [0u8; 512];
            let buf_offset = i as usize * sector_size;
            let copy_len = sector_size.min(buf.len().saturating_sub(buf_offset));
            data[..copy_len].copy_from_slice(&buf[buf_offset..buf_offset + copy_len]);
            sectors.insert(s, data);
        }
        Ok(())
    }

    fn flush(&self) -> AxResult {
        // In-memory disk, nothing to flush
        Ok(())
    }

    fn sector_count(&self) -> u64 {
        self.sector_count
    }
}

// ---------------------------------------------------------------------------
// VirtQueue structures (guest memory layout)
// ---------------------------------------------------------------------------

/// A single virtqueue descriptor (16 bytes, as laid out in guest memory).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct VirtqDescriptor {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

/// The available ring header (6 bytes, as laid out in guest memory).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct VirtqAvailHeader {
    flags: u16,
    idx: u16,
}

/// The used ring header (4 bytes, as laid out in guest memory).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct VirtqUsedHeader {
    flags: u16,
    idx: u16,
}

/// A single used ring entry (8 bytes, as laid out in guest memory).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct VirtqUsedElement {
    id: u32,
    len: u32,
}

// ---------------------------------------------------------------------------
// VirtQueueState — tracks the queue configuration and processing state
// ---------------------------------------------------------------------------

struct VirtQueueState {
    /// Page-frame number of the descriptor table (guest physical).
    /// In legacy mode, all three rings (descriptor, available, used) are
    /// located within a single contiguous 4K-aligned region starting at
    /// `queue_addr << 12`.
    queue_addr: u64,
    queue_size: u16,
    /// Index into the used ring for the next entry we will write.
    used_idx: u16,
    /// Last available index we have processed.
    last_avail_idx: u16,
}

impl Default for VirtQueueState {
    fn default() -> Self {
        Self {
            queue_addr: 0,
            queue_size: VIRTQ_SIZE,
            used_idx: 0,
            last_avail_idx: 0,
        }
    }
}

impl VirtQueueState {
    fn is_initialized(&self) -> bool {
        self.queue_addr != 0
    }

    /// Descriptor table GPA.
    fn desc_table_addr(&self) -> u64 {
        self.queue_addr
    }

    /// Available ring GPA (follows descriptor table).
    fn avail_ring_addr(&self) -> u64 {
        self.queue_addr + (self.queue_size as u64) * 16
    }

    /// Used ring GPA (page-aligned after available ring).
    fn used_ring_addr(&self) -> u64 {
        let avail_end = self.avail_ring_addr() + 2 + (self.queue_size as u64) * 2 + 2;
        // Align up to 4K
        (avail_end + 4095) & !4095
    }
}

// ---------------------------------------------------------------------------
// VirtioBlkConfig — device configuration space
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// VirtioBlkPci — the main device
// ---------------------------------------------------------------------------

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
    backend: Arc<dyn BlockBackend>,
    mem_accessor: Option<Arc<dyn GuestMemoryAccessor>>,
    interrupt_callback: Option<Arc<dyn InterruptCallback>>,
}

impl VirtioBlkPci {
    /// Create a new virtio-blk-pci device with a memory-backed disk.
    pub fn new(disk_size: u64, bdf: (u8, u8, u8)) -> Self {
        let config_space = PciConfigSpace::new_virtio_blk_legacy();
        let backend = Arc::new(MemDisk::new(disk_size));

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
            backend,
            mem_accessor: None,
            interrupt_callback: None,
        }
    }

    /// Create a new virtio-blk-pci device with a custom block backend.
    pub fn new_with_backend(
        disk_size: u64,
        bdf: (u8, u8, u8),
        backend: Arc<dyn BlockBackend>,
    ) -> Self {
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
            backend,
            mem_accessor: None,
            interrupt_callback: None,
        }
    }

    /// Create a new virtio-blk-pci device with pre-loaded disk data.
    pub fn new_with_disk(disk_data: Vec<u8>, bdf: (u8, u8, u8)) -> Self {
        let disk_size = disk_data.len() as u64;
        let config_space = PciConfigSpace::new_virtio_blk_legacy();
        let backend = Arc::new(MemDisk::from_vec(disk_data));

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
            backend,
            mem_accessor: None,
            interrupt_callback: None,
        }
    }

    /// Set the guest memory accessor. Must be called before the device
    /// can process virtqueue requests.
    pub fn set_mem_accessor(&mut self, accessor: Arc<dyn GuestMemoryAccessor>) {
        self.mem_accessor = Some(accessor);
    }

    /// Set the interrupt callback used to notify the guest when I/O completes.
    /// Must be called before the device can deliver interrupts.
    pub fn set_interrupt_callback(&mut self, callback: Arc<dyn InterruptCallback>) {
        self.interrupt_callback = Some(callback);
    }

    fn current_bar_addr(&self) -> u16 {
        self.config_space.get_bar_addr(0) as u16
    }

    // -----------------------------------------------------------------------
    // Legacy I/O register handlers
    // -----------------------------------------------------------------------

    fn handle_legacy_read(&self, offset: u16, width: AccessWidth) -> AxResult<usize> {
        match offset {
            VIRTIO_LEGIO_DEVICE_FEATURES => Ok(self.device_features as usize),
            VIRTIO_LEGIO_DRIVER_FEATURES => Ok(*self.driver_features.lock() as usize),
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
                        let mut queues = self.queues.lock();
                        queues[0].queue_addr = (val as u64) << 12;
                        queues[0].used_idx = 0;
                        queues[0].last_avail_idx = 0;
                        trace!(
                            "virtio-blk: queue_addr={:#x} (GPA {:#x})",
                            val,
                            (val as u64) << 12
                        );
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
                    // Reset queue state
                    let mut queues = self.queues.lock();
                    queues[0] = VirtQueueState::default();
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

    // -----------------------------------------------------------------------
    // VirtQueue processing
    // -----------------------------------------------------------------------

    /// Read a single descriptor from the descriptor table in guest memory.
    fn read_descriptor(
        mem: &dyn GuestMemoryAccessor,
        desc_addr: u64,
        index: u16,
    ) -> AxResult<VirtqDescriptor> {
        let offset = desc_addr + (index as u64) * 16;
        let mut buf = [0u8; 16];
        mem.read_guest_memory(offset, &mut buf)?;
        Ok(VirtqDescriptor {
            addr: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            len: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            flags: u16::from_le_bytes(buf[12..14].try_into().unwrap()),
            next: u16::from_le_bytes(buf[14..16].try_into().unwrap()),
        })
    }

    /// Read the available ring header from guest memory.
    fn read_avail_header(
        mem: &dyn GuestMemoryAccessor,
        avail_addr: u64,
    ) -> AxResult<VirtqAvailHeader> {
        let mut buf = [0u8; 4];
        mem.read_guest_memory(avail_addr, &mut buf)?;
        Ok(VirtqAvailHeader {
            flags: u16::from_le_bytes(buf[0..2].try_into().unwrap()),
            idx: u16::from_le_bytes(buf[2..4].try_into().unwrap()),
        })
    }

    /// Read a single entry from the available ring.
    fn read_avail_entry(
        mem: &dyn GuestMemoryAccessor,
        avail_addr: u64,
        index: u16,
        _queue_size: u16,
    ) -> AxResult<u16> {
        // Available ring layout: flags(2) + idx(2) + ring[queue_size](2 each)
        let offset = avail_addr + 4 + (index as u64) * 2;
        let mut buf = [0u8; 2];
        mem.read_guest_memory(offset, &mut buf)?;
        Ok(u16::from_le_bytes(buf))
    }

    /// Write the used ring header to guest memory.
    fn write_used_header(
        mem: &dyn GuestMemoryAccessor,
        used_addr: u64,
        header: &VirtqUsedHeader,
    ) -> AxResult {
        let buf = [header.flags.to_le_bytes(), header.idx.to_le_bytes()].concat();
        mem.write_guest_memory(used_addr, &buf)
    }

    /// Write a single entry to the used ring.
    fn write_used_entry(
        mem: &dyn GuestMemoryAccessor,
        used_addr: u64,
        index: u16,
        entry: &VirtqUsedElement,
    ) -> AxResult {
        // Used ring layout: flags(2) + idx(2) + ring[queue_size](8 each)
        let offset = used_addr + 4 + (index as u64) * 8;
        let mut buf = [0u8; 8];
        buf[0..4].copy_from_slice(&entry.id.to_le_bytes());
        buf[4..8].copy_from_slice(&entry.len.to_le_bytes());
        mem.write_guest_memory(offset, &buf)
    }

    /// Process the virtqueue: walk available descriptors, execute block I/O,
    /// update the used ring, and signal an interrupt.
    fn process_virtqueue(&self, queue_idx: usize) {
        let status = *self.device_status.lock();
        if (status & 0x04) == 0 {
            trace!("virtio-blk: queue notify but driver not OK");
            return;
        }

        let mem_accessor = match &self.mem_accessor {
            Some(m) => m.clone(),
            None => {
                warn!("virtio-blk: no memory accessor set, cannot process queue");
                return;
            }
        };

        let mut queues = self.queues.lock();
        let queue = &mut queues[queue_idx];
        if !queue.is_initialized() {
            trace!("virtio-blk: queue not initialized");
            return;
        }

        let desc_addr = queue.desc_table_addr();
        let avail_addr = queue.avail_ring_addr();
        let used_addr = queue.used_ring_addr();
        let queue_size = queue.queue_size;

        // Read available ring header to get current driver index
        let avail_header = match Self::read_avail_header(&*mem_accessor, avail_addr) {
            Ok(h) => h,
            Err(e) => {
                warn!("virtio-blk: failed to read avail header: {:?}", e);
                return;
            }
        };

        let mut last_avail_idx = queue.last_avail_idx;
        let mut used_idx = queue.used_idx;
        let mut processed = 0u32;

        // Process all available descriptors since last check
        while last_avail_idx != avail_header.idx {
            let avail_slot = last_avail_idx % queue_size;

            // Read the head descriptor index from the available ring
            let head_idx =
                match Self::read_avail_entry(&*mem_accessor, avail_addr, avail_slot, queue_size) {
                    Ok(idx) => idx,
                    Err(e) => {
                        warn!("virtio-blk: failed to read avail entry: {:?}", e);
                        break;
                    }
                };

            // Walk the descriptor chain
            let chain = match Self::read_descriptor_chain(
                &*mem_accessor,
                desc_addr,
                head_idx,
                queue_size,
            ) {
                Ok(c) => c,
                Err(e) => {
                    warn!("virtio-blk: failed to read descriptor chain: {:?}", e);
                    // Still advance to avoid infinite loop
                    last_avail_idx = last_avail_idx.wrapping_add(1);
                    continue;
                }
            };

            // Execute the block request
            let status_byte = self.execute_blk_request(&*mem_accessor, &chain);

            // Write status byte to the last descriptor in the chain (must be writable)
            if let Some(last_desc) = chain.last()
                && last_desc.flags & VIRTQ_DESC_F_WRITE != 0
                && last_desc.len >= 1
                && let Err(e) = mem_accessor.write_guest_memory(last_desc.addr, &[status_byte])
            {
                warn!("virtio-blk: failed to write status byte: {:?}", e);
            }

            // Write used ring entry
            let used_slot = used_idx % queue_size;
            let used_entry = VirtqUsedElement {
                id: head_idx as u32,
                len: 1, // We wrote 1 byte (status)
            };
            if let Err(e) =
                Self::write_used_entry(&*mem_accessor, used_addr, used_slot, &used_entry)
            {
                warn!("virtio-blk: failed to write used entry: {:?}", e);
                break;
            }

            used_idx = used_idx.wrapping_add(1);
            last_avail_idx = last_avail_idx.wrapping_add(1);
            processed += 1;
        }

        if processed > 0 {
            // Update used ring header (idx field)
            let used_header = VirtqUsedHeader {
                flags: 0,
                idx: used_idx,
            };
            if let Err(e) = Self::write_used_header(&*mem_accessor, used_addr, &used_header) {
                warn!("virtio-blk: failed to write used header: {:?}", e);
            }

            // Update our tracking state
            queue.last_avail_idx = last_avail_idx;
            queue.used_idx = used_idx;

            // Signal interrupt via ISR status
            *self.isr_status.lock() |= ISR_QUEUE_INTERRUPT;

            // Inject the actual CPU interrupt via the callback so the guest
            // is notified of I/O completion. Without this, the guest would
            // never wake from HLT after submitting a request.
            if let Some(cb) = &self.interrupt_callback {
                cb.raise_interrupt();
            } else {
                warn!("virtio-blk: no interrupt callback set, guest will not be notified");
            }

            debug!(
                "virtio-blk: processed {} requests, used_idx={}, last_avail_idx={}",
                processed, used_idx, last_avail_idx
            );
        }
    }

    /// Read a full descriptor chain starting from `head_idx`.
    fn read_descriptor_chain(
        mem: &dyn GuestMemoryAccessor,
        desc_addr: u64,
        head_idx: u16,
        queue_size: u16,
    ) -> AxResult<Vec<VirtqDescriptor>> {
        let mut chain = Vec::new();
        let mut idx = head_idx;
        let mut visited = 0u16;

        loop {
            if visited >= queue_size {
                warn!("virtio-blk: descriptor chain too long (loop?)");
                break;
            }
            let desc = Self::read_descriptor(mem, desc_addr, idx)?;
            let next_idx = desc.next;
            chain.push(desc);
            visited += 1;

            if desc.flags & VIRTQ_DESC_F_NEXT == 0 {
                break;
            }
            idx = next_idx;
        }

        Ok(chain)
    }

    /// Execute a single block request described by a descriptor chain.
    ///
    /// The descriptor chain layout for virtio-blk is:
    ///   - Desc 0 (read-only): BlkRequestHeader (16 bytes)
    ///   - Desc 1..N-1 (read or write): data buffer
    ///   - Desc N (write-only): status byte (1 byte)
    ///
    /// Returns the status byte to write back.
    fn execute_blk_request(&self, mem: &dyn GuestMemoryAccessor, chain: &[VirtqDescriptor]) -> u8 {
        if chain.len() < 2 {
            warn!(
                "virtio-blk: descriptor chain too short ({} descs)",
                chain.len()
            );
            return VIRTIO_BLK_S_IOERR;
        }

        // Parse request header from the first descriptor
        let head_desc = &chain[0];
        if head_desc.len < 16 {
            warn!(
                "virtio-blk: request header too short ({} bytes)",
                head_desc.len
            );
            return VIRTIO_BLK_S_IOERR;
        }

        let mut hdr_buf = [0u8; 16];
        if let Err(e) = mem.read_guest_memory(head_desc.addr, &mut hdr_buf) {
            warn!("virtio-blk: failed to read request header: {:?}", e);
            return VIRTIO_BLK_S_IOERR;
        }

        let req_type = u32::from_le_bytes(hdr_buf[0..4].try_into().unwrap());
        let sector = u64::from_le_bytes(hdr_buf[8..16].try_into().unwrap());

        debug!(
            "virtio-blk: request type={}, sector={}, chain_len={}",
            req_type,
            sector,
            chain.len()
        );

        match req_type {
            VIRTIO_BLK_T_IN => self.handle_read(mem, chain, sector),
            VIRTIO_BLK_T_OUT => self.handle_write(mem, chain, sector),
            VIRTIO_BLK_T_FLUSH => self.handle_flush(),
            _ => {
                warn!("virtio-blk: unsupported request type {}", req_type);
                VIRTIO_BLK_S_UNSUPP
            }
        }
    }

    /// Handle a VIRTIO_BLK_T_IN (read) request.
    fn handle_read(
        &self,
        mem: &dyn GuestMemoryAccessor,
        chain: &[VirtqDescriptor],
        sector: u64,
    ) -> u8 {
        // Collect writable descriptors (skip header desc 0, skip status desc last)
        let data_descs: Vec<&VirtqDescriptor> = chain[1..chain.len() - 1]
            .iter()
            .filter(|d| d.flags & VIRTQ_DESC_F_WRITE != 0)
            .collect();

        if data_descs.is_empty() {
            warn!("virtio-blk: read request has no data descriptors");
            return VIRTIO_BLK_S_IOERR;
        }

        // Calculate total data length and sector count
        let total_len: u32 = data_descs.iter().map(|d| d.len).sum();
        let sector_count = total_len / VIRTIO_BLK_SECTOR_SIZE as u32;
        if sector_count == 0 {
            return VIRTIO_BLK_S_IOERR;
        }

        // Read from backend into a temporary buffer
        let read_len = sector_count * VIRTIO_BLK_SECTOR_SIZE as u32;
        let mut disk_buf = alloc::vec![0u8; read_len as usize];
        if let Err(e) = self
            .backend
            .read_sectors(sector, sector_count, &mut disk_buf)
        {
            warn!("virtio-blk: backend read failed: {:?}", e);
            return VIRTIO_BLK_S_IOERR;
        }

        // Copy from temp buffer into guest memory via data descriptors
        let mut offset = 0usize;
        for desc in &data_descs {
            let copy_len = (desc.len as usize).min(disk_buf.len().saturating_sub(offset));
            if copy_len == 0 {
                break;
            }
            if let Err(e) = mem.write_guest_memory(desc.addr, &disk_buf[offset..offset + copy_len])
            {
                warn!("virtio-blk: failed to write read data to guest: {:?}", e);
                return VIRTIO_BLK_S_IOERR;
            }
            offset += copy_len;
        }

        debug!(
            "virtio-blk: read {} sectors from sector {} ({} bytes)",
            sector_count, sector, read_len
        );
        VIRTIO_BLK_S_OK
    }

    /// Handle a VIRTIO_BLK_T_OUT (write) request.
    fn handle_write(
        &self,
        mem: &dyn GuestMemoryAccessor,
        chain: &[VirtqDescriptor],
        sector: u64,
    ) -> u8 {
        // Collect readable descriptors (skip header desc 0, skip status desc last)
        let data_descs: Vec<&VirtqDescriptor> = chain[1..chain.len() - 1]
            .iter()
            .filter(|d| d.flags & VIRTQ_DESC_F_WRITE == 0)
            .collect();

        if data_descs.is_empty() {
            warn!("virtio-blk: write request has no data descriptors");
            return VIRTIO_BLK_S_IOERR;
        }

        // Calculate total data length and sector count
        let total_len: u32 = data_descs.iter().map(|d| d.len).sum();
        let sector_count = total_len / VIRTIO_BLK_SECTOR_SIZE as u32;
        if sector_count == 0 {
            return VIRTIO_BLK_S_IOERR;
        }

        // Read data from guest memory into a temporary buffer
        let write_len = sector_count * VIRTIO_BLK_SECTOR_SIZE as u32;
        let mut disk_buf = alloc::vec![0u8; write_len as usize];
        let mut offset = 0usize;
        for desc in &data_descs {
            let copy_len = (desc.len as usize).min(disk_buf.len().saturating_sub(offset));
            if copy_len == 0 {
                break;
            }
            if let Err(e) =
                mem.read_guest_memory(desc.addr, &mut disk_buf[offset..offset + copy_len])
            {
                warn!("virtio-blk: failed to read write data from guest: {:?}", e);
                return VIRTIO_BLK_S_IOERR;
            }
            offset += copy_len;
        }

        // Write to backend
        if let Err(e) = self.backend.write_sectors(sector, sector_count, &disk_buf) {
            warn!("virtio-blk: backend write failed: {:?}", e);
            return VIRTIO_BLK_S_IOERR;
        }

        debug!(
            "virtio-blk: write {} sectors to sector {} ({} bytes)",
            sector_count, sector, write_len
        );
        VIRTIO_BLK_S_OK
    }

    /// Handle a VIRTIO_BLK_T_FLUSH request.
    fn handle_flush(&self) -> u8 {
        if let Err(e) = self.backend.flush() {
            warn!("virtio-blk: backend flush failed: {:?}", e);
            return VIRTIO_BLK_S_IOERR;
        }
        debug!("virtio-blk: flush completed");
        VIRTIO_BLK_S_OK
    }
}

// ---------------------------------------------------------------------------
// PciDevice implementation
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// BaseDeviceOps<PortRange> implementation
// ---------------------------------------------------------------------------

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
        // Rate-limited logging for BAR reads
        static BAR_READ_COUNT: core::sync::atomic::AtomicU64 =
            core::sync::atomic::AtomicU64::new(0);
        let count = BAR_READ_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if count < 20 {
            info!(
                "[virtio-blk] BAR read #{count}: offset={offset:#x}, width={width:?}, \
                 addr={addr:#x}"
            );
        }
        self.handle_legacy_read(offset, width)
    }

    fn handle_write(&self, addr: Port, width: AccessWidth, val: usize) -> AxResult {
        let base = self.current_bar_addr();
        if addr.0 < base {
            return Ok(());
        }
        let offset = addr.0 - base;
        // Rate-limited logging for BAR writes
        static BAR_WRITE_COUNT: core::sync::atomic::AtomicU64 =
            core::sync::atomic::AtomicU64::new(0);
        let count = BAR_WRITE_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if count < 20 {
            info!(
                "[virtio-blk] BAR write #{count}: offset={offset:#x}, width={width:?}, \
                 val={val:#x}, addr={addr:#x}"
            );
        }
        self.handle_legacy_write(offset, width, val)
    }
}
