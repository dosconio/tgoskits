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

//! PCI constants for Host Bridge emulation.

pub const PCI_CONFIG_ADDRESS: u16 = 0xCF8;
pub const PCI_CONFIG_DATA: u16 = 0xCFC;

pub const PCI_CONFIG_ADDRESS_END: u16 = PCI_CONFIG_ADDRESS + 3;
pub const PCI_CONFIG_DATA_END: u16 = PCI_CONFIG_DATA + 3;

pub const PCI_CONFIG_ENABLE: u32 = 1 << 31;

pub const PCI_VENDOR_ID: u8 = 0x00;
pub const PCI_DEVICE_ID: u8 = 0x02;
pub const PCI_COMMAND: u8 = 0x04;
pub const PCI_STATUS: u8 = 0x06;
pub const PCI_REVISION_ID: u8 = 0x08;
pub const PCI_CLASS_CODE: u8 = 0x09;
pub const PCI_CACHE_LINE_SIZE: u8 = 0x0C;
pub const PCI_LATENCY_TIMER: u8 = 0x0D;
pub const PCI_HEADER_TYPE: u8 = 0x0E;
pub const PCI_BIST: u8 = 0x0F;
pub const PCI_BAR0: u8 = 0x10;
pub const PCI_BAR1: u8 = 0x14;
pub const PCI_BAR2: u8 = 0x18;
pub const PCI_BAR3: u8 = 0x1C;
pub const PCI_BAR4: u8 = 0x20;
pub const PCI_BAR5: u8 = 0x24;
pub const PCI_EXPANSION_ROM: u8 = 0x30;
pub const PCI_SUBSYSTEM_VENDOR_ID: u8 = 0x2C;
pub const PCI_SUBSYSTEM_ID: u8 = 0x2E;
pub const PCI_INTERRUPT_LINE: u8 = 0x3C;
pub const PCI_INTERRUPT_PIN: u8 = 0x3D;

pub const PCI_HEADER_TYPE_NORMAL: u8 = 0x00;
pub const PCI_HEADER_TYPE_BRIDGE: u8 = 0x01;
pub const PCI_HEADER_TYPE_CARDBUS: u8 = 0x02;
pub const PCI_HEADER_TYPE_MULTI_FUNC: u8 = 0x80;

pub const PCI_VENDOR_INTEL: u16 = 0x8086;
pub const PCI_DEVICE_Q35_MCH: u16 = 0x29C0;
pub const PCI_DEVICE_Q35_LPC: u16 = 0x2918;

pub const PCI_VENDOR_VIRTIO: u16 = 0x1AF4;
pub const PCI_DEVICE_VIRTIO_BLK: u16 = 0x1001;

pub const PCI_CLASS_HOST_BRIDGE: u8 = 0x06;
pub const PCI_SUBCLASS_HOST_BRIDGE: u8 = 0x00;
pub const PCI_CLASS_BRIDGE: u8 = 0x06;
pub const PCI_SUBCLASS_ISA_BRIDGE: u8 = 0x01;
pub const PCI_CLASS_MASS_STORAGE: u8 = 0x01;
pub const PCI_SUBCLASS_OTHER_MASS_STORAGE: u8 = 0x80;

pub const PCI_COMMAND_IO: u16 = 0x01;
pub const PCI_COMMAND_MEMORY: u16 = 0x02;
pub const PCI_COMMAND_BUS_MASTER: u16 = 0x04;
