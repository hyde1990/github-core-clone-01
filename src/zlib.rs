//! zlib (RFC 1950) over DEFLATE (RFC 1951), pure Rust, no crates.
//!
//! `inflate` is a full decoder: stored, fixed-Huffman and dynamic-Huffman
//! blocks. `deflate` emits stored blocks only — always valid, and the
//! fastest correct choice for the small objects this forge packs (the
//! loopback does not care about a few hundred extra bytes, and the client
//! inflates with Node's zlib either way).

/// Error text for a malformed stream.
pub type ZlibError = &'static str;

/// Inflate a whole zlib stream. Verifies the header and Adler-32 trailer.
/// Inflate a whole zlib stream (test helper: production code uses
/// `inflate_parts`, which also reports how many bytes it consumed).
#[cfg(test)]
pub fn inflate(data: &[u8]) -> Result<Vec<u8>, ZlibError> {
    inflate_parts(data).map(|(out, _)| out)
}

/// Inflate a zlib stream, returning the output and the total bytes consumed
/// (2-byte header + deflate stream + 4-byte Adler-32 trailer).
pub fn inflate_parts(data: &[u8]) -> Result<(Vec<u8>, usize), ZlibError> {
    if data.len() < 2 {
        return Err("zlib stream too short");
    }
    let cmf = data[0];
    let flg = data[1];
    if !(cmf as u32 * 256 + flg as u32).is_multiple_of(31) {
        return Err("bad zlib header check");
    }
    if cmf & 0x0f != 8 {
        return Err("not a deflate stream");
    }
    if flg & 0x20 != 0 {
        return Err("preset dictionary not supported");
    }
    let mut decoder = Decoder::new(&data[2..]);
    let out = decoder.run()?;
    let used = decoder.consumed();
    if data.len() < 2 + used + 4 {
        return Err("missing adler32 trailer");
    }
    let want = u32::from_be_bytes([
        data[2 + used],
        data[2 + used + 1],
        data[2 + used + 2],
        data[2 + used + 3],
    ]);
    if adler32(&out) != want {
        return Err("adler32 mismatch");
    }
    Ok((out, 2 + used + 4))
}

/// Deflate `data` as a zlib stream of stored blocks.
pub fn deflate(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 11);
    out.push(0x78);
    out.push(0x01);
    let mut chunks = data.chunks(65535).peekable();
    if data.is_empty() {
        // One empty final stored block.
        out.push(0x01);
        out.extend_from_slice(&[0, 0, 0xff, 0xff]);
    } else {
        while let Some(chunk) = chunks.next() {
            let last = chunks.peek().is_none();
            out.push(if last { 0x01 } else { 0x00 });
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// Adler-32 (mod 65521).
pub fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += byte as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

// ---------------------------------------------------------------------------
// Bit reader: bits arrive LSB-first out of each byte; a Huffman code's first
// bit is its most significant bit, so a peeked value holds the bit-reversed
// code.

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    count: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bits {
            data,
            pos: 0,
            buf: 0,
            count: 0,
        }
    }

    fn fill(&mut self) {
        while self.count <= 56 && self.pos < self.data.len() {
            self.buf |= (self.data[self.pos] as u64) << self.count;
            self.pos += 1;
            self.count += 8;
        }
    }

    /// One bit; 0 when the input is exhausted (padding — the decoder stops
    /// at the end-of-block symbol before this matters).
    fn bit(&mut self) -> u32 {
        if self.count == 0 {
            self.fill();
            if self.count == 0 {
                return 0;
            }
        }
        let bit = (self.buf & 1) as u32;
        self.buf >>= 1;
        self.count -= 1;
        bit
    }

    /// `n` bits, least-significant-first (for extra bits and stored lengths).
    fn bits(&mut self, n: u32) -> u32 {
        let mut value = 0u32;
        for i in 0..n {
            value |= self.bit() << i;
        }
        value
    }

    /// The next 9 bits without consuming them (missing bits read as 0).
    fn peek9(&mut self) -> usize {
        if self.count < 9 {
            self.fill();
        }
        (self.buf & 0x1ff) as usize
    }

    fn drop(&mut self, n: u32) {
        if n >= 64 {
            self.buf = 0;
            self.count = 0;
        } else {
            // Saturating: a truncated stream degrades to padding zeros
            // instead of panicking, and the decoder then fails on a bad code.
            self.buf >>= n;
            self.count = self.count.saturating_sub(n);
        }
    }

    /// Discard bits to the next byte boundary.
    fn align(&mut self) {
        let drop = self.count % 8;
        self.drop(drop);
    }

    /// The next whole byte after the current bit position, honouring
    /// buffered bytes first. Only valid after `align`.
    fn byte(&mut self) -> Option<u8> {
        if self.count >= 8 {
            let byte = (self.buf & 0xff) as u8;
            self.buf >>= 8;
            self.count -= 8;
            Some(byte)
        } else if self.pos < self.data.len() {
            let byte = self.data[self.pos];
            self.pos += 1;
            Some(byte)
        } else {
            None
        }
    }

    /// Whole bytes consumed from the input so far.
    fn consumed(&self) -> usize {
        self.pos - (self.count / 8) as usize
    }
}

fn reverse_bits(mut code: u32, len: u32) -> u32 {
    let mut reversed = 0u32;
    for _ in 0..len {
        reversed = (reversed << 1) | (code & 1);
        code >>= 1;
    }
    reversed
}

// ---------------------------------------------------------------------------
// Huffman tables: canonical codes, a 9-bit first-level lookup, and a
// bit-by-bit slow path for codes longer than 9 bits.

struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
    /// 512-entry lookup: (len << 10) | symbol, or 0xffff when the code is
    /// longer than 9 bits (slow path). Indexed by the bit-reversed 9-bit peek.
    table: [u16; 512],
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Self, ZlibError> {
        let mut counts = [0u16; 16];
        for &len in lengths {
            if len > 15 {
                return Err("bad code length");
            }
            counts[len as usize] += 1;
        }
        counts[0] = 0;
        // An all-zero table is legal (a distance table with no codes): it
        // simply never decodes, and any use of it errors in the slow path.
        let mut offsets = [0u16; 16];
        for len in 1..16 {
            offsets[len] = offsets[len - 1] + counts[len - 1];
        }
        // Symbols sorted by (length, code).
        let total: usize = counts.iter().map(|&c| c as usize).sum();
        let mut symbols = vec![0u16; total];
        let mut next = offsets;
        for (sym, &len) in lengths.iter().enumerate() {
            if len > 0 {
                symbols[next[len as usize] as usize] = sym as u16;
                next[len as usize] += 1;
            }
        }
        // First-level table.
        let mut table = [0xffffu16; 512];
        let mut first_code: u32 = 0;
        for len in 1..16usize {
            let count = counts[len] as u32;
            for i in 0..count {
                let code = first_code + i;
                if len <= 9 {
                    let reversed = reverse_bits(code, len as u32) as usize;
                    let sym = symbols[(offsets[len] + i as u16) as usize];
                    let step = 1usize << len;
                    let mut idx = reversed;
                    while idx < 512 {
                        table[idx] = ((len as u16) << 10) | sym;
                        idx += step;
                    }
                }
            }
            first_code = (first_code + count) << 1;
        }
        Ok(Huffman {
            counts,
            symbols,
            table,
        })
    }

    fn decode(&self, bits: &mut Bits) -> Result<u16, ZlibError> {
        let peek = bits.peek9();
        let entry = self.table[peek];
        if entry != 0xffff {
            let len = (entry >> 10) as u32;
            bits.drop(len);
            return Ok(entry & 0x3ff);
        }
        // Slow path: accumulate the code bit by bit (first bit = MSB).
        let mut code: u32 = 0;
        let mut first: u32 = 0;
        let mut index: u32 = 0;
        for len in 1..=15usize {
            code = (code << 1) | bits.bit();
            let count = self.counts[len] as u32;
            if code.wrapping_sub(first) < count {
                return Ok(self.symbols[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
        }
        Err("invalid huffman code")
    }
}

// ---------------------------------------------------------------------------
// The decoder.

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const CLEN_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

struct Decoder<'a> {
    bits: Bits<'a>,
    out: Vec<u8>,
}

impl<'a> Decoder<'a> {
    fn new(data: &'a [u8]) -> Self {
        Decoder {
            bits: Bits::new(data),
            out: Vec::new(),
        }
    }

    fn consumed(&self) -> usize {
        self.bits.consumed()
    }

    fn run(&mut self) -> Result<Vec<u8>, ZlibError> {
        loop {
            let last = self.bits.bit() == 1;
            let block_type = self.bits.bits(2);
            match block_type {
                0 => self.stored()?,
                1 => {
                    let (lit, dist) = fixed_tables();
                    self.compressed(&lit, &dist)?;
                }
                2 => {
                    let (lit, dist) = self.dynamic()?;
                    self.compressed(&lit, &dist)?;
                }
                _ => return Err("bad block type"),
            }
            if last {
                break;
            }
        }
        Ok(std::mem::take(&mut self.out))
    }

    fn stored(&mut self) -> Result<(), ZlibError> {
        self.bits.align();
        let b0 = self.bits.byte().ok_or("truncated stored block")?;
        let b1 = self.bits.byte().ok_or("truncated stored block")?;
        let b2 = self.bits.byte().ok_or("truncated stored block")?;
        let b3 = self.bits.byte().ok_or("truncated stored block")?;
        let len = u16::from_le_bytes([b0, b1]) as usize;
        let nlen = u16::from_le_bytes([b2, b3]);
        if len as u16 != !nlen {
            return Err("stored block length mismatch");
        }
        for _ in 0..len {
            let byte = self.bits.byte().ok_or("truncated stored block")?;
            self.out.push(byte);
        }
        Ok(())
    }

    fn dynamic(&mut self) -> Result<(Huffman, Huffman), ZlibError> {
        let hlit = self.bits.bits(5) as usize + 257;
        let hdist = self.bits.bits(5) as usize + 1;
        let hclen = self.bits.bits(4) as usize + 4;
        if hlit > 286 || hdist > 30 {
            return Err("bad dynamic header");
        }
        let mut clen = [0u8; 19];
        for i in 0..hclen {
            clen[CLEN_ORDER[i]] = self.bits.bits(3) as u8;
        }
        let cl = Huffman::new(&clen)?;
        let total = hlit + hdist;
        let mut lengths = vec![0u8; total];
        let mut i = 0;
        while i < total {
            let sym = cl.decode(&mut self.bits)?;
            match sym {
                0..=15 => {
                    lengths[i] = sym as u8;
                    i += 1;
                }
                16 => {
                    if i == 0 {
                        return Err("repeat with no previous length");
                    }
                    let prev = lengths[i - 1];
                    let rep = 3 + self.bits.bits(2) as usize;
                    if i + rep > total {
                        return Err("repeat overruns lengths");
                    }
                    for _ in 0..rep {
                        lengths[i] = prev;
                        i += 1;
                    }
                }
                17 => {
                    let rep = 3 + self.bits.bits(3) as usize;
                    if i + rep > total {
                        return Err("repeat overruns lengths");
                    }
                    i += rep;
                }
                18 => {
                    let rep = 11 + self.bits.bits(7) as usize;
                    if i + rep > total {
                        return Err("repeat overruns lengths");
                    }
                    i += rep;
                }
                _ => return Err("bad code-length symbol"),
            }
        }
        // A missing distance table means no distance codes are used; the
        // empty table errors if a match ever tries to use it.
        let lit = Huffman::new(&lengths[..hlit])?;
        let dist = Huffman::new(&lengths[hlit..])?;
        Ok((lit, dist))
    }

    fn compressed(&mut self, lit: &Huffman, dist: &Huffman) -> Result<(), ZlibError> {
        loop {
            let sym = lit.decode(&mut self.bits)?;
            match sym {
                0..=255 => self.out.push(sym as u8),
                256 => return Ok(()),
                257..=285 => {
                    let idx = (sym - 257) as usize;
                    let len = LEN_BASE[idx] as usize + self.bits.bits(LEN_EXTRA[idx]) as usize;
                    let dsym = dist.decode(&mut self.bits)?;
                    if dsym > 29 {
                        return Err("bad distance symbol");
                    }
                    let distance = DIST_BASE[dsym as usize] as usize
                        + self.bits.bits(DIST_EXTRA[dsym as usize]) as usize;
                    if distance == 0 || distance > self.out.len() {
                        return Err("distance too far back");
                    }
                    let mut remaining = len;
                    let mut from = self.out.len() - distance;
                    while remaining > 0 {
                        let chunk = remaining.min(distance);
                        self.out.extend_from_within(from..from + chunk);
                        remaining -= chunk;
                        from += chunk;
                    }
                }
                _ => return Err("bad literal/length symbol"),
            }
        }
    }
}

fn fixed_tables() -> (Huffman, Huffman) {
    let mut lit_len = [0u8; 288];
    for (i, entry) in lit_len.iter_mut().enumerate() {
        *entry = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let mut dist_len = [0u8; 30];
    for entry in dist_len.iter_mut() {
        *entry = 5;
    }
    // These tables are well-formed by construction.
    (
        Huffman::new(&lit_len).unwrap(),
        Huffman::new(&dist_len).unwrap(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Streams produced with Node's zlib (deflate, default level) so the
    // decoder is tested against a real compressor, not itself.
    const VEC_EMPTY: &str = "eJwDAAAAAAE=";
    const VEC_HELLO: &str = "eJzLSM3JyQcABiwCFQ==";
    const VEC_TEXT: &str =
        "eJwLyUhVKCzNTM5WSCrKL89TSMuvUMgqzS0oVsgvSy1SKMlIVchJrKpUSMlP11MIwVSsBwALphdp";

    fn from_b64(text: &str) -> Vec<u8> {
        crate::util::base64_decode(text).unwrap()
    }

    #[test]
    fn inflate_known_streams() {
        assert_eq!(inflate(&from_b64(VEC_EMPTY)).unwrap(), b"");
        assert_eq!(inflate(&from_b64(VEC_HELLO)).unwrap(), b"hello");
        assert_eq!(
            inflate(&from_b64(VEC_TEXT)).unwrap(),
            b"The quick brown fox jumps over the lazy dog. The quick brown fox."
        );
    }

    #[test]
    fn deflate_inflate_roundtrip() {
        let mut data = Vec::new();
        for i in 0..100_000 {
            data.push((i * 31 % 251) as u8);
        }
        let packed = deflate(&data);
        assert_eq!(inflate(&packed).unwrap(), data);
        // Random bytes (incompressible): stored blocks must still round-trip.
        let mut rng: u64 = 0x12345678;
        let mut random = Vec::with_capacity(50_000);
        for _ in 0..50_000 {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            random.push((rng >> 33) as u8);
        }
        assert_eq!(inflate(&deflate(&random)).unwrap(), random);
        // Empty and tiny inputs.
        assert_eq!(inflate(&deflate(b"")).unwrap(), b"");
        assert_eq!(inflate(&deflate(b"a")).unwrap(), b"a");
    }

    #[test]
    fn inflate_rejects_corruption() {
        let mut bad = from_b64(VEC_HELLO);
        bad[3] ^= 0xff;
        assert!(inflate(&bad).is_err());
        assert!(inflate(b"not zlib").is_err());
        assert!(inflate(&[]).is_err());
        // Bad adler trailer.
        let mut bad_trailer = deflate(b"hello world");
        let n = bad_trailer.len();
        bad_trailer[n - 1] ^= 0xff;
        assert!(inflate(&bad_trailer).is_err());
    }

    #[test]
    fn adler_known() {
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"abc"), 0x024d_0127);
    }
}
