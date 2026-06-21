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

//! SMP INIT/SIPI handling: drains a pending INIT-SIPI request from the vLAPIC
//! and translates it into an `AxVCpuExitReason::CpuUp` event for the VMM.
//! Extracted from `vcpu.rs`.

use axaddrspace::GuestPhysAddr;
use axvcpu::AxVCpuExitReason;

use super::vcpu::VmxVcpu;

impl VmxVcpu {
    /// Drain any pending INIT/SIPI from the vLAPIC and convert it to a
    /// `CpuUp` exit reason. Returns `None` if no INIT/SIPI is pending.
    pub(super) fn take_smp_init_sipi(&mut self) -> Option<AxVCpuExitReason> {
        let sipi = self.vlapic.take_pending_init_sipi()?;
        info!(
            "[SMP] INIT/SIPI: target_cpu={}, mode={:?}, vector={:#x}",
            sipi.target_cpu, sipi.mode, sipi.vector
        );
        let entry_point = if sipi.vector != 0 {
            GuestPhysAddr::from((sipi.vector as usize) << 12)
        } else {
            GuestPhysAddr::from(0x0)
        };
        Some(AxVCpuExitReason::CpuUp {
            target_cpu: sipi.target_cpu as u64,
            entry_point,
            arg: 0,
        })
    }
}
