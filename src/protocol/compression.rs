//! https://www.rfc-editor.org/rfc/rfc1950 and RFC 1951, implemented here to keep the dependency and native-library
//! footprint unchanged. The decoder accepts stored, fixed and dynamic blocks;
//! the encoder uses fixed Huffman codes and a bounded LZ77 match table.
use super::invalid;
use std::io;

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

struct Bits<'a> {
    bytes: &'a [u8],
    bit: usize,
}
impl Bits<'_> {
    fn read(&mut self, count: usize) -> io::Result<usize> {
        if self.bit + count > self.bytes.len() * 8 {
            return Err(invalid("truncated DEFLATE stream"));
        }
        let mut value = 0;
        for offset in 0..count {
            value |= usize::from((self.bytes[self.bit / 8] >> (self.bit % 8)) & 1) << offset;
            self.bit += 1;
        }
        Ok(value)
    }
    fn align(&mut self) {
        self.bit = self.bit.div_ceil(8) * 8;
    }
}

struct Huffman {
    counts: [usize; 16],
    symbols: Vec<usize>,
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
        Ok(Self { counts, symbols })
    }
    fn decode(&self, bits: &mut Bits<'_>) -> io::Result<usize> {
        let (mut code, mut first, mut index) = (0, 0, 0);
        for length in 1..=15 {
            code = code * 2 + bits.read(1)?;
            let count = self.counts[length];
            if code < first + count {
                return Ok(self.symbols[index + code - first]);
            }
            index += count;
            first = (first + count) * 2;
        }
        Err(invalid("invalid Huffman code"))
    }
}

fn fixed() -> io::Result<(Huffman, Huffman)> {
    let mut lengths = vec![8; 288];
    lengths[144..256].fill(9);
    lengths[256..280].fill(7);
    Ok((
        Huffman::new(&lengths, false)?,
        Huffman::new(&[5; 32], false)?,
    ))
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
    let mut bits = Bits {
        bytes: &bytes[2..bytes.len() - 4],
        bit: 0,
    };
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
                for _ in 0..length {
                    output.push(bits.read(8)? as u8);
                }
            }
            kind @ (1 | 2) => {
                let (literal, distance) = if kind == 1 {
                    fixed()?
                } else {
                    dynamic(&mut bits)?
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
                            for _ in 0..length {
                                output.push(output[output.len() - distance]);
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
    if output.len() != expected || bits.bit.div_ceil(8) != bits.bytes.len() {
        return Err(invalid("zlib size mismatch or trailing data"));
    }
    if adler32(&output) != u32::from_be_bytes(bytes[bytes.len() - 4..].try_into().unwrap()) {
        return Err(invalid("zlib checksum mismatch"));
    }
    Ok(output)
}

struct Writer {
    bytes: Vec<u8>,
    bit: usize,
}
impl Writer {
    fn write(&mut self, value: usize, count: usize) {
        for offset in 0..count {
            if self.bit.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let last = self.bytes.len() - 1;
            self.bytes[last] |= (((value >> offset) & 1) as u8) << (self.bit % 8);
            self.bit += 1;
        }
    }
    fn code(&mut self, value: usize, count: usize) {
        self.write(
            value.reverse_bits() >> (usize::BITS as usize - count),
            count,
        );
    }
    fn literal(&mut self, value: usize) {
        let (code, length) = match value {
            0..=143 => (value + 0x30, 8),
            144..=255 => (value - 144 + 0x190, 9),
            256..=279 => (value - 256, 7),
            _ => (value - 280 + 0xc0, 8),
        };
        self.code(code, length);
    }
}

pub(super) fn deflate(bytes: &[u8]) -> Vec<u8> {
    let mut out = Writer {
        bytes: vec![0x78, 0x01],
        bit: 16,
    };
    out.write(3, 3); // Final fixed-Huffman block.
    let mut table = vec![usize::MAX; 32768];
    let mut pos = 0;
    while pos < bytes.len() {
        let mut length = 0;
        let mut distance = 0;
        if pos + 2 < bytes.len() {
            let hash = ((usize::from(bytes[pos]) * 251 + usize::from(bytes[pos + 1])) * 251
                + usize::from(bytes[pos + 2]))
                & 32767;
            let previous = table[hash];
            table[hash] = pos;
            if previous != usize::MAX && pos - previous <= 32768 {
                while length < 258
                    && pos + length < bytes.len()
                    && bytes[previous + length] == bytes[pos + length]
                {
                    length += 1;
                }
                distance = pos - previous;
            }
        }
        if length >= 3 {
            let index = LENGTH_BASE
                .iter()
                .rposition(|&base| base <= length)
                .unwrap();
            out.literal(index + 257);
            out.write(length - LENGTH_BASE[index], LENGTH_EXTRA[index]);
            let index = DIST_BASE
                .iter()
                .rposition(|&base| base <= distance)
                .unwrap();
            out.code(index, 5);
            out.write(distance - DIST_BASE[index], DIST_EXTRA[index]);
            pos += length;
        } else {
            out.literal(usize::from(bytes[pos]));
            pos += 1;
        }
    }
    out.literal(256);
    out.bytes.extend_from_slice(&adler32(bytes).to_be_bytes());
    out.bytes
}
