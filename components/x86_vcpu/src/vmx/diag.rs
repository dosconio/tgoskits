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

//! VMCS state dumping and VM-entry failure diagnostics / defensive workarounds.
//! Extracted from `vcpu.rs`.

use ax_errno::AxResult;
use axvcpu::AxVCpuExitReason;

use super::{
    vcpu::{EXIT_HISTORY, VmxVcpu},
    vmcs::{
        VmcsControl32,
        VmcsGuest16,
        VmcsGuest32,
        VmcsGuest64,
        VmcsGuestNW,
        // Following imports are only used by dump_vmcs_state() (commented out):
        //   VmcsControl16, VmcsControl64, VmcsControlNW,
        //   VmcsHost16, VmcsHost64, VmcsHostNW,
    },
};

impl VmxVcpu {
    pub(super) fn dump_vmcs_state(&self) {
        // Commented out: too verbose, flooded the log with ~25 debug lines
        // per vCPU launch. Uncomment for debugging VM-entry failures only.
        //
        // let rip = VmcsGuestNW::RIP.read().unwrap_or(0);
        // let rsp = VmcsGuestNW::RSP.read().unwrap_or(0);
        // ... (full dump omitted)
        // debug!("[VMCS-DUMP] (suppressed; uncomment in diag.rs to enable)");
    }

    /// Dump guest state on VM-entry failure and apply defensive workarounds
    /// (clear CS Unusable bit, sanitize IA32_PAT, clear reserved bits in
    /// segment access-rights). Returns the `FailEntry` exit reason that the
    /// caller should propagate to the VMM.
    pub(super) fn diag_entry_failure(
        &mut self,
        exit_reason_raw: u64,
    ) -> AxResult<AxVCpuExitReason> {
        // Dump guest state on the first few VM-entry failures to
        // diagnose the invalid guest state (reason 0x21).
        static ENTRY_FAIL_DUMPED: core::sync::atomic::AtomicU32 =
            core::sync::atomic::AtomicU32::new(0);
        let dumped = ENTRY_FAIL_DUMPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if dumped < 3 {
            // Log the last 16 VM-exits before this VM-entry failure to
            // identify what caused the guest state to become invalid.
            {
                let hist = EXIT_HISTORY.lock();
                error!(
                    "[ENTRY-FAIL #{dumped}] Last {} VM-exits before failure (total {}):",
                    hist.buf.len(),
                    hist.count
                );
                let start = hist.idx; // oldest entry
                for i in 0..16 {
                    let idx = (start + i) % 16;
                    let (reason, rip, cs_ar) = hist.buf[idx];
                    if reason != 0 || rip != 0 {
                        error!(
                            "  [{}] reason={reason:#x} RIP={rip:#x} CS_ar={cs_ar:#010x}",
                            i
                        );
                    }
                }
            }
            let cs_sel = VmcsGuest16::CS_SELECTOR.read().unwrap_or(0);
            let cs_ar = VmcsGuest32::CS_ACCESS_RIGHTS.read().unwrap_or(0);
            let cs_base = VmcsGuestNW::CS_BASE.read().unwrap_or(0);
            let cs_limit = VmcsGuest32::CS_LIMIT.read().unwrap_or(0);
            let ss_sel = VmcsGuest16::SS_SELECTOR.read().unwrap_or(0);
            let ss_ar = VmcsGuest32::SS_ACCESS_RIGHTS.read().unwrap_or(0);
            let ds_sel = VmcsGuest16::DS_SELECTOR.read().unwrap_or(0);
            let ds_ar = VmcsGuest32::DS_ACCESS_RIGHTS.read().unwrap_or(0);
            let es_sel = VmcsGuest16::ES_SELECTOR.read().unwrap_or(0);
            let es_ar = VmcsGuest32::ES_ACCESS_RIGHTS.read().unwrap_or(0);
            let fs_sel = VmcsGuest16::FS_SELECTOR.read().unwrap_or(0);
            let fs_ar = VmcsGuest32::FS_ACCESS_RIGHTS.read().unwrap_or(0);
            let gs_sel = VmcsGuest16::GS_SELECTOR.read().unwrap_or(0);
            let gs_ar = VmcsGuest32::GS_ACCESS_RIGHTS.read().unwrap_or(0);
            let tr_sel = VmcsGuest16::TR_SELECTOR.read().unwrap_or(0);
            let tr_ar = VmcsGuest32::TR_ACCESS_RIGHTS.read().unwrap_or(0);
            let tr_base = VmcsGuestNW::TR_BASE.read().unwrap_or(0);
            let tr_limit = VmcsGuest32::TR_LIMIT.read().unwrap_or(0);
            let ldtr_sel = VmcsGuest16::LDTR_SELECTOR.read().unwrap_or(0);
            let ldtr_ar = VmcsGuest32::LDTR_ACCESS_RIGHTS.read().unwrap_or(0);
            let ldtr_base = VmcsGuestNW::LDTR_BASE.read().unwrap_or(0);
            let ldtr_limit = VmcsGuest32::LDTR_LIMIT.read().unwrap_or(0);
            let cr0 = VmcsGuestNW::CR0.read().unwrap_or(0);
            let cr3 = VmcsGuestNW::CR3.read().unwrap_or(0);
            let cr4 = VmcsGuestNW::CR4.read().unwrap_or(0);
            let efer = VmcsGuest64::IA32_EFER.read().unwrap_or(0);
            let rflags = VmcsGuestNW::RFLAGS.read().unwrap_or(0);
            let rip = self.rip();
            let rsp = VmcsGuestNW::RSP.read().unwrap_or(0);
            let dr7 = VmcsGuestNW::DR7.read().unwrap_or(0);
            let fs_base = VmcsGuestNW::FS_BASE.read().unwrap_or(0);
            let gs_base = VmcsGuestNW::GS_BASE.read().unwrap_or(0);
            let pat = VmcsGuest64::IA32_PAT.read().unwrap_or(0);
            let perf_global = VmcsGuest64::IA32_PERF_GLOBAL_CTRL.read().unwrap_or(0);
            let pdpte0 = VmcsGuest64::PDPTE0.read().unwrap_or(0);
            let pdpte1 = VmcsGuest64::PDPTE1.read().unwrap_or(0);
            let pdpte2 = VmcsGuest64::PDPTE2.read().unwrap_or(0);
            let pdpte3 = VmcsGuest64::PDPTE3.read().unwrap_or(0);
            let gdtr_base = VmcsGuestNW::GDTR_BASE.read().unwrap_or(0);
            let gdtr_limit = VmcsGuest32::GDTR_LIMIT.read().unwrap_or(0);
            let idtr_base = VmcsGuestNW::IDTR_BASE.read().unwrap_or(0);
            let idtr_limit = VmcsGuest32::IDTR_LIMIT.read().unwrap_or(0);
            let link_ptr = VmcsGuest64::LINK_PTR.read().unwrap_or(0);
            let debugctl = VmcsGuest64::IA32_DEBUGCTL.read().unwrap_or(0);
            let intr_state = VmcsGuest32::INTERRUPTIBILITY_STATE.read().unwrap_or(0);
            let act_state = VmcsGuest32::ACTIVITY_STATE.read().unwrap_or(0);
            let sysenter_cs = VmcsGuest32::IA32_SYSENTER_CS.read().unwrap_or(0);
            let pending_dbg = VmcsGuestNW::PENDING_DBG_EXCEPTIONS.read().unwrap_or(0);
            let sysenter_esp = VmcsGuestNW::IA32_SYSENTER_ESP.read().unwrap_or(0);
            let sysenter_eip = VmcsGuestNW::IA32_SYSENTER_EIP.read().unwrap_or(0);
            let vmentry_intr = VmcsControl32::VMENTRY_INTERRUPTION_INFO_FIELD
                .read()
                .unwrap_or(0);
            error!(
                "[ENTRY-FAIL #{dumped}] reason={exit_reason_raw:#x} RIP={rip:#x} RSP={rsp:#x} \
                 RFLAGS={rflags:#x}\nCR0={cr0:#x} CR3={cr3:#x} CR4={cr4:#x} EFER={efer:#x} \
                 DR7={dr7:#x}\nCS={cs_sel:#x} ar={cs_ar:#08x} base={cs_base:#x} \
                 lim={cs_limit:#x}\nSS={ss_sel:#x} ar={ss_ar:#08x}\nDS={ds_sel:#x} \
                 ar={ds_ar:#08x} ES={es_sel:#x} ar={es_ar:#08x}\nFS={fs_sel:#x} ar={fs_ar:#08x} \
                 base={fs_base:#x} GS={gs_sel:#x} ar={gs_ar:#08x} \
                 base={gs_base:#x}\nTR={tr_sel:#x} ar={tr_ar:#08x} base={tr_base:#x} \
                 lim={tr_limit:#x}\nLDTR={ldtr_sel:#x} ar={ldtr_ar:#08x} base={ldtr_base:#x} \
                 lim={ldtr_limit:#x}\nGDTR={gdtr_base:#x}:{gdtr_limit:#x} \
                 IDTR={idtr_base:#x}:{idtr_limit:#x}\nLINK_PTR={link_ptr:#x} \
                 DEBUGCTL={debugctl:#x}\nINTR_STATE={intr_state:#x} \
                 ACT_STATE={act_state:#x}\nPENDING_DBG={pending_dbg:#x} \
                 SYSENTER_CS={sysenter_cs:#x}\nSYSENTER_ESP={sysenter_esp:#x} \
                 SYSENTER_EIP={sysenter_eip:#x}\nVMENTRY_INTR_INFO={vmentry_intr:#x}\\
                 nPAT={pat:#x} PERF_GLOBAL_CTRL={perf_global:#x}\nPDPTE0={pdpte0:#x} \
                 PDPTE1={pdpte1:#x} PDPTE2={pdpte2:#x} PDPTE3={pdpte3:#x}"
            );
        }
        // Workaround: if CS is marked Unusable (bit 15), clear it.
        // CS must always be usable; the processor may incorrectly set
        // this bit in some edge cases. Clearing it allows VM-entry to
        // succeed and the guest to continue.
        let cs_ar = VmcsGuest32::CS_ACCESS_RIGHTS.read().unwrap_or(0);
        if cs_ar & 0x8000 != 0 {
            let fixed_ar = cs_ar & !0x8000;
            warn!(
                "[ENTRY-FAIL] CS marked Unusable (ar={cs_ar:#010x}), clearing bit 15 -> \
                 ar={fixed_ar:#010x}"
            );
            VmcsGuest32::CS_ACCESS_RIGHTS.write(fixed_ar)?;
        }
        // Workaround: fix invalid IA32_PAT entries. Intel SDM 26.3.1.1
        // requires bits 2:0 of each PAT entry to be 0, 1, 4, 5, or 6.
        // Values 2, 3, 7 are invalid and cause VM-entry failure.
        // The guest may write these via passthrough (no VM-exit), so we
        // sanitize the saved PAT on VM-entry failure.
        let pat = VmcsGuest64::IA32_PAT.read().unwrap_or(0);
        let mut fixed_pat = pat;
        let mut pat_changed = false;
        for i in 0..8u64 {
            let shift = i * 8;
            let entry = (pat >> shift) & 0x7;
            if entry == 2 || entry == 3 || entry == 7 {
                // Replace invalid entry with UC (0), the safest value.
                fixed_pat &= !(0x7 << shift);
                pat_changed = true;
            }
        }
        if pat_changed {
            warn!(
                "[ENTRY-FAIL] IA32_PAT has invalid entries (pat={pat:#018x}), fixing -> \
                 {fixed_pat:#018x}"
            );
            VmcsGuest64::IA32_PAT.write(fixed_pat)?;
        }
        // Workaround: clear reserved bits (31:16) of segment access
        // rights. Intel SDM 26.3.1.2 says no checks are performed on
        // unusable segments, but bits 31:16 are reserved and must be 0
        // for ALL segment access-rights fields. Some guests leave bit 16
        // set, which the processor may reject on VM-entry.
        for (name, field) in [
            ("SS", VmcsGuest32::SS_ACCESS_RIGHTS),
            ("DS", VmcsGuest32::DS_ACCESS_RIGHTS),
            ("ES", VmcsGuest32::ES_ACCESS_RIGHTS),
            ("FS", VmcsGuest32::FS_ACCESS_RIGHTS),
            ("GS", VmcsGuest32::GS_ACCESS_RIGHTS),
            ("LDTR", VmcsGuest32::LDTR_ACCESS_RIGHTS),
        ] {
            let ar = field.read().unwrap_or(0);
            if ar & 0xFFFF0000 != 0 {
                let fixed_ar = ar & 0x0000FFFF;
                warn!(
                    "[ENTRY-FAIL] {name} ar={ar:#010x} has reserved bits set, clearing -> \
                     {fixed_ar:#010x}"
                );
                field.write(fixed_ar)?;
            }
        }
        Ok(AxVCpuExitReason::FailEntry {
            hardware_entry_failure_reason: exit_reason_raw,
        })
    }
}
