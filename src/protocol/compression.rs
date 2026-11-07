//! https://www.rfc-editor.org/rfc/rfc1950 and RFC 1951, implemented here to keep the dependency and native-library
//! footprint unchanged. The decoder accepts stored, fixed and dynamic blocks;
//! the encoder uses a bounded LZ77 match table and writes each block stored or
//! with fixed or dynamic Huffman codes, whichever is smallest, so its output
//! grows by at most 5 bytes per 64 KiB of input plus the 6-byte zlib wrapper.
use super::invalid;
use std::{io, sync::OnceLock};

const LENGTH_BASE: [usize; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [usize; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [usize; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [usize; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in bytes.chunks(5552) {
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    b << 16 | a
}

/// Reads DEFLATE's LSB-first bit stream through a 64-bit buffer.
struct Bits<'a> {
    bytes: &'a [u8],
    /// The next byte not yet loaded into `buffer`.
    position: usize,
    buffer: u64,
    count: u32,
}
impl<'a> Bits<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            position: 0,
            buffer: 0,
            count: 0,
        }
    }
    fn refill(&mut self) {
        if self.count <= 32 && self.position + 4 <= self.bytes.len() {
            let word = u32::from_le_bytes(
                self.bytes[self.position..self.position + 4]
                    .try_into()
                    .unwrap(),
            );
            self.buffer |= u64::from(word) << self.count;
            self.position += 4;
            self.count += 32;
        }
        while self.count <= 56 && self.position < self.bytes.len() {
            self.buffer |= u64::from(self.bytes[self.position]) << self.count;
            self.position += 1;
            self.count += 8;
        }
    }
    fn consume(&mut self, count: u32) {
        self.buffer >>= count;
        self.count -= count;
    }
    fn read(&mut self, count: usize) -> io::Result<usize> {
        let count = count as u32;
        if self.count < count {
            self.refill();
            if self.count < count {
                return Err(invalid("truncated DEFLATE stream"));
            }
        }
        let value = (self.buffer & ((1u64 << count) - 1)) as usize;
        self.consume(count);
        Ok(value)
    }
    fn align(&mut self) {
        self.consume(self.count % 8);
    }
    /// Copy `length` whole bytes; the stream must be byte-aligned.
    fn copy(&mut self, length: usize, output: &mut Vec<u8>) -> io::Result<()> {
        let buffered = (self.count / 8) as usize;
        if buffered + (self.bytes.len() - self.position) < length {
            return Err(invalid("truncated DEFLATE stream"));
        }
        for _ in 0..length.min(buffered) {
            output.push(self.buffer as u8);
            self.consume(8);
        }
        let rest = length.saturating_sub(buffered);
        output.extend_from_slice(&self.bytes[self.position..self.position + rest]);
        self.position += rest;
        Ok(())
    }
    /// Bytes touched so far, counting a partly consumed final byte.
    fn consumed_bytes(&self) -> usize {
        (self.position * 8 - self.count as usize).div_ceil(8)
    }
}

/// Codes up to this length decode with one table lookup.
const FAST_BITS: u32 = 10;

struct Huffman {
    counts: [usize; 16],
    symbols: Vec<usize>,
    /// Indexed by the next `FAST_BITS` stream bits: `symbol << 4 | length`,
    /// or 0 for longer codes and unused prefixes.
    fast: Vec<u16>,
}
impl Huffman {
    fn new(lengths: &[usize], code_lengths: bool) -> io::Result<Self> {
        let mut counts = [0; 16];
        for &length in lengths {
            if length > 15 {
                return Err(invalid("invalid Huffman length"));
            }
            counts[length] += 1;
        }
        let mut left = 1isize;
        for &count in &counts[1..] {
            left = left * 2 - count as isize;
            if left < 0 {
                return Err(invalid("oversubscribed Huffman tree"));
            }
        }
        let used = lengths.len() - counts[0];
        if left != 0 && (code_lengths || (used != 0 && !(used == 1 && counts[1] == 1))) {
            return Err(invalid("incomplete Huffman tree"));
        }
        let mut symbols = Vec::with_capacity(used);
        for length in 1..=15 {
            for (symbol, &size) in lengths.iter().enumerate() {
                if size == length {
                    symbols.push(symbol);
                }
            }
        }
        // Canonical codes (RFC 1951 3.2.2), reversed into stream bit order.
        let mut next = [0usize; 16];
        let mut code = 0;
        for length in 2..16 {
            code = (code + counts[length - 1]) << 1;
            next[length] = code;
        }
        let mut fast = vec![0u16; 1 << FAST_BITS];
        for (symbol, &length) in lengths.iter().enumerate() {
            if length == 0 {
                continue;
            }
            let code = next[length];
            next[length] += 1;
            if length as u32 <= FAST_BITS {
                let reversed = code.reverse_bits() >> (usize::BITS as usize - length);
                let entry = (symbol << 4 | length) as u16;
                for slot in fast.iter_mut().skip(reversed).step_by(1 << length) {
                    *slot = entry;
                }
            }
        }
        Ok(Self {
            counts,
            symbols,
            fast,
        })
    }
    fn decode(&self, bits: &mut Bits<'_>) -> io::Result<usize> {
        if bits.count < 15 {
            bits.refill();
        }
        let entry = self.fast[(bits.buffer & ((1 << FAST_BITS) - 1)) as usize];
        if entry != 0 {
            let length = u32::from(entry & 15);
            if length > bits.count {
                return Err(invalid("truncated DEFLATE stream"));
            }
            bits.consume(length);
            return Ok(usize::from(entry >> 4));
        }
        let (mut code, mut first, mut index) = (0, 0, 0);
        for length in 1..=15u32 {
            if length > bits.count {
                return Err(invalid("truncated DEFLATE stream"));
            }
            code = code * 2 + ((bits.buffer >> (length - 1)) & 1) as usize;
            let count = self.counts[length as usize];
            if code < first + count {
                bits.consume(length);
                return Ok(self.symbols[index + code - first]);
            }
            index += count;
            first = (first + count) * 2;
        }
        Err(invalid("invalid Huffman code"))
    }
}

fn fixed() -> &'static (Huffman, Huffman) {
    static FIXED: OnceLock<(Huffman, Huffman)> = OnceLock::new();
    FIXED.get_or_init(|| {
        let mut lengths = vec![8; 288];
        lengths[144..256].fill(9);
        lengths[256..280].fill(7);
        (
            Huffman::new(&lengths, false).unwrap(),
            Huffman::new(&[5; 32], false).unwrap(),
        )
    })
}

fn dynamic(bits: &mut Bits<'_>) -> io::Result<(Huffman, Huffman)> {
    let literal_count = bits.read(5)? + 257;
    let distance_count = bits.read(5)? + 1;
    let count = bits.read(4)? + 4;
    if literal_count > 286 {
        return Err(invalid("invalid literal code count"));
    }
    let order = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let mut lengths = [0; 19];
    for &index in &order[..count] {
        lengths[index] = bits.read(3)?;
    }
    let codes = Huffman::new(&lengths, true)?;
    let mut lengths = Vec::with_capacity(literal_count + distance_count);
    while lengths.len() < literal_count + distance_count {
        let symbol = codes.decode(bits)?;
        let (value, count) = match symbol {
            0..=15 => (symbol, 1),
            16 => (
                *lengths
                    .last()
                    .ok_or_else(|| invalid("repeat without previous length"))?,
                bits.read(2)? + 3,
            ),
            17 => (0, bits.read(3)? + 3),
            18 => (0, bits.read(7)? + 11),
            _ => return Err(invalid("invalid code length")),
        };
        if lengths.len() + count > literal_count + distance_count {
            return Err(invalid("code length repeat overflow"));
        }
        lengths.resize(lengths.len() + count, value);
    }
    if lengths[256] == 0 {
        return Err(invalid("missing end-of-block code"));
    }
    Ok((
        Huffman::new(&lengths[..literal_count], false)?,
        Huffman::new(&lengths[literal_count..], false)?,
    ))
}

pub(super) fn inflate(bytes: &[u8], expected: usize) -> io::Result<Vec<u8>> {
    if bytes.len() < 6
        || bytes[0] & 15 != 8
        || bytes[0] >> 4 > 7
        || !u16::from_be_bytes([bytes[0], bytes[1]]).is_multiple_of(31)
        || bytes[1] & 32 != 0
    {
        return Err(invalid("invalid zlib header or preset dictionary"));
    }
    let window = 1usize << ((bytes[0] >> 4) + 8);
    let mut bits = Bits::new(&bytes[2..bytes.len() - 4]);
    let mut output = Vec::with_capacity(expected);
    loop {
        let last = bits.read(1)? != 0;
        match bits.read(2)? {
            0 => {
                bits.align();
                let length = bits.read(16)?;
                if length ^ bits.read(16)? != 0xffff || output.len() + length > expected {
                    return Err(invalid("invalid stored block length"));
                }
                bits.copy(length, &mut output)?;
            }
            kind @ (1 | 2) => {
                let tables;
                let (literal, distance) = if kind == 1 {
                    let (literal, distance) = fixed();
                    (literal, distance)
                } else {
                    tables = dynamic(&mut bits)?;
                    (&tables.0, &tables.1)
                };
                loop {
                    match literal.decode(&mut bits)? {
                        value @ 0..=255 => {
                            if output.len() == expected {
                                return Err(invalid("inflated packet exceeds declared size"));
                            }
                            output.push(value as u8);
                        }
                        256 => break,
                        value @ 257..=285 => {
                            let index = value - 257;
                            let length = LENGTH_BASE[index] + bits.read(LENGTH_EXTRA[index])?;
                            let index = distance.decode(&mut bits)?;
                            if index >= 30 {
                                return Err(invalid("reserved distance code"));
                            }
                            let distance = DIST_BASE[index] + bits.read(DIST_EXTRA[index])?;
                            if distance > output.len()
                                || distance > window
                                || output.len() + length > expected
                            {
                                return Err(invalid(
                                    "invalid DEFLATE back reference or output length",
                                ));
                            }
                            let start = output.len() - distance;
                            if distance >= length {
                                output.extend_from_within(start..start + length);
                            } else {
                                // Overlapping copies repeat bytes written by this one.
                                for offset in 0..length {
                                    output.push(output[start + offset]);
                                }
                            }
                        }
                        _ => return Err(invalid("reserved literal code")),
                    }
                }
            }
            _ => return Err(invalid("reserved DEFLATE block type")),
        }
        if last {
            break;
        }
    }
    if output.len() != expected || bits.consumed_bytes() != bits.bytes.len() {
        return Err(invalid("zlib size mismatch or trailing data"));
    }
    if adler32(&output) != u32::from_be_bytes(bytes[bytes.len() - 4..].try_into().unwrap()) {
        return Err(invalid("zlib checksum mismatch"));
    }
    Ok(output)
}

/// Writes DEFLATE's LSB-first bit stream through a 64-bit buffer.
struct Writer {
    bytes: Vec<u8>,
    buffer: u64,
    /// Always below 32 between writes.
    count: u32,
}
impl Writer {
    /// `count` is at most 32.
    fn write(&mut self, value: u32, count: u32) {
        self.buffer |= u64::from(value) << self.count;
        self.count += count;
        if self.count >= 32 {
            self.bytes
                .extend_from_slice(&(self.buffer as u32).to_le_bytes());
            self.buffer >>= 32;
            self.count -= 32;
        }
    }
    /// Pad with zero bits to a byte boundary and flush.
    fn align(&mut self) {
        while self.count > 0 {
            self.bytes.push(self.buffer as u8);
            self.buffer >>= 8;
            self.count = self.count.saturating_sub(8);
        }
        self.buffer = 0;
    }
}

/// Fixed-Huffman encodings, with codes already reversed into stream bit order.
struct FixedCodes {
    /// Literal/length symbols: `(code, bits)`.
    literals: [(u32, u32); 288],
    distances: [u32; 30],
    /// Indexed by match length 3..=258: `(symbol, extra bits, extra value)`.
    lengths: [(u16, u8, u8); 259],
}

fn fixed_codes() -> &'static FixedCodes {
    static CODES: OnceLock<FixedCodes> = OnceLock::new();
    CODES.get_or_init(|| {
        let reverse = |code: u32, bits: u32| code.reverse_bits() >> (32 - bits);
        let mut literals = [(0, 0); 288];
        for (value, slot) in literals.iter_mut().enumerate() {
            let value = value as u32;
            let (code, bits) = match value {
                0..=143 => (value + 0x30, 8),
                144..=255 => (value - 144 + 0x190, 9),
                256..=279 => (value - 256, 7),
                _ => (value - 280 + 0xc0, 8),
            };
            *slot = (reverse(code, bits), bits);
        }
        let mut distances = [0; 30];
        for (index, slot) in distances.iter_mut().enumerate() {
            *slot = reverse(index as u32, 5);
        }
        let mut lengths = [(0, 0, 0); 259];
        for (length, slot) in lengths.iter_mut().enumerate().skip(3) {
            let index = LENGTH_BASE
                .iter()
                .rposition(|&base| base <= length)
                .unwrap();
            *slot = (
                (index + 257) as u16,
                LENGTH_EXTRA[index] as u8,
                (length - LENGTH_BASE[index]) as u8,
            );
        }
        FixedCodes {
            literals,
            distances,
            lengths,
        }
    })
}

fn distance_code(distance: usize) -> usize {
    let value = distance - 1;
    if value < 4 {
        return value;
    }
    let bits = (usize::BITS - 1 - value.leading_zeros()) as usize;
    2 * bits + ((value >> (bits - 1)) & 1)
}

/// Input bytes per block: the most a stored block can hold.
const BLOCK_BYTES: usize = 65535;
/// Symbols per block before its codes are chosen, as in zlib, so dynamic
/// codes can follow changes in the data.
const BLOCK_SYMBOLS: usize = 16 * 1024;
/// Blocks whose fixed-code size is below this many bytes are not tried with
/// dynamic codes.
const DYNAMIC_MIN_BYTES: usize = 512;
/// Match tokens carry the length above this shift and the distance below it;
/// literal tokens have length 0.
const MATCH_SHIFT: u32 = 16;
/// Transmission order of the code length code lengths (RFC 1951 3.2.7).
const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Huffman code lengths for `frequencies`, at most `limit` bits. At least two
/// symbols get a code (adding unused ones if needed), so every code is
/// complete, which strict decoders such as zlib require.
fn code_lengths(frequencies: &[u32], limit: u8) -> Vec<u8> {
    let mut frequencies: Vec<u64> = frequencies.iter().map(|&f| u64::from(f)).collect();
    let mut used = frequencies.iter().filter(|&&f| f > 0).count();
    for frequency in frequencies.iter_mut() {
        if used >= 2 {
            break;
        }
        if *frequency == 0 {
            *frequency = 1;
            used += 1;
        }
    }
    let mut lengths = vec![0u8; frequencies.len()];
    loop {
        // Two-queue construction: leaves sorted by weight, and inner nodes,
        // which are created in nondecreasing weight order.
        let mut leaves: Vec<(u64, usize)> = frequencies
            .iter()
            .enumerate()
            .filter(|&(_, &f)| f > 0)
            .map(|(symbol, &f)| (f, symbol))
            .collect();
        leaves.sort_unstable();
        let count = leaves.len();
        let mut weights = vec![0u64; count - 1];
        let mut leaf_parent = vec![0usize; count];
        let mut inner_parent = vec![0usize; count - 1];
        let (mut leaf, mut inner) = (0, 0);
        for node in 0..count - 1 {
            for _ in 0..2 {
                if leaf < count && (inner == node || leaves[leaf].0 <= weights[inner]) {
                    weights[node] += leaves[leaf].0;
                    leaf_parent[leaf] = node;
                    leaf += 1;
                } else {
                    weights[node] += weights[inner];
                    inner_parent[inner] = node;
                    inner += 1;
                }
            }
        }
        // The last inner node is the root; parents always come later.
        let mut depths = vec![0u8; count - 1];
        for node in (0..count.saturating_sub(2)).rev() {
            depths[node] = depths[inner_parent[node]] + 1;
        }
        let mut longest = 0;
        for (&(_, symbol), &parent) in leaves.iter().zip(&leaf_parent) {
            lengths[symbol] = depths[parent] + 1;
            longest = longest.max(lengths[symbol]);
        }
        if longest <= limit {
            return lengths;
        }
        // Flatten the distribution and rebuild; rare, and only costs ratio.
        for frequency in frequencies.iter_mut().filter(|f| **f > 0) {
            *frequency = (*frequency >> 1) | 1;
        }
    }
}

/// Canonical codes for `lengths` (RFC 1951 3.2.2), reversed into stream bit
/// order: `(code, bits)` per symbol.
fn canonical_codes(lengths: &[u8]) -> Vec<(u32, u32)> {
    let mut counts = [0u32; 16];
    for &length in lengths {
        counts[usize::from(length)] += 1;
    }
    counts[0] = 0;
    let mut next = [0u32; 16];
    let mut code = 0;
    for length in 1..16 {
        code = (code + counts[length - 1]) << 1;
        next[length] = code;
    }
    lengths
        .iter()
        .map(|&length| {
            if length == 0 {
                return (0, 0);
            }
            let code = next[usize::from(length)];
            next[usize::from(length)] += 1;
            let bits = u32::from(length);
            (code.reverse_bits() >> (32 - bits), bits)
        })
        .collect()
}

/// A dynamic-Huffman block header and its codes.
struct DynamicCodes {
    literals: Vec<(u32, u32)>,
    distances: Vec<(u32, u32)>,
    /// Literal and distance code lengths as code length symbols:
    /// `(symbol, extra value)`.
    runs: Vec<(u8, u8)>,
    code_length_codes: Vec<(u32, u32)>,
    code_length_count: usize,
    literal_count: usize,
    distance_count: usize,
    /// Header and symbol bits, without length and distance extra bits.
    bits: usize,
}

impl DynamicCodes {
    fn new(literal_frequencies: &[u32; 286], distance_frequencies: &[u32; 30]) -> Self {
        let literal_lengths = code_lengths(literal_frequencies, 15);
        let distance_lengths = code_lengths(distance_frequencies, 15);
        let literal_count = 257.max(literal_lengths.iter().rposition(|&l| l > 0).unwrap() + 1);
        let distance_count = distance_lengths.iter().rposition(|&l| l > 0).unwrap() + 1;
        let all: Vec<u8> = literal_lengths[..literal_count]
            .iter()
            .chain(&distance_lengths[..distance_count])
            .copied()
            .collect();
        // Run-length encode the lengths with symbols 16 (repeat the previous
        // length 3-6 times), 17 (3-10 zeros) and 18 (11-138 zeros).
        let mut runs = Vec::new();
        let mut index = 0;
        while index < all.len() {
            let length = all[index];
            let run = all[index..].iter().take_while(|&&l| l == length).count();
            let mut left = run;
            if length == 0 {
                while left >= 11 {
                    let take = left.min(138);
                    runs.push((18, (take - 11) as u8));
                    left -= take;
                }
                if left >= 3 {
                    runs.push((17, (left - 3) as u8));
                    left = 0;
                }
            } else {
                runs.push((length, 0));
                left -= 1;
                while left >= 3 {
                    let take = left.min(6);
                    runs.push((16, (take - 3) as u8));
                    left -= take;
                }
            }
            runs.extend(std::iter::repeat_n((length, 0), left));
            index += run;
        }
        let mut code_length_frequencies = [0u32; 19];
        for &(symbol, _) in &runs {
            code_length_frequencies[usize::from(symbol)] += 1;
        }
        let code_length_lengths = code_lengths(&code_length_frequencies, 7);
        let code_length_count = 4.max(
            CODE_LENGTH_ORDER
                .iter()
                .rposition(|&symbol| code_length_lengths[symbol] > 0)
                .unwrap()
                + 1,
        );
        let literals = canonical_codes(&literal_lengths);
        let distances = canonical_codes(&distance_lengths);
        let code_length_codes = canonical_codes(&code_length_lengths);
        let mut bits = 3 + 5 + 5 + 4 + 3 * code_length_count;
        for &(symbol, _) in &runs {
            bits += code_length_codes[usize::from(symbol)].1 as usize
                + [2, 3, 7]
                    .get(usize::from(symbol).wrapping_sub(16))
                    .unwrap_or(&0);
        }
        for (frequency, (_, length)) in literal_frequencies.iter().zip(&literals) {
            bits += *frequency as usize * *length as usize;
        }
        for (frequency, (_, length)) in distance_frequencies.iter().zip(&distances) {
            bits += *frequency as usize * *length as usize;
        }
        Self {
            literals,
            distances,
            runs,
            code_length_codes,
            code_length_count,
            literal_count,
            distance_count,
            bits,
        }
    }

    fn write_header(&self, out: &mut Writer, last: u32) {
        out.write(last | 2 << 1, 3);
        out.write((self.literal_count - 257) as u32, 5);
        out.write((self.distance_count - 1) as u32, 5);
        out.write((self.code_length_count - 4) as u32, 4);
        for &symbol in &CODE_LENGTH_ORDER[..self.code_length_count] {
            out.write(self.code_length_codes[symbol].1, 3);
        }
        for &(symbol, extra) in &self.runs {
            let (code, bits) = self.code_length_codes[usize::from(symbol)];
            out.write(code, bits);
            match symbol {
                16 => out.write(u32::from(extra), 2),
                17 => out.write(u32::from(extra), 3),
                18 => out.write(u32::from(extra), 7),
                _ => {}
            }
        }
    }
}

/// Write `tokens` and the end-of-block code with the given codes.
fn write_symbols(
    out: &mut Writer,
    tokens: &[u32],
    literals: &[(u32, u32)],
    distances: &[(u32, u32)],
) {
    let lengths = &fixed_codes().lengths;
    for &token in tokens {
        let length = (token >> MATCH_SHIFT) as usize;
        if length == 0 {
            let (code, bits) = literals[token as usize];
            out.write(code, bits);
            continue;
        }
        let distance = (token & ((1 << MATCH_SHIFT) - 1)) as usize;
        let (symbol, extra, value) = lengths[length];
        let (code, bits) = literals[usize::from(symbol)];
        out.write(code, bits);
        out.write(u32::from(value), u32::from(extra));
        let index = distance_code(distance);
        let (code, bits) = distances[index];
        out.write(code, bits);
        out.write(
            (distance - DIST_BASE[index]) as u32,
            DIST_EXTRA[index] as u32,
        );
    }
    let (code, bits) = literals[256];
    out.write(code, bits);
}

/// Each block is stored, or uses fixed or dynamic codes, whichever is
/// smallest. Output is at most 5 bytes per 64 KiB larger than the input, plus
/// the 6-byte zlib wrapper, and dynamic codes keep the ratio close to zlib's.
pub(super) fn deflate(bytes: &[u8]) -> Vec<u8> {
    let codes = fixed_codes();
    let fixed_distances: Vec<(u32, u32)> = codes.distances.iter().map(|&code| (code, 5)).collect();
    let mut out = Writer {
        bytes: Vec::with_capacity(bytes.len() / 2 + 64),
        buffer: 0,
        count: 0,
    };
    out.bytes.extend_from_slice(&[0x78, 0x01]);
    // Small packets must not pay for clearing a full-size match table.
    let hash_bits = bytes
        .len()
        .next_power_of_two()
        .trailing_zeros()
        .clamp(8, 15);
    let mut table = vec![u32::MAX; 1 << hash_bits];
    let mut tokens = Vec::with_capacity(BLOCK_SYMBOLS.min(bytes.len()) + 64);
    let mut pos = 0;
    let mut misses = 0;
    loop {
        let start = pos;
        tokens.clear();
        let mut literal_frequencies = [0u32; 286];
        let mut distance_frequencies = [0u32; 30];
        let mut extra_bits = 0;
        while pos < bytes.len() && pos - start < BLOCK_BYTES && tokens.len() < BLOCK_SYMBOLS {
            let mut length = 0;
            let mut distance = 0;
            if pos + 2 < bytes.len() {
                let key = u32::from(bytes[pos])
                    | u32::from(bytes[pos + 1]) << 8
                    | u32::from(bytes[pos + 2]) << 16;
                let hash = (key.wrapping_mul(0x9e37_79b1) >> (32 - hash_bits)) as usize;
                let previous = table[hash];
                table[hash] = pos as u32;
                if previous != u32::MAX && pos - previous as usize <= 32768 {
                    let previous = previous as usize;
                    let limit = (bytes.len() - pos).min(258);
                    let mut mismatch = false;
                    while length + 8 <= limit {
                        let a = u64::from_le_bytes(
                            bytes[previous + length..previous + length + 8]
                                .try_into()
                                .unwrap(),
                        );
                        let b = u64::from_le_bytes(
                            bytes[pos + length..pos + length + 8].try_into().unwrap(),
                        );
                        if a != b {
                            length += ((a ^ b).trailing_zeros() / 8) as usize;
                            mismatch = true;
                            break;
                        }
                        length += 8;
                    }
                    while !mismatch
                        && length < limit
                        && bytes[previous + length] == bytes[pos + length]
                    {
                        length += 1;
                    }
                    distance = pos - previous;
                }
            }
            if length >= 3 {
                let (symbol, extra, _) = codes.lengths[length];
                let index = distance_code(distance);
                literal_frequencies[usize::from(symbol)] += 1;
                distance_frequencies[index] += 1;
                extra_bits += usize::from(extra) + DIST_EXTRA[index];
                tokens.push((length as u32) << MATCH_SHIFT | distance as u32);
                pos += length;
                misses = 0;
            } else {
                // Probe less often while nothing matches, so incompressible
                // data costs little more than storing it.
                let end = (pos + 1 + (misses >> 5).min(63))
                    .min(start + BLOCK_BYTES)
                    .min(bytes.len());
                for &byte in &bytes[pos..end] {
                    literal_frequencies[usize::from(byte)] += 1;
                    tokens.push(u32::from(byte));
                }
                pos = end;
                misses += 1;
            }
        }
        literal_frequencies[256] = 1;
        let last = u32::from(pos == bytes.len());
        let fixed_bits = 3
            + extra_bits
            + literal_frequencies
                .iter()
                .zip(&codes.literals)
                .map(|(&frequency, &(_, bits))| frequency as usize * bits as usize)
                .sum::<usize>()
            + 5 * distance_frequencies.iter().sum::<u32>() as usize;
        // A dynamic header costs dozens of bytes, so small blocks skip it.
        let dynamic = (fixed_bits >= 8 * DYNAMIC_MIN_BYTES)
            .then(|| DynamicCodes::new(&literal_frequencies, &distance_frequencies));
        let dynamic_bits = dynamic
            .as_ref()
            .map_or(usize::MAX, |dynamic| dynamic.bits + extra_bits);
        let padding = ((8 - (out.count + 3) % 8) % 8) as usize;
        let stored_bits = 3 + padding + 32 + 8 * (pos - start);
        if stored_bits < fixed_bits.min(dynamic_bits) {
            out.write(last, 3);
            out.align();
            let length = (pos - start) as u16;
            out.bytes.extend_from_slice(&length.to_le_bytes());
            out.bytes.extend_from_slice(&(!length).to_le_bytes());
            out.bytes.extend_from_slice(&bytes[start..pos]);
        } else if let Some(dynamic) = dynamic.filter(|_| dynamic_bits < fixed_bits) {
            dynamic.write_header(&mut out, last);
            write_symbols(&mut out, &tokens, &dynamic.literals, &dynamic.distances);
        } else {
            out.write(last | 1 << 1, 3);
            write_symbols(&mut out, &tokens, &codes.literals, &fixed_distances);
        }
        if last == 1 {
            break;
        }
    }
    out.align();
    out.bytes.extend_from_slice(&adler32(bytes).to_be_bytes());
    out.bytes
}
