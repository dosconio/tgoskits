#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use core::cell::RefCell;

use ax_errno::AxResult;
use axaddrspace::device::{AccessWidth, Port, PortRange};
use axdevice_base::{BaseDeviceOps, EmuDeviceType};
use log::{debug, info};

const PIC_MASTER_CMD: u16 = 0x20;
const PIC_MASTER_DATA: u16 = 0x21;
const PIC_SLAVE_CMD: u16 = 0xA0;
const PIC_SLAVE_DATA: u16 = 0xA1;

const ICW1_INIT: u8 = 0x10;

/// Global Master PIC instance, set once during VM creation.
pub static GLOBAL_PIC_MASTER: spin::Once<Arc<I8259MasterPic>> = spin::Once::new();

#[derive(Debug, Clone, Copy, PartialEq)]
enum PicState {
    /// Initial state before any ICW1 has been received. Kept for completeness;
    /// the emulated PIC is initialized directly into `Ready` (firmware-ready
    /// Virtual Wire Mode default) so this variant is never constructed at
    /// runtime, but a guest ICW1 reset could conceptually target it.
    #[allow(dead_code)]
    Idle,
    Icw2,
    Icw3,
    Icw4,
    Ready,
}

struct PicChip {
    imr: u8,
    base: u8,
    icw3: u8,
    icw4: u8,
    state: PicState,
    ocw3: u8,
    irr: u8,
    isr: u8,
}

impl PicChip {
    fn new() -> Self {
        // Initialize the PIC in a "firmware-ready" Virtual Wire Mode default
        // state. On real hardware the BIOS (or SeaBIOS) programs the 8259 PIC
        // before handing control to the firmware/OS. OVMF's PEI/DXE phases
        // expect the 8259 to already be in Ready state with IRQ0 (timer)
        // routable so that the PIT can deliver periodic timer interrupts.
        //
        // Default mapping (IBM PC/AT):
        //   Master base = 0x08  → IRQ0..IRQ7  map to vectors 0x08..0x0F
        //   IMR = 0xFB          → only IRQ0 (timer) unmasked
        //   state = Ready       → ICW sequence already "done"
        //
        // If the guest later issues its own ICW1-4 sequence (OVMF's
        // PlatformPei does this), the state machine resets and overwrites
        // these defaults, so this is safe.
        Self {
            imr: 0xFB,
            base: 0x08,
            icw3: 0,
            icw4: 0,
            state: PicState::Ready,
            ocw3: 0,
            irr: 0,
            isr: 0,
        }
    }

    /// Raise an IRQ line (set the corresponding IRR bit).
    fn raise_irq(&mut self, irq: u8) {
        if irq < 8 {
            self.irr |= 1 << irq;
            debug!("[i8259] raise_irq: irq={}, irr={:#04x}", irq, self.irr);
        }
    }

    /// Lower an IRQ line (clear the corresponding IRR bit).
    fn lower_irq(&mut self, irq: u8) {
        if irq < 8 {
            self.irr &= !(1 << irq);
        }
    }

    /// Find the highest priority pending interrupt.
    /// Priority: IRQ0 > IRQ1 > ... > IRQ7 (lower bit = higher priority).
    /// An interrupt is pending if:
    /// - Its IRR bit is set
    /// - Its IMR bit is not set (not masked)
    /// - No higher-priority ISR bit is set (no higher-priority interrupt in service)
    fn pending_irq(&self) -> Option<u8> {
        if self.state != PicState::Ready {
            return None;
        }
        // Pending = IRR & ~IMR
        let pending = self.irr & !self.imr;
        if pending == 0 {
            return None;
        }
        // Find highest priority pending IRQ (lowest bit number)
        let irq = pending.trailing_zeros() as u8;
        // Check if a higher or equal priority interrupt is in service
        // Higher priority = lower bit number. ISR bits below `irq` represent
        // higher priority interrupts that are in service.
        let higher_isr = self.isr & ((1 << irq) - 1);
        if higher_isr != 0 {
            // A higher priority interrupt is in service, cannot deliver
            return None;
        }
        Some(irq)
    }

    /// Perform INTA (interrupt acknowledge): return the vector for the
    /// highest priority pending interrupt and move IRR→ISR.
    fn acknowledge(&mut self) -> Option<u8> {
        if let Some(irq) = self.pending_irq() {
            // Clear IRR bit, set ISR bit
            self.irr &= !(1 << irq);
            self.isr |= 1 << irq;
            let vector = self.base + irq;
            // Rate-limited info logging: first 5 acknowledges, then every 1000th.
            static ACK_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
            let count = ACK_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if count < 5 || count.is_multiple_of(1000) {
                info!(
                    "[i8259] acknowledge #{}: irq={}, vector={:#x}, irr={:#04x}, isr={:#04x}",
                    count, irq, vector, self.irr, self.isr
                );
            }
            Some(vector)
        } else {
            None
        }
    }

    fn handle_cmd_write(&mut self, val: u8, name: &str) {
        if (val & ICW1_INIT) != 0 {
            info!("[i8259] {} ICW1: {:#04x}", name, val);
            self.state = PicState::Icw2;
            self.ocw3 = 0;
            self.irr = 0;
            self.isr = 0;
            self.imr = 0;
        } else if (val & 0x08) != 0 {
            debug!("[i8259] {} OCW3: {:#04x}", name, val);
            self.ocw3 = val;
        } else {
            // OCW2: EOI handling
            let eoi_mode = (val >> 5) & 0x03;
            let level = val & 0x07;
            match eoi_mode {
                0b00 => {
                    // Non-specific EOI: clear the highest priority ISR bit
                    if self.isr != 0 {
                        let highest = self.isr.trailing_zeros() as u8;
                        self.isr &= !(1 << highest);
                        static PIC_EOI_COUNT: core::sync::atomic::AtomicU64 =
                            core::sync::atomic::AtomicU64::new(0);
                        let count =
                            PIC_EOI_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        if count < 10 || count.is_multiple_of(1000) {
                            info!(
                                "[i8259] {} Non-specific EOI #{count}: cleared ISR bit {}, \
                                 isr={:#04x}",
                                name, highest, self.isr
                            );
                        }
                    }
                }
                0b01 => {
                    // Non-specific EOI + rotate (priority rotation)
                    if self.isr != 0 {
                        let highest = self.isr.trailing_zeros() as u8;
                        self.isr &= !(1 << highest);
                    }
                }
                0b10 => {
                    // No EOI, no operation (used for set/reset rotate mode)
                }
                0b11 => {
                    // Specific EOI: clear the ISR bit specified by level
                    if level < 8 {
                        self.isr &= !(1 << level);
                        static PIC_SPEC_EOI_COUNT: core::sync::atomic::AtomicU64 =
                            core::sync::atomic::AtomicU64::new(0);
                        let count =
                            PIC_SPEC_EOI_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        if count < 10 || count.is_multiple_of(1000) {
                            info!(
                                "[i8259] {} Specific EOI #{count}: cleared ISR bit {}, isr={:#04x}",
                                name, level, self.isr
                            );
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
    }

    fn handle_data_write(&mut self, val: u8, name: &str) {
        match self.state {
            PicState::Idle | PicState::Ready => {
                debug!("[i8259] {} OCW1 (IMR): {:#04x}", name, val);
                self.imr = val;
            }
            PicState::Icw2 => {
                info!("[i8259] {} ICW2 (base): {:#04x}", name, val);
                self.base = val & 0xF8;
                self.state = PicState::Icw3;
            }
            PicState::Icw3 => {
                info!("[i8259] {} ICW3: {:#04x}", name, val);
                self.icw3 = val;
                self.state = PicState::Icw4;
            }
            PicState::Icw4 => {
                info!("[i8259] {} ICW4: {:#04x}", name, val);
                self.icw4 = val;
                self.state = PicState::Ready;
                self.imr = 0xFF;
            }
        }
    }

    fn handle_cmd_read(&self) -> u8 {
        if (self.ocw3 & 0x02) != 0 {
            self.isr
        } else if (self.ocw3 & 0x01) != 0 {
            self.irr
        } else {
            let mut status = 0u8;
            if self.irr != 0 {
                status |= 0x80;
            }
            status
        }
    }

    fn handle_data_read(&self) -> u8 {
        self.imr
    }
}

/// i8259 Master PIC (ports 0x20-0x21)
pub struct I8259MasterPic {
    chip: RefCell<PicChip>,
}

impl I8259MasterPic {
    pub fn new() -> Self {
        Self {
            chip: RefCell::new(PicChip::new()),
        }
    }

    /// Raise an IRQ line on the master PIC (set IRR bit).
    /// IRQ must be 0-7.
    pub fn raise_irq(&self, irq: u8) {
        self.chip.borrow_mut().raise_irq(irq);
    }

    /// Lower an IRQ line on the master PIC (clear IRR bit).
    pub fn lower_irq(&self, irq: u8) {
        self.chip.borrow_mut().lower_irq(irq);
    }

    /// Check if the master PIC has a pending interrupt to deliver.
    /// Returns the interrupt vector if one is pending, or None.
    /// This does not modify PIC state.
    pub fn pending_vector(&self) -> Option<u8> {
        self.chip
            .borrow()
            .pending_irq()
            .map(|irq| self.chip.borrow().base + irq)
    }

    /// Perform INTA (interrupt acknowledge): return the vector for the
    /// highest priority pending interrupt and update PIC state (IRR→ISR).
    pub fn acknowledge(&self) -> Option<u8> {
        self.chip.borrow_mut().acknowledge()
    }
}

impl Default for I8259MasterPic {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: I8259MasterPic uses RefCell for interior mutability but is only accessed
// from the vCPU run loop (single-threaded).
unsafe impl Send for I8259MasterPic {}
unsafe impl Sync for I8259MasterPic {}

impl BaseDeviceOps<PortRange> for I8259MasterPic {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::Dummy
    }

    fn address_range(&self) -> PortRange {
        PortRange::new(Port(PIC_MASTER_CMD), Port(PIC_MASTER_DATA))
    }

    fn handle_read(&self, addr: Port, _width: AccessWidth) -> AxResult<usize> {
        let port = addr.0;
        let val = match port {
            PIC_MASTER_CMD => self.chip.borrow().handle_cmd_read(),
            PIC_MASTER_DATA => self.chip.borrow().handle_data_read(),
            _ => {
                info!("[i8259] Master read unknown port {:#x}", port);
                0
            }
        };
        Ok(val as usize)
    }

    fn handle_write(&self, addr: Port, _width: AccessWidth, val: usize) -> AxResult {
        let port = addr.0;
        let val = val as u8;
        match port {
            PIC_MASTER_CMD => self.chip.borrow_mut().handle_cmd_write(val, "Master"),
            PIC_MASTER_DATA => self.chip.borrow_mut().handle_data_write(val, "Master"),
            _ => {
                info!(
                    "[i8259] Master write unknown port {:#x}, val {:#04x}",
                    port, val
                );
            }
        }
        Ok(())
    }
}

/// i8259 Slave PIC (ports 0xA0-0xA1)
pub struct I8259SlavePic {
    chip: RefCell<PicChip>,
}

impl I8259SlavePic {
    pub fn new() -> Self {
        Self {
            chip: RefCell::new(PicChip::new()),
        }
    }

    /// Raise an IRQ line on the slave PIC (set IRR bit).
    /// IRQ must be 0-7 (maps to hardware IRQ 8-15).
    pub fn raise_irq(&self, irq: u8) {
        self.chip.borrow_mut().raise_irq(irq);
    }

    /// Lower an IRQ line on the slave PIC (clear IRR bit).
    pub fn lower_irq(&self, irq: u8) {
        self.chip.borrow_mut().lower_irq(irq);
    }
}

impl Default for I8259SlavePic {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: I8259SlavePic uses RefCell for interior mutability but is only accessed
// from the vCPU run loop (single-threaded).
unsafe impl Send for I8259SlavePic {}
unsafe impl Sync for I8259SlavePic {}

impl BaseDeviceOps<PortRange> for I8259SlavePic {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::Dummy
    }

    fn address_range(&self) -> PortRange {
        PortRange::new(Port(PIC_SLAVE_CMD), Port(PIC_SLAVE_DATA))
    }

    fn handle_read(&self, addr: Port, _width: AccessWidth) -> AxResult<usize> {
        let port = addr.0;
        let val = match port {
            PIC_SLAVE_CMD => self.chip.borrow().handle_cmd_read(),
            PIC_SLAVE_DATA => self.chip.borrow().handle_data_read(),
            _ => {
                info!("[i8259] Slave read unknown port {:#x}", port);
                0
            }
        };
        Ok(val as usize)
    }

    fn handle_write(&self, addr: Port, _width: AccessWidth, val: usize) -> AxResult {
        let port = addr.0;
        let val = val as u8;
        match port {
            PIC_SLAVE_CMD => self.chip.borrow_mut().handle_cmd_write(val, "Slave"),
            PIC_SLAVE_DATA => self.chip.borrow_mut().handle_data_write(val, "Slave"),
            _ => {
                info!(
                    "[i8259] Slave write unknown port {:#x}, val {:#04x}",
                    port, val
                );
            }
        }
        Ok(())
    }
}
