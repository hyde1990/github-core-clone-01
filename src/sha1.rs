//! SHA-1 (FIPS 180-1), incremental. Git object ids and pack trailers
//! depend on it, so this is on the hot path for pushes and clones.

pub struct Sha1 {
    state: [u32; 5],
    total_len: u64,
    block: [u8; 64],
    block_len: usize,
}

impl Sha1 {
    pub fn new() -> Self {
        Sha1 {
            state: [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0],
            total_len: 0,
            block: [0; 64],
            block_len: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        if self.block_len > 0 {
            let want = 64 - self.block_len;
            let take = want.min(data.len());
            self.block[self.block_len..self.block_len + take].copy_from_slice(&data[..take]);
            self.block_len += take;
            data = &data[take..];
            if self.block_len == 64 {
                let block = self.block;
                self.compress(&block);
                self.block_len = 0;
            }
        }
        while data.len() >= 64 {
            let (head, tail) = data.split_at(64);
            let mut block = [0u8; 64];
            block.copy_from_slice(head);
            self.compress(&block);
            data = tail;
        }
        if !data.is_empty() {
            self.block[..data.len()].copy_from_slice(data);
            self.block_len = data.len();
        }
    }

    pub fn finish(mut self) -> [u8; 20] {
        let bit_len = self.total_len.wrapping_mul(8);
        self.update(&[0x80]);
        while self.block_len != 56 {
            self.update(&[0]);
        }
        self.update(&bit_len.to_be_bytes());
        let mut digest = [0u8; 20];
        for (i, word) in self.state.iter().enumerate() {
            digest[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        digest
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut schedule = [0u32; 80];
        for i in 0..16 {
            schedule[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            schedule[i] = (schedule[i - 3] ^ schedule[i - 8] ^ schedule[i - 14] ^ schedule[i - 16])
                .rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (
            self.state[0],
            self.state[1],
            self.state[2],
            self.state[3],
            self.state[4],
        );
        for (i, &word) in schedule.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
    }
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

/// SHA-1 of a whole slice (test helper: production code streams via `Sha1`).
#[cfg(test)]
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(data);
    hasher.finish()
}

/// A git object id: SHA-1 of `<type> <size>\0<data>`.
pub fn hash_object(object_type: &str, data: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(object_type.as_bytes());
    hasher.update(b" ");
    hasher.update(data.len().to_string().as_bytes());
    hasher.update(&[0]);
    hasher.update(data);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::hex_encode;

    #[test]
    fn sha1_vectors() {
        assert_eq!(
            hex_encode(&sha1(b"")),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            hex_encode(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex_encode(&sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        assert_eq!(
            hex_encode(&sha1(&vec![b'a'; 1_000_000])),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    #[test]
    fn sha1_incremental_matches_oneshot() {
        let data: Vec<u8> = (0..10_000).map(|i| (i * 7) as u8).collect();
        let mut incremental = Sha1::new();
        for chunk in data.chunks(137) {
            incremental.update(chunk);
        }
        assert_eq!(incremental.finish(), sha1(&data));
    }

    #[test]
    fn hash_object_known_blob() {
        // `printf hello | git hash-object --stdin`
        assert_eq!(
            hex_encode(&hash_object("blob", b"hello")),
            "b6fc4c620b67d95f953a5c1c1230aaab5db5a1b0"
        );
    }
}
