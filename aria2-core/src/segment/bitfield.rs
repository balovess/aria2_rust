use crate::error::{Aria2Error, Result};

pub struct Bitfield {
    bits: Vec<u8>,
    num_bits: usize,
}

impl Bitfield {
    pub fn new(num_bits: usize) -> Self {
        let num_bytes = (num_bits + 7) / 8;
        Bitfield {
            bits: vec![0u8; num_bytes],
            num_bits,
        }
    }

    pub fn from_bytes(data: &[u8], num_bits: usize) -> Self {
        let mut bitfield = Bitfield::new(num_bits);
        let copy_len = std::cmp::min(data.len(), bitfield.bits.len());
        if copy_len > 0 {
            bitfield.bits[..copy_len].copy_from_slice(&data[..copy_len]);
        }
        bitfield
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bits
    }

    pub fn len(&self) -> usize {
        self.num_bits
    }

    pub fn is_empty(&self) -> bool {
        self.num_bits == 0
    }

    pub fn set(&mut self, index: usize) -> Result<()> {
        if index >= self.num_bits {
            return Err(Aria2Error::DownloadFailed(format!(
                "Bitfield索引超出范围: {} >= {}",
                index, self.num_bits
            )));
        }

        let byte_index = index / 8;
        let bit_offset = index % 8;
        self.bits[byte_index] |= 1 << (7 - bit_offset);
        Ok(())
    }

    pub fn unset(&mut self, index: usize) -> Result<()> {
        if index >= self.num_bits {
            return Err(Aria2Error::DownloadFailed(format!(
                "Bitfield索引超出范围: {} >= {}",
                index, self.num_bits
            )));
        }

        let byte_index = index / 8;
        let bit_offset = index % 8;
        self.bits[byte_index] &= !(1 << (7 - bit_offset));
        Ok(())
    }

    pub fn get(&self, index: usize) -> bool {
        if index >= self.num_bits {
            return false;
        }

        let byte_index = index / 8;
        let bit_offset = index % 8;
        (self.bits[byte_index] & (1 << (7 - bit_offset))) != 0
    }

    pub fn is_all_set(&self) -> bool {
        let full_bytes = self.num_bits / 8;
        let remaining_bits = self.num_bits % 8;

        for i in 0..full_bytes {
            if self.bits[i] != 0xFF {
                return false;
            }
        }

        if remaining_bits > 0 {
            let mask = ((1u8 << remaining_bits) - 1) << (8 - remaining_bits);
            self.bits[full_bytes] == mask
        } else {
            true
        }
    }

    pub fn is_all_unset(&self) -> bool {
        for byte in &self.bits {
            if *byte != 0x00 {
                return false;
            }
        }
        true
    }

    pub fn find_first_unset(&self) -> Option<usize> {
        for (byte_index, byte) in self.bits.iter().enumerate() {
            if *byte != 0xFF {
                for bit_offset in 0..8 {
                    let index = byte_index * 8 + bit_offset;
                    if index < self.num_bits && !self.get(index) {
                        return Some(index);
                    }
                }
            }
        }
        None
    }

    pub fn find_first_set(&self) -> Option<usize> {
        for (byte_index, byte) in self.bits.iter().enumerate() {
            if *byte != 0x00 {
                for bit_offset in 0..8 {
                    let index = byte_index * 8 + bit_offset;
                    if index < self.num_bits && self.get(index) {
                        return Some(index);
                    }
                }
            }
        }
        None
    }

    pub fn count_set_bits(&self) -> usize {
        let mut count = 0;
        for i in 0..self.num_bits {
            if self.get(i) {
                count += 1;
            }
        }
        count
    }

    pub fn count_unset_bits(&self) -> usize {
        self.num_bits - self.count_set_bits()
    }
}
