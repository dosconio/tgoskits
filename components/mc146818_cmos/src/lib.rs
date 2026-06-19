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

//! MC146818 RTC/CMOS device emulation.
//!
//! The MC146818 is a Real-Time Clock with 128 bytes of CMOS RAM,
//! accessed via I/O ports 0x70 (address/index) and 0x71 (data).
//!
//! OVMF (UEFI firmware) reads CMOS offsets 0x34-0x35 to determine
//! the amount of system memory below 4 GB (in 64 KB blocks, above 16 MB),
//! and offsets 0x5B-0x5D for memory above 4 GB.
//!
//! Port 0x70: Write the CMOS register index (bit 7 = NMI disable).
//! Port 0x71: Read/write the selected CMOS register.

#![no_std]

use core::cell::RefCell;

use ax_errno::AxResult;
use axaddrspace::device::{AccessWidth, Port, PortRange};
use axdevice_base::{BaseDeviceOps, EmuDeviceType};
use log::info;

/// CMOS address/index register (write-only from guest perspective).
const CMOS_INDEX_PORT: u16 = 0x70;
/// CMOS data register.
const CMOS_DATA_PORT: u16 = 0x71;

/// CMOS RAM size: 128 bytes (standard MC146818).
const CMOS_RAM_SIZE: usize = 128;

/// CMOS offset for memory below 4 GB (low byte).
/// Value = (memory_above_16MB / 64KB) & 0xFF
const CMOS_MEM_BELOW_4G_LO: usize = 0x34;
/// CMOS offset for memory below 4 GB (high byte).
/// Value = ((memory_above_16MB / 64KB) >> 8) & 0xFF
const CMOS_MEM_BELOW_4G_HI: usize = 0x35;

/// CMOS offset for memory above 4 GB (low byte).
const CMOS_MEM_ABOVE_4G_LO: usize = 0x5B;
/// CMOS offset for memory above 4 GB (middle byte).
const CMOS_MEM_ABOVE_4G_MID: usize = 0x5C;
/// CMOS offset for memory above 4 GB (high byte).
const CMOS_MEM_ABOVE_4G_HI: usize = 0x5D;

/// MC146818 CMOS/RTC device.
pub struct Mc146818Cmos {
    /// 128-byte CMOS RAM, pre-populated with memory size information.
    ram: RefCell<[u8; CMOS_RAM_SIZE]>,
    /// Current register index (written to port 0x70).
    index: RefCell<u8>,
}

// SAFETY: Mc146818Cmos uses RefCell for interior mutability but is only
// accessed from the vCPU run loop (single-threaded).
unsafe impl Send for Mc146818Cmos {}
unsafe impl Sync for Mc146818Cmos {}

impl Mc146818Cmos {
    /// Create a new CMOS device with the given RAM size (in bytes).
    ///
    /// The RAM size is encoded into CMOS offsets 0x34-0x35 (below 4 GB)
    /// and 0x5B-0x5D (above 4 GB) following the QEMU convention:
    ///
    /// - Below 4 GB: `((CMOS[0x35] << 8) | CMOS[0x34]) * 64KB + 16MB`
    /// - Above 4 GB: `((CMOS[0x5D] << 16) | (CMOS[0x5C] << 8) | CMOS[0x5B]) * 64KB`
    pub fn new(ram_size: usize) -> Self {
        let mut ram = [0u8; CMOS_RAM_SIZE];

        // Memory below 4 GB: stored as (size - 16MB) / 64KB in 16-bit value
        // at offsets 0x34 (low) and 0x35 (high).
        let below_4g = if ram_size < 0x100_0000 {
            0usize // Less than 16 MB
        } else {
            (ram_size - 0x100_0000) / 0x1_0000 // (size - 16MB) / 64KB
        };
        let below_4g = below_4g.min(0xFFFF); // Cap at 16-bit max
        ram[CMOS_MEM_BELOW_4G_LO] = (below_4g & 0xFF) as u8;
        ram[CMOS_MEM_BELOW_4G_HI] = ((below_4g >> 8) & 0xFF) as u8;

        // Memory above 4 GB: stored as size / 64KB in 24-bit value
        // at offsets 0x5B (low), 0x5C (mid), 0x5D (high).
        let above_4g = if ram_size > 0x1_0000_0000 {
            (ram_size - 0x1_0000_0000) / 0x1_0000
        } else {
            0
        };
        let above_4g = above_4g.min(0xFFFFFF); // Cap at 24-bit max
        ram[CMOS_MEM_ABOVE_4G_LO] = (above_4g & 0xFF) as u8;
        ram[CMOS_MEM_ABOVE_4G_MID] = ((above_4g >> 8) & 0xFF) as u8;
        ram[CMOS_MEM_ABOVE_4G_HI] = ((above_4g >> 16) & 0xFF) as u8;

        info!(
            "[CMOS] Initialized with ram_size={:#x}, below_4g_blocks={:#x} (CMOS[0x34]={:#x}, \
             CMOS[0x35]={:#x}), above_4g_blocks={:#x}",
            ram_size, below_4g, ram[CMOS_MEM_BELOW_4G_LO], ram[CMOS_MEM_BELOW_4G_HI], above_4g
        );

        Self {
            ram: RefCell::new(ram),
            index: RefCell::new(0),
        }
    }
}

impl BaseDeviceOps<PortRange> for Mc146818Cmos {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::Dummy
    }

    fn address_range(&self) -> PortRange {
        PortRange::new(Port(CMOS_INDEX_PORT), Port(CMOS_DATA_PORT))
    }

    fn handle_read(&self, addr: Port, _width: AccessWidth) -> AxResult<usize> {
        let port = addr.0;
        match port {
            CMOS_INDEX_PORT => {
                // Reading port 0x70 returns the current index (some implementations)
                Ok(*self.index.borrow() as usize)
            }
            CMOS_DATA_PORT => {
                let idx = (*self.index.borrow()) as usize & 0x7F; // Mask NMI bit
                // RTC registers have special semantics:
                // - Register C (0x0C): Reading clears all flag bits (IRQF, PF, AF, UF).
                //   Return the stored flags and then clear them.
                // - Register D (0x0D): Bit 7 (VRT) is read-only and always 1,
                //   indicating the RTC battery is OK and time is valid. This is
                //   required by OVMF's PcRtc driver to produce the
                //   gEfiRealTimeClockArchProtocolGuid, which QemuKernelLoaderFsDxe
                //   depends on for direct kernel boot via fw_cfg.
                let val = match idx {
                    0x0C => {
                        let flags = self.ram.borrow()[idx];
                        // Reading Register C clears all flag bits
                        self.ram.borrow_mut()[idx] = 0;
                        flags
                    }
                    0x0D => {
                        // VRT bit (bit 7) is always 1; lower bits from RAM
                        self.ram.borrow()[idx] | 0x80
                    }
                    _ => self.ram.borrow()[idx],
                };
                info!("[CMOS] Read register {:#x} = {:#x}", idx, val);
                Ok(val as usize)
            }
            _ => Ok(0),
        }
    }

    fn handle_write(&self, addr: Port, _width: AccessWidth, val: usize) -> AxResult {
        let port = addr.0;
        let val = val as u8;
        match port {
            CMOS_INDEX_PORT => {
                // Bit 7 = NMI disable (we ignore it, just store the index)
                let idx = val & 0x7F;
                info!(
                    "[CMOS] Write index register: {:#x} (NMI={})",
                    idx,
                    (val >> 7) & 1
                );
                *self.index.borrow_mut() = val;
            }
            CMOS_DATA_PORT => {
                let idx = (*self.index.borrow()) as usize & 0x7F;
                // Register D bit 7 (VRT) is read-only; ignore writes to it.
                let val = if idx == 0x0D { val & 0x7F } else { val };
                info!("[CMOS] Write register {:#x} = {:#x}", idx, val);
                self.ram.borrow_mut()[idx] = val;
            }
            _ => {}
        }
        Ok(())
    }
}
