#![no_std]

use ax_errno::AxResult;
use axaddrspace::device::{AccessWidth, Port, PortRange};
use axdevice_base::{BaseDeviceOps, EmuDeviceType};
use log::info;

/// PM1a Event Register Block base (0x600)
const PM1A_EVT_BLK: u16 = 0x600;
/// PM Timer Register Block base (0x608)
const PM_TMR_BLK: u16 = 0x608;
/// Full PM register block: 0x600 - 0x60B
const PM_BLOCK_START: u16 = PM1A_EVT_BLK;
const PM_BLOCK_END: u16 = PM_TMR_BLK + 3; // 0x60B

/// ACPI PM Timer frequency: 3.579545 MHz (24-bit counter).
const PM_TIMER_HZ: u64 = 3_579_545;

pub struct PmTimer;

impl Default for PmTimer {
    fn default() -> Self {
        Self
    }
}

impl PmTimer {
    pub fn new() -> Self {
        Self
    }

    pub fn new_default() -> Self {
        Self
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
                // PM1a_EVT_BLK: Event Status Register
                // Bit 0 = Timer status, other bits reserved
                info!(
                    "[PM] Read PM1a_EVT port {:#x}, width {:?}, returning 0",
                    port, width
                );
                Ok(0)
            }
            0x604..=0x607 => {
                // PM1a_CNT_BLK: Control Register
                // Bit 13: SCI_EN (SCI enable), Bit 12: SLP_TYP, Bit 10: SLP_EN
                // Return SCI_EN=1 to indicate ACPI mode is enabled
                let val = 1 << 13; // SCI_EN = 1
                info!(
                    "[PM] Read PM1a_CNT port {:#x}, width {:?}, returning {:#x}",
                    port, width, val
                );
                Ok(val)
            }
            0x608..=0x60B => {
                // PM_TMR_BLK: Timer Register (accessed very frequently,
                // skip per-read logging to avoid flooding the log).
                // Diagnostics are handled inside get_timer_value().
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
            _ => {
                info!(
                    "[PM] Read unknown port {:#x}, width {:?}, returning 0",
                    port, width
                );
                Ok(0)
            }
        }
    }

    fn handle_write(&self, addr: Port, width: AccessWidth, val: usize) -> AxResult {
        let port = addr.0;
        match port {
            0x600..=0x603 => {
                info!(
                    "[PM] Write PM1a_EVT port {:#x}, width {:?}, val {:#x}",
                    port, width, val
                );
            }
            0x604..=0x607 => {
                info!(
                    "[PM] Write PM1a_CNT port {:#x}, width {:?}, val {:#x}",
                    port, width, val
                );
            }
            0x608..=0x60B => {
                // PM_TMR_BLK is read-only; silently ignore writes.
            }
            _ => {
                info!(
                    "[PM] Write unknown port {:#x}, width {:?}, val {:#x}",
                    port, width, val
                );
            }
        }
        Ok(())
    }
}
