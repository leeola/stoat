use std::hash::{BuildHasherDefault, Hasher};

/// [`std::hash::BuildHasher`] that produces [`FxHasher`], for use as the third
/// type parameter of a [`std::collections::HashMap`].
pub(crate) type FxBuildHasher = BuildHasherDefault<FxHasher>;

const SEED: usize = 0x51_7c_c1_b7_27_22_0a_95;
const ROTATE: u32 = 5;
const WORD_SIZE: usize = size_of::<usize>();

/// Non-cryptographic hasher folding each machine word with a rotate, xor, and
/// multiply.
///
/// The Dijkstra seen-map is looked up once per successor on the hottest path in
/// the diff, and its keys are small tuples of integers. This trades the default
/// hasher's collision resistance for a few arithmetic ops per key, which is the
/// right bargain for a map that is never exposed to adversarial input.
#[derive(Default)]
pub(crate) struct FxHasher {
    hash: usize,
}

impl FxHasher {
    fn add(&mut self, word: usize) {
        self.hash = (self.hash.rotate_left(ROTATE) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(WORD_SIZE) {
            let mut word = [0u8; WORD_SIZE];
            word[..chunk.len()].copy_from_slice(chunk);
            self.add(usize::from_le_bytes(word));
        }
    }

    fn write_u8(&mut self, i: u8) {
        self.add(i as usize);
    }

    fn write_u16(&mut self, i: u16) {
        self.add(i as usize);
    }

    fn write_u32(&mut self, i: u32) {
        self.add(i as usize);
    }

    fn write_u64(&mut self, i: u64) {
        self.add(i as usize);
    }

    fn write_usize(&mut self, i: usize) {
        self.add(i);
    }

    fn finish(&self) -> u64 {
        self.hash as u64
    }
}

#[cfg(test)]
mod tests {
    use super::FxHasher;
    use std::hash::{Hash, Hasher};

    fn hash_of<T: Hash>(value: &T) -> u64 {
        let mut hasher = FxHasher::default();
        value.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn same_input_hashes_stably_field_order_matters() {
        let key = (Some(7usize), None::<usize>, 3u8);
        assert_eq!(hash_of(&key), hash_of(&key), "same key hashes identically");
        assert_ne!(
            hash_of(&(1usize, 2usize)),
            hash_of(&(2usize, 1usize)),
            "field order changes the hash"
        );
    }
}
