//! HAProxy PROXY protocol v1/v2 headers sent by trusted load balancers.
//!
//! The reader consumes exactly one header and leaves the following Minecraft
//! bytes in the stream. It returns `None` when the header does not describe a
//! TCP client (v2 LOCAL health checks, v1 UNKNOWN, unspecified or UNIX
//! families); callers then keep the socket peer address.
use std::{
    fmt,
    io::{self, ErrorKind},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    str::FromStr,
};
use tokio::io::{AsyncRead, AsyncReadExt};

const V2_SIGNATURE: &[u8; 12] = b"\r\n\r\n\0\r\nQUIT\n";
/// The longest v1 line, including the trailing CRLF.
const V1_MAX: usize = 107;
/// The shortest v1 line (`PROXY UNKNOWN\r\n`); v2 headers are longer.
const PREFIX: usize = 15;

pub async fn read_header<R: AsyncRead + Unpin>(stream: &mut R) -> io::Result<Option<SocketAddr>> {
    let mut header = [0; V1_MAX];
    stream.read_exact(&mut header[..PREFIX]).await?;
    if header[..12] == V2_SIGNATURE[..] {
        stream.read_exact(&mut header[PREFIX..16]).await?;
        let length = u16::from_be_bytes([header[14], header[15]]) as usize;
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await?;
        return v2(header[12], header[13], &body);
    }
    if !header.starts_with(b"PROXY ") {
        return Err(invalid("missing header"));
    }
    // Read the remainder byte by byte so no client data is consumed.
    let mut length = PREFIX;
    while !header[..length].ends_with(b"\r\n") {
        if length == V1_MAX {
            return Err(invalid("v1 header exceeds 107 bytes"));
        }
        stream.read_exact(&mut header[length..length + 1]).await?;
        length += 1;
    }
    v1(&header[..length - 2])
}

fn v1(line: &[u8]) -> io::Result<Option<SocketAddr>> {
    let line = std::str::from_utf8(line).map_err(|_| invalid("v1 header is not ASCII"))?;
    let fields: Vec<&str> = line.split(' ').collect();
    match fields[..] {
        ["PROXY", "UNKNOWN", ..] => Ok(None),
        ["PROXY", family @ ("TCP4" | "TCP6"), source, _, port, _] => {
            let ip = match family {
                "TCP4" => source.parse::<Ipv4Addr>().map(IpAddr::V4),
                _ => source.parse::<Ipv6Addr>().map(IpAddr::V6),
            }
            .map_err(|_| invalid("v1 source address"))?;
            let port = port.parse().map_err(|_| invalid("v1 source port"))?;
            Ok(Some(SocketAddr::new(ip, port)))
        }
        _ => Err(invalid("malformed v1 header")),
    }
}

fn v2(version_command: u8, family: u8, body: &[u8]) -> io::Result<Option<SocketAddr>> {
    if version_command >> 4 != 2 {
        return Err(invalid("unsupported v2 version"));
    }
    match version_command & 0x0f {
        0 => return Ok(None), // LOCAL: the proxy's own connection, e.g. a health check.
        1 => {}
        _ => return Err(invalid("unsupported v2 command")),
    }
    let (ip, port) = match family {
        0x11 if body.len() >= 12 => (
            IpAddr::from(<[u8; 4]>::try_from(&body[..4]).unwrap()),
            &body[8..10],
        ),
        0x21 if body.len() >= 36 => (
            IpAddr::from(<[u8; 16]>::try_from(&body[..16]).unwrap()),
            &body[32..34],
        ),
        0x11 | 0x21 => return Err(invalid("truncated v2 addresses")),
        0x00 | 0x31 | 0x32 => return Ok(None),
        _ => return Err(invalid("unsupported v2 address family")),
    };
    Ok(Some(SocketAddr::new(
        ip,
        u16::from_be_bytes([port[0], port[1]]),
    )))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, format!("PROXY protocol: {message}"))
}

/// An address or CIDR block such as `10.0.0.0/8`, `192.0.2.10` or `2001:db8::/32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Network {
    address: IpAddr,
    prefix: u8,
}

impl Network {
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ((network, bits), (ip, ip_bits)) = (widen(self.address), widen(ip.to_canonical()));
        bits == ip_bits && (network ^ ip) & !host_mask(bits - self.prefix) == 0
    }
}

/// The address as a 128-bit integer and the width of its family.
fn widen(address: IpAddr) -> (u128, u8) {
    match address {
        IpAddr::V4(ip) => (u32::from(ip).into(), 32),
        IpAddr::V6(ip) => (ip.into(), 128),
    }
}

fn host_mask(host_bits: u8) -> u128 {
    u128::MAX.checked_shr(128 - host_bits as u32).unwrap_or(0)
}

impl FromStr for Network {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        let (address, prefix) = match value.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (value, None),
        };
        let address: IpAddr = address
            .parse()
            .map_err(|_| format!("invalid address {value:?}"))?;
        let (bits_value, bits) = widen(address);
        let prefix = match prefix {
            None => bits,
            Some(prefix) => prefix
                .parse::<u8>()
                .ok()
                .filter(|length| *length <= bits && prefix.bytes().all(|b| b.is_ascii_digit()))
                .ok_or_else(|| format!("invalid prefix length in {value:?}"))?,
        };
        if bits_value & host_mask(bits - prefix) != 0 {
            return Err(format!("{value:?} has host bits set beyond /{prefix}"));
        }
        Ok(Self { address, prefix })
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.address, self.prefix)
    }
}

#[cfg(test)]
mod tests;
