//! The encoding of rig's files and messages, and jig's database: numbers in little-endian, a length as a `u32`, a
//! string as its length and its UTF-8, and a list as its length and its items.

/// Bytes being written.
#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    pub fn bytes(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
    pub fn u8(&mut self, value: u8) {
        self.0.push(value);
    }
    pub fn u32(&mut self, value: u32) {
        self.bytes(&value.to_le_bytes());
    }
    pub fn u64(&mut self, value: u64) {
        self.bytes(&value.to_le_bytes());
    }
    pub fn i64(&mut self, value: i64) {
        self.bytes(&value.to_le_bytes());
    }
    /// Write a length, which is kept within a `u32`.
    ///
    /// # Panics
    ///
    /// If it is not.
    pub fn len(&mut self, len: usize) {
        self.u32(u32::try_from(len).expect("a length within a u32"));
    }
    pub fn str(&mut self, string: &str) {
        self.len(string.len());
        self.bytes(string.as_bytes());
    }
    pub fn strs<S: AsRef<str>>(&mut self, strings: &[S]) {
        self.len(strings.len());
        for string in strings {
            self.str(string.as_ref());
        }
    }
    pub fn hashes(&mut self, hashes: &[crate::cache::Hash]) {
        self.len(hashes.len());
        for hash in hashes {
            self.bytes(hash);
        }
    }
}

/// The bytes which remain to be read, each read of which is `None` if they end first, or are not what is read.
pub struct Reader<'a>(pub &'a [u8]);

impl<'a> Reader<'a> {
    pub fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let (taken, rest) = self.0.split_at_checked(len)?;
        self.0 = rest;
        Some(taken)
    }
    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }
    pub fn u8(&mut self) -> Option<u8> {
        Some(self.array::<1>()?[0])
    }
    /// Read a flag, a byte of `0` or `1`.
    pub fn bool(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
    pub fn u32(&mut self) -> Option<u32> {
        self.array().map(u32::from_le_bytes)
    }
    pub fn u64(&mut self) -> Option<u64> {
        self.array().map(u64::from_le_bytes)
    }
    pub fn i64(&mut self) -> Option<i64> {
        self.array().map(i64::from_le_bytes)
    }
    pub fn len(&mut self) -> Option<usize> {
        self.u32().map(|len| len as usize)
    }
    pub fn hash(&mut self) -> Option<crate::cache::Hash> {
        self.array()
    }
    pub fn str(&mut self) -> Option<&'a str> {
        let len = self.len()?;
        std::str::from_utf8(self.take(len)?).ok()
    }
    pub fn string(&mut self) -> Option<String> {
        self.str().map(String::from)
    }
    pub fn strs(&mut self) -> Option<Vec<&'a str>> {
        self.list(Self::str)
    }
    pub fn strings(&mut self) -> Option<Vec<String>> {
        self.list(Self::string)
    }
    pub fn hashes(&mut self) -> Option<Vec<crate::cache::Hash>> {
        self.list(Self::hash)
    }
    /// Read a list, as its length and each item, as `item` reads it.
    pub fn list<T>(&mut self, mut item: impl FnMut(&mut Self) -> Option<T>) -> Option<Vec<T>> {
        (0..self.len()?).map(|_| item(self)).collect()
    }
    /// Whether every byte has been read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut w = Writer::new();
        w.u8(7);
        w.u8(1);
        w.u32(70_000);
        w.u64(u64::MAX);
        w.i64(-1);
        w.str("é");
        w.strs(&["a", ""]);
        w.hashes(&[[1; 32]]);
        let mut r = Reader(&w.0);
        assert_eq!(r.u8(), Some(7));
        assert_eq!(r.bool(), Some(true));
        assert_eq!(r.u32(), Some(70_000));
        assert_eq!(r.u64(), Some(u64::MAX));
        assert_eq!(r.i64(), Some(-1));
        assert_eq!(r.str(), Some("é"));
        assert_eq!(r.strings(), Some(vec!["a".to_string(), String::new()]));
        assert_eq!(r.hashes(), Some(vec![[1; 32]]));
        assert!(r.is_empty());
        // reading past the end, or what is not UTF-8, fails
        assert_eq!(r.u8(), None);
        assert_eq!(Reader(&[1, 0, 0, 0, 0xff]).str(), None);
        assert_eq!(Reader(&[2, 0, 0, 0, b'a']).str(), None);
        assert_eq!(Reader(&[2]).bool(), None);
    }
}
