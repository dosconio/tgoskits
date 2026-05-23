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

use ax_errno::AxResult;
use axaddrspace::{GuestPhysAddr, HostVirtAddr};

use axvm::VMMemoryRegion;
use axvm::config::AxVMCrateConfig;
use axvm::config::VMBootMode;
use byte_unit::Byte;

use crate::hal::CacheOp;
use crate::vmm::VMRef;
use crate::vmm::config::{config, get_vm_dtb_arc};

#[cfg(target_arch = "x86_64")]
use alloc::sync::Arc;

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
                info!(
                    "[pflash0] Loading {} (file {} bytes, region {:#x}) at offset {:#x}, GPA {:#x}",
                    pflash0_path.path,
                    file_size,
                    region_size,
                    load_offset,
                    load_gpa.as_usize()
                );
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

        // Load kernel image (optional in UEFI mode — OVMF can load from disk)
        if !self.config.kernel.kernel_path.is_empty() {
            match self.config.kernel.image_location.as_deref() {
                Some("memory") => {
                    let vm_imags = config::get_memory_images()
                        .iter()
                        .find(|&v| v.id == self.config.base.id)
                        .expect("VM images is missed");
                    if !vm_imags.kernel.is_empty() {
                        load_vm_image_from_memory(
                            vm_imags.kernel,
                            self.kernel_load_gpa,
                            self.vm.clone(),
                        )?;
                    }
                }
                #[cfg(feature = "fs")]
                Some("fs") => {
                    let _ = fs::load_vm_image(
                        &self.config.kernel.kernel_path,
                        self.kernel_load_gpa,
                        self.vm.clone(),
                    )?;
                }
                _ => {}
            }
        }

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

        // Calculate RAM size from memory_regions
        let ram_size: usize = self
            .config
            .kernel
            .memory_regions
            .iter()
            .filter(|r| r.gpa == 0) // Only count RAM regions starting at 0
            .map(|r| r.size)
            .sum();
        let cpu_num = self.config.base.cpu_num;

        info!(
            "Setting up fw_cfg and ACPI tables: ram_size={:#x}, cpu_num={}",
            ram_size, cpu_num
        );

        // Generate ACPI tables
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
            rsdp_gpa: 0x000F_0000,
        };
        let acpi_tables = AcpiTableBuilder::new(acpi_config).build();

        // Write ACPI tables into guest memory
        // RSDP at 0xF0000
        let rsdp_gpa = GuestPhysAddr::from(0xF0000usize);
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
        let tables_gpa = GuestPhysAddr::from(0xF0000usize + acpi_tables.rsdp.len());
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

        // Create fw_cfg device
        let fw_cfg = FwCfgDevice::new(ram_size, cpu_num);

        // Register ACPI tables as fw_cfg file items
        fw_cfg.add_file("etc/acpi/tables", &acpi_tables.tables);
        fw_cfg.add_file("etc/acpi/rsdp", &acpi_tables.rsdp);

        // Register kernel command line if provided
        if let Some(cmdline) = &self.config.kernel.cmdline {
            fw_cfg.add_file("etc/boot-cmdline", cmdline.as_bytes());
        }

        // Register fw_cfg as a port I/O device
        self.vm.get_devices().lock().add_port_dev(Arc::new(fw_cfg));
        info!("Registered fw_cfg device at I/O ports 0x510-0x511");

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
