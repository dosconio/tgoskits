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

//! PCI Configuration Space emulation.

use axaddrspace::device::AccessWidth;
use spin::Mutex;

use crate::consts::*;

const BAR_COUNT: usize = 6;

#[derive(Clone, Copy, Default)]
pub struct BarInfo {
    pub size: u32,
    pub is_io: bool,
    pub is_64bit: bool,
    pub is_prefetchable: bool,
}

impl BarInfo {
    pub fn io(size: u32) -> Self {
        Self {
            size,
            is_io: true,
            ..Default::default()
        }
    }

    pub fn mmio32(size: u32) -> Self {
        Self {
            size,
            is_io: false,
            is_64bit: false,
            ..Default::default()
        }
    }

    pub fn mmio64(size: u32) -> Self {
        Self {
            size,
            is_io: false,
            is_64bit: true,
            ..Default::default()
        }
    }

    pub fn sizing_mask(&self) -> u32 {
        if self.size == 0 {
            return 0;
        }
        let mask = !(self.size - 1);
        if self.is_io {
            mask & 0xFFFF_FFF0 | 0x01
        } else {
            let type_bits = if self.is_64bit { 0x04 } else { 0x00 };
            let prefetch = if self.is_prefetchable { 0x08 } else { 0x00 };
            mask & 0xFFFF_FFF0 | type_bits | prefetch
        }
    }
}

pub struct PciConfigSpace {
    data: Mutex<[u8; 256]>,
    bar_info: [BarInfo; BAR_COUNT],
}

impl Default for PciConfigSpace {
    fn default() -> Self {
        Self::new()
    }
}

impl PciConfigSpace {
    pub fn new() -> Self {
        Self {
            data: Mutex::new([0u8; 256]),
            bar_info: [Default::default(); BAR_COUNT],
        }
    }

    pub fn new_host_bridge() -> Self {
        let config = Self::new();
        {
            let mut data = config.data.lock();
            data[PCI_VENDOR_ID as usize..PCI_VENDOR_ID as usize + 2]
                .copy_from_slice(&PCI_VENDOR_INTEL.to_le_bytes());
            data[PCI_DEVICE_ID as usize..PCI_DEVICE_ID as usize + 2]
                .copy_from_slice(&PCI_DEVICE_Q35_MCH.to_le_bytes());
            data[PCI_REVISION_ID as usize] = 0x02;
            data[PCI_CLASS_CODE as usize..PCI_CLASS_CODE as usize + 3].copy_from_slice(&[
                0x00,
                PCI_SUBCLASS_HOST_BRIDGE,
                PCI_CLASS_HOST_BRIDGE,
            ]);
            data[PCI_HEADER_TYPE as usize] = PCI_HEADER_TYPE_NORMAL;
        }
        config
    }

    pub fn new_lpc_bridge() -> Self {
        let config = Self::new();
        {
            let mut data = config.data.lock();
            data[PCI_VENDOR_ID as usize..PCI_VENDOR_ID as usize + 2]
                .copy_from_slice(&PCI_VENDOR_INTEL.to_le_bytes());
            data[PCI_DEVICE_ID as usize..PCI_DEVICE_ID as usize + 2]
                .copy_from_slice(&PCI_DEVICE_Q35_LPC.to_le_bytes());
            data[PCI_REVISION_ID as usize] = 0x02;
            data[PCI_CLASS_CODE as usize..PCI_CLASS_CODE as usize + 3].copy_from_slice(&[
                0x00,
                PCI_SUBCLASS_ISA_BRIDGE,
                PCI_CLASS_BRIDGE,
            ]);
            data[PCI_HEADER_TYPE as usize] = PCI_HEADER_TYPE_NORMAL;

            // ICH9 LPC bridge Power Management registers:
            // Offset 0x40 (ICH9_PMBASE): PM Base Address = 0x0600
            //   Bits [15:7] = PMBA, Bit 0 = RTE (Reserved, must be 0)
            //   OVMF expects ICH9_PMBASE_VALUE = 0x0600
            data[0x40..0x44].copy_from_slice(&0x0600u32.to_le_bytes());
            // Offset 0x44 (ICH9_ACPI_CNTL): ACPI Control
            //   Bit 7 = ACPI_EN (ACPI I/O space enable)
            //   Set ACPI_EN=1 so OVMF's AcpiTimerLib constructor sees it enabled
            data[0x44] = 0x80; // ACPI_EN = BIT7
            // Offset 0x48 (ICH9_GPIO_BASE): GPIO Base Address = 0x0500
            data[0x48..0x4C].copy_from_slice(&0x0500u32.to_le_bytes());
            // Offset 0x4C (ICH9_GPIO_CNTL): GPIO Control
            //   Bit 0 = GPIO_EN (GPIO I/O space enable)
            data[0x4C] = 0x01;
        }
        config
    }

    pub fn new_virtio_blk_legacy() -> Self {
        let mut config = Self::new();
        config.bar_info[0] = BarInfo::io(0x40);

        {
            let mut data = config.data.lock();
            data[PCI_VENDOR_ID as usize..PCI_VENDOR_ID as usize + 2]
                .copy_from_slice(&PCI_VENDOR_VIRTIO.to_le_bytes());
            data[PCI_DEVICE_ID as usize..PCI_DEVICE_ID as usize + 2]
                .copy_from_slice(&PCI_DEVICE_VIRTIO_BLK.to_le_bytes());
            data[PCI_REVISION_ID as usize] = 0x00;
            data[PCI_CLASS_CODE as usize..PCI_CLASS_CODE as usize + 3].copy_from_slice(&[
                0x00,
                PCI_SUBCLASS_OTHER_MASS_STORAGE,
                PCI_CLASS_MASS_STORAGE,
            ]);
            data[PCI_HEADER_TYPE as usize] = PCI_HEADER_TYPE_NORMAL;

            data[PCI_SUBSYSTEM_VENDOR_ID as usize..PCI_SUBSYSTEM_VENDOR_ID as usize + 2]
                .copy_from_slice(&PCI_VENDOR_VIRTIO.to_le_bytes());
            data[PCI_SUBSYSTEM_ID as usize..PCI_SUBSYSTEM_ID as usize + 2]
                .copy_from_slice(&0x02u16.to_le_bytes());

            data[PCI_INTERRUPT_PIN as usize] = 1;

            data[PCI_COMMAND as usize] = 0x01;
        }
        config
    }

    pub fn new_with_config(config: PciDeviceConfig) -> Self {
        let space = Self::new();
        {
            let mut data = space.data.lock();
            data[PCI_VENDOR_ID as usize..PCI_VENDOR_ID as usize + 2]
                .copy_from_slice(&config.vendor_id.to_le_bytes());
            data[PCI_DEVICE_ID as usize..PCI_DEVICE_ID as usize + 2]
                .copy_from_slice(&config.device_id.to_le_bytes());
            data[PCI_REVISION_ID as usize] = config.revision_id;
            data[PCI_CLASS_CODE as usize..PCI_CLASS_CODE as usize + 3]
                .copy_from_slice(&config.class_code);
            data[PCI_HEADER_TYPE as usize] = config.header_type;
            if let Some(subsystem_vendor_id) = config.subsystem_vendor_id {
                data[PCI_SUBSYSTEM_VENDOR_ID as usize..PCI_SUBSYSTEM_VENDOR_ID as usize + 2]
                    .copy_from_slice(&subsystem_vendor_id.to_le_bytes());
            }
            if let Some(subsystem_id) = config.subsystem_id {
                data[PCI_SUBSYSTEM_ID as usize..PCI_SUBSYSTEM_ID as usize + 2]
                    .copy_from_slice(&subsystem_id.to_le_bytes());
            }
            if let Some(interrupt_pin) = config.interrupt_pin {
                data[PCI_INTERRUPT_PIN as usize] = interrupt_pin;
            }
        }
        space
    }

    pub fn read(&self, offset: u8, width: AccessWidth) -> usize {
        let offset = (offset & 0xFC) as usize;
        let data = self.data.lock();
        let bytes = width.size();
        let mut val = 0usize;
        for i in 0..bytes {
            if offset + i < 256 {
                val |= (data[offset + i] as usize) << (i * 8);
            }
        }
        val
    }

    pub fn write(&self, offset: u8, width: AccessWidth, val: usize) {
        let offset = offset as usize;
        if let Some(bar_idx) = Self::bar_index_from_offset(offset) {
            self.write_bar(bar_idx, offset, width, val);
            return;
        }
        // Expansion ROM BAR (offset 0x30): no ROM present.
        // Ignore all writes so readback always returns 0, indicating no ROM.
        // This prevents Linux from assigning a bogus ROM BAR during sizing.
        if offset >= PCI_EXPANSION_ROM as usize && offset < PCI_EXPANSION_ROM as usize + 4 {
            return;
        }
        let offset = offset & 0xFC;
        let mut data = self.data.lock();
        let bytes = width.size();
        for i in 0..bytes {
            if offset + i < 256 {
                data[offset + i] = ((val >> (i * 8)) & 0xFF) as u8;
            }
        }
    }

    fn bar_index_from_offset(offset: usize) -> Option<usize> {
        if offset >= PCI_BAR0 as usize && offset < PCI_BAR0 as usize + BAR_COUNT * 4 {
            let idx = (offset - PCI_BAR0 as usize) / 4;
            if idx < BAR_COUNT { Some(idx) } else { None }
        } else {
            None
        }
    }

    fn write_bar(&self, bar_idx: usize, _offset: usize, width: AccessWidth, val: usize) {
        if bar_idx >= BAR_COUNT {
            return;
        }
        let info = &self.bar_info[bar_idx];
        if info.size == 0 {
            return;
        }

        let is_sizing_probe = if width == AccessWidth::Dword {
            (val as u32) == 0xFFFF_FFFF
        } else {
            false
        };

        let write_val = if is_sizing_probe {
            info.sizing_mask()
        } else {
            let v = val as u32;
            if info.is_io {
                v & 0xFFFF_FFFC | 0x01
            } else {
                v & 0xFFFF_FFF0
            }
        };

        let bar_offset = PCI_BAR0 as usize + bar_idx * 4;
        let mut data = self.data.lock();
        data[bar_offset..bar_offset + 4].copy_from_slice(&write_val.to_le_bytes());
    }

    pub fn set_bar(&self, bar_index: usize, value: u32) {
        if bar_index < BAR_COUNT {
            let offset = PCI_BAR0 as usize + bar_index * 4;
            let mut data = self.data.lock();
            data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
    }

    pub fn get_bar(&self, bar_index: usize) -> u32 {
        if bar_index < BAR_COUNT {
            let offset = PCI_BAR0 as usize + bar_index * 4;
            let data = self.data.lock();
            u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap_or([0; 4]))
        } else {
            0
        }
    }

    pub fn get_bar_addr(&self, bar_index: usize) -> u32 {
        let raw = self.get_bar(bar_index);
        let info = &self.bar_info[bar_index];
        if info.is_io {
            raw & 0xFFFF_FFFC
        } else {
            raw & 0xFFFF_FFF0
        }
    }

    pub fn vendor_id(&self) -> u16 {
        let data = self.data.lock();
        u16::from_le_bytes(
            data[PCI_VENDOR_ID as usize..PCI_VENDOR_ID as usize + 2]
                .try_into()
                .unwrap_or([0; 2]),
        )
    }

    pub fn device_id(&self) -> u16 {
        let data = self.data.lock();
        u16::from_le_bytes(
            data[PCI_DEVICE_ID as usize..PCI_DEVICE_ID as usize + 2]
                .try_into()
                .unwrap_or([0; 2]),
        )
    }

    pub fn header_type(&self) -> u8 {
        self.data.lock()[PCI_HEADER_TYPE as usize]
    }

    pub fn class_code(&self) -> u32 {
        let data = self.data.lock();
        let mut val = 0u32;
        for i in 0..3 {
            val |= (data[PCI_CLASS_CODE as usize + i] as u32) << (i * 8);
        }
        val
    }

    pub fn command(&self) -> u16 {
        let data = self.data.lock();
        u16::from_le_bytes(
            data[PCI_COMMAND as usize..PCI_COMMAND as usize + 2]
                .try_into()
                .unwrap_or([0; 2]),
        )
    }
}

pub struct PciDeviceConfig {
    pub vendor_id: u16,
    pub device_id: u16,
    pub revision_id: u8,
    pub class_code: [u8; 3],
    pub header_type: u8,
    pub subsystem_vendor_id: Option<u16>,
    pub subsystem_id: Option<u16>,
    pub interrupt_pin: Option<u8>,
}

impl Default for PciDeviceConfig {
    fn default() -> Self {
        Self {
            vendor_id: 0xFFFF,
            device_id: 0xFFFF,
            revision_id: 0,
            class_code: [0, 0, 0],
            header_type: PCI_HEADER_TYPE_NORMAL,
            subsystem_vendor_id: None,
            subsystem_id: None,
            interrupt_pin: None,
        }
    }
}
