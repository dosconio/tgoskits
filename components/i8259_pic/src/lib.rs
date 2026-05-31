#![no_std]

use core::cell::RefCell;

use ax_errno::AxResult;
use axaddrspace::device::{AccessWidth, Port, PortRange};
use axdevice_base::{BaseDeviceOps, EmuDeviceType};
use log::info;

const PIC_MASTER_CMD: u16 = 0x20;
const PIC_MASTER_DATA: u16 = 0x21;
const PIC_SLAVE_CMD: u16 = 0xA0;
const PIC_SLAVE_DATA: u16 = 0xA1;

const ICW1_INIT: u8 = 0x10;

#[derive(Debug, Clone, Copy, PartialEq)]
enum PicState {
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
        Self {
            imr: 0xFF,
            base: 0,
            icw3: 0,
            icw4: 0,
            state: PicState::Idle,
            ocw3: 0,
            irr: 0,
            isr: 0,
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
            info!("[i8259] {} OCW3: {:#04x}", name, val);
            self.ocw3 = val;
        } else {
            info!("[i8259] {} OCW2: {:#04x} (EOI)", name, val);
            let level = val & 0x07;
            if level < 8 {
                self.isr &= !(1 << level);
            }
        }
    }

    fn handle_data_write(&mut self, val: u8, name: &str) {
        match self.state {
            PicState::Idle | PicState::Ready => {
                info!("[i8259] {} OCW1 (IMR): {:#04x}", name, val);
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

pub struct I8259Pic {
    master: RefCell<PicChip>,
    slave: RefCell<PicChip>,
}

impl I8259Pic {
    pub fn new() -> Self {
        Self {
            master: RefCell::new(PicChip::new()),
            slave: RefCell::new(PicChip::new()),
        }
    }
}

impl Default for I8259Pic {
    fn default() -> Self {
        Self::new()
    }
}

impl BaseDeviceOps<PortRange> for I8259Pic {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::Dummy
    }

    fn address_range(&self) -> PortRange {
        PortRange::new(Port(PIC_MASTER_CMD), Port(PIC_SLAVE_DATA))
    }

    fn handle_read(&self, addr: Port, _width: AccessWidth) -> AxResult<usize> {
        let port = addr.0;
        let val = match port {
            PIC_MASTER_CMD => self.master.borrow().handle_cmd_read(),
            PIC_MASTER_DATA => self.master.borrow().handle_data_read(),
            PIC_SLAVE_CMD => self.slave.borrow().handle_cmd_read(),
            PIC_SLAVE_DATA => self.slave.borrow().handle_data_read(),
            _ => {
                info!("[i8259] Read unknown port {:#x}", port);
                0
            }
        };
        Ok(val as usize)
    }

    fn handle_write(&self, addr: Port, _width: AccessWidth, val: usize) -> AxResult {
        let port = addr.0;
        let val = val as u8;
        match port {
            PIC_MASTER_CMD => self.master.borrow_mut().handle_cmd_write(val, "Master"),
            PIC_MASTER_DATA => self.master.borrow_mut().handle_data_write(val, "Master"),
            PIC_SLAVE_CMD => self.slave.borrow_mut().handle_cmd_write(val, "Slave"),
            PIC_SLAVE_DATA => self.slave.borrow_mut().handle_data_write(val, "Slave"),
            _ => {
                info!("[i8259] Write unknown port {:#x}, val {:#04x}", port, val);
            }
        }
        Ok(())
    }
}