//! BLAKE3, the hash of the contents of files when they are stamped by what they hold rather than when they were
//! modified.
//!
//! This is the portable form of the reference implementation: the input is split into chunks of 1024 bytes, each
//! compressed a block of 64 bytes at a time, and the chaining values of the chunks are merged into a binary tree,
//! whose root gives the 32-byte hash. It is checked against the official test vectors.

const OUT_LEN: usize = 32;
const BLOCK_LEN: usize = 64;
const CHUNK_LEN: usize = 1024;

const CHUNK_START: u32 = 1;
const CHUNK_END: u32 = 2;
const PARENT: u32 = 4;
const ROOT: u32 = 8;

const IV: [u32; 8] =
    [0x6A09_E667, 0xBB67_AE85, 0x3C6E_F372, 0xA54F_F53A, 0x510E_527F, 0x9B05_688C, 0x1F83_D9AB, 0x5BE0_CD19];

const MSG_PERMUTATION: [usize; 16] = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8];

/// The quarter-round, which mixes two message words, `mx` and `my`, into the four words of the state at `ia`, `ib`,
/// `ic` and `id`.
fn g(state: &mut [u32; 16], [ia, ib, ic, id]: [usize; 4], mx: u32, my: u32) {
    state[ia] = state[ia].wrapping_add(state[ib]).wrapping_add(mx);
    state[id] = (state[id] ^ state[ia]).rotate_right(16);
    state[ic] = state[ic].wrapping_add(state[id]);
    state[ib] = (state[ib] ^ state[ic]).rotate_right(12);
    state[ia] = state[ia].wrapping_add(state[ib]).wrapping_add(my);
    state[id] = (state[id] ^ state[ia]).rotate_right(8);
    state[ic] = state[ic].wrapping_add(state[id]);
    state[ib] = (state[ib] ^ state[ic]).rotate_right(7);
}

/// A round, which mixes the columns of the state, then its diagonals.
fn round(state: &mut [u32; 16], m: &[u32; 16]) {
    g(state, [0, 4, 8, 12], m[0], m[1]);
    g(state, [1, 5, 9, 13], m[2], m[3]);
    g(state, [2, 6, 10, 14], m[4], m[5]);
    g(state, [3, 7, 11, 15], m[6], m[7]);
    g(state, [0, 5, 10, 15], m[8], m[9]);
    g(state, [1, 6, 11, 12], m[10], m[11]);
    g(state, [2, 7, 8, 13], m[12], m[13]);
    g(state, [3, 4, 9, 14], m[14], m[15]);
}

/// The compression function, of a chaining value and a block.
fn compress(cv: &[u32; 8], block: &[u32; 16], counter: u64, block_len: u32, flags: u32) -> [u32; 16] {
    let mut state = [
        cv[0],
        cv[1],
        cv[2],
        cv[3],
        cv[4],
        cv[5],
        cv[6],
        cv[7],
        IV[0],
        IV[1],
        IV[2],
        IV[3],
        counter as u32,
        (counter >> 32) as u32,
        block_len,
        flags,
    ];
    let mut message = *block;
    for i in 0..7 {
        round(&mut state, &message);
        if i < 6 {
            message = std::array::from_fn(|j| message[MSG_PERMUTATION[j]]);
        }
    }
    for i in 0..8 {
        state[i] ^= state[i + 8];
        state[i + 8] ^= cv[i];
    }
    state
}

/// Returns the words of a block, little-endian.
fn words(block: &[u8; BLOCK_LEN]) -> [u32; 16] {
    std::array::from_fn(|i| u32::from_le_bytes([block[i * 4], block[i * 4 + 1], block[i * 4 + 2], block[i * 4 + 3]]))
}

fn first_8(words: &[u32; 16]) -> [u32; 8] {
    std::array::from_fn(|i| words[i])
}

/// What a chunk or parent node gives, which is a chaining value, or the root's output.
struct Output {
    input_cv: [u32; 8],
    block: [u32; 16],
    counter: u64,
    block_len: u32,
    flags: u32,
}
impl Output {
    fn chaining_value(&self) -> [u32; 8] {
        first_8(&compress(&self.input_cv, &self.block, self.counter, self.block_len, self.flags))
    }
    fn root_hash(&self) -> [u8; OUT_LEN] {
        let words = compress(&self.input_cv, &self.block, 0, self.block_len, self.flags | ROOT);
        let mut hash = [0; OUT_LEN];
        for (bytes, word) in hash.chunks_exact_mut(4).zip(words) {
            bytes.copy_from_slice(&word.to_le_bytes());
        }
        hash
    }
}

/// The state of the chunk which is being compressed.
struct Chunk {
    cv: [u32; 8],
    counter: u64,
    block: [u8; BLOCK_LEN],
    block_len: usize,
    blocks_compressed: usize,
}
impl Chunk {
    fn new(counter: u64) -> Self {
        Self { cv: IV, counter, block: [0; BLOCK_LEN], block_len: 0, blocks_compressed: 0 }
    }
    fn len(&self) -> usize {
        BLOCK_LEN * self.blocks_compressed + self.block_len
    }
    fn start_flag(&self) -> u32 {
        if self.blocks_compressed == 0 { CHUNK_START } else { 0 }
    }
    fn update(&mut self, mut input: &[u8]) {
        while !input.is_empty() {
            // a full block is compressed only once more input follows, as the last block of a chunk is its output
            if self.block_len == BLOCK_LEN {
                let block = words(&self.block);
                self.cv = first_8(&compress(&self.cv, &block, self.counter, BLOCK_LEN as u32, self.start_flag()));
                self.blocks_compressed += 1;
                self.block = [0; BLOCK_LEN];
                self.block_len = 0;
            }
            let take = (BLOCK_LEN - self.block_len).min(input.len());
            self.block[self.block_len..self.block_len + take].copy_from_slice(&input[..take]);
            self.block_len += take;
            input = &input[take..];
        }
    }
    fn output(&self) -> Output {
        Output {
            input_cv: self.cv,
            block: words(&self.block),
            counter: self.counter,
            block_len: self.block_len as u32,
            flags: self.start_flag() | CHUNK_END,
        }
    }
}

fn parent_output(left: &[u32; 8], right: &[u32; 8]) -> Output {
    let block = std::array::from_fn(|i| if i < 8 { left[i] } else { right[i - 8] });
    Output { input_cv: IV, block, counter: 0, block_len: BLOCK_LEN as u32, flags: PARENT }
}

/// A BLAKE3 hasher, which takes its input in parts.
pub struct Hasher {
    chunk: Chunk,
    /// The chaining values of the subtrees which are complete, from the left.
    stack: Vec<[u32; 8]>,
}
impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}
impl Hasher {
    #[must_use]
    pub fn new() -> Self {
        Self { chunk: Chunk::new(0), stack: Vec::new() }
    }
    /// Add a completed chunk's chaining value, merging the subtrees it completes, as many as the trailing zeros of the
    /// number of chunks so far.
    fn add_chunk(&mut self, mut cv: [u32; 8], mut chunks: u64) {
        while chunks & 1 == 0 {
            let left = self.stack.pop().unwrap_or(IV);
            cv = parent_output(&left, &cv).chaining_value();
            chunks >>= 1;
        }
        self.stack.push(cv);
    }
    pub fn update(&mut self, mut input: &[u8]) {
        while !input.is_empty() {
            // a full chunk is finished only once more input follows, as the last chunk may be the root
            if self.chunk.len() == CHUNK_LEN {
                let cv = self.chunk.output().chaining_value();
                let chunks = self.chunk.counter + 1;
                self.add_chunk(cv, chunks);
                self.chunk = Chunk::new(chunks);
            }
            let take = (CHUNK_LEN - self.chunk.len()).min(input.len());
            self.chunk.update(&input[..take]);
            input = &input[take..];
        }
    }
    #[must_use]
    pub fn finalize(&self) -> [u8; OUT_LEN] {
        let mut output = self.chunk.output();
        for left in self.stack.iter().rev() {
            output = parent_output(left, &output.chaining_value());
        }
        output.root_hash()
    }
}

/// Returns the BLAKE3 hash of bytes.
#[must_use]
pub fn hash(input: &[u8]) -> [u8; OUT_LEN] {
    let mut hasher = Hasher::new();
    hasher.update(input);
    hasher.finalize()
}

/// Returns the BLAKE3 hash of the contents of a file, read in parts.
///
/// # Errors
///
/// If the file cannot be read.
pub fn hash_file(path: &std::path::Path) -> std::io::Result<[u8; OUT_LEN]> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Hasher::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => return Ok(hasher.finalize()),
            Ok(read) => hasher.update(&buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => (),
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(hash: &[u8]) -> String {
        use std::fmt::Write as _;
        hash.iter().fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
    }

    /// The input of the official test vectors, a repeating sequence of the bytes `0` to `250`.
    fn input(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// The official test vectors, from BLAKE3's `test_vectors.json`: the length of the input, and its hash.
    const VECTORS: &[(usize, &str)] = &[
        (0, "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"),
        (1, "2d3adedff11b61f14c886e35afa036736dcd87a74d27b5c1510225d0f592e213"),
        (8, "2351207d04fc16ade43ccab08600939c7c1fa70a5c0aaca76063d04c3228eaeb"),
        (63, "e9bc37a594daad83be9470df7f7b3798297c3d834ce80ba85d6e207627b7db7b"),
        (64, "4eed7141ea4a5cd4b788606bd23f46e212af9cacebacdc7d1f4c6dc7f2511b98"),
        (65, "de1e5fa0be70df6d2be8fffd0e99ceaa8eb6e8c93a63f2d8d1c30ecb6b263dee"),
        (1023, "10108970eeda3eb932baac1428c7a2163b0e924c9a9e25b35bba72b28f70bd11"),
        (1024, "42214739f095a406f3fc83deb889744ac00df831c10daa55189b5d121c855af7"),
        (1025, "d00278ae47eb27b34faecf67b4fe263f82d5412916c1ffd97c8cb7fb814b8444"),
        (2048, "e776b6028c7cd22a4d0ba182a8bf62205d2ef576467e838ed6f2529b85fba24a"),
        (2049, "5f4d72f40d7a5f82b15ca2b2e44b1de3c2ef86c426c95c1af0b6879522563030"),
        (3072, "b98cb0ff3623be03326b373de6b9095218513e64f1ee2edd2525c7ad1e5cffd2"),
        (3073, "7124b49501012f81cc7f11ca069ec9226cecb8a2c850cfe644e327d22d3e1cd3"),
        (4096, "015094013f57a5277b59d8475c0501042c0b642e531b0a1c8f58d2163229e969"),
        (4097, "9b4052b38f1c5fc8b1f9ff7ac7b27cd242487b3d890d15c96a1c25b8aa0fb995"),
        (5121, "628bd2cb2004694adaab7bbd778a25df25c47b9d4155a55f8fbd79f2fe154cff"),
        (7169, "a003fc7a51754a9b3c7fae0367ab3d782dccf28855a03d435f8cfe74605e7817"),
        (8193, "bab6c09cb8ce8cf459261398d2e7aef35700bf488116ceb94a36d0f5f1b7bc3b"),
        (16384, "f875d6646de28985646f34ee13be9a576fd515f76b5b0a26bb324735041ddde4"),
        (31744, "62b6960e1a44bcc1eb1a611a8d6235b6b4b78f32e7abc4fb4c6cdcce94895c47"),
        (102_400, "bc3e3d41a1146b069abffad3c0d44860cf664390afce4d9661f7902e7943e085"),
    ];

    #[test]
    fn matches_the_test_vectors() {
        for &(len, expected) in VECTORS {
            assert_eq!(hex(&hash(&input(len))), expected, "for {len} bytes");
        }
    }

    #[test]
    fn takes_its_input_in_parts() {
        // in parts which straddle the blocks and chunks, as a file is read
        let input = input(102_400);
        for part in [1, 63, 64, 65, 1000, 1024, 1025, 4096] {
            let mut hasher = Hasher::new();
            input.chunks(part).for_each(|part| hasher.update(part));
            assert_eq!(hasher.finalize(), hash(&input), "in parts of {part}");
        }
    }

    #[test]
    fn hashes_files() {
        let path = std::env::temp_dir().join(format!("rig-blake3-{}", std::process::id()));
        std::fs::write(&path, input(200_000)).unwrap();
        assert_eq!(hash_file(&path).unwrap(), hash(&input(200_000)));
        let _ = std::fs::remove_file(&path);
        assert!(hash_file(&path).is_err());
    }
}
