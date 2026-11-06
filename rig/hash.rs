//! A fast hasher for the maps of paths a build looks up many times, as loading a graph and its state does: `FxHash`, as
//! rustc uses, which takes eight bytes at a time.
//!
//! It is not resistant to collisions chosen by an attacker, which a build's own paths are not.

use std::hash::{BuildHasherDefault, Hasher};

/// A map with the fast hasher.
pub type Map<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<Fx>>;

/// `FxHash`, which mixes each word into the hash by a rotation, an exclusive or and a multiplication.
#[derive(Clone, Copy, Default)]
pub struct Fx(u64);

impl Fx {
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

impl Hasher for Fx {
    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for &word in words {
            self.add(u64::from_le_bytes(word));
        }
        if !rest.is_empty() {
            let mut word = [0; 8];
            word[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(word));
        }
    }
    fn write_u8(&mut self, value: u8) {
        self.add(u64::from(value));
    }
    fn write_u32(&mut self, value: u32) {
        self.add(u64::from(value));
    }
    fn write_u64(&mut self, value: u64) {
        self.add(value);
    }
    fn write_usize(&mut self, value: usize) {
        self.add(value as u64);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::{BuildHasher, Hash};

    fn hash(value: &impl Hash) -> u64 {
        BuildHasherDefault::<Fx>::default().hash_one(value)
    }

    #[test]
    fn hashes_by_every_byte() {
        // paths which differ only past a word, or in their length, differ
        assert_ne!(hash(&"build/x86_64/liba.rlib"), hash(&"build/x86_64/libb.rlib"));
        assert_ne!(hash(&"abcdefgh"), hash(&"abcdefgh\\0"));
        assert_eq!(hash(&"same"), hash(&String::from("same")));
        let mut map = Map::default();
        for i in 0..1000 {
            map.insert(format!("src/f{i}.rs"), i);
        }
        assert!((0..1000).all(|i| map[&format!("src/f{i}.rs")] == i));
    }
}
