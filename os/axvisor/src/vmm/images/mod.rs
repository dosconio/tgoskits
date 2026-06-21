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

use ax_errno::{AxResult, ax_err};
use axaddrspace::{GuestPhysAddr, HostPhysAddr, HostVirtAddr, MappingFlags};

use axvm::VMMemoryRegion;
use axvm::config::AxVMCrateConfig;
use axvm::config::VMBootMode;
use byte_unit::Byte;

use crate::hal::CacheOp;
use crate::vmm::VMRef;
use crate::vmm::config::{config, get_vm_dtb_arc};

#[cfg(target_arch = "x86_64")]
use alloc::sync::Arc;

#[cfg(target_arch = "x86_64")]
use alloc::vec::Vec;

/// Guest memory accessor that wraps an `AxVMRef` to provide GPA→HVA
/// translation for device emulation. This implements the
/// `virtio_blk_pci::GuestMemoryAccessor` trait so that the virtio-blk
/// device can read/write guest physical memory for VirtQueue processing.
///
/// Uses `read_from_guest_of::<u8>` / `write_to_guest_of::<u8>` which
/// handle translation errors gracefully (no panic) and have no alignment
/// requirements, making them safe for arbitrary VirtQueue addresses.
#[cfg(target_arch = "x86_64")]
struct VmGuestMemoryAccessor {
    vm: VMRef,
}

#[cfg(target_arch = "x86_64")]
impl virtio_blk_pci::GuestMemoryAccessor for VmGuestMemoryAccessor {
    fn read_guest_memory(&self, gpa: u64, buf: &mut [u8]) -> AxResult {
        use axaddrspace::GuestPhysAddr;
        for (i, byte) in buf.iter_mut().enumerate() {
            let gpa = GuestPhysAddr::from(gpa as usize + i);
            *byte = self.vm.read_from_guest_of::<u8>(gpa)?;
        }
        Ok(())
    }

    fn write_guest_memory(&self, gpa: u64, buf: &[u8]) -> AxResult {
        use axaddrspace::GuestPhysAddr;
        for (i, byte) in buf.iter().enumerate() {
            let gpa = GuestPhysAddr::from(gpa as usize + i);
            self.vm.write_to_guest_of(gpa, byte)?;
        }
        Ok(())
    }
}

#[cfg(target_arch = "x86_64")]
impl fw_cfg::GuestMemoryAccessor for VmGuestMemoryAccessor {
    fn read_guest_memory(&self, gpa: u64, buf: &mut [u8]) -> AxResult {
        use ax_hal::mem::phys_to_virt;
        use axaddrspace::GuestPhysAddr;

        for (i, byte) in buf.iter_mut().enumerate() {
            let gpa_addr = GuestPhysAddr::from(gpa as usize + i);
            match self.vm.read_from_guest_of::<u8>(gpa_addr) {
                Ok(b) => *byte = b,
                Err(_) => {
                    // Fallback: try EPT page table translation directly.
                    // This handles addresses mapped by EPT violation handlers
                    // (e.g., dummy_ff_page) that are not tracked in Address Space areas.
                    if let Some(hpa) = self.vm.translate_gpa_pt_only(gpa_addr) {
                        let hva = phys_to_virt(hpa);
                        unsafe {
                            *byte = core::ptr::read_volatile(hva.as_mut_ptr());
                        }
                    } else {
                        return ax_err!(InvalidInput, "GPA not found in areas or EPT");
                    }
                }
            }
        }
        Ok(())
    }

    fn write_guest_memory(&self, gpa: u64, buf: &[u8]) -> AxResult {
        use ax_hal::mem::phys_to_virt;
        use axaddrspace::GuestPhysAddr;

        for (i, byte) in buf.iter().enumerate() {
            let gpa_addr = GuestPhysAddr::from(gpa as usize + i);
            match self.vm.write_to_guest_of(gpa_addr, byte) {
                Ok(()) => {}
                Err(_) => {
                    // Fallback: try EPT page table translation directly.
                    if let Some(hpa) = self.vm.translate_gpa_pt_only(gpa_addr) {
                        let hva = phys_to_virt(hpa);
                        unsafe {
                            core::ptr::write_volatile(hva.as_mut_ptr(), *byte);
                        }
                    } else {
                        return ax_err!(InvalidInput, "GPA not found in areas or EPT");
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(target_arch = "x86_64")]
mod guest_serial {
    use ax_errno::AxResult;
    use axaddrspace::device::{AccessWidth, Port, PortRange};
    use axdevice_base::{BaseDeviceOps, EmuDeviceType};
    use core::sync::atomic::{AtomicU8, Ordering};
    use std::print;

    pub struct GuestSerial;

    const COM1_BASE: u16 = 0x3F8;
    const COM1_END: u16 = 0x3FF; // Include SCR (offset 7) for serial port detection

    /// Scratch register for serial port detection (OVMF writes 0xAA/0x55 and reads back).
    static SCRATCH: AtomicU8 = AtomicU8::new(0);

    impl BaseDeviceOps<PortRange> for GuestSerial {
        fn emu_type(&self) -> EmuDeviceType {
            EmuDeviceType::Console
        }

        fn address_range(&self) -> PortRange {
            PortRange::new(Port(COM1_BASE), Port(COM1_END))
        }

        fn handle_read(&self, addr: Port, _width: AccessWidth) -> AxResult<usize> {
            let offset = addr.0 - COM1_BASE;
            match offset {
                0 => {
                    // Receive Buffer Register - no data available
                    Ok(0)
                }
                5 => {
                    // Line Status Register - Transmitter Holding Register Empty
                    Ok(0x60)
                }
                6 => {
                    // Modem Status Register - DCD, DSR, CTS
                    Ok(0x30)
                }
                7 => {
                    // Scratch Register - return last written value
                    // OVMF writes test patterns (0xAA, 0x55) to detect serial port
                    Ok(SCRATCH.load(Ordering::Relaxed) as usize)
                }
                _ => Ok(0),
            }
        }

        fn handle_write(&self, addr: Port, _width: AccessWidth, val: usize) -> AxResult {
            let offset = addr.0 - COM1_BASE;
            if offset == 0 {
                let ch = val as u8;
                if ch >= 0x20 && ch < 0x7F {
                    print!("{}", ch as char);
                } else if ch == b'\n' {
                    print!("\n");
                } else if ch == b'\r' {
                    // ignore CR
                } else {
                    print!("\\x{:02x}", ch);
                }
            } else if offset == 7 {
                // Scratch Register - store for serial port detection
                SCRATCH.store(val as u8, Ordering::Relaxed);
            }
            Ok(())
        }
    }
}

/// Simple i8042 keyboard controller emulation.
///
/// OVMF polls port 0x64 (status register) during boot to check for
/// keyboard input and to verify the controller self-test completed.
/// Without this emulation, port 0x64 reads return 0 (unregistered
/// port default), so bit 2 (SYS_FLAG / self-test passed) is never
/// set. OVMF then spins in a polling loop reading PM-TIMER and
/// port 0x64 repeatedly, never progressing to the boot device
/// selection phase.
///
/// The status register is returned with:
///   bit 2 = 1  (system flag: self-test passed)
///   bit 1 = 0  (input buffer empty: ready for commands)
///   bit 0 = 0  (output buffer empty: no data available)
/// which gives a status value of 0x04.
mod i8042 {
    use ax_errno::AxResult;
    use axaddrspace::device::{AccessWidth, Port, PortRange};
    use axdevice_base::{BaseDeviceOps, EmuDeviceType};

    pub struct I8042;

    impl Default for I8042 {
        fn default() -> Self {
            Self
        }
    }

    impl I8042 {
        pub fn new() -> Self {
            Self
        }
    }

    impl BaseDeviceOps<PortRange> for I8042 {
        fn emu_type(&self) -> EmuDeviceType {
            EmuDeviceType::Dummy
        }

        fn address_range(&self) -> PortRange {
            // i8042 keyboard controller: data port 0x60, status/command port 0x64
            PortRange::new(Port(0x60), Port(0x64))
        }

        fn handle_read(&self, addr: Port, _width: AccessWidth) -> AxResult<usize> {
            match addr.0 {
                0x60 => {
                    // Data register: no data available
                    Ok(0)
                }
                0x64 => {
                    // Status register: bit 2 (SYS_FLAG) = 1 (self-test passed)
                    Ok(0x04)
                }
                _ => Ok(0),
            }
        }

        fn handle_write(&self, addr: Port, _width: AccessWidth, _val: usize) -> AxResult {
            // Silently ignore writes to command (0x64) and data (0x60) ports
            Ok(())
        }
    }
}

mod linux;
#[cfg(target_arch = "x86_64")]
mod x86_boot;

pub fn get_image_header(config: &AxVMCrateConfig) -> Option<linux::Header> {
    match config.kernel.image_location.as_deref() {
        Some("memory") => with_memory_image(config, linux::Header::parse),
        #[cfg(feature = "fs")]
        Some("fs") => {
            let read_size = linux::Header::hdr_size();
            let data = fs::kernal_read(config, read_size).ok()?;
            linux::Header::parse(&data)
        }
        _ => unimplemented!(
            "Check your \"image_location\" in config.toml, \"memory\" and \"fs\" are supported,\n NOTE: \"fs\" feature should be enabled if you want to load images from filesystem. (APP_FEATURES=fs)"
        ),
    }
}

fn with_memory_image<F, R>(config: &AxVMCrateConfig, func: F) -> R
where
    F: FnOnce(&[u8]) -> R,
{
    let vm_imags = config::get_memory_images()
        .iter()
        .find(|&v| v.id == config.base.id)
        .expect("VM images is missed, Perhaps add `VM_CONFIGS=PATH/CONFIGS/FILE` command.");

    func(vm_imags.kernel)
}

pub struct ImageLoader {
    main_memory: VMMemoryRegion,
    vm: VMRef,
    config: AxVMCrateConfig,
    kernel_load_gpa: GuestPhysAddr,
    bios_load_gpa: Option<GuestPhysAddr>,
    dtb_load_gpa: Option<GuestPhysAddr>,
    pflash0_load_gpa: Option<GuestPhysAddr>,
    pflash1_load_gpa: Option<GuestPhysAddr>,
}

impl ImageLoader {
    pub fn new(main_memory: VMMemoryRegion, config: AxVMCrateConfig, vm: VMRef) -> Self {
        Self {
            main_memory,
            vm,
            config,
            kernel_load_gpa: GuestPhysAddr::default(),
            bios_load_gpa: None,
            dtb_load_gpa: None,
            pflash0_load_gpa: None,
            pflash1_load_gpa: None,
        }
    }

    pub fn load(&mut self) -> AxResult {
        info!(
            "Loading VM[{}] images into memory region: gpa={:#x}, hva={:#x}, size={:#}",
            self.vm.id(),
            self.main_memory.gpa,
            self.main_memory.hva,
            Byte::from(self.main_memory.size())
        );

        self.vm.with_config(|config| {
            self.kernel_load_gpa = config.image_config.kernel_load_gpa;
            self.dtb_load_gpa = config.image_config.dtb_load_gpa;
            self.bios_load_gpa = config.image_config.bios_load_gpa;
            self.pflash0_load_gpa = config.image_config.pflash0_load_gpa;
            self.pflash1_load_gpa = config.image_config.pflash1_load_gpa;
            info!(
                "[ImageLoader] pflash0_load_gpa: {:?}, pflash1_load_gpa: {:?}",
                self.pflash0_load_gpa, self.pflash1_load_gpa
            );
        });

        match self.config.kernel.boot_mode {
            VMBootMode::Trampoline => match self.config.kernel.image_location.as_deref() {
                Some("memory") => self.load_vm_images_from_memory(),
                #[cfg(feature = "fs")]
                Some("fs") => fs::load_vm_images_from_filesystem(self),
                _ => unimplemented!(
                    "Check your \"image_location\" in config.toml, \"memory\" and \"fs\" are supported,\n NOTE: \"fs\" feature should be enabled if you want to load images from filesystem. (APP_FEATURES=fs)"
                ),
            },
            VMBootMode::Uefi => self.load_vm_images_uefi(),
        }
    }

    /// Load VM images from memory
    /// into the guest VM's memory space based on the VM configuration.
    fn load_vm_images_from_memory(&self) -> AxResult {
        info!("Loading VM[{}] images from memory", self.config.base.id);

        let vm_imags = config::get_memory_images()
            .iter()
            .find(|&v| v.id == self.config.base.id)
            .expect("VM images is missed, Perhaps add `VM_CONFIGS=PATH/CONFIGS/FILE` command.");

        load_vm_image_from_memory(vm_imags.kernel, self.kernel_load_gpa, self.vm.clone())
            .expect("Failed to load VM images");

        // Load Ramdisk image and record its size before regenerating the DTB.
        if let Some(buffer) = vm_imags.ramdisk {
            self.load_ramdisk_from_memory(buffer)
                .expect("Failed to load Ramdisk images");
        }
        // Load DTB image
        let vm_config = axvm::config::AxVMConfig::from(self.config.clone());

        if let Some(dtb_arc) = get_vm_dtb_arc(&vm_config) {
            let _dtb_slice: &[u8] = &dtb_arc;
            #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
            crate::vmm::fdt::update_fdt(
                core::ptr::NonNull::new(_dtb_slice.as_ptr() as *mut u8).unwrap(),
                _dtb_slice.len(),
                self.vm.clone(),
                &self.config,
            );
            #[cfg(target_arch = "loongarch64")]
            load_vm_image_from_memory(_dtb_slice, self.dtb_load_gpa.unwrap(), self.vm.clone())
                .expect("Failed to load DTB images");
        } else {
            #[cfg(any(target_arch = "loongarch64", target_arch = "riscv64"))]
            if let Some(buffer) = vm_imags.dtb {
                load_vm_image_from_memory(buffer, self.dtb_load_gpa.unwrap(), self.vm.clone())
                    .expect("Failed to load DTB images");
            } else {
                info!("dtb_load_gpa not provided");
            }

            #[cfg(not(target_arch = "riscv64"))]
            {
                info!("dtb_load_gpa not provided");
            }
        }

        self.load_boot_image_from_memory(vm_imags.bios)?;

        Ok(())
    }

    fn load_boot_image_from_memory(&self, bios: Option<&[u8]>) -> AxResult {
        if !self.config.kernel.enable_bios {
            return Ok(());
        }

        if let Some(buffer) = bios {
            let load_gpa = self
                .bios_load_gpa
                .expect("BIOS image present but BIOS load addr is missed");
            load_vm_image_from_memory(buffer, load_gpa, self.vm.clone())
                .expect("Failed to load BIOS images");
            #[cfg(target_arch = "x86_64")]
            self.load_x86_multiboot_info(buffer, load_gpa)?;
            return Ok(());
        }

        #[cfg(target_arch = "x86_64")]
        if self.should_load_default_x86_boot_image() {
            let bios_load_gpa = builtin_x86_bios_load_gpa(self.bios_load_gpa)?;
            info!(
                "Loading built-in x86 boot image at GPA {:#x}",
                bios_load_gpa.as_usize()
            );
            load_vm_image_from_memory(x86_boot::DEFAULT_BIOS_IMAGE, bios_load_gpa, self.vm.clone())
                .expect("Failed to load built-in x86 boot image");
            #[cfg(target_arch = "x86_64")]
            self.load_x86_multiboot_info(x86_boot::DEFAULT_BIOS_IMAGE, bios_load_gpa)?;
        }

        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    fn should_load_default_x86_boot_image(&self) -> bool {
        self.config.kernel.enable_bios && self.config.kernel.bios_path.is_none()
    }

    #[cfg(target_arch = "x86_64")]
    fn load_x86_multiboot_info(&self, bios_image: &[u8], bios_load_gpa: GuestPhysAddr) -> AxResult {
        const MULTIBOOT_INFO_GPA: usize = 0x6000;
        const MULTIBOOT_MMAP_GPA: usize = 0x6040;
        const MULTIBOOT_INFO_FLAGS: u32 = (1 << 0) | (1 << 6);
        const MULTIBOOT_MEMORY_AVAILABLE: u32 = 1;

        let mem_base = self.main_memory.gpa.as_usize() as u64;
        let mem_size = self.main_memory.size() as u64;
        let mem_upper_kb = mem_size.saturating_sub(0x100000) / 1024;

        let mut mbi = [0u8; 52];
        write_u32(&mut mbi, 0, MULTIBOOT_INFO_FLAGS);
        write_u32(&mut mbi, 4, 639);
        write_u32(&mut mbi, 8, mem_upper_kb as u32);
        write_u32(&mut mbi, 44, 24);
        write_u32(&mut mbi, 48, MULTIBOOT_MMAP_GPA as u32);

        let mut mmap = [0u8; 24];
        write_u32(&mut mmap, 0, 20);
        write_u64(&mut mmap, 4, mem_base);
        write_u64(&mut mmap, 12, mem_size);
        write_u32(&mut mmap, 20, MULTIBOOT_MEMORY_AVAILABLE);

        let mbi_gpa = (MULTIBOOT_INFO_GPA as u32).to_le_bytes();
        validate_x86_bios_patch_region(bios_image)?;
        load_vm_image_from_memory(&mbi, MULTIBOOT_INFO_GPA.into(), self.vm.clone())?;
        load_vm_image_from_memory(&mmap, MULTIBOOT_MMAP_GPA.into(), self.vm.clone())?;
        load_vm_image_from_memory(
            &mbi_gpa,
            (bios_load_gpa.as_usize() + x86_boot::AXVM_BIOS_EBX_IMM_OFFSET).into(),
            self.vm.clone(),
        )?;
        Ok(())
    }

    fn load_ramdisk_from_memory(&self, ramdisk: &[u8]) -> AxResult {
        let load_gpa = self
            .vm
            .with_config(|config| config.image_config.ramdisk.as_ref().map(|r| r.load_gpa))
            .expect("Ramdisk image present but ramdisk info is missing");
        let size = ramdisk.len();
        self.vm.with_config(|config| {
            if let Some(ref mut rd) = config.image_config.ramdisk {
                rd.size = Some(size);
            }
        });
        info!(
            "Loading ramdisk image from memory ({} bytes) into GPA @{:#x}",
            size,
            load_gpa.as_usize()
        );
        load_vm_image_from_memory(ramdisk, load_gpa, self.vm.clone())
    }

    /// Load VM images for UEFI boot mode.
    ///
    /// Loads OVMF_CODE and OVMF_VARS as pflash regions, then optionally loads
    /// the kernel and ramdisk. The reset vector mapping at 0xFFFFFFF0 is handled
    /// by EPT setup elsewhere (the pflash0 region must include the firmware image
    /// whose last 16 bytes contain the reset vector jump instruction).
    fn load_vm_images_uefi(&self) -> AxResult {
        info!("Loading VM[{}] images in UEFI mode", self.config.base.id);

        // Load OVMF_CODE (pflash0) — read-only firmware code
        // Pflash files are aligned to the END of the pflash region, matching QEMU's
        // pflash model. If the file is smaller than the region, the file is loaded at
        // an offset so that the last byte of the file aligns with the last byte of the
        // region. This ensures the x86 reset vector (at the end of OVMF_CODE) maps to
        // GPA 0xFFFFFFF0.
        if let (Some(pflash0_path), Some(pflash0_gpa)) =
            (&self.config.kernel.pflash0, self.pflash0_load_gpa)
        {
            #[cfg(feature = "fs")]
            {
                let (_, file_size) = fs::open_image_file(&pflash0_path.path)?;
                let region_size = pflash0_path.size;
                let load_offset = region_size.saturating_sub(file_size);
                let load_gpa = GuestPhysAddr::from(pflash0_gpa.as_usize() + load_offset);
                let file_end_gpa = load_gpa.as_usize() + file_size;
                info!(
                    "[pflash0] Loading {} (file {} bytes, region {:#x}) at offset {:#x}, GPA {:#x}, file_end_gpa {:#x}",
                    pflash0_path.path,
                    file_size,
                    region_size,
                    load_offset,
                    load_gpa.as_usize(),
                    file_end_gpa
                );
                // Verify: file_end_gpa should equal pflash0_gpa + region_size
                let expected_end = pflash0_gpa.as_usize() + region_size;
                if file_end_gpa != expected_end {
                    warn!(
                        "[pflash0] WARNING: file_end_gpa {:#x} != expected {:#x} (gap = {:#x} bytes at end of pflash0 region!)",
                        file_end_gpa,
                        expected_end,
                        expected_end - file_end_gpa
                    );
                }
                // Fill the entire pflash0 region with 0xFF before loading the file.
                // Unprogrammed Flash memory reads as 0xFF; the region was zero-initialized
                // by vm_alloc_memorys, so the gap before the file (load_offset bytes) must
                // be patched to 0xFF. Without this, OVMF Firmware Volume traversal may
                // misinterpret the 0x00-filled gap as valid FV structures.
                let mut pflash0_region_data = self
                    .vm
                    .get_image_load_region(pflash0_gpa, region_size)
                    .unwrap_or_default();
                for slice in &mut pflash0_region_data {
                    for b in slice.iter_mut() {
                        *b = 0xFF;
                    }
                }

                let _pflash0_hva =
                    fs::load_vm_image(&pflash0_path.path, load_gpa, self.vm.clone())?;

                // Verify reset vector at 0xFFFFFFF0 (last 16 bytes of pflash0 region)
                let rv_gpa = GuestPhysAddr::from(0xFFFFFFF0usize);
                let rv_data = self
                    .vm
                    .get_image_load_region(rv_gpa, 16)
                    .unwrap_or_default();
                if !rv_data.is_empty() {
                    let bytes = &rv_data[0][..16.min(rv_data[0].len())];
                    info!(
                        "[UEFI] GPA 0xFFFFFFF0 HVA: {:#x}",
                        rv_data[0].as_ptr() as usize
                    );
                    info!("[UEFI] GPA 0xFFFFFFF0 data: {:02x?}", bytes);
                    // Check if reset vector is valid (should be 90 90 e9 ... or ea ...)
                    let is_valid = bytes.iter().any(|&b| b != 0 && b != 0xff);
                    if !is_valid {
                        warn!(
                            "[UEFI] Reset vector at GPA 0xFFFFFFF0 appears invalid (all zeros or 0xFF)!"
                        );
                    }
                } else {
                    warn!("[UEFI] Cannot read GPA 0xFFFFFFF0 - no image load region found!");
                }
            }
            #[cfg(not(feature = "fs"))]
            {
                let _ = (pflash0_path, pflash0_gpa);
                return Err(ax_errno::ax_err_type!(
                    Unsupported,
                    "UEFI boot requires fs feature for loading OVMF images"
                ));
            }
        } else {
            return Err(ax_errno::ax_err_type!(
                InvalidInput,
                "UEFI boot mode requires pflash0 (OVMF_CODE) configuration"
            ));
        }

        // Load OVMF_VARS (pflash1) — read-write UEFI variable store
        if let (Some(pflash1_path), Some(pflash1_gpa)) =
            (&self.config.kernel.pflash1, self.pflash1_load_gpa)
        {
            #[cfg(feature = "fs")]
            {
                let (_, file_size) = fs::open_image_file(&pflash1_path.path)?;
                let region_size = pflash1_path.size;
                let load_offset = region_size.saturating_sub(file_size);
                let load_gpa = GuestPhysAddr::from(pflash1_gpa.as_usize() + load_offset);
                info!(
                    "[pflash1] Loading {} (file {} bytes, region {:#x}) at offset {:#x}, GPA {:#x}",
                    pflash1_path.path,
                    file_size,
                    region_size,
                    load_offset,
                    load_gpa.as_usize()
                );
                // Fill pflash1 region with 0xFF before loading (same rationale as pflash0).
                let mut pflash1_region_data = self
                    .vm
                    .get_image_load_region(pflash1_gpa, region_size)
                    .unwrap_or_default();
                for slice in &mut pflash1_region_data {
                    for b in slice.iter_mut() {
                        *b = 0xFF;
                    }
                }
                let _ = fs::load_vm_image(&pflash1_path.path, load_gpa, self.vm.clone())?;
            }
            #[cfg(not(feature = "fs"))]
            {
                let _ = (pflash1_path, pflash1_gpa);
            }
        } else {
            warn!(
                "UEFI boot mode: pflash1 (OVMF_VARS) not configured, OVMF may use default variables"
            );
        }

        // In UEFI mode, kernel is passed to OVMF via fw_cfg (Path A).
        // OVMF's QemuLoadKernelImage driver loads it from fw_cfg at the right GPA.
        // Skip loading kernel here to avoid conflicting with limited guest memory region.

        // Load ramdisk if provided
        if let Some(ramdisk_path) = &self.config.kernel.ramdisk_path {
            #[cfg(feature = "fs")]
            {
                self.load_ramdisk_from_filesystem(ramdisk_path)?;
            }
            #[cfg(not(feature = "fs"))]
            {
                let _ = ramdisk_path;
            }
        }

        // Create fw_cfg device and inject boot info
        #[cfg(target_arch = "x86_64")]
        self.setup_fw_cfg_and_acpi()?;

        Ok(())
    }

    /// Set up fw_cfg device and ACPI tables for UEFI boot (x86_64 only).
    ///
    /// This method:
    /// 1. Creates a fw_cfg device with boot information (RAM size, CPU count)
    /// 2. Generates minimal ACPI tables (RSDP, XSDT, FADT, MADT, MCFG)
    /// 3. Writes ACPI tables into guest memory
    /// 4. Registers ACPI tables as fw_cfg file items
    /// 5. Registers the fw_cfg device as a port I/O device
    #[cfg(target_arch = "x86_64")]
    fn setup_fw_cfg_and_acpi(&self) -> AxResult {
        use acpi_tables::{AcpiConfig, AcpiTableBuilder};
        use fw_cfg::FwCfgDevice;

        // Calculate RAM size from memory_regions (exclude pflash regions)
        let ram_size: usize = self
            .config
            .kernel
            .memory_regions
            .iter()
            .filter(|r| r.gpa < 0xFF00_0000)
            .map(|r| r.size)
            .sum();
        let cpu_num = self.config.base.cpu_num;

        info!(
            "Setting up fw_cfg and ACPI tables: ram_size={:#x}, cpu_num={}",
            ram_size, cpu_num
        );

        // Map pflash0 alias for legacy BIOS area 0xF0000-0xFFFFF
        // OVMF needs this area aliased to pflash0 content for legacy x86 boot
        // compatibility (SEC phase in real mode reads this region)
        let pflash0_gpa = 0xFFC0_0000usize;
        let legacy_alias_gpa = 0xF0000usize;
        let alias_size = 0x10000usize;
        for region in self.vm.memory_regions() {
            if region.gpa.as_usize() == pflash0_gpa {
                let pflash_hpa = region.host_paddr();
                let alias_offset = 0xFFFF_0000 - pflash0_gpa;
                let alias_hpa = HostPhysAddr::from(pflash_hpa.as_usize() + alias_offset);
                self.vm.map_region(
                    GuestPhysAddr::from(legacy_alias_gpa),
                    alias_hpa,
                    alias_size,
                    MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE,
                )?;
                info!(
                    "Mapped pflash0 alias: GPA {:#x} -> HPA {:#x} (size {:#x}, offset {:#x})",
                    legacy_alias_gpa,
                    alias_hpa.as_usize(),
                    alias_size,
                    alias_offset
                );
                break;
            }
        }

        // Generate ACPI tables
        // RSDP at 0x200000 (2MB, in main RAM) — this region is marked as
        // E820_ACPI in the E820 map so Linux reserves it for ACPI tables.
        // We use main RAM instead of the BIOS ROM area (0xE0000) because
        // Linux's direct map only covers E820_RAM regions, and accessing
        // ACPI tables in reserved memory through __va() causes page faults.
        let acpi_config = AcpiConfig {
            cpu_num,
            ram_size,
            lapic_addr: 0xFEE0_0000,
            ioapic_addr: 0xFEC0_0000,
            ioapic_id: 0,
            ioapic_gsi_base: 0,
            ecam_base_addr: 0xB000_0000,
            ecam_segment: 0,
            ecam_bus_start: 0,
            ecam_bus_end: 0xFF,
            rsdp_gpa: 0x20_0000,
        };
        let acpi_tables = AcpiTableBuilder::new(acpi_config).build();

        // Write ACPI tables into guest memory
        // RSDP at 0x200000 (2MB, E820_ACPI region)
        // Tables (XSDT+FADT+MADT+MCFG+DSDT) follow immediately after RSDP.
        let rsdp_gpa = GuestPhysAddr::from(0x20_0000usize);
        let mut rsdp_regions = self
            .vm
            .get_image_load_region(rsdp_gpa, acpi_tables.rsdp.len())?;
        let mut offset = 0;
        for region in &mut rsdp_regions {
            let copy_len = region.len().min(acpi_tables.rsdp.len() - offset);
            region[..copy_len].copy_from_slice(&acpi_tables.rsdp[offset..offset + copy_len]);
            offset += copy_len;
        }
        info!(
            "Wrote RSDP ({} bytes) to GPA {:#x}",
            acpi_tables.rsdp.len(),
            rsdp_gpa.as_usize()
        );

        // Other tables follow RSDP
        let tables_gpa = GuestPhysAddr::from(0x20_0000usize + acpi_tables.rsdp.len());
        let mut table_regions = self
            .vm
            .get_image_load_region(tables_gpa, acpi_tables.tables.len())?;
        offset = 0;
        for region in &mut table_regions {
            let copy_len = region.len().min(acpi_tables.tables.len() - offset);
            region[..copy_len].copy_from_slice(&acpi_tables.tables[offset..offset + copy_len]);
            offset += copy_len;
        }
        info!(
            "Wrote ACPI tables ({} bytes) to GPA {:#x}",
            acpi_tables.tables.len(),
            tables_gpa.as_usize()
        );

        // Debug: dump DSDT bytes for AML verification
        for (name, offset, size) in &acpi_tables.table_offsets {
            if name == "DSDT" {
                let dsdt_bytes = &acpi_tables.tables[*offset..*offset + *size];
                info!("DSDT dump ({} bytes):", size);
                for (i, chunk) in dsdt_bytes.chunks(16).enumerate() {
                    let mut hex = alloc::string::String::new();
                    for b in chunk {
                        hex.push_str(&format!("{:02x} ", b));
                    }
                    info!("  DSDT[{:04x}]: {}", i * 16, hex);
                }
            }
        }

        // Create fw_cfg device
        let fw_cfg = FwCfgDevice::new(ram_size, cpu_num);

        // Register ACPI tables as fw_cfg file items
        fw_cfg.add_file("etc/acpi/tables", &acpi_tables.tables);
        fw_cfg.add_file("etc/acpi/rsdp", &acpi_tables.rsdp);

        // Register kernel command line if provided
        if let Some(cmdline) = &self.config.kernel.cmdline {
            fw_cfg.add_file("etc/boot-cmdline", cmdline.as_bytes());
            fw_cfg.add_file("opt/org.qemu/cmdline", cmdline.as_bytes());
        }

        // Register kernel image as fw_cfg file for direct kernel boot (Path A)
        #[cfg(feature = "fs")]
        {
            let kernel_path = &self.config.kernel.kernel_path;
            if !kernel_path.is_empty() && fs::file_exists(kernel_path) {
                match fs::read_file_bytes(kernel_path) {
                    Ok(kernel_data) => {
                        info!(
                            "[fw_cfg] Registering kernel file: {} ({} bytes)",
                            kernel_path,
                            kernel_data.len()
                        );
                        fw_cfg.add_file("opt/org.qemu/kernel", &kernel_data);

                        // Parse bzImage header and set up legacy well-known
                        // selectors for OVMF direct kernel boot.
                        // The bzImage format: offset 0x1F1 has setup_sects (u8).
                        // If setup_sects == 0, use 4 sectors.
                        // Setup size = (setup_sects + 1) * 512 bytes.
                        // Kernel data starts after the setup sectors.
                        if kernel_data.len() > 0x1F2 {
                            let setup_sects = kernel_data[0x1F1] as usize;
                            let setup_sects = if setup_sects == 0 { 4 } else { setup_sects };
                            let setup_size = (setup_sects + 1) * 512;
                            let setup_size = setup_size.min(kernel_data.len());
                            let kernel_size = kernel_data.len().saturating_sub(setup_size);

                            info!(
                                "[fw_cfg] bzImage: setup_sects={}, setup_size={}, \
                                 kernel_size={}",
                                setup_sects, setup_size, kernel_size
                            );

                            // FW_CFG_SETUP_SIZE (0x0017) + FW_CFG_SETUP_DATA (0x0018)
                            fw_cfg.add_item_at(
                                fw_cfg::consts::FW_CFG_SETUP_SIZE,
                                &(setup_size as u32).to_le_bytes(),
                            );
                            fw_cfg.add_item_at(
                                fw_cfg::consts::FW_CFG_SETUP_DATA,
                                &kernel_data[..setup_size],
                            );

                            // FW_CFG_KERNEL_SIZE (0x0008) + FW_CFG_KERNEL_DATA (0x0011)
                            fw_cfg.add_item_at(
                                fw_cfg::consts::FW_CFG_KERNEL_SIZE,
                                &(kernel_size as u32).to_le_bytes(),
                            );
                            fw_cfg.add_item_at(
                                fw_cfg::consts::FW_CFG_KERNEL_DATA,
                                &kernel_data[setup_size..],
                            );

                            // FW_CFG_CMDLINE_SIZE (0x0014) + FW_CFG_CMDLINE_DATA (0x0015)
                            if let Some(cmdline) = &self.config.kernel.cmdline {
                                let cmdline_bytes = cmdline.as_bytes();
                                let cmdline_with_nul = if cmdline_bytes.last() != Some(&b'\0') {
                                    let mut v = cmdline_bytes.to_vec();
                                    v.push(0);
                                    v
                                } else {
                                    cmdline_bytes.to_vec()
                                };
                                fw_cfg.add_item_at(
                                    fw_cfg::consts::FW_CFG_CMDLINE_SIZE,
                                    &(cmdline_with_nul.len() as u32).to_le_bytes(),
                                );
                                fw_cfg.add_item_at(
                                    fw_cfg::consts::FW_CFG_CMDLINE_DATA,
                                    &cmdline_with_nul,
                                );
                                info!(
                                    "[fw_cfg] Registered legacy kernel cmdline ({} bytes)",
                                    cmdline_with_nul.len()
                                );
                            }
                        } else {
                            warn!(
                                "[fw_cfg] Kernel file too small to parse bzImage header \
                                 ({} bytes), skipping legacy selectors",
                                kernel_data.len()
                            );
                        }
                    }
                    Err(e) => {
                        warn!(
                            "[fw_cfg] Failed to read kernel file {}: {:?}, \
                             skipping direct kernel boot",
                            kernel_path, e
                        );
                    }
                }
            } else if !kernel_path.is_empty() {
                warn!(
                    "[fw_cfg] Kernel file not found: {}, skipping direct kernel boot",
                    kernel_path
                );
            }
        }

        // Register initrd (ramdisk) as fw_cfg file for direct kernel boot (Path A)
        #[cfg(feature = "fs")]
        if let Some(ramdisk_path) = &self.config.kernel.ramdisk_path {
            if !ramdisk_path.is_empty() && fs::file_exists(ramdisk_path) {
                match fs::read_file_bytes(ramdisk_path) {
                    Ok(initrd_data) => {
                        info!(
                            "[fw_cfg] Registering initrd file: {} ({} bytes)",
                            ramdisk_path,
                            initrd_data.len()
                        );
                        fw_cfg.add_file("opt/org.qemu/initrd", &initrd_data);

                        // FW_CFG_INITRD_SIZE (0x000b) + FW_CFG_INITRD_DATA (0x0012)
                        fw_cfg.add_item_at(
                            fw_cfg::consts::FW_CFG_INITRD_SIZE,
                            &(initrd_data.len() as u32).to_le_bytes(),
                        );
                        fw_cfg.add_item_at(fw_cfg::consts::FW_CFG_INITRD_DATA, &initrd_data);
                    }
                    Err(e) => {
                        warn!(
                            "[fw_cfg] Failed to read initrd file {}: {:?}, \
                             skipping initrd registration",
                            ramdisk_path, e
                        );
                    }
                }
            } else if !ramdisk_path.is_empty() {
                warn!(
                    "[fw_cfg] Initrd file not found: {}, skipping initrd registration",
                    ramdisk_path
                );
            }
        }

        // Always register FW_CFG_INITRD_SIZE with 0 when no initrd is provided.
        // OVMF's QemuKernelLoaderFsDxe reads this selector unconditionally; if it
        // returns an error (selector not found), OVMF can get stuck in a polling
        // loop. Returning 0 tells OVMF there is no initrd, which is the correct
        // behavior when no ramdisk is configured.
        if !self
            .config
            .kernel
            .ramdisk_path
            .as_ref()
            .map(|p| !p.is_empty() && fs::file_exists(p))
            .unwrap_or(false)
        {
            fw_cfg.add_item_at(fw_cfg::consts::FW_CFG_INITRD_SIZE, &0u32.to_le_bytes());
            info!("[fw_cfg] Registered empty initrd (size=0) — no ramdisk configured");
        }

        // Build E820 memory map
        let e820 = build_e820_table(ram_size);

        // Register E820 table as fw_cfg file
        fw_cfg.add_file("etc/e820", &e820);

        // Set up guest memory accessor for fw_cfg DMA operations
        let gma = Arc::new(VmGuestMemoryAccessor {
            vm: self.vm.clone(),
        });
        fw_cfg.set_mem_accessor(gma.clone());

        // Register fw_cfg as a port I/O device
        self.vm.get_devices().lock().add_port_dev(Arc::new(fw_cfg));
        info!("Registered fw_cfg device at I/O ports 0x510-0x51B (with DMA support)");

        // Create and register PCI Host Bridge
        use pci_host::PciHostBridge;
        let pci_host = Arc::new(PciHostBridge::new());

        // Create virtio-blk-pci device (Bus 0, Device 1, Function 0)
        use virtio_blk_pci::VirtioBlkPci;
        let blk_disk_size = 64 * 1024 * 1024; // 64 MiB disk (sparse allocation)
        let mut virtio_blk_device = VirtioBlkPci::new(blk_disk_size, (0, 1, 0));

        // Set up guest memory accessor so the device can read/write guest memory
        // for VirtQueue descriptor/avail/used ring processing
        virtio_blk_device.set_mem_accessor(gma);

        // Set up interrupt callback. PCI INTA# (pin 1) on Q35 is routed to
        // GSI 16 on the IOAPIC. In Virtual Wire Mode (8259 PIC), PCI INTA#
        // is typically routed to IRQ 10. We raise both to cover whichever
        // path the guest is using.
        //
        // The callback uses the global singletons which are initialized
        // later in this function; at call time they will be available.
        use i8259_pic;
        use log::debug;
        struct BlkInterruptCallback;
        impl virtio_blk_pci::InterruptCallback for BlkInterruptCallback {
            fn raise_interrupt(&self) {
                // IOAPIC path (GSI 16 = PCI INTA# on Q35)
                if let Some(vioapic) = x86_vioapic::GLOBAL_VIOAPIC.get() {
                    vioapic.raise_irq(16);
                }
                // 8259 PIC path (IRQ 10 = common PCI INTA# PIC routing)
                if let Some(pic) = i8259_pic::GLOBAL_PIC_MASTER.get() {
                    pic.raise_irq(10);
                }
                debug!("virtio-blk: raised interrupt (GSI 16 + PIC IRQ 10)");
            }
        }
        virtio_blk_device.set_interrupt_callback(Arc::new(BlkInterruptCallback));

        let virtio_blk = Arc::new(virtio_blk_device);
        info!(
            "Created virtio-blk-pci device: BDF=(0,1,0), disk_size={:#x}",
            blk_disk_size
        );

        // Add virtio-blk to PCI host bridge's device list
        pci_host.add_device(virtio_blk.clone());
        info!("Added virtio-blk-pci to PCI host bridge device list");

        // Register PCI Host Bridge as a port I/O device (PIO config access at 0xCF8-0xCFF)
        self.vm.get_devices().lock().add_port_dev(pci_host.clone());
        info!("Registered PCI Host Bridge at I/O ports 0xCF8-0xCFF");

        // Register PCI Host Bridge as an MMIO device (ECAM at 0xB000_0000)
        self.vm.get_devices().lock().add_mmio_dev(pci_host.clone());
        info!(
            "Registered PCI Host Bridge at ECAM MMIO {:#x}-{:#x}",
            pci_host::ECAM_BASE,
            pci_host::ECAM_BASE + pci_host::ECAM_SIZE
        );

        // Register virtio-blk-pci as a port I/O device
        // (its I/O BAR address is dynamic, assigned by OVMF at runtime)
        self.vm.get_devices().lock().add_port_dev(virtio_blk);
        info!("Registered virtio-blk-pci as port I/O device (dynamic BAR)");

        // Create and register PM Timer
        use pm_timer::PmTimer;
        let pm_timer = PmTimer::new_default();
        self.vm
            .get_devices()
            .lock()
            .add_port_dev(Arc::new(pm_timer));
        info!("Registered PM device at I/O ports 0x600-0x60B");

        // Create and register Guest Serial (COM1)
        use guest_serial::GuestSerial;
        let serial = GuestSerial;
        self.vm.get_devices().lock().add_port_dev(Arc::new(serial));
        info!("Registered Guest Serial at I/O ports 0x3F8-0x3FE");

        // Create and register i8042 keyboard controller
        let kbd = i8042::I8042::new();
        self.vm.get_devices().lock().add_port_dev(Arc::new(kbd));
        info!("Registered i8042 keyboard controller at I/O ports 0x60-0x64");

        // Create and register vIOAPIC as MMIO device
        use x86_vioapic::{GLOBAL_VIOAPIC, IoApic};
        let vioapic = Arc::new(IoApic::new(0, 0));
        let _ = GLOBAL_VIOAPIC.call_once(|| vioapic.clone());
        self.vm.get_devices().lock().add_mmio_dev(vioapic);
        info!(
            "Registered vIOAPIC at MMIO {:#x}-{:#x}",
            x86_vioapic::IOAPIC_MMIO_BASE,
            x86_vioapic::IOAPIC_MMIO_BASE + x86_vioapic::IOAPIC_MMIO_SIZE
        );

        // Create and register MC146818 CMOS/RTC device (ports 0x70-0x71)
        // OVMF reads CMOS offsets 0x34-0x35 to determine memory size below 4 GB.
        use mc146818_cmos::Mc146818Cmos;
        let cmos = Mc146818Cmos::new(ram_size);
        self.vm.get_devices().lock().add_port_dev(Arc::new(cmos));
        info!(
            "Registered MC146818 CMOS at I/O ports 0x70-0x71 (ram_size={:#x})",
            ram_size
        );

        // Create and register i8259 PIC as port I/O devices (master + slave)
        use i8259_pic::{I8259MasterPic, I8259SlavePic};
        let pic_master = Arc::new(I8259MasterPic::new());
        let pic_slave = Arc::new(I8259SlavePic::new());
        // Register Master PIC as the global singleton so the vCPU run loop
        // can check for pending ExtINT interrupts (Virtual Wire Mode).
        let _ = i8259_pic::GLOBAL_PIC_MASTER.call_once(|| pic_master.clone());
        self.vm.get_devices().lock().add_port_dev(pic_master);
        self.vm.get_devices().lock().add_port_dev(pic_slave);
        info!("Registered i8259 PIC at I/O ports 0x20-0x21, 0xA0-0xA1");

        // Create and register i8254 PIT as port I/O device (ports 0x40-0x43)
        // Connect PIT counter 0 (IRQ0) to BOTH vIOAPIC and 8259 Master PIC.
        // In Virtual Wire Mode (before APIC is enabled), OVMF receives timer
        // interrupts through the 8259 PIC path. After APIC is enabled, the
        // IOAPIC path takes over.
        use i8254_pit::I8254Pit;
        let pit = I8254Pit::new_with_irq_callback(|gsi, level| {
            if level {
                // Route to IOAPIC (for APIC-enabled path)
                if let Some(vioapic) = GLOBAL_VIOAPIC.get() {
                    vioapic.raise_irq(gsi);
                }
                // Route to 8259 Master PIC (for Virtual Wire Mode path)
                if let Some(pic) = i8259_pic::GLOBAL_PIC_MASTER.get() {
                    pic.raise_irq(gsi as u8);
                }
            }
        });
        let pit_arc: Arc<I8254Pit> = Arc::new(pit);
        // Register PIT as the global singleton so the vCPU run loop can
        // advance its counters based on real time.
        i8254_pit::GLOBAL_PIT.call_once(|| pit_arc.clone());
        self.vm.get_devices().lock().add_port_dev(pit_arc);
        info!("Registered i8254 PIT at I/O ports 0x40-0x43 (IRQ0 -> vIOAPIC + 8259 PIC)");

        Ok(())
    }

    #[cfg(feature = "fs")]
    fn load_ramdisk_from_filesystem(&self, ramdisk_path: &str) -> AxResult {
        let load_gpa = self
            .vm
            .with_config(|config| config.image_config.ramdisk.as_ref().map(|r| r.load_gpa))
            .ok_or_else(|| ax_errno::ax_err_type!(NotFound, "Ramdisk load addr is missed"))?;
        let (_, ramdisk_size) = fs::open_image_file(ramdisk_path)?;
        self.vm.with_config(|config| {
            if let Some(ref mut rd) = config.image_config.ramdisk {
                rd.size = Some(ramdisk_size);
            }
        });
        info!(
            "Loading ramdisk image from filesystem {} ({} bytes) into GPA @{:#x}",
            ramdisk_path,
            ramdisk_size,
            load_gpa.as_usize()
        );
        let _ = fs::load_vm_image(ramdisk_path, load_gpa, self.vm.clone())?;
        Ok(())
    }
}

/// Build an E820 memory map table for the guest.
///
/// The E820 table describes which physical address ranges are usable RAM (type 1)
/// and which are reserved (type 2). OVMF reads this via fw_cfg "etc/e820" to
/// determine the guest memory layout and avoid accessing unmapped regions.
///
/// Each entry is 20 bytes: addr (u64 LE), size (u64 LE), type (u32 LE).
#[cfg(target_arch = "x86_64")]
fn build_e820_table(ram_size: usize) -> Vec<u8> {
    const E820_RAM: u32 = 1;
    const E820_RESERVED: u32 = 2;
    const E820_ACPI: u32 = 3;
    const ONE_MB: u64 = 0x10_0000;
    const SIX40_KB: u64 = 0xA_0000;
    const TWO_MB: u64 = 0x20_0000;
    const ACPI_REGION_SIZE: u64 = 0x2_0000; // 128KB for ACPI tables
    const FOUR_GB: u64 = 0x1_0000_0000;

    let ram_size = ram_size as u64;
    let mut data = Vec::new();

    // Entry 0: Low RAM [0, 640KB) — conventional memory
    data.extend_from_slice(&0u64.to_le_bytes());
    data.extend_from_slice(&SIX40_KB.to_le_bytes());
    data.extend_from_slice(&E820_RAM.to_le_bytes());

    // Entry 1: Reserved [640KB, 1MB) — legacy BIOS area, VGA, etc.
    data.extend_from_slice(&SIX40_KB.to_le_bytes());
    data.extend_from_slice(&(ONE_MB - SIX40_KB).to_le_bytes());
    data.extend_from_slice(&E820_RESERVED.to_le_bytes());

    // Entry 2: RAM [1MB, 2MB) — early kernel boot area
    data.extend_from_slice(&ONE_MB.to_le_bytes());
    data.extend_from_slice(&(TWO_MB - ONE_MB).to_le_bytes());
    data.extend_from_slice(&E820_RAM.to_le_bytes());

    // Entry 3: ACPI data [2MB, 2MB+128KB) — ACPI tables (RSDP, XSDT, FADT, MADT, MCFG, DSDT)
    data.extend_from_slice(&TWO_MB.to_le_bytes());
    data.extend_from_slice(&ACPI_REGION_SIZE.to_le_bytes());
    data.extend_from_slice(&E820_ACPI.to_le_bytes());

    // Entry 4: High RAM [2MB+128KB, ram_size)
    let acpi_end = TWO_MB + ACPI_REGION_SIZE;
    data.extend_from_slice(&acpi_end.to_le_bytes());
    data.extend_from_slice(&(ram_size - acpi_end).to_le_bytes());
    data.extend_from_slice(&E820_RAM.to_le_bytes());

    // Entry 5: Reserved [ram_size, 4GB)
    if ram_size < FOUR_GB {
        data.extend_from_slice(&ram_size.to_le_bytes());
        data.extend_from_slice(&(FOUR_GB - ram_size).to_le_bytes());
        data.extend_from_slice(&E820_RESERVED.to_le_bytes());
    }

    data
}

pub fn load_vm_image_from_memory(
    image_buffer: &[u8],
    load_addr: GuestPhysAddr,
    vm: VMRef,
) -> AxResult {
    let mut buffer_pos = 0;

    let image_size = image_buffer.len();

    debug!(
        "loading VM image from memory {:?} {}",
        load_addr,
        image_buffer.len()
    );

    let image_load_regions = vm.get_image_load_region(load_addr, image_size)?;

    for region in image_load_regions {
        let region_len = region.len();
        let bytes_to_write = region_len.min(image_size - buffer_pos);

        // copy data from memory
        unsafe {
            core::ptr::copy_nonoverlapping(
                image_buffer[buffer_pos..].as_ptr(),
                region.as_mut_ptr().cast(),
                bytes_to_write,
            );
        }

        crate::hal::arch::cache::dcache_range(
            CacheOp::Clean,
            (region.as_ptr() as usize).into(),
            region_len,
        );

        // Update the position of the buffer.
        buffer_pos += bytes_to_write;

        // If the buffer is fully written, exit the loop.
        if buffer_pos >= image_size {
            debug!("copy size: {bytes_to_write}");
            break;
        }
    }

    Ok(())
}

#[cfg(feature = "fs")]
pub mod fs {
    use super::*;
    use ax_errno::{AxResult, ax_err, ax_err_type};
    use std::{fs::File, vec::Vec};

    pub fn kernal_read(config: &AxVMCrateConfig, read_size: usize) -> AxResult<Vec<u8>> {
        use std::fs::File;
        use std::io::Read;
        let file_name = &config.kernel.kernel_path;

        let mut file = File::open(file_name).map_err(|err| {
            ax_err_type!(
                NotFound,
                format!(
                    "Failed to open {}, err {:?}, please check your disk.img",
                    file_name, err
                )
            )
        })?;

        let mut buffer = vec![0u8; read_size];

        file.read_exact(&mut buffer).map_err(|err| {
            ax_err_type!(
                NotFound,
                format!(
                    "Failed to read {}, err {:?}, please check your disk.img",
                    file_name, err
                )
            )
        })?;

        Ok(buffer)
    }

    /// Loads the VM image files from the filesystem
    /// into the guest VM's memory space based on the VM configuration.
    pub(crate) fn load_vm_images_from_filesystem(loader: &ImageLoader) -> AxResult {
        info!("Loading VM images from filesystem");
        // Load kernel image.
        let _ = load_vm_image(
            &loader.config.kernel.kernel_path,
            loader.kernel_load_gpa,
            loader.vm.clone(),
        )?;
        // Load BIOS image if needed.
        if loader.config.kernel.enable_bios
            && let Some(bios_path) = &loader.config.kernel.bios_path
        {
            if let Some(bios_load_addr) = loader.bios_load_gpa {
                #[cfg(target_arch = "x86_64")]
                let bios_image = read_image_file(bios_path)?;
                #[cfg(target_arch = "x86_64")]
                {
                    validate_x86_bios_patch_region(&bios_image)?;
                    load_vm_image_from_memory(&bios_image, bios_load_addr, loader.vm.clone())?;
                    loader.load_x86_multiboot_info(&bios_image, bios_load_addr)?;
                }
                #[cfg(not(target_arch = "x86_64"))]
                let _ = load_vm_image(bios_path, bios_load_addr, loader.vm.clone())?;
            } else {
                return ax_err!(NotFound, "BIOS load addr is missed");
            }
        };
        #[cfg(target_arch = "x86_64")]
        if loader.should_load_default_x86_boot_image() {
            let bios_load_gpa = builtin_x86_bios_load_gpa(loader.bios_load_gpa)?;
            info!(
                "Loading built-in x86 boot image at GPA {:#x}",
                bios_load_gpa.as_usize()
            );
            load_vm_image_from_memory(
                x86_boot::DEFAULT_BIOS_IMAGE,
                bios_load_gpa,
                loader.vm.clone(),
            )
            .expect("Failed to load built-in x86 boot image");
            #[cfg(target_arch = "x86_64")]
            loader.load_x86_multiboot_info(x86_boot::DEFAULT_BIOS_IMAGE, bios_load_gpa)?;
        }
        // Load Ramdisk image if needed.
        if let Some(ramdisk_path) = &loader.config.kernel.ramdisk_path {
            loader.load_ramdisk_from_filesystem(ramdisk_path)?;
        };
        // Load DTB image if needed.
        let vm_config = axvm::config::AxVMConfig::from(loader.config.clone());
        if let Some(dtb_arc) = get_vm_dtb_arc(&vm_config) {
            let _dtb_slice: &[u8] = &dtb_arc;
            #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
            crate::vmm::fdt::update_fdt(
                core::ptr::NonNull::new(_dtb_slice.as_ptr() as *mut u8).unwrap(),
                _dtb_slice.len(),
                loader.vm.clone(),
                &loader.config,
            );
            #[cfg(target_arch = "loongarch64")]
            load_vm_image_from_memory(_dtb_slice, loader.dtb_load_gpa.unwrap(), loader.vm.clone())
                .expect("Failed to load DTB images");
        }

        Ok(())
    }

    pub(crate) fn load_vm_image(
        image_path: &str,
        image_load_gpa: GuestPhysAddr,
        vm: VMRef,
    ) -> AxResult<Option<HostVirtAddr>> {
        use std::io::{BufReader, Read};
        let (image_file, image_size) = open_image_file(image_path)?;
        info!(
            "[load_vm_image] Loading {} ({} bytes) to GPA {:#x}",
            image_path, image_size, image_load_gpa
        );

        let mut image_load_regions = vm.get_image_load_region(image_load_gpa, image_size)?;
        info!(
            "[load_vm_image] Got {} region(s) for GPA {:#x}",
            image_load_regions.len(),
            image_load_gpa
        );
        for (i, region) in image_load_regions.iter().enumerate() {
            info!(
                "[load_vm_image] Region {}: HVA {:#x}, size {} bytes",
                i,
                region.as_ptr() as usize,
                region.len()
            );
        }

        let mut file = BufReader::new(image_file);

        for (i, buffer) in image_load_regions.iter_mut().enumerate() {
            file.read_exact(buffer).map_err(|err| {
                ax_err_type!(
                    Io,
                    format!("Failed in reading from file {}, err {:?}", image_path, err)
                )
            })?;

            info!(
                "[load_vm_image] Read {} bytes into region {}, first 16 bytes: {:02x?}",
                buffer.len(),
                i,
                &buffer[..16.min(buffer.len())]
            );

            crate::hal::arch::cache::dcache_range(
                CacheOp::Clean,
                (buffer.as_ptr() as usize).into(),
                buffer.len(),
            );
        }

        let first_hva = if !image_load_regions.is_empty() {
            Some(HostVirtAddr::from(image_load_regions[0].as_ptr() as usize))
        } else {
            None
        };
        Ok(first_hva)
    }

    #[cfg(target_arch = "x86_64")]
    fn read_image_file(image_path: &str) -> AxResult<Vec<u8>> {
        use std::io::{BufReader, Read};
        let (image_file, image_size) = open_image_file(image_path)?;
        let mut image = vec![0; image_size];
        BufReader::new(image_file)
            .read_exact(&mut image)
            .map_err(|err| {
                ax_err_type!(
                    Io,
                    format!("Failed in reading from file {}, err {:?}", image_path, err)
                )
            })?;
        Ok(image)
    }

    pub fn open_image_file(file_name: &str) -> AxResult<(File, usize)> {
        let file = File::open(file_name).map_err(|err| {
            ax_err_type!(
                NotFound,
                format!(
                    "Failed to open {}, err {:?}, please check your disk.img",
                    file_name, err
                )
            )
        })?;
        let file_size = file
            .metadata()
            .map_err(|err| {
                ax_err_type!(
                    Io,
                    format!(
                        "Failed to get metadate of file {}, err {:?}",
                        file_name, err
                    )
                )
            })?
            .size() as usize;
        Ok((file, file_size))
    }

    pub fn read_file_bytes(path: &str) -> AxResult<alloc::vec::Vec<u8>> {
        use std::io::Read;
        let mut file = File::open(path).map_err(|err| {
            ax_err_type!(NotFound, format!("Failed to open {}, err {:?}", path, err))
        })?;
        let mut buffer = alloc::vec::Vec::new();
        file.read_to_end(&mut buffer)
            .map_err(|err| ax_err_type!(Io, format!("Failed to read {}, err {:?}", path, err)))?;
        Ok(buffer)
    }

    pub fn file_exists(path: &str) -> bool {
        std::fs::metadata(path).is_ok()
    }
}

#[cfg(target_arch = "x86_64")]
fn builtin_x86_bios_load_gpa(configured_gpa: Option<GuestPhysAddr>) -> AxResult<GuestPhysAddr> {
    let default_gpa = GuestPhysAddr::from(x86_boot::DEFAULT_BIOS_LOAD_GPA);
    match configured_gpa {
        Some(gpa) if gpa != default_gpa => Err(ax_errno::ax_err_type!(
            InvalidInput,
            format!(
                "built-in x86 BIOS must be loaded at GPA {:#x}, but bios_load_addr is {:#x}; set bios_path to use a relocatable external BIOS image",
                default_gpa.as_usize(),
                gpa.as_usize()
            )
        )),
        Some(gpa) => Ok(gpa),
        None => Ok(default_gpa),
    }
}

#[cfg(target_arch = "x86_64")]
fn validate_x86_bios_patch_region(bios_image: &[u8]) -> AxResult {
    let patch_end = x86_boot::AXVM_BIOS_EBX_IMM_OFFSET + core::mem::size_of::<u32>();
    if bios_image.len() < patch_end {
        return Err(ax_errno::ax_err_type!(
            InvalidInput,
            format!(
                "x86 BIOS image is too small for multiboot info patch: size {}, need at least {} bytes for EBX immediate at offset {:#x}",
                bios_image.len(),
                patch_end,
                x86_boot::AXVM_BIOS_EBX_IMM_OFFSET
            )
        ));
    }

    if bios_image[x86_boot::AXVM_BIOS_EBX_IMM_OFFSET - 1] != x86_boot::MOV_EBX_IMM32_OPCODE {
        return Err(ax_errno::ax_err_type!(
            InvalidInput,
            format!(
                "x86 BIOS image does not match axvm-bios layout: expected mov ebx, imm32 opcode at offset {:#x}",
                x86_boot::AXVM_BIOS_EBX_IMM_OFFSET - 1
            )
        ));
    }

    Ok(())
}

#[cfg(target_arch = "x86_64")]
fn write_u32(buffer: &mut [u8], offset: usize, value: u32) {
    buffer[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(target_arch = "x86_64")]
fn write_u64(buffer: &mut [u8], offset: usize, value: u64) {
    buffer[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;

    #[test]
    fn built_in_x86_bios_uses_default_gpa_when_unspecified() {
        assert_eq!(
            builtin_x86_bios_load_gpa(None).unwrap(),
            GuestPhysAddr::from(x86_boot::DEFAULT_BIOS_LOAD_GPA)
        );
    }

    #[test]
    fn built_in_x86_bios_accepts_explicit_default_gpa() {
        let default_gpa = GuestPhysAddr::from(x86_boot::DEFAULT_BIOS_LOAD_GPA);

        assert_eq!(
            builtin_x86_bios_load_gpa(Some(default_gpa)).unwrap(),
            default_gpa
        );
    }

    #[test]
    fn built_in_x86_bios_rejects_non_default_gpa() {
        let invalid_gpa = GuestPhysAddr::from(x86_boot::DEFAULT_BIOS_LOAD_GPA + 0x1000);

        assert!(builtin_x86_bios_load_gpa(Some(invalid_gpa)).is_err());
    }
}
