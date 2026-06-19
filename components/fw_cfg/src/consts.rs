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

//! fw_cfg constants from QEMU

/// fw_cfg I/O ports
pub const FW_CFG_IO_SELECTOR: u16 = 0x510;
pub const FW_CFG_IO_DATA: u16 = 0x511;

/// fw_cfg version (bit 0: traditional interface, bit 1: DMA interface)
/// FW_CFG_VERSION: 0x01 = traditional (PIO only), 0x02 = DMA, 0x03 = both.
/// Enable DMA (0x03) so OVMF uses bulk DMA transfers instead of byte-by-byte
/// PIO reads. PIO mode is extremely slow in a VM because each byte requires
/// 2 VM-exits (selector write + data read), making fw_cfg file directory
/// reads take minutes instead of milliseconds.
pub const FW_CFG_VERSION: u32 = 0x03;
pub const FW_CFG_VERSION_TRADITIONAL: u32 = 0x01;
pub const FW_CFG_VERSION_DMA: u32 = 0x02;

/// fw_cfg DMA I/O port (x86)
pub const FW_CFG_IO_DMA: u16 = 0x514;

/// fw_cfg DMA control bits
pub const FW_CFG_DMA_CTL_ERROR: u32 = 0x00000001;
pub const FW_CFG_DMA_CTL_READ: u32 = 0x00000002;
pub const FW_CFG_DMA_CTL_WRITE: u32 = 0x00000004;
pub const FW_CFG_DMA_CTL_SKIP: u32 = 0x00000008;
pub const FW_CFG_DMA_CTL_SELECT: u32 = 0x00000010;

/// fw_cfg selector keys (well-known items)
pub const FW_CFG_SIGNATURE: u16 = 0x0000;
pub const FW_CFG_ID: u16 = 0x0001;
pub const FW_CFG_UUID: u16 = 0x0002;
pub const FW_CFG_RAM_SIZE: u16 = 0x0003;
pub const FW_CFG_NOGRAPHIC: u16 = 0x0004;
pub const FW_CFG_NB_CPUS: u16 = 0x0005;
pub const FW_CFG_MACHINE_ID: u16 = 0x0006;
pub const FW_CFG_KERNEL_ADDR: u16 = 0x0007;
pub const FW_CFG_KERNEL_SIZE: u16 = 0x0008;
pub const FW_CFG_KERNEL_CMDLINE: u16 = 0x0009;
pub const FW_CFG_INITRD_ADDR: u16 = 0x000a;
pub const FW_CFG_INITRD_SIZE: u16 = 0x000b;
pub const FW_CFG_BOOT_DEVICE: u16 = 0x000c;
pub const FW_CFG_NUMA: u16 = 0x000d;
pub const FW_CFG_BOOT_MENU: u16 = 0x000e;
pub const FW_CFG_MAX_CPUS: u16 = 0x000f;
pub const FW_CFG_KERNEL_ENTRY: u16 = 0x0010;
pub const FW_CFG_KERNEL_DATA: u16 = 0x0011;
pub const FW_CFG_INITRD_DATA: u16 = 0x0012;
pub const FW_CFG_CMDLINE_ADDR: u16 = 0x0013;
pub const FW_CFG_CMDLINE_SIZE: u16 = 0x0014;
pub const FW_CFG_CMDLINE_DATA: u16 = 0x0015;
pub const FW_CFG_SETUP_ADDR: u16 = 0x0016;
pub const FW_CFG_SETUP_SIZE: u16 = 0x0017;
pub const FW_CFG_SETUP_DATA: u16 = 0x0018;
pub const FW_CFG_FILE_DIR: u16 = 0x0019;

/// Start of file selector keys
pub const FW_CFG_FILE_START: u16 = 0x8000;

/// Maximum number of file entries
pub const FW_CFG_MAX_FILE: u32 = 0x10000 - FW_CFG_FILE_START as u32;
