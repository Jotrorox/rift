//! The blocks of every loaded column as the client has them, for the
//! connection rules. States that no rule tells apart are stored as one
//! representative (see [`super::connect::Info::stored`]), so most sections
//! hold a handful of distinct values and pack into a few bits per block;
//! uniform sections (all stone, all air) take no space for their blocks.
use super::chunk::SECTION_BLOCKS;
use std::collections::HashMap;

/// One section: a palette and packed indices into it, `bits` wide, without
/// spanning longs; no indices while the palette has one entry.
#[derive(Clone, Debug)]
pub struct Packed {
    palette: Vec<u16>,
    bits: u32,
    data: Vec<u64>,
}

impl Packed {
    pub fn uniform(state: u16) -> Self {
        Self {
            palette: vec![state],
            bits: 0,
            data: Vec::new(),
        }
    }

    pub fn from_states(states: &[u16; SECTION_BLOCKS]) -> Self {
        let mut packed = Self::uniform(states[0]);
        for (index, state) in states.iter().enumerate() {
            packed.set(index, *state);
        }
        packed
    }

    fn per_long(bits: u32) -> usize {
        (64 / bits) as usize
    }

    pub fn get(&self, index: usize) -> u16 {
        if self.bits == 0 {
            return self.palette[0];
        }
        let per_long = Self::per_long(self.bits);
        let value = (self.data[index / per_long] >> ((index % per_long) as u32 * self.bits))
            & ((1 << self.bits) - 1);
        self.palette[value as usize]
    }

    pub fn set(&mut self, index: usize, state: u16) {
        let entry = match self.palette.iter().position(|known| *known == state) {
            Some(entry) => entry,
            None => {
                self.palette.push(state);
                let needed = usize::BITS - (self.palette.len() - 1).leading_zeros();
                if needed > self.bits {
                    self.resize(needed);
                }
                self.palette.len() - 1
            }
        };
        if self.bits == 0 {
            return;
        }
        let per_long = Self::per_long(self.bits);
        let shift = (index % per_long) as u32 * self.bits;
        let mask = ((1u64 << self.bits) - 1) << shift;
        let word = &mut self.data[index / per_long];
        *word = (*word & !mask) | ((entry as u64) << shift);
    }

    fn resize(&mut self, bits: u32) {
        let old = std::mem::replace(
            self,
            Self {
                palette: Vec::new(),
                bits,
                data: vec![0; SECTION_BLOCKS.div_ceil(Self::per_long(bits))],
            },
        );
        self.palette = old.palette.clone();
        if old.bits == 0 {
            return; // Every index is 0, the first entry.
        }
        let per_long = Self::per_long(bits);
        for index in 0..SECTION_BLOCKS {
            let state = old.get(index);
            let entry = self
                .palette
                .iter()
                .position(|known| *known == state)
                .unwrap_or(0);
            let shift = (index % per_long) as u32 * bits;
            self.data[index / per_long] |= (entry as u64) << shift;
        }
    }

    /// Approximate heap use, for tests.
    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.palette.len() * 2 + self.data.len() * 8
    }
}

/// Loaded columns; a missing section is air.
#[derive(Default)]
pub struct Store {
    columns: HashMap<(i32, i32), [Option<Packed>; 16]>,
}

impl Store {
    pub fn clear(&mut self) {
        self.columns.clear();
    }

    pub fn has_column(&self, x: i32, z: i32) -> bool {
        self.columns.contains_key(&(x, z))
    }

    pub fn load(&mut self, x: i32, z: i32) -> &mut [Option<Packed>; 16] {
        self.columns.entry((x, z)).or_default()
    }

    pub fn unload(&mut self, x: i32, z: i32) {
        self.columns.remove(&(x, z));
    }

    /// The stored state at a block position; `None` outside loaded columns
    /// and the world's height.
    pub fn get(&self, x: i32, y: i32, z: i32) -> Option<u16> {
        if !(0..256).contains(&y) {
            return None;
        }
        let column = self.columns.get(&(x >> 4, z >> 4))?;
        Some(column[(y >> 4) as usize].as_ref().map_or(0, |section| {
            section.get(((y & 15) << 8 | (z & 15) << 4 | (x & 15)) as usize)
        }))
    }

    /// Store a state; ignored outside loaded columns.
    pub fn set(&mut self, x: i32, y: i32, z: i32, state: u16) {
        if !(0..256).contains(&y) {
            return;
        }
        let Some(column) = self.columns.get_mut(&(x >> 4, z >> 4)) else {
            return;
        };
        let section = column[(y >> 4) as usize].get_or_insert_with(|| Packed::uniform(0));
        section.set(((y & 15) << 8 | (z & 15) << 4 | (x & 15)) as usize, state);
    }

    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.columns
            .values()
            .flat_map(|column| column.iter().flatten())
            .map(Packed::bytes)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_sections_grow_and_keep_their_states() {
        let mut packed = Packed::uniform(7);
        assert_eq!(packed.get(4095), 7);
        let mut expected = [7u16; SECTION_BLOCKS];
        for index in (0..SECTION_BLOCKS).step_by(7) {
            let state = (index % 300) as u16;
            packed.set(index, state);
            expected[index] = state;
        }
        for (index, state) in expected.iter().enumerate() {
            assert_eq!(packed.get(index), *state, "{index}");
        }
        assert_eq!(Packed::from_states(&expected).bytes(), packed.bytes());
        let uniform = Packed::from_states(&[3; SECTION_BLOCKS]);
        assert_eq!(uniform.bytes(), 2);
    }
}
