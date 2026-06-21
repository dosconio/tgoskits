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

//! Interrupt injection and pending-event delivery. Extracted from `vcpu.rs`.

use ax_errno::AxResult;
use x86_vioapic::GLOBAL_VIOAPIC;

use super::{
    definitions::VmxInterruptionType,
    vcpu::VmxVcpu,
    vmcs::{self, VmcsGuest32, VmcsGuestNW},
};

impl VmxVcpu {
    /// Whether the guest interrupts are blocked. (SDM Vol. 3C, Section 24.4.2, Table 24-3)
    fn allow_interrupt(&self) -> bool {
        let rflags = VmcsGuestNW::RFLAGS.read().unwrap();
        let block_state = VmcsGuest32::INTERRUPTIBILITY_STATE.read().unwrap();
        let if_flag = rflags as u64 & x86_64::registers::rflags::RFlags::INTERRUPT_FLAG.bits() != 0;
        let blocked = block_state != 0;
        if !if_flag || blocked {
            // Log why interrupts can't be delivered (rate-limited)
            static LOG_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
            let count = LOG_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if count < 5 || count == 100 || count == 1000 {
                trace!(
                    "[INTR-BLOCK] IF={}, block_state={:#x}, RFLAGS={:#x} (count={})",
                    if_flag, block_state, rflags, count
                );
            }
        }
        if_flag && !blocked
    }

    /// Try to inject a pending event before next VM entry.
    pub(super) fn inject_pending_events(&mut self) -> AxResult {
        vmcs::clear_injection()?;

        let apic_enabled = self.vlapic.is_software_enabled();

        if apic_enabled {
            // APIC-enabled path: IOAPIC → vLAPIC → inject as External interrupt
            if let Some(vioapic) = GLOBAL_VIOAPIC.get() {
                let vcpu_id = self.vlapic.timer_where_am_i().1 as u32;
                let pending = vioapic.take_pending_irqs(vcpu_id);
                for vector in pending {
                    debug!(
                        "[IOAPIC] Injecting pending IRQ vector={:#x} to vcpu={}",
                        vector, vcpu_id
                    );
                    self.vlapic.set_intr(vcpu_id, vector as u32);
                    self.queue_external_interrupt(vector);
                }
            }

            // Check for pending timer interrupt from LVT_TIMER unmask.
            let pending_timer = self.vlapic.take_pending_timer_vector();
            if pending_timer > 0 {
                self.queue_external_interrupt(pending_timer);
                debug!("[VLAPIC] Queued pending timer interrupt vector={pending_timer:#x}");
            }

            // Also check 8259 PIC for pending interrupts (Virtual Wire Mode fallback).
            // In real hardware with Virtual Wire Mode, the 8259 PIC output is connected
            // to LAPIC LINT0 (configured as ExtINT). When OVMF enables APIC but hasn't
            // fully configured IOAPIC RTEs yet, PIT IRQ0 may still need to be delivered
            // through the 8259 PIC path. This simulates the LINT0 ExtINT connection.
            if self.pending_events.is_empty()
                && let Some(pic) = i8259_pic::GLOBAL_PIC_MASTER.get()
                && let Some(vector) = pic.acknowledge()
            {
                // debug!(
                //     "[PIC] Virtual Wire Mode (APIC on, IOAPIC empty): acknowledging IRQ, \
                //      vector={:#x}",
                //     vector
                // );
                self.queue_external_interrupt(vector);
            }
        } else {
            // Virtual Wire Mode: 8259 PIC → inject as External interrupt
            // When APIC is not software-enabled, OVMF receives interrupts
            // through the 8259 PIC path (INTR pin → ExtINT).
            //
            // Per Intel SDM 10.4.3, the local APIC timer still generates
            // interrupts even when software-disabled. Check for pending
            // timer interrupts here as well.
            let pending_timer = self.vlapic.take_pending_timer_vector();
            if pending_timer > 0 {
                self.queue_external_interrupt(pending_timer);
                debug!(
                    "[VLAPIC] Queued pending timer interrupt (APIC disabled) \
                     vector={pending_timer:#x}"
                );
            }

            if let Some(pic) = i8259_pic::GLOBAL_PIC_MASTER.get()
                && let Some(vector) = pic.acknowledge()
            {
                // debug!(
                //     "[PIC] Virtual Wire Mode: acknowledging IRQ, vector={:#x}",
                //     vector
                // );
                self.queue_external_interrupt(vector);
            }
        }

        if let Some(event) = self.pending_events.front() {
            let can_inject =
                !matches!(event.int_type, VmxInterruptionType::External) || self.allow_interrupt();
            if can_inject {
                // Rate-limited logging for interrupt injection
                static INJECT_COUNT: core::sync::atomic::AtomicU64 =
                    core::sync::atomic::AtomicU64::new(0);
                let count = INJECT_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                if count < 10 || count == 100 || count == 1000 || count == 10000 {
                    info!(
                        "[INJECT] Injecting event #{}: vector={:#x}, type={:?}",
                        count, event.vector, event.int_type
                    );
                }
                vmcs::inject_event_with_type(event.vector, event.err_code, event.int_type)?;
                self.pending_events.pop_front();
            } else {
                self.set_interrupt_window(true)?;
            }
        }
        Ok(())
    }
}
