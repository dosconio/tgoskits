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

//! QEMU fw_cfg (Firmware Configuration) device emulation.
//!
//! This device provides a key-value interface for passing boot information
//! to UEFI firmware (OVMF). It supports both PIO and DMA access modes:
//! - PIO mode: ports 0x510 (selector) and 0x511 (data), byte-by-byte access
//! - DMA mode: port 0x514-0x51B, bulk transfer via FwCfgDmaAccess structure

#![no_std]

extern crate alloc;

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::sync::atomic::{AtomicU16, AtomicU32, Ordering};

use ax_errno::AxResult;
use axaddrspace::device::{AccessWidth, Port, PortRange};
use axdevice_base::{BaseDeviceOps, EmuDeviceType};
use log::{debug, info, trace, warn};
use spin::Mutex;

pub mod consts;
pub use consts::*;

/// Trait for accessing guest physical memory, used by DMA transfers.
///
/// The VMM must implement this trait and inject it via `set_mem_accessor()`
/// before DMA operations can be processed.
pub trait GuestMemoryAccessor: Send + Sync {
    /// Read `buf.len()` bytes from guest physical address `gpa` into `buf`.
    fn read_guest_memory(&self, gpa: u64, buf: &mut [u8]) -> AxResult;
    /// Write `buf.len()` bytes from `buf` to guest physical address `gpa`.
    fn write_guest_memory(&self, gpa: u64, buf: &[u8]) -> AxResult;
}

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
    /// Guest memory accessor for DMA operations
    mem_accessor: Mutex<Option<Arc<dyn GuestMemoryAccessor>>>,
    /// DMA address high 32 bits (accumulated from port 0x514 writes)
    dma_addr_high: AtomicU32,
}

/// QEMU fw_cfg device
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
            mem_accessor: Mutex::new(None),
            dma_addr_high: AtomicU32::new(0),
        };
        device.init_builtin_items(ram_size, cpu_num);
        device
    }

    /// Set the guest memory accessor for DMA operations.
    ///
    /// Must be called before the VM starts, so that DMA transfers can
    /// read/write guest memory.
    pub fn set_mem_accessor(&self, accessor: Arc<dyn GuestMemoryAccessor>) {
        *self.mem_accessor.lock() = Some(accessor);
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

        items.insert(
            FW_CFG_MAX_CPUS,
            FwCfgItem {
                size: 2,
                data: (cpu_num as u16).to_le_bytes().to_vec(),
            },
        );

        // FW_CFG_BOOT_MENU (0x0e): 2-byte little-endian boot menu timeout.
        // Value 0 means no boot menu / immediate boot. OVMF reads this via
        // fw_cfg DMA during PlatformBootManagerBeforeConsole; returning ERROR
        // for an unknown selector causes OVMF to spin in a `test eax, eax;
        // jnz` loop waiting for DMA completion.
        items.insert(
            FW_CFG_BOOT_MENU,
            FwCfgItem {
                size: 2,
                data: 0u16.to_le_bytes().to_vec(),
            },
        );
    }

    /// Add an item at a specific selector key (for legacy well-known selectors).
    ///
    /// This is used for selectors like FW_CFG_KERNEL_DATA (0x0011),
    /// FW_CFG_SETUP_DATA (0x0018), etc. that are not file-based.
    pub fn add_item_at(&self, selector: u16, data: &[u8]) {
        let mut items = self.items.lock();
        items.insert(
            selector,
            FwCfgItem {
                size: data.len(),
                data: data.to_vec(),
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
        data.extend_from_slice(&count.to_be_bytes());

        for (name, &selector) in files.iter() {
            let items = self.items.lock();
            if let Some(item) = items.get(&selector) {
                data.extend_from_slice(&(item.size as u32).to_be_bytes());
                data.extend_from_slice(&selector.to_be_bytes());
                data.extend_from_slice(&0u16.to_be_bytes()); // reserved
                let mut name_buf = [0u8; 56];
                let name_bytes = name.as_bytes();
                let copy_len = name_bytes.len().min(55);
                name_buf[..copy_len].copy_from_slice(&name_bytes[..copy_len]);
                data.extend_from_slice(&name_buf);
                info!(
                    "[fw_cfg] FILE_DIR entry: name={name:?}, selector={selector:#x}, size={}",
                    item.size
                );
            }
        }

        info!(
            "[fw_cfg] FILE_DIR updated: count={count}, data_len={}, files={:?}",
            data.len(),
            files.keys().collect::<Vec<_>>()
        );

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
        if value >= FW_CFG_FILE_START {
            let files = self.files.lock();
            let name = files
                .iter()
                .find_map(|(n, &s)| if s == value { Some(n.as_str()) } else { None })
                .unwrap_or("<unknown>");
            let items = self.items.lock();
            let size = items.get(&value).map(|i| i.size).unwrap_or(0);
            info!(
                "fw_cfg: selector set to {:#x} (file: {}, size: {})",
                value, name, size
            );
        } else {
            info!("fw_cfg: selector set to {:#x}", value);
        }
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
            debug!(
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

    /// Handle DMA address high 32 bits write (port 0x514).
    /// Stores the high 32 bits; DMA is not triggered yet.
    ///
    /// Per the QEMU fw_cfg spec, the DMA address is written in big-endian
    /// byte order. OVMF calls `SwapBytes32()` before writing, so we must
    /// swap back to obtain the native (little-endian) address.
    fn handle_dma_addr_high_write(&self, val: u32) {
        let native = val.swap_bytes();
        debug!(
            "fw_cfg DMA: write high 32 bits = {:#x} (big-endian), native = {:#x}",
            val, native
        );
        self.dma_addr_high.store(native, Ordering::SeqCst);
    }

    /// Handle DMA address low 32 bits write (port 0x518).
    /// This triggers the DMA operation using the accumulated 64-bit address.
    ///
    /// Per the QEMU fw_cfg spec, the DMA address is written in big-endian
    /// byte order. OVMF calls `SwapBytes32()` before writing, so we must
    /// swap back to obtain the native (little-endian) address.
    fn handle_dma_addr_low_write(&self, val: u32) {
        let native_low = val.swap_bytes();
        let high = self.dma_addr_high.load(Ordering::SeqCst) as u64;
        let low = native_low as u64;
        let dma_gpa = (high << 32) | low;
        debug!(
            "fw_cfg DMA: write low 32 bits = {:#x} (big-endian), native = {:#x}, composed GPA = \
             {:#x}",
            val, native_low, dma_gpa
        );
        self.process_dma(dma_gpa);
    }

    /// Process a DMA transfer.
    ///
    /// Reads the `FwCfgDmaAccess` structure from guest memory at `dma_gpa`,
    /// performs the requested operation, and writes back the updated structure.
    fn process_dma(&self, dma_gpa: u64) {
        let accessor = self.mem_accessor.lock();
        let Some(accessor) = accessor.as_ref() else {
            warn!(
                "fw_cfg DMA: no guest memory accessor, dropping DMA request at GPA {:#x}",
                dma_gpa
            );
            return;
        };

        // Read FwCfgDmaAccess from guest memory (16 bytes)
        let mut dma_buf = [0u8; 16];
        if let Err(e) = accessor.read_guest_memory(dma_gpa, &mut dma_buf) {
            warn!(
                "fw_cfg DMA: failed to read FwCfgDmaAccess from GPA {:#x}: {:?}",
                dma_gpa, e
            );
            return;
        }

        // Debug: dump raw bytes of FwCfgDmaAccess
        debug!(
            "fw_cfg DMA: raw FwCfgDmaAccess at GPA {:#x}: {:02x?}",
            dma_gpa, dma_buf
        );

        // Parse big-endian fields
        let control = u32::from_be_bytes([dma_buf[0], dma_buf[1], dma_buf[2], dma_buf[3]]);
        let length = u32::from_be_bytes([dma_buf[4], dma_buf[5], dma_buf[6], dma_buf[7]]);
        let address = u64::from_be_bytes([
            dma_buf[8],
            dma_buf[9],
            dma_buf[10],
            dma_buf[11],
            dma_buf[12],
            dma_buf[13],
            dma_buf[14],
            dma_buf[15],
        ]);

        let is_read = (control & FW_CFG_DMA_CTL_READ) != 0;
        let is_write = (control & FW_CFG_DMA_CTL_WRITE) != 0;
        let is_skip = (control & FW_CFG_DMA_CTL_SKIP) != 0;
        let is_select = (control & FW_CFG_DMA_CTL_SELECT) != 0;
        let selector_from_control = (control >> 16) as u16;

        debug!(
            "fw_cfg DMA: GPA={:#x} control={:#x} (select={} read={} write={} skip={}) \
             selector={:#x} length={} address={:#x}",
            dma_gpa,
            control,
            is_select,
            is_read,
            is_write,
            is_skip,
            selector_from_control,
            length,
            address
        );

        // Handle SELECT: set the selector and reset offset
        if is_select {
            self.handle_selector_write(selector_from_control);
        }

        // Per the QEMU fw_cfg spec, after a DMA operation completes:
        // - All bits cleared in control → transfer finished successfully
        // - Error bit set → something went wrong
        // - Otherwise → transfer still in progress (guest will spin-wait)
        // Therefore, on success we must clear ALL control bits (not just
        // preserve the original request bits), otherwise OVMF sees
        // control != 0 and assumes the transfer is still in progress.
        let mut new_control: u32 = 0;
        let mut transferred: u32 = 0;

        if is_read {
            transferred = self.dma_read(accessor.as_ref(), length, address);
        } else if is_write {
            transferred = self.dma_write(accessor.as_ref(), length, address);
        } else if is_skip {
            transferred = self.dma_skip(length);
        }

        // If we transferred less than requested, set error bit
        if transferred < length && (is_read || is_write || is_skip) {
            new_control = FW_CFG_DMA_CTL_ERROR;
        }

        // Write back updated FwCfgDmaAccess to guest memory
        let new_length = length - transferred;
        let new_control_be = new_control.to_be_bytes();
        let new_length_be = new_length.to_be_bytes();
        dma_buf[0..4].copy_from_slice(&new_control_be);
        dma_buf[4..8].copy_from_slice(&new_length_be);
        // address field is not modified on write-back

        if let Err(e) = accessor.write_guest_memory(dma_gpa, &dma_buf) {
            warn!(
                "fw_cfg DMA: failed to write back FwCfgDmaAccess to GPA {:#x}: {:?}",
                dma_gpa, e
            );
        }

        debug!(
            "fw_cfg DMA: completed, transferred={} bytes, new_control={:#x}, new_length={}, \
             selector={:#x} offset={}",
            transferred,
            new_control,
            new_length,
            self.selector.load(Ordering::SeqCst),
            *self.offset.lock()
        );
    }

    /// DMA read: copy data from fw_cfg item to guest buffer.
    /// Returns the number of bytes actually transferred.
    fn dma_read(&self, accessor: &dyn GuestMemoryAccessor, length: u32, address: u64) -> u32 {
        let selector = self.selector.load(Ordering::SeqCst);
        let mut offset = self.offset.lock();

        let items = self.items.lock();
        let Some(item) = items.get(&selector) else {
            warn!("fw_cfg DMA read: unknown selector {:#x}", selector);
            return 0;
        };

        if *offset >= item.size {
            trace!(
                "fw_cfg DMA read: offset {} past end of item {:#x} (size {})",
                *offset, selector, item.size
            );
            return 0;
        }

        let available = (item.size - *offset) as u32;
        let to_transfer = length.min(available) as usize;
        let src_slice = &item.data[*offset..*offset + to_transfer];

        if let Err(e) = accessor.write_guest_memory(address, src_slice) {
            warn!(
                "fw_cfg DMA read: failed to write to guest GPA {:#x}: {:?}",
                address, e
            );
            return 0;
        }

        *offset += to_transfer;
        to_transfer as u32
    }

    /// DMA write: copy data from guest buffer to fw_cfg item.
    /// Returns the number of bytes actually transferred.
    fn dma_write(&self, accessor: &dyn GuestMemoryAccessor, length: u32, address: u64) -> u32 {
        let selector = self.selector.load(Ordering::SeqCst);
        let mut offset = self.offset.lock();

        let mut items = self.items.lock();
        let Some(item) = items.get_mut(&selector) else {
            warn!("fw_cfg DMA write: unknown selector {:#x}", selector);
            return 0;
        };

        let available = (item.size - *offset) as u32;
        let to_transfer = length.min(available) as usize;

        let mut buf = alloc::vec![0u8; to_transfer];
        if let Err(e) = accessor.read_guest_memory(address, &mut buf) {
            warn!(
                "fw_cfg DMA write: failed to read from guest GPA {:#x}: {:?}",
                address, e
            );
            return 0;
        }

        item.data[*offset..*offset + to_transfer].copy_from_slice(&buf);
        *offset += to_transfer;
        to_transfer as u32
    }

    /// DMA skip: advance the offset without transferring data.
    /// Returns the number of bytes actually skipped.
    fn dma_skip(&self, length: u32) -> u32 {
        let selector = self.selector.load(Ordering::SeqCst);
        let mut offset = self.offset.lock();

        let items = self.items.lock();
        let Some(item) = items.get(&selector) else {
            warn!("fw_cfg DMA skip: unknown selector {:#x}", selector);
            return 0;
        };

        let available = (item.size - *offset) as u32;
        let to_skip = length.min(available) as usize;
        *offset += to_skip;
        to_skip as u32
    }
}

impl BaseDeviceOps<PortRange> for FwCfgDevice {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::FwCfg
    }

    fn address_range(&self) -> PortRange {
        // Port range 0x510-0x51B covers:
        //   0x510-0x511: PIO selector + data
        //   0x512-0x513: reserved
        //   0x514-0x517: DMA address high 32 bits
        //   0x518-0x51B: DMA address low 32 bits (triggers DMA)
        PortRange::new(Port(FW_CFG_IO_SELECTOR), Port(FW_CFG_IO_DMA + 7))
    }

    fn handle_read(&self, addr: Port, width: AccessWidth) -> AxResult<usize> {
        match addr.0 {
            FW_CFG_IO_SELECTOR => Ok(self.selector.load(Ordering::SeqCst) as usize),
            FW_CFG_IO_DATA => self.handle_data_read(width),
            // DMA ports are write-only, reads return 0
            0x514..=0x51B => Ok(0),
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
            0x514..=0x517 => {
                // DMA address high 32 bits (port 0x514)
                debug!(
                    "fw_cfg: write port {:#x} width={:?} val={:#x}",
                    addr.0, _width, val
                );
                self.handle_dma_addr_high_write(val as u32);
            }
            0x518..=0x51B => {
                // DMA address low 32 bits (port 0x518) — triggers the DMA operation
                debug!(
                    "fw_cfg: write port {:#x} width={:?} val={:#x}",
                    addr.0, _width, val
                );
                self.handle_dma_addr_low_write(val as u32);
            }
            _ => {
                warn!("fw_cfg: write to unknown port {:#x}", addr.0);
            }
        }
        Ok(())
    }
}
