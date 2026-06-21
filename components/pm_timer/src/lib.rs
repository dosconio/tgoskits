#![no_std]

use core::sync::atomic::{AtomicU16, Ordering};

use ax_errno::AxResult;
use axaddrspace::device::{AccessWidth, Port, PortRange};
use axdevice_base::{BaseDeviceOps, EmuDeviceType};

/// PM1a Event Register Block base (0x600)
const PM1A_EVT_BLK: u16 = 0x600;
/// PM Timer Register Block base (0x608)
const PM_TMR_BLK: u16 = 0x608;
/// Full PM register block: 0x600 - 0x60B
const PM_BLOCK_START: u16 = PM1A_EVT_BLK;
const PM_BLOCK_END: u16 = PM_TMR_BLK + 3; // 0x60B

/// ACPI PM Timer frequency: 3.579545 MHz (24-bit counter).
const PM_TIMER_HZ: u64 = 3_579_545;

pub struct PmTimer {
    /// PM1a Status Register (offset 0x00-0x01 within PM1a_EVT).
    /// Write-1-to-clear semantics per ACPI spec.
    pm1a_sts: AtomicU16,
    /// PM1a Enable Register (offset 0x02-0x03 within PM1a_EVT).
    /// Read/write semantics per ACPI spec.
    pm1a_en: AtomicU16,
}

impl Default for PmTimer {
    fn default() -> Self {
        Self {
            pm1a_sts: AtomicU16::new(0),
            pm1a_en: AtomicU16::new(0),
        }
    }
}

impl PmTimer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn new_default() -> Self {
        Self::default()
    }

    fn get_timer_value(&self) -> u32 {
        // PM Timer is a 24-bit counter that increments at 3.579545 MHz.
        // Use the platform-calibrated time API (based on CPUID-derived TSC
        // frequency) instead of raw TSC, so the rate is correct regardless
        // of the host CPU frequency.
        let now_ns = axvisor_api::time::ticks_to_nanos(axvisor_api::time::current_ticks());
        ((now_ns * PM_TIMER_HZ / 1_000_000_000) as u32) & 0xFFFFFF
    }
}

impl BaseDeviceOps<PortRange> for PmTimer {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::PmTimer
    }

    fn address_range(&self) -> PortRange {
        // Cover the entire PM register block: 0x600 - 0x60B
        PortRange::new(Port(PM_BLOCK_START), Port(PM_BLOCK_END))
    }

    fn handle_read(&self, addr: Port, width: AccessWidth) -> AxResult<usize> {
        let port = addr.0;

        match port {
            0x600..=0x603 => {
                // PM1a_EVT_BLK: Status (0x600-0x601) + Enable (0x602-0x603)
                let sts = self.pm1a_sts.load(Ordering::Relaxed) as u32;
                let en = self.pm1a_en.load(Ordering::Relaxed) as u32;
                // Combine into a 32-bit value: [EN_HI:EN_LO:STS_HI:STS_LO]
                let combined: u32 = (en << 16) | sts;
                let offset = (port - PM1A_EVT_BLK) as u32;
                let shift = offset * 8;
                let val = match width {
                    AccessWidth::Byte => (combined >> shift) & 0xFF,
                    AccessWidth::Word => (combined >> shift) & 0xFFFF,
                    AccessWidth::Dword => combined,
                    AccessWidth::Qword => combined,
                };
                Ok(val as usize)
            }
            0x604..=0x607 => {
                // PM1a_CNT_BLK: Control Register
                // Bit 13: SCI_EN (SCI enable), Bit 12: SLP_TYP, Bit 10: SLP_EN
                // Return SCI_EN=1 to indicate ACPI mode is enabled
                Ok((1 << 13) as usize)
            }
            0x608..=0x60B => {
                // PM_TMR_BLK: Timer Register
                let timer_val = self.get_timer_value();
                let offset = (port - PM_TMR_BLK) as u32;
                match width {
                    AccessWidth::Byte => {
                        let val = (timer_val >> (offset * 8)) & 0xFF;
                        Ok(val as usize)
                    }
                    AccessWidth::Word => {
                        let val = (timer_val >> (offset * 8)) & 0xFFFF;
                        Ok(val as usize)
                    }
                    AccessWidth::Dword => Ok(timer_val as usize),
                    AccessWidth::Qword => Ok(timer_val as usize),
                }
            }
            _ => Ok(0),
        }
    }

    fn handle_write(&self, addr: Port, _width: AccessWidth, val: usize) -> AxResult {
        let port = addr.0;
        match port {
            0x600..=0x601 => {
                // PM1a Status: write-1-to-clear
                let clear_mask = val as u16;
                let _ = self.pm1a_sts.fetch_and(!clear_mask, Ordering::Relaxed);
            }
            0x602..=0x603 => {
                // PM1a Enable: read/write
                self.pm1a_en.store(val as u16, Ordering::Relaxed);
            }
            0x604..=0x607 => {
                // PM1a_CNT_BLK: accept writes (sleep control, etc.)
                // No-op for now — we don't actually sleep.
            }
            0x608..=0x60B => {
                // PM_TMR_BLK is read-only; silently ignore writes.
            }
            _ => {}
        }
        Ok(())
    }
}
