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

//! Intel 8254 Programmable Interval Timer (PIT) emulation.
//!
//! The 8254 PIT provides three 16-bit counters that can be configured
//! in various operating modes. It uses four I/O ports:
//! - 0x40: Counter 0 data (system timer, connected to IRQ0)
//! - 0x41: Counter 1 data (DRAM refresh)
//! - 0x42: Counter 2 data (PC speaker / TSC calibration)
//! - 0x43: Mode/Command register (write-only)
//!
//! OVMF (UEFI firmware) depends on PIT counter 0 for periodic timer
//! interrupts during SEC/PEI/DXE phase (Mode 2 rate generator), and
//! counter 2 in one-shot mode for TSC calibration.
//!
//! Time advancement is driven by real time (platform ticks), not by
//! I/O access count. Call `advance_to_now()` before each VM entry to
//! ensure counters are up-to-date and IRQ0 is asserted at the correct
//! rate.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use core::cell::RefCell;

use ax_errno::AxResult;
use axaddrspace::device::{AccessWidth, Port, PortRange};
use axdevice_base::{BaseDeviceOps, EmuDeviceType};
use axvisor_api::time::{current_ticks, ticks_to_nanos};
use log::{debug, info};

/// Callback type for PIT interrupt notification.
/// Called with (gsi, level) where gsi is the IRQ number and level is true=assert, false=deassert.
pub type PitIrqCallback = fn(u32, bool);

const PIT_COUNTER0_DATA: u16 = 0x40;
const PIT_COUNTER1_DATA: u16 = 0x41;
const PIT_COUNTER2_DATA: u16 = 0x42;
const PIT_MODE_CMD: u16 = 0x43;
/// Port 0x61: NMI Control / PC Speaker / Timer 2 status
const PORT_61_NMI_CTRL: u16 = 0x61;

/// PIT input clock frequency in Hz.
const PIT_FREQUENCY_HZ: u64 = 1193182;

/// Global PIT instance, set once during VM creation.
pub static GLOBAL_PIT: spin::Once<Arc<I8254Pit>> = spin::Once::new();

/// PIT counter operating mode
#[derive(Debug, Clone, Copy, PartialEq)]
enum PitMode {
    /// Mode 0: Interrupt on Terminal Count
    Mode0,
    /// Mode 1: Hardware Retriggerable One-Shot
    Mode1,
    /// Mode 2: Rate Generator (periodic, reload after reaching 1)
    Mode2,
    /// Mode 3: Square Wave Generator (periodic, count down by 2)
    Mode3,
    /// Mode 4: Software Triggered Strobe
    Mode4,
    /// Mode 5: Hardware Triggered Strobe
    Mode5,
}

impl PitMode {
    fn from_bits(bits: u8) -> Self {
        match bits & 0x07 {
            0 => PitMode::Mode0,
            1 => PitMode::Mode1,
            2 => PitMode::Mode2, // x010
            3 => PitMode::Mode3, // x011
            4 => PitMode::Mode4,
            5 => PitMode::Mode5,
            6 => PitMode::Mode2, // x110 maps to mode 2
            7 => PitMode::Mode3, // x111 maps to mode 3
            _ => unreachable!(),
        }
    }
}

/// Access mode for counter read/write
#[derive(Debug, Clone, Copy, PartialEq)]
enum PitAccess {
    /// Counter latch command
    Latch,
    /// Read/write LSB only
    LoByte,
    /// Read/write MSB only
    HiByte,
    /// Read/write LSB then MSB
    LoHiByte,
}

/// Per-counter state
#[derive(Debug, Clone)]
struct PitCounter {
    /// Initial (reload) count value (0 means 65536)
    reload_value: u16,
    /// Current count value
    count: u16,
    /// Latched count value (for read-back)
    latched_count: u16,
    /// Latch status
    latch_status: u8,
    /// Current operating mode
    mode: PitMode,
    /// Read/write access mode
    access: PitAccess,
    /// Read state for LoHiByte access
    read_state: u8,
    /// Write state for LoHiByte access
    write_state: u8,
    /// Whether the latch is valid (from counter-latch or read-back)
    latch_valid: bool,
    /// OUT pin state
    out_pin: bool,
    /// Null count flag: set when new count is written but not yet loaded
    null_count: bool,
}

impl PitCounter {
    fn new() -> Self {
        Self {
            reload_value: 0,
            count: 0,
            latched_count: 0,
            latch_status: 0,
            mode: PitMode::Mode0,
            access: PitAccess::LoByte,
            read_state: 0,
            write_state: 0,
            latch_valid: false,
            out_pin: true,
            null_count: true,
        }
    }

    /// Write initial count value (LSB or MSB depending on access mode)
    fn write_count(&mut self, val: u8) {
        match self.access {
            PitAccess::LoByte => {
                self.reload_value = (self.reload_value & 0xFF00) | val as u16;
                self.load_count();
            }
            PitAccess::HiByte => {
                self.reload_value = (self.reload_value & 0x00FF) | ((val as u16) << 8);
                self.load_count();
            }
            PitAccess::LoHiByte => {
                if self.write_state == 0 {
                    self.reload_value = (self.reload_value & 0xFF00) | val as u16;
                    self.write_state = 1;
                } else {
                    self.reload_value = (self.reload_value & 0x00FF) | ((val as u16) << 8);
                    self.write_state = 0;
                    self.load_count();
                }
            }
            PitAccess::Latch => {
                // Should not happen - counter latch doesn't modify count
            }
        }
    }

    /// Load the current count from the reload value.
    fn load_count(&mut self) {
        self.count = self.reload_value;
        self.null_count = false;
        self.out_pin = match self.mode {
            PitMode::Mode0 => false,
            PitMode::Mode2 | PitMode::Mode3 => true,
            _ => true,
        };
    }

    /// Advance the counter by `pit_ticks` PIT clock ticks in one batch.
    /// Returns true if an IRQ should be asserted for counter 0.
    fn advance_by_ticks(&mut self, pit_ticks: u64) -> bool {
        if self.null_count || pit_ticks == 0 {
            return false;
        }

        let initial = if self.reload_value == 0 {
            65536u64
        } else {
            self.reload_value as u64
        };

        if initial == 0 {
            return false;
        }

        match self.mode {
            PitMode::Mode0 => {
                // Interrupt on terminal count: count down to 0, OUT goes high
                let current = self.count as u64;
                if pit_ticks >= current {
                    self.count = 0;
                    self.out_pin = true;
                    // IRQ0 should be asserted when counter reaches 0
                    return true;
                } else {
                    self.count = (current - pit_ticks) as u16;
                }
            }
            PitMode::Mode2 => {
                // Rate generator: count down to 1, then reload.
                // Period = initial_count PIT ticks.
                // IRQ0 is asserted (pulse) at each reload boundary.
                let current = self.count as u64;
                let total_from_reload = initial - current; // ticks since last reload
                let total_elapsed = total_from_reload + pit_ticks;

                if total_elapsed >= initial {
                    // At least one period boundary crossed
                    let remaining = total_elapsed % initial;
                    self.count = if remaining == 0 {
                        initial as u16 // just reloaded
                    } else {
                        (initial - remaining) as u16
                    };
                    return true; // IRQ0 pulse
                } else {
                    self.count = (initial - total_elapsed) as u16;
                }
            }
            PitMode::Mode3 => {
                // Square wave: count down by 2 each tick
                let effective_ticks = pit_ticks * 2;
                let initial_even = if initial % 2 == 0 {
                    initial
                } else {
                    initial + 1
                };
                if initial_even == 0 {
                    return false;
                }
                let half_periods = effective_ticks / initial_even;
                let remaining = effective_ticks % initial_even;
                self.count = if remaining == 0 {
                    initial_even as u16
                } else {
                    (initial_even - remaining) as u16
                };
                if half_periods > 0 {
                    // Toggle OUT pin for each half-period
                    if half_periods % 2 == 1 {
                        self.out_pin = !self.out_pin;
                    }
                    return true; // IRQ0 pulse
                }
            }
            _ => {
                // Other modes: simple decrement
                let current = self.count as u64;
                if pit_ticks >= current {
                    self.count = 0;
                } else {
                    self.count = (current - pit_ticks) as u16;
                }
            }
        }
        false
    }

    /// Latch the current count value
    fn latch(&mut self) {
        self.latched_count = self.count;
        self.latch_valid = true;
    }

    /// Read count value (returns latched or current based on state)
    fn read_count(&mut self) -> u8 {
        let val = if self.latch_valid {
            self.latch_valid = false;
            self.latched_count
        } else {
            self.count
        };

        match self.access {
            PitAccess::LoByte | PitAccess::LoHiByte => {
                if self.read_state == 0 {
                    self.read_state = 1;
                    (val & 0xFF) as u8
                } else {
                    self.read_state = 0;
                    ((val >> 8) & 0xFF) as u8
                }
            }
            PitAccess::HiByte => ((val >> 8) & 0xFF) as u8,
            PitAccess::Latch => 0,
        }
    }

    /// Read-back status byte
    fn read_status(&self) -> u8 {
        let mut status = 0u8;
        if self.out_pin {
            status |= 0x80;
        }
        if self.null_count {
            status |= 0x40;
        }
        match self.access {
            PitAccess::Latch => {}
            PitAccess::LoByte => status |= 0x10,
            PitAccess::HiByte => status |= 0x20,
            PitAccess::LoHiByte => status |= 0x30,
        }
        status |= match self.mode {
            PitMode::Mode0 => 0x00,
            PitMode::Mode1 => 0x02,
            PitMode::Mode2 => 0x04,
            PitMode::Mode3 => 0x06,
            PitMode::Mode4 => 0x08,
            PitMode::Mode5 => 0x0A,
        };
        status
    }
}

/// i8254 PIT device (ports 0x40-0x43, plus port 0x61 for Channel 2 status)
pub struct I8254Pit {
    counters: [RefCell<PitCounter>; 3],
    /// Read-back command latch state
    readback_active: RefCell<bool>,
    /// Last advance time in nanoseconds
    last_advance_ns: RefCell<u64>,
    /// IRQ callback: called when counter 0 reaches terminal count or reload
    irq_callback: Option<PitIrqCallback>,
    /// Track whether IRQ0 is currently asserted (for edge-triggered signaling)
    irq0_asserted: RefCell<bool>,
    /// Port 0x61 (NMI Control / PC Speaker) shadow byte.
    /// Bit 0: Timer 2 gate (1 = enabled, 0 = disabled)
    /// Bit 1: Speaker data enable
    /// Bit 5: Timer 2 OUT pin (read-only, reflects counter 2 output)
    port_61: RefCell<u8>,
}

// SAFETY: I8254Pit uses RefCell for interior mutability but is only accessed
// from the vCPU run loop (single-threaded). The RefCell borrows are always
// short-lived and non-overlapping within a single method call.
unsafe impl Send for I8254Pit {}
unsafe impl Sync for I8254Pit {}

impl I8254Pit {
    pub fn new() -> Self {
        let now_ns = ticks_to_nanos(current_ticks());
        Self {
            counters: [
                RefCell::new(PitCounter::new()),
                RefCell::new(PitCounter::new()),
                RefCell::new(PitCounter::new()),
            ],
            readback_active: RefCell::new(false),
            last_advance_ns: RefCell::new(now_ns),
            irq_callback: None,
            irq0_asserted: RefCell::new(false),
            port_61: RefCell::new(0x10), // bit 4 = refresh clock toggle (always 1 on PC)
        }
    }

    /// Create a new PIT with an IRQ callback for counter 0.
    pub fn new_with_irq_callback(callback: PitIrqCallback) -> Self {
        let now_ns = ticks_to_nanos(current_ticks());
        Self {
            counters: [
                RefCell::new(PitCounter::new()),
                RefCell::new(PitCounter::new()),
                RefCell::new(PitCounter::new()),
            ],
            readback_active: RefCell::new(false),
            last_advance_ns: RefCell::new(now_ns),
            irq_callback: Some(callback),
            irq0_asserted: RefCell::new(false),
            port_61: RefCell::new(0x10), // bit 4 = refresh clock toggle (always 1 on PC)
        }
    }

    /// Advance PIT counters based on elapsed real time.
    /// Must be called before each VM entry to ensure counters are up-to-date
    /// and IRQ0 is asserted at the correct rate.
    pub fn advance_to_now(&self) {
        let now_ns = ticks_to_nanos(current_ticks());
        let mut last = self.last_advance_ns.borrow_mut();
        let elapsed_ns = now_ns.saturating_sub(*last);
        if elapsed_ns == 0 {
            return;
        }
        let pit_ticks = if elapsed_ns < 1_000_000_000 {
            (elapsed_ns * PIT_FREQUENCY_HZ) / 1_000_000_000
        } else {
            // Avoid overflow for large elapsed times
            (elapsed_ns / 1_000_000_000) * PIT_FREQUENCY_HZ
                + ((elapsed_ns % 1_000_000_000) * PIT_FREQUENCY_HZ) / 1_000_000_000
        };

        if pit_ticks == 0 {
            // Do NOT update last_advance_ns when pit_ticks == 0,
            // otherwise the elapsed time is lost and the counter
            // never advances if advance_to_now() is called frequently
            // (e.g., OVMF polling port 0x61 in a tight loop).
            return;
        }

        // Only update last_advance_ns when we actually advance counters
        *last = now_ns;
        drop(last);

        // Advance counter 0 and check for IRQ0
        let should_irq = self.counters[0].borrow_mut().advance_by_ticks(pit_ticks);
        if should_irq {
            // PIT Mode 2 (Rate Generator) and Mode 3 (Square Wave) produce
            // a *pulse* on each period boundary, not a level. For OVMF to
            // receive periodic timer interrupts, we must:
            //   1. Deassert the previous pulse (so the IRQ line returns low)
            //   2. Re-assert a new pulse (rising edge → IRR bit set in PIC)
            // Previously the code only set `irq0_asserted = true` and never
            // cleared it, so the PIC only ever saw one rising edge and all
            // subsequent periods were silently dropped.
            let mut asserted = self.irq0_asserted.borrow_mut();
            if *asserted {
                // Drop the previous assertion first (falling edge).
                *asserted = false;
                if let Some(cb) = self.irq_callback {
                    cb(0, false);
                }
            }
            // Rising edge for the new period.
            *asserted = true;
            // Rate-limited info logging: first 3 IRQs, then every 1000th.
            static PIT_IRQ_COUNT: core::sync::atomic::AtomicU64 =
                core::sync::atomic::AtomicU64::new(0);
            let count = PIT_IRQ_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if count < 3 || count.is_multiple_of(1000) {
                info!(
                    "[i8254] Counter 0 IRQ0 pulse #{} (mode={:?}, reload={:#x})",
                    count,
                    self.counters[0].borrow().mode,
                    self.counters[0].borrow().reload_value,
                );
            }
            if let Some(cb) = self.irq_callback {
                cb(0, true);
            }
        }

        // Advance counters 1 and 2 (for TSC calibration, etc.)
        for cnt in &self.counters[1..] {
            cnt.borrow_mut().advance_by_ticks(pit_ticks);
        }
    }

    /// Legacy advance_time: now just delegates to advance_to_now().
    fn advance_time(&self) {
        self.advance_to_now();
    }

    /// Check if counter 0 has reached terminal count (for interrupt injection)
    pub fn counter0_irq_pending(&self) -> bool {
        let cnt = self.counters[0].borrow();
        !cnt.out_pin && !cnt.null_count
    }

    /// Set counter 0 OUT pin high (acknowledge IRQ)
    pub fn counter0_ack_irq(&self) {
        self.counters[0].borrow_mut().out_pin = true;
    }

    /// Deassert IRQ0 (called when the guest handles the interrupt)
    pub fn counter0_deassert_irq(&self) {
        let mut asserted = self.irq0_asserted.borrow_mut();
        if *asserted {
            *asserted = false;
            if let Some(cb) = self.irq_callback {
                cb(0, false);
            }
        }
    }
}

impl Default for I8254Pit {
    fn default() -> Self {
        Self::new()
    }
}

impl BaseDeviceOps<PortRange> for I8254Pit {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::Dummy
    }

    fn address_range(&self) -> PortRange {
        PortRange::new(Port(PIT_COUNTER0_DATA), Port(PORT_61_NMI_CTRL))
    }

    fn handle_read(&self, addr: Port, _width: AccessWidth) -> AxResult<usize> {
        self.advance_time();
        let port = addr.0;
        let val = match port {
            PIT_COUNTER0_DATA => self.counters[0].borrow_mut().read_count(),
            PIT_COUNTER1_DATA => self.counters[1].borrow_mut().read_count(),
            PIT_COUNTER2_DATA => self.counters[2].borrow_mut().read_count(),
            PIT_MODE_CMD => {
                // Mode/command register is write-only
                0
            }
            PORT_61_NMI_CTRL => {
                // Port 0x61: bit 5 = Timer 2 OUT pin, bit 4 = refresh clock
                let mut val = *self.port_61.borrow();
                // Update bit 5 with current counter 2 OUT pin state
                let ch2_out = self.counters[2].borrow().out_pin;
                if ch2_out {
                    val |= 0x20;
                } else {
                    val &= !0x20;
                }
                val
            }
            _ => {
                debug!("[i8254] Read unknown port {:#x}", port);
                0
            }
        };
        debug!("[i8254] Read port {:#x} -> {:#04x}", port, val);
        Ok(val as usize)
    }

    fn handle_write(&self, addr: Port, _width: AccessWidth, val: usize) -> AxResult {
        self.advance_time();
        let port = addr.0;
        let val = val as u8;
        if port == PIT_MODE_CMD {
            info!("[i8254] Write mode cmd: {:#04x}", val);
        } else {
            info!("[i8254] Write port {:#x} <- {:#04x}", port, val);
        }

        match port {
            PIT_MODE_CMD => {
                let sc = (val >> 6) & 0x03;
                let rw = (val >> 4) & 0x03;
                let mode_bits = (val >> 1) & 0x07;
                let bcd = val & 0x01;

                if bcd != 0 {
                    debug!("[i8254] BCD mode not supported, ignoring");
                    return Ok(());
                }

                if sc == 3 {
                    let latch_count = (val & 0x20) == 0;
                    let latch_status = (val & 0x10) == 0;

                    for (i, cnt) in self.counters.iter().enumerate() {
                        if (val >> (i + 1)) & 1 != 0 {
                            let mut c = cnt.borrow_mut();
                            if latch_count {
                                c.latch();
                            }
                            if latch_status {
                                c.latch_status = c.read_status();
                            }
                        }
                    }
                    *self.readback_active.borrow_mut() = true;
                    debug!(
                        "[i8254] Read-back: count={latch_count}, status={latch_status}, sel={:#x}",
                        val & 0x0E
                    );
                } else {
                    let access = match rw {
                        0 => PitAccess::Latch,
                        1 => PitAccess::LoByte,
                        2 => PitAccess::HiByte,
                        3 => PitAccess::LoHiByte,
                        _ => unreachable!(),
                    };
                    let mode = PitMode::from_bits(mode_bits);

                    {
                        let mut cnt = self.counters[sc as usize].borrow_mut();
                        cnt.access = access;
                        cnt.mode = mode;
                        cnt.read_state = 0;
                        cnt.write_state = 0;
                        if mode == PitMode::Mode0 {
                            cnt.out_pin = false;
                            cnt.null_count = true;
                        }
                    }
                    debug!(
                        "[i8254] Counter {} configured: access={:?}, mode={:?}, bcd={}",
                        sc, access, mode, bcd
                    );
                }
            }
            PIT_COUNTER0_DATA => self.counters[0].borrow_mut().write_count(val),
            PIT_COUNTER1_DATA => self.counters[1].borrow_mut().write_count(val),
            PIT_COUNTER2_DATA => self.counters[2].borrow_mut().write_count(val),
            PORT_61_NMI_CTRL => {
                // Port 0x61 write: bit 0 = Timer 2 gate, bit 1 = Speaker data
                *self.port_61.borrow_mut() = val & 0x03;
                debug!(
                    "[i8254] Port 0x61 write: gate={}, speaker={}",
                    val & 1,
                    (val >> 1) & 1
                );
            }
            _ => {
                debug!("[i8254] Write unknown port {:#x} <- {:#04x}", port, val);
            }
        }
        Ok(())
    }
}
