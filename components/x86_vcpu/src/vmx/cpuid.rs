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

//! CPUID emulation for the guest. Extracted from `vcpu.rs`.

use ax_errno::AxResult;
use bit_field::BitField;

use super::vcpu::VmxVcpu;

impl VmxVcpu {
    pub(super) fn handle_cpuid(&mut self) -> AxResult {
        use raw_cpuid::{CpuIdResult, cpuid};

        const VM_EXIT_INSTR_LEN_CPUID: u8 = 2;
        const LEAF_FEATURE_INFO: u32 = 0x1;
        const LEAF_CACHE_PARAMETERS: u32 = 0x4;
        const LEAF_STRUCTURED_EXTENDED_FEATURE_FLAGS_ENUMERATION: u32 = 0x7;
        const LEAF_PROCESSOR_EXTENDED_STATE_ENUMERATION: u32 = 0xd;
        const LEAF_TSC_CORE_CRYSTAL_RATIO: u32 = 0x15;
        const EAX_FREQUENCY_INFO: u32 = 0x16;
        const LEAF_X2APIC_TOPOLOGY: u32 = 0xb;
        const LEAF_X2APIC_TOPOLOGY_V2: u32 = 0x1f;
        const LEAF_HYPERVISOR_INFO: u32 = 0x4000_0000;
        const LEAF_HYPERVISOR_FEATURE: u32 = 0x4000_0001;
        const VENDOR_STR: &[u8; 12] = b"RVMRVMRVMRVM";
        let vendor_regs = unsafe { &*(VENDOR_STR.as_ptr() as *const [u32; 3]) };

        let regs_clone = *self.regs_mut();
        let function = regs_clone.rax as u32;
        let res = match function {
            LEAF_FEATURE_INFO => {
                // CPUID leaf 0x1 has NO sub-leaves per Intel SDM.
                // ECX is not a sub-leaf index and is ignored by hardware.
                // Previously, checking rcx >= 4 caused most leaf-1 queries
                // to return all-zeros, breaking OVMF's APIC/feature detection.
                const FEATURE_VMX: u32 = 1 << 5;
                const FEATURE_HYPERVISOR: u32 = 1 << 31;
                const FEATURE_MCE: u32 = 1 << 7;
                const FEATURE_TSC_DEADLINE: u32 = 1 << 24;
                const FEATURE_MONITOR: u32 = 1 << 3;
                let mut res = cpuid!(regs_clone.rax, 0);
                res.ecx &= !FEATURE_VMX;
                res.ecx &= !FEATURE_TSC_DEADLINE;
                res.ecx &= !FEATURE_MONITOR;
                res.ecx &= !FEATURE_HYPERVISOR;
                res.edx &= !FEATURE_MCE;
                res.ebx = 0x0001_0800; // BrandIndex=0, CLFLUSH=64B, MaxLogicalProc=1, APIC ID=0
                res
            }
            LEAF_CACHE_PARAMETERS => {
                let mut res = cpuid!(regs_clone.rax, regs_clone.rcx);
                if regs_clone.rcx > 0 {
                    res.eax = 0;
                }
                res
            }
            // See SDM Table 3-8. Information Returned by CPUID Instruction (Contd.)
            LEAF_STRUCTURED_EXTENDED_FEATURE_FLAGS_ENUMERATION => {
                let mut res = cpuid!(regs_clone.rax, regs_clone.rcx);
                if regs_clone.rcx == 0 {
                    // Bit 05: WAITPKG.
                    res.ecx.set_bit(5, false); // clear waitpkg
                    // Bit 16: LA57. Supports 57-bit linear addresses and five-level paging if 1.
                    res.ecx.set_bit(16, false); // clear LA57
                }

                res
            }
            LEAF_PROCESSOR_EXTENDED_STATE_ENUMERATION => {
                self.load_guest_xstate();
                let res = cpuid!(regs_clone.rax, regs_clone.rcx);
                self.load_host_xstate();

                res
            }
            LEAF_X2APIC_TOPOLOGY => {
                if regs_clone.rcx == 0 {
                    CpuIdResult {
                        eax: 0,
                        ebx: 1,
                        ecx: 0x100,
                        edx: 0,
                    }
                } else {
                    CpuIdResult {
                        eax: 0,
                        ebx: 0,
                        ecx: 0,
                        edx: 0,
                    }
                }
            }
            LEAF_X2APIC_TOPOLOGY_V2 => {
                if regs_clone.rcx == 0 {
                    CpuIdResult {
                        eax: 0,
                        ebx: 1,
                        ecx: 0x100,
                        edx: 0,
                    }
                } else {
                    CpuIdResult {
                        eax: 0,
                        ebx: 0,
                        ecx: 0,
                        edx: 0,
                    }
                }
            }
            LEAF_HYPERVISOR_INFO => CpuIdResult {
                eax: LEAF_HYPERVISOR_FEATURE,
                ebx: vendor_regs[0],
                ecx: vendor_regs[1],
                edx: vendor_regs[2],
            },
            LEAF_HYPERVISOR_FEATURE => CpuIdResult {
                eax: 0,
                ebx: 0,
                ecx: 0,
                edx: 0,
            },
            LEAF_TSC_CORE_CRYSTAL_RATIO => {
                let mut res = cpuid!(regs_clone.rax, regs_clone.rcx);
                // Always log CPUID 0x15 values for diagnostics
                info!(
                    "[CPUID-0x15] Raw: eax={:#x} ebx={:#x} ecx={:#x} edx={:#x}",
                    res.eax, res.ebx, res.ecx, res.edx
                );
                // OVMF's InternalGetApicTimerFrequency() computes:
                //   APIC_freq = ECX * EAX / EBX  (ECX=crystal Hz, EAX=denominator, EBX=numerator)
                // If ECX=0 (crystal frequency unknown) or EAX/EBX=0, the result is 0,
                // which triggers ASSERT(ApicFrequency != 0) in MicroSecondDelay().
                // Provide fallback values when the hardware doesn't report a valid frequency.
                if res.eax == 0 || res.ebx == 0 || res.ecx == 0 {
                    info!("[CPUID-0x15] Values invalid, using fallback (100 MHz crystal)");
                    res.eax = 1;
                    res.ebx = 1;
                    res.ecx = 100_000_000; // 100 MHz crystal
                }
                res
            }
            EAX_FREQUENCY_INFO => {
                const TIMER_FREQUENCY_MHZ: u32 = 3_000;
                let mut res = cpuid!(regs_clone.rax, regs_clone.rcx);
                if res.eax == 0 {
                    warn!(
                        "handle_cpuid: Failed to get TSC frequency by CPUID, default to \
                         {TIMER_FREQUENCY_MHZ} MHz"
                    );
                    res.eax = TIMER_FREQUENCY_MHZ;
                }
                res
            }
            0x8000_0000 => {
                let mut res = cpuid!(regs_clone.rax, regs_clone.rcx);
                const MAX_EXT_LEAF: u32 = 0x8000_0008;
                if res.eax > MAX_EXT_LEAF {
                    res.eax = MAX_EXT_LEAF;
                }
                // Dump 96 bytes from RIP-8 to see code after CPUID 0x80000000
                if let Some(ept_root) = self.ept_root {
                    let rip = self.rip() as u64;
                    let base_gpa = (rip - 8) & !0x7u64;
                    let mut dump = [0u8; 96];
                    for (i, byte) in dump.iter_mut().enumerate() {
                        let gpa = base_gpa + i as u64;
                        if let Some(hpa) = self.gpa_to_hpa_via_ept(ept_root, gpa) {
                            const PHYS_VIRT_OFFSET: u64 = 0xffff_8000_0000_0000;
                            *byte = unsafe {
                                core::ptr::read_volatile(
                                    (hpa as u64 + PHYS_VIRT_OFFSET) as *const u8,
                                )
                            };
                        }
                    }
                    debug!("[8k] e={:x} rip={:x} {:02x?}", res.eax, rip, &dump[..96]);
                } else {
                    debug!("[8k] e={:x}", res.eax);
                }
                res
            }
            0x8000_0001 => {
                let mut res = cpuid!(regs_clone.rax, regs_clone.rcx);
                res.ecx &= !(1 << 2);
                info!(
                    "[C8k1] a={:#x} b={:#x} c={:#x} d={:#x} LM={}",
                    res.eax,
                    res.ebx,
                    res.ecx,
                    res.edx,
                    (res.edx >> 29) & 1
                );
                res
            }
            0x8000_0008 => {
                let mut res = cpuid!(regs_clone.rax, regs_clone.rcx);
                res.ecx = 0;
                res
            }
            _ => cpuid!(regs_clone.rax, regs_clone.rcx),
        };

        trace!(
            "VM exit: CPUID({:#x}, {:#x}): {:?}",
            regs_clone.rax, regs_clone.rcx, res
        );

        {
            static CPUID_OUT_COUNT: core::sync::atomic::AtomicU64 =
                core::sync::atomic::AtomicU64::new(0);
            let out_count = CPUID_OUT_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            // Log ALL CPUID calls at info level for the first 500 calls to diagnose
            // why OVMF never calls leaf 0x80000001 after 0x80000000.
            if out_count < 500 {
                let rip = self.rip() as u64;
                match function {
                    0x80000000 => {
                        // Dump 32 bytes from RIP-8 to see code around CPUID 0x80000000
                        // Only dump first 3 occurrences to avoid log truncation
                        if out_count < 3 {
                            if let Some(ept_root) = self.ept_root {
                                let base_gpa = (rip - 8) & !0x7u64;
                                let mut dump = [0u8; 32];
                                for (i, byte) in dump.iter_mut().enumerate() {
                                    let gpa = base_gpa + i as u64;
                                    if let Some(hpa) = self.gpa_to_hpa_via_ept(ept_root, gpa) {
                                        const PHYS_VIRT_OFFSET: u64 = 0xffff_8000_0000_0000;
                                        *byte = unsafe {
                                            core::ptr::read_volatile(
                                                (hpa as u64 + PHYS_VIRT_OFFSET) as *const u8,
                                            )
                                        };
                                    }
                                }
                                debug!("[8k]#{out_count} {rip:x} {:02x?}", &dump[..32]);
                            } else {
                                debug!("[8k]#{out_count} {rip:x}");
                            }
                        } else {
                            debug!("[8k]#{out_count} {rip:x}");
                        }
                        self.trace_after_cpuid_8k = true;
                        self.trace_after_8k_count = 0;
                    }
                    0x80000001 => debug!("[81]#{out_count} {rip:x}"),
                    0x1 => {
                        if out_count < 1 {
                            if let Some(ept_root) = self.ept_root {
                                // Dump 128 bytes from RIP-8 to see more context
                                let base_gpa = (rip - 8) & !0x7u64;
                                let mut dump = [0u8; 128];
                                for (i, byte) in dump.iter_mut().enumerate() {
                                    let gpa = base_gpa + i as u64;
                                    if let Some(hpa) = self.gpa_to_hpa_via_ept(ept_root, gpa) {
                                        const PHYS_VIRT_OFFSET: u64 = 0xffff_8000_0000_0000;
                                        *byte = unsafe {
                                            core::ptr::read_volatile(
                                                (hpa as u64 + PHYS_VIRT_OFFSET) as *const u8,
                                            )
                                        };
                                    }
                                }
                                debug!("[1]#{out_count} {rip:x} {:02x?}", &dump[..128]);
                            } else {
                                debug!("[1]#{out_count} {rip:x}");
                            }
                        } else {
                            debug!("[1]#{out_count} {rip:x}");
                        }
                    }
                    _ => debug!("[C]#{out_count} {function:x} {rip:x}"),
                }
            }
        }

        let regs = self.regs_mut();
        regs.rax = res.eax as _;
        regs.rbx = res.ebx as _;
        regs.rcx = res.ecx as _;
        regs.rdx = res.edx as _;
        self.advance_rip(VM_EXIT_INSTR_LEN_CPUID)?;

        Ok(())
    }
}
