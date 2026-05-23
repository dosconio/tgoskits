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

//! ACPI System Description Table (SDT) builder helper.

use alloc::vec::Vec;

/// ACPI OEM ID
const OEM_ID: [u8; 6] = *b"AXVSOR";
/// ACPI OEM Table ID
const OEM_TABLE_ID: [u8; 8] = *b"AXVSOR01";
/// ACPI Creator ID
const CREATOR_ID: u32 = u32::from_be_bytes(*b"AXVS");
/// ACPI Creator Revision
const CREATOR_REVISION: u32 = 1;

/// Builder for ACPI System Description Tables
pub struct SdtBuilder {
    /// Table signature (4 bytes)
    signature: [u8; 4],
    /// Total table length including header
    length: u32,
    /// Table body data (after the 36-byte header)
    body: Vec<u8>,
}

impl SdtBuilder {
    /// Create a new SDT builder
    pub fn new(signature: [u8; 4], length: u32) -> Self {
        Self {
            signature,
            length,
            body: Vec::new(),
        }
    }

    /// Append a u8 value
    pub fn append_u8(&mut self, val: u8) {
        self.body.push(val);
    }

    /// Append a u16 value (little-endian)
    pub fn append_u16(&mut self, val: u16) {
        self.body.extend_from_slice(&val.to_le_bytes());
    }

    /// Append a u32 value (little-endian)
    pub fn append_u32(&mut self, val: u32) {
        self.body.extend_from_slice(&val.to_le_bytes());
    }

    /// Append a u64 value (little-endian)
    pub fn append_u64(&mut self, val: u64) {
        self.body.extend_from_slice(&val.to_le_bytes());
    }

    /// Append a Generic Address Structure (GAS, 12 bytes)
    ///
    /// # Arguments
    /// * `space_id` - Address Space ID (0=SystemMemory, 1=SystemIO)
    /// * `address` - 64-bit address
    /// * `access_size` - Access size (0=Undefined, 1=Byte, 2=Word, 3=DWord, 4=QWord)
    pub fn append_gas(&mut self, space_id: u8, address: u64, access_size: u8) {
        self.append_u8(space_id); // Address Space ID
        self.append_u8(0); // Register Bit Width (filled by OSPM)
        self.append_u8(0); // Register Bit Offset
        self.append_u8(access_size); // Access Size
        self.append_u64(address); // Address
    }

    /// Build the complete SDT with header and checksum
    pub fn build(self) -> Vec<u8> {
        let mut table = Vec::with_capacity(self.length as usize);

        // SDT Header (36 bytes)
        // Signature (4 bytes)
        table.extend_from_slice(&self.signature);
        // Length (4 bytes)
        table.extend_from_slice(&self.length.to_le_bytes());
        // Revision (1 byte)
        table.push(2); // ACPI 2.0
        // Checksum (1 byte, filled later)
        table.push(0);
        // OEM ID (6 bytes)
        table.extend_from_slice(&OEM_ID);
        // OEM Table ID (8 bytes)
        table.extend_from_slice(&OEM_TABLE_ID);
        // OEM Revision (4 bytes)
        table.extend_from_slice(&1u32.to_le_bytes());
        // Creator ID (4 bytes)
        table.extend_from_slice(&CREATOR_ID.to_le_bytes());
        // Creator Revision (4 bytes)
        table.extend_from_slice(&CREATOR_REVISION.to_le_bytes());

        // Body data
        table.extend_from_slice(&self.body);

        // Pad to declared length if needed
        while table.len() < self.length as usize {
            table.push(0);
        }

        // Truncate if body was too long
        table.truncate(self.length as usize);

        // Compute checksum over entire table
        let sum = table.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        table[9] = (0u8).wrapping_sub(sum);

        table
    }
}
