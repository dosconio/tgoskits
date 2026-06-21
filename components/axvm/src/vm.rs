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

use alloc::{boxed::Box, format, sync::Arc, vec::Vec};
use core::{alloc::Layout, fmt};

use ax_cpumask::CpuMask;
use ax_errno::{AxError, AxResult, ax_err, ax_err_type};
use ax_memory_addr::{PhysAddr, align_down_4k, align_up_4k};
use axaddrspace::{
    AddrSpace, GuestPhysAddr, HostPhysAddr, HostVirtAddr, MappingFlags, device::AccessWidth,
};
use axdevice::{AxVmDeviceConfig, AxVmDevices};
use axvcpu::{AxVCpu, AxVCpuExitReason};
use axvisor_api::vmm::InterruptVector;
use spin::{Mutex, Once};

#[cfg(not(target_arch = "x86_64"))]
use crate::vcpu::AxVCpuCreateConfig;
#[cfg(target_arch = "aarch64")]
use crate::vcpu::get_sysreg_device;
use crate::{
    config::{AxVMConfig, PhysCpuList},
    hal::PagingHandlerImpl,
    has_hardware_support,
    vcpu::AxArchVCpuImpl,
};

const VM_ASPACE_BASE: usize = 0x0;
const VM_ASPACE_SIZE: usize = 0x7fff_ffff_f000;

/// A vCPU with architecture-independent interface.
type VCpu = AxVCpu<AxArchVCpuImpl>;
/// A reference to a vCPU.
pub type AxVCpuRef = Arc<VCpu>;
/// A reference to a VM.
pub type AxVMRef = Arc<AxVM>;

struct AxVMInnerConst {
    phys_cpu_ls: PhysCpuList,
    vcpu_list: Box<[AxVCpuRef]>,
}

unsafe impl Send for AxVMInnerConst {}
unsafe impl Sync for AxVMInnerConst {}

/// Represents a memory region in a virtual machine.
#[derive(Debug, Clone)]
pub struct VMMemoryRegion {
    /// Guest physical address.
    pub gpa: GuestPhysAddr,
    /// Host virtual address.
    pub hva: HostVirtAddr,
    /// Memory layout of the region.
    pub layout: Layout,
    /// Whether this region was allocated by the allocator and needs to be deallocated
    pub needs_dealloc: bool,
}

impl VMMemoryRegion {
    /// Returns the size of the memory region.
    pub fn size(&self) -> usize {
        self.layout.size()
    }

    /// Returns the host physical address backing this guest memory region.
    pub fn host_paddr(&self) -> HostPhysAddr {
        axvisor_api::memory::virt_to_phys(self.hva)
    }

    /// Returns `true` if the guest physical address is identical to the host physical address.
    pub fn is_identical(&self) -> bool {
        self.gpa.as_usize() == self.host_paddr().as_usize()
    }
}

struct AxVMInnerMut {
    // Todo: use more efficient lock.
    address_space: AddrSpace<PagingHandlerImpl>,
    memory_regions: Vec<VMMemoryRegion>,
    config: AxVMConfig,
    vm_status: VMStatus,
}

/// VM status enumeration representing the lifecycle states of a virtual machine
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VMStatus {
    /// VM is being created/loaded
    Loading,
    /// VM is loaded but not yet started
    Loaded,
    /// VM is currently running
    Running,
    /// VM is suspended (paused but can be resumed)
    Suspended,
    /// VM is in the process of shutting down
    Stopping,
    /// VM is stopped
    Stopped,
}

impl VMStatus {
    /// Get status as a string (lowercase)
    pub fn as_str(&self) -> &'static str {
        match self {
            VMStatus::Loading => "loading",
            VMStatus::Loaded => "loaded",
            VMStatus::Running => "running",
            VMStatus::Suspended => "suspended",
            VMStatus::Stopping => "stopping",
            VMStatus::Stopped => "stopped",
        }
    }

    /// Get status with emoji icon
    pub fn as_str_with_icon(&self) -> &'static str {
        match self {
            VMStatus::Loading => "🔄 loading",
            VMStatus::Loaded => "📦 loaded",
            VMStatus::Running => "🚀 running",
            VMStatus::Suspended => "🛑 suspended",
            VMStatus::Stopping => "⏹️ stopping",
            VMStatus::Stopped => "💤 stopped",
        }
    }
}

impl fmt::Display for VMStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

const TEMP_MAX_VCPU_NUM: usize = 64;

/// A Virtual Machine.
pub struct AxVM {
    id: usize,
    inner_const: Once<AxVMInnerConst>,
    inner_mut: Mutex<AxVMInnerMut>,
    devices: Mutex<AxVmDevices>,
}

impl AxVM {
    /// Creates a new VM with the given configuration.
    /// Returns an error if the configuration is invalid.
    /// The VM is not started until `boot` is called.
    pub fn new(config: AxVMConfig) -> AxResult<AxVMRef> {
        let address_space = AddrSpace::new_empty(
            crate::vcpu::max_guest_page_table_levels(),
            GuestPhysAddr::from(VM_ASPACE_BASE),
            VM_ASPACE_SIZE,
        )?;

        let result = Arc::new(Self {
            id: config.id(),
            inner_const: Once::new(),
            inner_mut: Mutex::new(AxVMInnerMut {
                address_space,
                config,
                memory_regions: Vec::new(),
                vm_status: VMStatus::Loading,
            }),
            devices: Mutex::new(axdevice::AxVmDevices::new(AxVmDeviceConfig {
                emu_configs: Vec::new(),
            })),
        });

        info!("VM created: id={}", result.id());

        Ok(result)
    }

    /// Returns the VM id.
    #[inline]
    pub fn id(&self) -> usize {
        self.id
    }

    /// Sets up the VM before booting.
    pub fn init(&self) -> AxResult {
        let mut inner_mut = self.inner_mut.lock();

        let dtb_addr = inner_mut.config.image_config().dtb_load_gpa;
        let vcpu_id_pcpu_sets = inner_mut.config.phys_cpu_ls.get_vcpu_affinities_pcpu_ids();

        info!("dtb_load_gpa: {dtb_addr:?}");
        debug!("id: {}, VCpuIdPCpuSets: {vcpu_id_pcpu_sets:#x?}", self.id());

        let mut vcpu_list = Vec::with_capacity(vcpu_id_pcpu_sets.len());
        for (vcpu_id, phys_cpu_set, _pcpu_id) in vcpu_id_pcpu_sets {
            #[cfg(target_arch = "aarch64")]
            let arch_config = AxVCpuCreateConfig {
                mpidr_el1: _pcpu_id as _,
                dtb_addr: dtb_addr.unwrap_or_default().as_usize(),
            };
            #[cfg(target_arch = "riscv64")]
            let arch_config = AxVCpuCreateConfig {
                hart_id: vcpu_id as _,
                dtb_addr: dtb_addr.unwrap_or_default().as_usize(),
            };
            #[cfg(target_arch = "loongarch64")]
            let arch_config = AxVCpuCreateConfig {
                cpu_id: vcpu_id,
                dtb_addr: dtb_addr.unwrap_or_default().as_usize(),
            };

            // FIXME: VCpu is neither `Send` nor `Sync` by design, check whether
            // 1. we should make it `Send` and `Sync`, or
            // 2. we can guarantee that no cross-thread access is performed
            #[allow(clippy::arc_with_non_send_sync)]
            vcpu_list.push(Arc::new(VCpu::new(
                self.id(),
                vcpu_id,
                0, // Currently not used.
                phys_cpu_set,
                #[cfg(target_arch = "aarch64")]
                arch_config,
                #[cfg(target_arch = "loongarch64")]
                arch_config,
                #[cfg(target_arch = "riscv64")]
                arch_config,
                #[cfg(target_arch = "x86_64")]
                (),
            )?));
        }

        let mut pt_dev_region = Vec::new();
        for pt_device in inner_mut.config.pass_through_devices() {
            trace!(
                "PT dev {:?} region: [{:#x}~{:#x}] -> [{:#x}~{:#x}]",
                pt_device.name,
                pt_device.base_gpa,
                pt_device.base_gpa + pt_device.length,
                pt_device.base_hpa,
                pt_device.base_hpa + pt_device.length
            );
            // Align the base address and length to 4K boundaries.
            pt_dev_region.push((
                align_down_4k(pt_device.base_gpa),
                align_up_4k(pt_device.length),
            ));
        }

        for pt_addr in inner_mut.config.pass_through_addresses() {
            debug!(
                "PT addr region: [{:#x}~{:#x}]",
                pt_addr.base_gpa,
                pt_addr.base_gpa + pt_addr.length,
            );
            // Align the base address and length to 4K boundaries.
            pt_dev_region.push((align_down_4k(pt_addr.base_gpa), align_up_4k(pt_addr.length)));
        }

        pt_dev_region.sort_by_key(|(gpa, _)| *gpa);

        // Merge overlapping regions.
        let pt_dev_region =
            pt_dev_region
                .into_iter()
                .fold(Vec::<(usize, usize)>::new(), |mut acc, (gpa, len)| {
                    if let Some(last) = acc.last_mut() {
                        if last.0 + last.1 >= gpa {
                            // Merge with the last region.
                            last.1 = (last.0 + last.1).max(gpa + len) - last.0;
                        } else {
                            acc.push((gpa, len));
                        }
                    } else {
                        acc.push((gpa, len));
                    }
                    acc
                });

        for (gpa, len) in &pt_dev_region {
            inner_mut.address_space.map_linear(
                GuestPhysAddr::from(*gpa),
                HostPhysAddr::from(*gpa),
                *len,
                MappingFlags::DEVICE
                    | MappingFlags::READ
                    | MappingFlags::WRITE
                    | MappingFlags::USER,
            )?;
        }

        #[cfg_attr(not(target_arch = "aarch64"), expect(unused_mut))]
        let mut devices = axdevice::AxVmDevices::new(AxVmDeviceConfig {
            emu_configs: inner_mut.config.emu_devices().to_vec(),
        });

        #[cfg(target_arch = "aarch64")]
        {
            let passthrough =
                inner_mut.config.interrupt_mode() == axvmconfig::VMInterruptMode::Passthrough;
            if passthrough {
                let spis = inner_mut.config.pass_through_spis();
                let cpu_id = self.id() - 1; // FIXME: get the real CPU id.
                let mut gicd_found = false;

                for device in devices.iter_mmio_dev() {
                    if let Some(result) = axdevice_base::map_device_of_type(
                        device,
                        |gicd: &arm_vgic::v3::vgicd::VGicD| {
                            debug!("VGicD found, assigning SPIs...");

                            for spi in spis {
                                gicd.assign_irq(*spi + 32, cpu_id, (0, 0, 0, cpu_id as _))
                            }

                            AxResult::Ok(())
                        },
                    ) {
                        result?;
                        gicd_found = true;
                        break;
                    }
                }

                if !gicd_found {
                    warn!("Failed to assign SPIs: No VGicD found in device list");
                }
            } else {
                // non-passthrough mode, we need to set up the virtual timer.
                //
                // FIXME: maybe let `axdevice` handle this automatically?
                // how to let `axdevice` know whether the VM is in passthrough mode or not?
                for dev in get_sysreg_device() {
                    devices.add_sys_reg_dev(dev);
                }
            }
        }

        self.inner_const.call_once(|| AxVMInnerConst {
            phys_cpu_ls: inner_mut.config.phys_cpu_ls.clone(),
            vcpu_list: vcpu_list.into_boxed_slice(),
        });

        // Merge configured devices into self.devices (preserving any devices
        // that were added earlier, e.g. fw_cfg during UEFI image loading).
        self.devices.lock().merge(devices);

        // Setup VCpus.
        for vcpu in self.vcpu_list() {
            #[cfg(target_arch = "aarch64")]
            let setup_config = {
                let passthrough =
                    inner_mut.config.interrupt_mode() == axvmconfig::VMInterruptMode::Passthrough;
                crate::vcpu::AxVCpuSetupConfig {
                    passthrough_interrupt: passthrough,
                    passthrough_timer: passthrough,
                }
            };
            #[cfg(target_arch = "x86_64")]
            let setup_config = {
                use axvmconfig::VMBootMode;
                let boot_mode = match inner_mut.config.boot_mode() {
                    VMBootMode::Trampoline => x86_vcpu::X86BootMode::Trampoline,
                    VMBootMode::Uefi => x86_vcpu::X86BootMode::Uefi,
                };
                let ram_size = inner_mut
                    .memory_regions
                    .iter()
                    .filter(|r| r.gpa.as_usize() < 0xFF00_0000)
                    .map(|r| r.size())
                    .sum::<usize>();
                crate::vcpu::AxVCpuSetupConfig {
                    boot_mode,
                    ram_size,
                }
            };
            #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
            #[allow(clippy::let_unit_value)]
            let setup_config = <AxArchVCpuImpl as axvcpu::AxArchVCpu>::SetupConfig::default();

            let entry = if vcpu.id() == 0 {
                inner_mut.config.bsp_entry()
            } else {
                inner_mut.config.ap_entry()
            };

            debug!("Setting up vCPU[{}] entry at {:#x}", vcpu.id(), entry);

            vcpu.setup(
                entry,
                inner_mut.address_space.page_table_root(),
                setup_config,
            )?;

            // I/O bitmap is set to intercept_all() in VmxVcpu::new(), so all
            // port I/O accesses cause VM exits. No per-device setup needed.
            // This is required because virtio-blk-pci uses a dynamic I/O BAR
            // address assigned by OVMF at runtime.
        }
        info!("VM setup: id={}", self.id());
        Ok(())
    }

    /// Sets the VM status.
    pub fn set_vm_status(&self, status: VMStatus) {
        let mut inner_mut = self.inner_mut.lock();
        inner_mut.vm_status = status;
    }

    /// Returns the current VM status.
    pub fn vm_status(&self) -> VMStatus {
        let inner_mut = self.inner_mut.lock();
        inner_mut.vm_status
    }

    /// Retrieves the vCPU corresponding to the given vcpu_id for the VM.
    /// Returns None if the vCPU does not exist.
    #[inline]
    pub fn vcpu(&self, vcpu_id: usize) -> Option<AxVCpuRef> {
        self.vcpu_list().get(vcpu_id).cloned()
    }

    /// Returns the number of vCPUs corresponding to the VM.
    #[inline]
    pub fn vcpu_num(&self) -> usize {
        self.inner_const().vcpu_list.len()
    }

    fn inner_const(&self) -> &AxVMInnerConst {
        self.inner_const
            .get()
            .expect("VM inner_const not initialized")
    }

    /// Returns a reference to the list of vCPUs corresponding to the VM.
    #[inline]
    pub fn vcpu_list(&self) -> &[AxVCpuRef] {
        &self.inner_const().vcpu_list
    }

    /// Returns the base address of the two-stage address translation page table for the VM.
    pub fn ept_root(&self) -> HostPhysAddr {
        self.inner_mut.lock().address_space.page_table_root()
    }

    /// Returns to the VM's configuration.
    pub fn with_config<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut AxVMConfig) -> R,
    {
        let mut g = self.inner_mut.lock();
        f(&mut g.config)
    }

    /// Returns guest VM image load region in `Vec<&'static mut [u8]>`,
    /// according to the given `image_load_gpa` and `image_size.
    /// `Vec<&'static mut [u8]>` is a series of (HVA) address segments,
    /// which may correspond to non-contiguous physical addresses,
    ///
    /// FIXME:
    /// Find a more elegant way to manage potentially non-contiguous physical memory
    ///         instead of `Vec<&'static mut [u8]>`.
    pub fn get_image_load_region(
        &self,
        image_load_gpa: GuestPhysAddr,
        image_size: usize,
    ) -> AxResult<Vec<&'static mut [u8]>> {
        let g = self.inner_mut.lock();
        let image_load_hva = match g
            .address_space
            .translated_byte_buffer(image_load_gpa, image_size)
        {
            Some(v) => v,
            None => {
                warn!(
                    "[get_image_load_region] GPA {:#x} size {:#x} not translatable",
                    image_load_gpa, image_size
                );
                return ax_err!(BadState, "guest address not in address space");
            }
        };
        debug!(
            "[get_image_load_region] GPA {:#x} -> {} regions, first HVA {:#x}",
            image_load_gpa,
            image_load_hva.len(),
            if !image_load_hva.is_empty() {
                image_load_hva[0].as_ptr() as usize
            } else {
                0
            }
        );
        Ok(image_load_hva)
    }

    /// Boots the VM by transitioning to Running state.
    pub fn boot(&self) -> AxResult {
        if !has_hardware_support() {
            ax_err!(Unsupported, "Hardware does not support virtualization")
        } else if self.running() {
            ax_err!(BadState, format!("VM[{}] is already running", self.id()))
        } else {
            info!("Booting VM[{}]", self.id());
            self.set_vm_status(VMStatus::Running);
            Ok(())
        }
    }

    /// Returns if the VM is running.
    pub fn running(&self) -> bool {
        self.vm_status() == VMStatus::Running
    }

    /// Returns if the VM is shutting down (in Stopping state).
    pub fn stopping(&self) -> bool {
        self.vm_status() == VMStatus::Stopping
    }

    /// Returns if the VM is suspended.
    pub fn suspending(&self) -> bool {
        self.vm_status() == VMStatus::Suspended
    }

    /// Returns if the VM is stopped.
    pub fn stopped(&self) -> bool {
        self.vm_status() == VMStatus::Stopped
    }

    /// Shuts down the VM by transitioning to Stopping state.
    ///
    /// This method sets the VM status to Stopping, which signals all vCPUs to exit.
    /// Currently, the "re-init" process of the VM is not implemented. Therefore, a VM can only be
    /// booted once. And after the VM is shut down, it cannot be booted again.
    pub fn shutdown(&self) -> AxResult {
        if self.stopping() {
            ax_err!(BadState, format!("VM[{}] is already stopping", self.id()))
        } else if self.stopped() {
            ax_err!(BadState, format!("VM[{}] is already stopped", self.id()))
        } else {
            info!("Shutting down VM[{}]", self.id());
            self.set_vm_status(VMStatus::Stopping);
            Ok(())
        }
    }

    // TODO: implement suspend/resume.
    // TODO: implement re-init.

    /// Returns this VM's emulated devices.
    pub fn get_devices(&self) -> &Mutex<AxVmDevices> {
        &self.devices
    }

    /// Run a vCPU according to the given vcpu_id.
    ///
    /// ## Arguments
    /// * `vcpu_id` - the id of the vCPU to run.
    ///
    /// ## Returns
    /// * `AxVCpuExitReason` - the exit reason of the vCPU, wrapped in an `AxResult`.
    pub fn run_vcpu(&self, vcpu_id: usize) -> AxResult<AxVCpuExitReason> {
        let vcpu = self
            .vcpu(vcpu_id)
            .ok_or_else(|| ax_err_type!(InvalidInput, "Invalid vcpu_id"))?;

        vcpu.bind()?;

        let mut diag_ept_count: u64 = 0;
        let mut diag_mmio_count: u64 = 0;
        // diag_io_count disabled together with DIAG IO read/write logging.
        // let mut diag_io_count: u64 = 0;

        let exit_reason = loop {
            let exit_reason = vcpu.run()?;
            trace!("{exit_reason:#x?}");

            // Diagnostic: log total exit count periodically to detect tight
            // loops where the guest is stuck.  We cannot read the guest RIP
            // here because `rip()` is not part of the `AxArchVCpu` trait, so
            // we rely on the exit count and exit reason instead.
            static TOTAL_EXIT_COUNT: core::sync::atomic::AtomicU64 =
                core::sync::atomic::AtomicU64::new(0);
            let total = TOTAL_EXIT_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if total.is_multiple_of(1_000_000) {
                info!("[DIAG] Total VM exits: {total}, current exit: {exit_reason:?}");
            }

            let handled = match &exit_reason {
                AxVCpuExitReason::MmioRead {
                    addr,
                    width,
                    reg,
                    reg_width: _,
                    signed_ext: _,
                } => {
                    diag_mmio_count += 1;
                    if diag_mmio_count <= 10 || diag_mmio_count.is_multiple_of(100000) {
                        info!("[DIAG] MMIO read: addr={:#x}, width={:?}", addr, width);
                    }
                    let val = self.get_devices().lock().handle_mmio_read(*addr, *width)?;
                    vcpu.set_gpr(*reg, val);
                    true
                }
                AxVCpuExitReason::MmioWrite { addr, width, data } => {
                    diag_mmio_count += 1;
                    if diag_mmio_count <= 10 || diag_mmio_count.is_multiple_of(100000) {
                        info!(
                            "[DIAG] MMIO write: addr={:#x}, width={:?}, data={:#x}",
                            addr, width, data
                        );
                    }
                    self.get_devices()
                        .lock()
                        .handle_mmio_write(*addr, *width, *data as usize)?;
                    true
                }
                AxVCpuExitReason::IoRead { port, width } => {
                    // diag_io_count += 1;  // disabled with DIAG IO logging
                    let val = self.get_devices().lock().handle_port_read(*port, *width)?;
                    // Disabled verbose IO read logging — OVMF polls some ports
                    // (e.g. port 0x6) in tight loops, producing excessive output.
                    // if diag_io_count <= 200 || diag_io_count.is_multiple_of(10000) {
                    //     info!(
                    //         "[DIAG] IO read #{diag_io_count}: port={:#x}, width={:?}, val={:#x}",
                    //         port.0, width, val
                    //     );
                    // }
                    #[cfg(not(target_arch = "riscv64"))]
                    vcpu.set_gpr(0, val);

                    #[cfg(target_arch = "riscv64")]
                    vcpu.set_gpr(riscv_vcpu::GprIndex::A0 as usize, val);

                    true
                }
                AxVCpuExitReason::IoWrite { port, width, data } => {
                    // diag_io_count += 1;  // disabled with DIAG IO logging
                    // Disabled verbose IO write logging — see IoRead comment above.
                    // if diag_io_count <= 200 || diag_io_count.is_multiple_of(10000) {
                    //     info!(
                    //         "[DIAG] IO write #{diag_io_count}: port={:#x}, width={:?}, data={:#x}",
                    //         port.0, width, data
                    //     );
                    // }
                    self.get_devices()
                        .lock()
                        .handle_port_write(*port, *width, *data as usize)?;
                    true
                }
                AxVCpuExitReason::IoStringIn {
                    port,
                    width,
                    count,
                    guest_addr,
                    dir_down,
                } => {
                    let width_bytes = match width {
                        AccessWidth::Byte => 1usize,
                        AccessWidth::Word => 2,
                        AccessWidth::Dword => 4,
                        AccessWidth::Qword => 8,
                    };
                    let total_bytes = *count as usize * width_bytes;
                    // Translate the entire buffer range once
                    let base_gpa = GuestPhysAddr::from(*guest_addr as usize);
                    if let Ok(slice) = self.get_image_load_region(base_gpa, total_bytes)
                        && !slice.is_empty()
                    {
                        let base_ptr = slice[0].as_ptr() as *mut u8;
                        let step = if *dir_down {
                            -(width_bytes as isize)
                        } else {
                            width_bytes as isize
                        };
                        let mut offset: isize = 0;
                        for _ in 0..*count {
                            let val = self.get_devices().lock().handle_port_read(*port, *width)?;
                            let dst = unsafe { base_ptr.offset(offset) };
                            match width {
                                AccessWidth::Byte => unsafe { dst.write_volatile(val as u8) },
                                AccessWidth::Word => unsafe {
                                    (dst as *mut u16).write_volatile(val as u16)
                                },
                                AccessWidth::Dword => unsafe {
                                    (dst as *mut u32).write_volatile(val as u32)
                                },
                                AccessWidth::Qword => unsafe {
                                    (dst as *mut u64).write_volatile(val as u64)
                                },
                            }
                            offset += step;
                        }
                        // Update RDI and RCX: all count processed
                        let new_addr = *guest_addr as i64
                            + if *dir_down {
                                -(*count as i64 * width_bytes as i64)
                            } else {
                                *count as i64 * width_bytes as i64
                            };
                        vcpu.set_gpr(7, new_addr as usize); // RDI
                        vcpu.set_gpr(1, 0); // RCX = 0 (all done)
                    } else {
                        // Failed to translate guest address (e.g. beyond RAM during
                        // memory probing): skip the I/O and advance registers as if
                        // all count was processed so the guest doesn't loop forever.
                        let new_addr = *guest_addr as i64
                            + if *dir_down {
                                -(*count as i64 * width_bytes as i64)
                            } else {
                                *count as i64 * width_bytes as i64
                            };
                        vcpu.set_gpr(7, new_addr as usize); // RDI
                        vcpu.set_gpr(1, 0); // RCX = 0 (all done)
                    }
                    true
                }
                AxVCpuExitReason::IoStringOut {
                    port,
                    width,
                    count,
                    guest_addr,
                    dir_down,
                } => {
                    let width_bytes = match width {
                        AccessWidth::Byte => 1usize,
                        AccessWidth::Word => 2,
                        AccessWidth::Dword => 4,
                        AccessWidth::Qword => 8,
                    };
                    let total_bytes = *count as usize * width_bytes;
                    // Translate the entire buffer range once
                    let base_gpa = GuestPhysAddr::from(*guest_addr as usize);
                    if let Ok(slice) = self.get_image_load_region(base_gpa, total_bytes)
                        && !slice.is_empty()
                    {
                        let base_ptr = slice[0].as_ptr();
                        let step = if *dir_down {
                            -(width_bytes as isize)
                        } else {
                            width_bytes as isize
                        };
                        let mut offset: isize = 0;
                        for _ in 0..*count {
                            let data = match width {
                                AccessWidth::Byte => unsafe {
                                    base_ptr.offset(offset).read_volatile() as usize
                                },
                                AccessWidth::Word => unsafe {
                                    (base_ptr.offset(offset) as *const u16).read_volatile() as usize
                                },
                                AccessWidth::Dword => unsafe {
                                    (base_ptr.offset(offset) as *const u32).read_volatile() as usize
                                },
                                AccessWidth::Qword => unsafe {
                                    (base_ptr.offset(offset) as *const u64).read_volatile() as usize
                                },
                            };
                            self.get_devices()
                                .lock()
                                .handle_port_write(*port, *width, data)?;
                            offset += step;
                        }
                        // Update RSI and RCX: all count processed
                        let new_addr = *guest_addr as i64
                            + if *dir_down {
                                -(*count as i64 * width_bytes as i64)
                            } else {
                                *count as i64 * width_bytes as i64
                            };
                        vcpu.set_gpr(6, new_addr as usize); // RSI
                        vcpu.set_gpr(1, 0); // RCX = 0 (all done)
                    } else {
                        // Failed to translate guest address: skip and advance registers.
                        let new_addr = *guest_addr as i64
                            + if *dir_down {
                                -(*count as i64 * width_bytes as i64)
                            } else {
                                *count as i64 * width_bytes as i64
                            };
                        vcpu.set_gpr(6, new_addr as usize); // RSI
                        vcpu.set_gpr(1, 0); // RCX = 0 (all done)
                    }
                    true
                }
                AxVCpuExitReason::SysRegRead { addr, reg } => {
                    let val = self.get_devices().lock().handle_sys_reg_read(
                        *addr,
                        // Generally speaking, the width of system register is fixed and needless to be specified.
                        // AccessWidth::Qword here is just a placeholder, may be changed in the future.
                        AccessWidth::Qword,
                    )?;
                    vcpu.set_gpr(*reg, val);
                    true
                }
                AxVCpuExitReason::SysRegWrite { addr, value } => {
                    self.get_devices().lock().handle_sys_reg_write(
                        *addr,
                        AccessWidth::Qword,
                        *value as usize,
                    )?;
                    true
                }
                AxVCpuExitReason::NestedPageFault { addr, access_flags } => {
                    diag_ept_count += 1;
                    if diag_ept_count <= 20 || diag_ept_count.is_multiple_of(10000) {
                        info!(
                            "[DIAG] EPT violation #{diag_ept_count}: GPA={:#x}, access={:?}",
                            addr, access_flags
                        );
                    }
                    let handled = self
                        .inner_mut
                        .lock()
                        .address_space
                        .handle_page_fault(*addr, *access_flags);
                    if !handled {
                        info!(
                            "EPT violation UNHANDLED: GPA={:#x}, access={:?}",
                            addr, access_flags
                        );
                    }
                    handled
                }
                _ => false,
            };
            if !handled && !matches!(exit_reason, AxVCpuExitReason::Nothing) {
                break exit_reason;
            }
        };

        vcpu.unbind()?;
        Ok(exit_reason)
    }

    /// Injects an interrupt to the vCPU.
    pub fn inject_interrupt_to_vcpu(
        &self,
        targets: CpuMask<TEMP_MAX_VCPU_NUM>,
        irq: usize,
    ) -> AxResult {
        let vm_id = self.id();
        // Check if the current running vm is self.
        //
        // It is not supported to inject interrupt to a vcpu in another VM yet.
        //
        // It may be supported in the future, as a essential feature for cross-VM communication.
        let current_running_vm = axvisor_api::vmm::current_vm_id();
        if current_running_vm != vm_id {
            panic!("Injecting interrupt to a vcpu in another VM is not supported");
        }

        axvisor_api::vmm::inject_interrupt_to_cpus(vm_id, targets, irq as InterruptVector);

        Ok(())
    }

    /// Returns vCpu id list and its corresponding pCpu affinity list, as well as its physical id.
    /// If the pCpu affinity is None, it means the vCpu will be allocated to any available pCpu randomly.
    /// if the pCPU id is not provided, the vCpu's physical id will be set as vCpu id.
    ///
    /// Returns a vector of tuples, each tuple contains:
    /// - The vCpu id.
    /// - The pCpu affinity mask, `None` if not set.
    /// - The physical id of the vCpu, equal to vCpu id if not provided.
    pub fn get_vcpu_affinities_pcpu_ids(&self) -> Vec<(usize, Option<usize>, usize)> {
        self.inner_const()
            .phys_cpu_ls
            .get_vcpu_affinities_pcpu_ids()
    }

    // /// Returns a reference to the VM's configuration.
    // pub fn config(&self) -> &AxVMConfig {
    //     &self.inner_const.config
    // }

    /// Maps a region of host physical memory to guest physical memory.
    pub fn map_region(
        &self,
        gpa: GuestPhysAddr,
        hpa: HostPhysAddr,
        size: usize,
        flags: MappingFlags,
    ) -> AxResult {
        self.inner_mut
            .lock()
            .address_space
            .map_linear(gpa, hpa, size, flags)?;
        Ok(())
    }

    /// Unmaps a region of guest physical memory.
    pub fn unmap_region(&self, gpa: GuestPhysAddr, size: usize) -> AxResult {
        self.inner_mut.lock().address_space.unmap(gpa, size)?;
        Ok(())
    }

    /// Translate a guest physical address to a host physical address using only
    /// the EPT page table, without requiring the address to be tracked in `areas`.
    ///
    /// This is useful for accessing guest memory that was mapped by EPT violation
    /// handlers (e.g., dummy_ff_page for PCI MMIO probing) which update the EPT
    /// hardware page table but not the software-level `areas` tracking.
    pub fn translate_gpa_pt_only(&self, gpa: GuestPhysAddr) -> Option<PhysAddr> {
        let g = self.inner_mut.lock();
        g.address_space.translate_pt_only(gpa)
    }

    /// Reads an object of type `T` from the guest physical address.
    pub fn read_from_guest_of<T>(&self, gpa_ptr: GuestPhysAddr) -> AxResult<T> {
        let size = core::mem::size_of::<T>();

        // Ensure the address is properly aligned for the type.
        if !gpa_ptr
            .as_usize()
            .is_multiple_of(core::mem::align_of::<T>())
        {
            return ax_err!(InvalidInput, "Unaligned guest physical address");
        }

        let g = self.inner_mut.lock();
        match g.address_space.translated_byte_buffer(gpa_ptr, size) {
            Some(buffers) => {
                let mut data_bytes = Vec::with_capacity(size);
                for chunk in buffers {
                    let remaining = size - data_bytes.len();
                    let chunk_size = remaining.min(chunk.len());
                    data_bytes.extend_from_slice(&chunk[..chunk_size]);
                    if data_bytes.len() >= size {
                        break;
                    }
                }
                if data_bytes.len() < size {
                    return ax_err!(
                        InvalidInput,
                        "Insufficient data in guest memory to read the requested object"
                    );
                }
                let data: T = unsafe {
                    // Use `ptr::read_unaligned` for safety in case of unaligned memory.
                    core::ptr::read_unaligned(data_bytes.as_ptr() as *const T)
                };
                Ok(data)
            }
            None => ax_err!(
                InvalidInput,
                "Failed to translate guest physical address or insufficient buffer size"
            ),
        }
    }

    /// Writes an object of type `T` to the guest physical address.
    pub fn write_to_guest_of<T>(&self, gpa_ptr: GuestPhysAddr, data: &T) -> AxResult {
        match self
            .inner_mut
            .lock()
            .address_space
            .translated_byte_buffer(gpa_ptr, core::mem::size_of::<T>())
        {
            Some(mut buffer) => {
                let bytes = unsafe {
                    core::slice::from_raw_parts(
                        data as *const T as *const u8,
                        core::mem::size_of::<T>(),
                    )
                };
                let mut copied_bytes = 0;
                for chunk in buffer.iter_mut() {
                    let end = copied_bytes + chunk.len();
                    chunk.copy_from_slice(&bytes[copied_bytes..end]);
                    copied_bytes += chunk.len();
                }
                Ok(())
            }
            None => ax_err!(InvalidInput, "Failed to translate guest physical address"),
        }
    }

    /// Allocates an IVC channel for inter-VM communication region.
    ///
    /// ## Arguments
    /// * `expected_size` - The expected size of the IVC channel in bytes.
    /// ## Returns
    /// * `AxResult<(GuestPhysAddr, usize)>` - A tuple containing the guest physical address of the allocated IVC channel and its actual size.
    pub fn alloc_ivc_channel(&self, expected_size: usize) -> AxResult<(GuestPhysAddr, usize)> {
        // Ensure the expected size is aligned to 4K.
        let size = align_up_4k(expected_size);
        let gpa = self.devices.lock().alloc_ivc_channel(size)?;
        Ok((gpa, size))
    }

    /// Releases an IVC channel for inter-VM communication region.
    /// ## Arguments
    /// * `gpa` - The guest physical address of the IVC channel to release.
    /// * `size` - The size of the IVC channel in bytes.
    /// ## Returns
    /// * `AxResult<()>` - An empty result indicating success or failure.
    pub fn release_ivc_channel(&self, gpa: GuestPhysAddr, size: usize) -> AxResult {
        self.devices.lock().release_ivc_channel(gpa, size)?;
        Ok(())
    }

    /// Allocates a new memory region for the VM.
    pub fn alloc_memory_region(
        &self,
        layout: Layout,
        gpa: Option<GuestPhysAddr>,
    ) -> AxResult<&[u8]> {
        assert!(
            layout.size() > 0,
            "Cannot allocate zero-sized memory region"
        );

        let hva = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if hva.is_null() {
            return Err(AxError::NoMemory);
        }
        let s = unsafe { core::slice::from_raw_parts_mut(hva, layout.size()) };
        let hva = HostVirtAddr::from_mut_ptr_of(hva);

        let hpa = axvisor_api::memory::virt_to_phys(hva);

        let gpa = gpa.unwrap_or_else(|| hpa.as_usize().into());

        let mut g = self.inner_mut.lock();
        g.address_space.map_linear(
            gpa,
            hpa,
            layout.size(),
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE | MappingFlags::USER,
        )?;
        g.memory_regions.push(VMMemoryRegion {
            gpa,
            hva,
            layout,
            needs_dealloc: true,
        });

        Ok(s)
    }

    /// Registers a pre-allocated memory region for the VM.
    ///
    /// Unlike [`alloc_memory_region`], this method does not allocate memory.
    /// The caller provides a pointer to already-allocated memory (e.g., from
    /// the page allocator) and is responsible for ensuring the memory remains
    /// valid for the VM's lifetime.
    ///
    /// When `needs_dealloc` is `false`, the region will NOT be freed by
    /// [`cleanup_resources`]. The caller must handle deallocation externally
    /// (e.g., via `dealloc_pages`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that `hva` points to a valid, sufficiently
    /// sized, and properly aligned memory region that remains valid for the
    /// VM's lifetime.
    pub unsafe fn register_memory_region(
        &self,
        hva: *mut u8,
        layout: Layout,
        gpa: Option<GuestPhysAddr>,
        needs_dealloc: bool,
    ) -> AxResult<&[u8]> {
        assert!(
            layout.size() > 0,
            "Cannot register zero-sized memory region"
        );
        assert!(!hva.is_null(), "Cannot register null memory region");

        let s = unsafe { core::slice::from_raw_parts_mut(hva, layout.size()) };
        let hva = HostVirtAddr::from_mut_ptr_of(hva);
        let hpa = axvisor_api::memory::virt_to_phys(hva);
        let gpa = gpa.unwrap_or_else(|| hpa.as_usize().into());

        let mut g = self.inner_mut.lock();
        g.address_space.map_linear(
            gpa,
            hpa,
            layout.size(),
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE | MappingFlags::USER,
        )?;
        g.memory_regions.push(VMMemoryRegion {
            gpa,
            hva,
            layout,
            needs_dealloc,
        });

        Ok(s)
    }

    /// Returns a list of all memory regions in the VM.
    pub fn memory_regions(&self) -> Vec<VMMemoryRegion> {
        self.inner_mut.lock().memory_regions.clone()
    }

    /// Maps a reserved memory region for the VM.
    pub fn map_reserved_memory_region(
        &self,
        layout: Layout,
        gpa: Option<GuestPhysAddr>,
    ) -> AxResult<&[u8]> {
        assert!(
            layout.size() > 0,
            "Cannot allocate zero-sized memory region"
        );

        let hva = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if hva.is_null() {
            return Err(AxError::NoMemory);
        }
        let s = unsafe { core::slice::from_raw_parts_mut(hva, layout.size()) };
        let hva = HostVirtAddr::from_mut_ptr_of(hva);

        let hpa = axvisor_api::memory::virt_to_phys(hva);

        let gpa = gpa.unwrap_or_else(|| hpa.as_usize().into());

        let mut g = self.inner_mut.lock();
        g.address_space.map_linear(
            gpa,
            hpa,
            layout.size(),
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE | MappingFlags::USER,
        )?;
        g.memory_regions.push(VMMemoryRegion {
            gpa,
            hva,
            layout,
            needs_dealloc: true,
        });

        Ok(s)
    }

    /// Cleanup resources for the VM before drop.
    /// This is called internally by the Drop implementation.
    fn cleanup_resources(&self) {
        info!("Cleaning up VM[{}] resources...", self.id());

        // 1. Ensure the VM is in Stopping or Stopped state
        let current_status = self.vm_status();
        if !matches!(current_status, VMStatus::Stopping | VMStatus::Stopped) {
            warn!(
                "VM[{}] is being dropped without explicit shutdown (status: {:?}), marking as \
                 stopping",
                self.id(),
                current_status
            );
            self.set_vm_status(VMStatus::Stopping);
        }

        let mut inner_mut = self.inner_mut.lock();

        // First, collect all memory regions to clean up
        // We need to clone the regions to avoid borrowing issues
        let regions_to_cleanup: Vec<VMMemoryRegion> = inner_mut.memory_regions.clone();

        // Unmap all memory regions from the address space
        // This must be done BEFORE deallocating memory to avoid use-after-free
        for region in &regions_to_cleanup {
            debug!(
                "VM[{}] unmapping memory region: GPA={:#x}, size={:#x}",
                self.id(),
                region.gpa.as_usize(),
                region.size()
            );
            // Unmap the region from guest physical address space
            if let Err(e) = inner_mut.address_space.unmap(region.gpa, region.size()) {
                warn!(
                    "VM[{}] failed to unmap region at GPA={:#x}: {:?}",
                    self.id(),
                    region.gpa.as_usize(),
                    e
                );
            }
        }

        // Now it's safe to deallocate the memory
        for region in &regions_to_cleanup {
            // Only deallocate memory regions that were allocated by the allocator
            if region.needs_dealloc {
                debug!(
                    "VM[{}] deallocating memory region: HVA={:#x}, size={:#x}",
                    self.id(),
                    region.hva.as_usize(),
                    region.size()
                );
                unsafe {
                    alloc::alloc::dealloc(region.hva.as_mut_ptr(), region.layout);
                }
            } else {
                debug!(
                    "VM[{}] skipping dealloc for reserved memory region: GPA={:#x}, HVA={:#x}, \
                     size={:#x}",
                    self.id(),
                    region.gpa.as_usize(),
                    region.hva.as_usize(),
                    region.size()
                );
            }
        }
        inner_mut.memory_regions.clear();

        // Clear remaining address space mappings
        // This includes:
        // - Passthrough device MMIO mappings
        // - Emulated device MMIO mappings
        // - Reserved memory mappings
        // - All other page table entries
        debug!(
            "VM[{}] clearing remaining address space mappings",
            self.id()
        );
        inner_mut.address_space.clear();

        // Release the lock before accessing inner_const
        drop(inner_mut);

        // Device cleanup
        // Although devices will be automatically dropped when inner_const is dropped,
        // we should perform explicit cleanup if devices hold resources like:
        // - Hardware interrupt registrations
        // - DMA mappings
        // - Background threads or timers
        debug!(
            "VM[{}] devices cleanup: {} MMIO devices, {} SysReg devices",
            self.id(),
            self.devices.lock().iter_mmio_dev().count(),
            self.devices.lock().iter_sys_reg_dev().count()
        );

        // TODO: Add device-specific cleanup if needed
        // For example:
        // - Stop device background tasks
        // - Unregister interrupts
        // - Release device-specific resources

        // Note: Device Arc references will be dropped automatically when
        // AxVM is dropped

        info!("VM[{}] resources cleanup completed", self.id());
    }
}

impl Drop for AxVM {
    fn drop(&mut self) {
        info!("Dropping VM[{}]", self.id());

        // Clean up all allocated resources
        self.cleanup_resources();

        info!("VM[{}] dropped", self.id());
    }
}
