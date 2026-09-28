//! A packet-aware fixture for the operational tests. Application test bytes are
//! carried in opaque play packets after a real 1.8 handshake and offline login.
use super::*;
use rift::protocol::{Codec, Handshake, NextState, Packet, read_string, write_string};
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicU64, Ordering},
};

pub struct GameStream {
    socket: TcpStream,
    pending: VecDeque<u8>,
    login: bool,
}
impl std::ops::Deref for GameStream {
    type Target = TcpStream;
    fn deref(&self) -> &Self::Target {
        &self.socket
    }
}
impl Read for GameStream {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        while self.pending.is_empty() {
            let packet = match read_packet(&mut self.socket) {
                Ok(packet) => packet,
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(0),
                Err(error) => return Err(error),
            };
            if self.login && packet.id == 2 {
                self.login = false;
                continue;
            }
            if packet.id == 0x40 {
                return Ok(0);
            }
            assert_eq!(packet.id, 0x7f, "unexpected fixture packet: {packet:?}");
            self.pending.extend(packet.data);
        }
        let length = output.len().min(self.pending.len());
        for byte in &mut output[..length] {
            *byte = self.pending.pop_front().unwrap();
        }
        Ok(length)
    }
}
impl Write for GameStream {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.login {
            let packet = read_packet(&mut self.socket)?;
            if packet.id != 2 {
                return Err(std::io::Error::other("fixture login failed"));
            }
            self.login = false;
        }
        let length = bytes.len().min(65536);
        if length == 0 {
            return Ok(0);
        }
        self.socket
            .write_all(&Codec::default().encode(&Packet::new(0x7f, bytes[..length].to_vec()))?)?;
        Ok(length)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.socket.flush()
    }
}

pub fn read_packet(stream: &mut TcpStream) -> std::io::Result<Packet> {
    let mut length = 0usize;
    for shift in (0..21).step_by(7) {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        length |= usize::from(byte[0] & 127) << shift;
        if byte[0] & 128 == 0 {
            assert!(length <= rift::protocol::MAX_FRAME_SIZE);
            let mut body = vec![0; length];
            stream.read_exact(&mut body)?;
            return Packet::from_body(&body);
        }
    }
    panic!("oversized fixture packet");
}

pub fn setup() -> Vec<u8> {
    setup_for("Player")
}

fn setup_for(name: &str) -> Vec<u8> {
    let handshake = Handshake {
        protocol: 47,
        address: "localhost".into(),
        port: 25565,
        next_state: NextState::Login,
    };
    let mut bytes = Codec::default().encode(&handshake.packet()).unwrap();
    let mut data = Vec::new();
    write_string(name, &mut data);
    bytes.extend(Codec::default().encode(&Packet::new(0, data)).unwrap());
    bytes
}
#[allow(dead_code)] // Shared fixtures are compiled by several integration suites.
pub fn success() -> Vec<u8> {
    success_for("Player", 1)
}

fn success_for(name: &str, identity: u64) -> Vec<u8> {
    assert!(identity < (1 << 48), "fixture UUID counter exhausted");
    let mut data = Vec::new();
    write_string(
        &format!("00000000-0000-0000-0000-{identity:012x}"),
        &mut data,
    );
    write_string(name, &mut data);
    Codec::default().encode(&Packet::new(2, data)).unwrap()
}

pub fn connect_game(address: SocketAddr) -> GameStream {
    static NEXT_PLAYER: AtomicU64 = AtomicU64::new(1);
    let identity = NEXT_PLAYER.fetch_add(1, Ordering::Relaxed);
    assert!(identity < (1 << 20), "fixture name counter exhausted");
    // Exactly six characters, like setup()'s Player, so wire-byte metrics keep
    // their existing expectations while simultaneous sessions have unique names.
    let name = format!("P{identity:05x}");
    let mut socket = connect(address);
    // Admission/hook rejections can close immediately; the read still observes
    // that outcome when the initial write races the rejection.
    let _ = socket.write_all(&setup_for(&name));
    GameStream {
        socket,
        pending: VecDeque::new(),
        login: true,
    }
}

pub fn accept_game(listener: &TcpListener) -> GameStream {
    accept_socket(accept(listener))
}

pub fn accept_socket(mut socket: TcpStream) -> GameStream {
    static NEXT_IDENTITY: AtomicU64 = AtomicU64::new(1);
    assert_eq!(
        Handshake::decode(&read_packet(&mut socket).unwrap())
            .unwrap()
            .next_state,
        NextState::Login
    );
    let login = read_packet(&mut socket).unwrap();
    assert_eq!(login.id, 0);
    let mut fields = login.data.as_slice();
    let name = read_string(&mut fields, 16).unwrap();
    assert!(fields.is_empty());
    let identity = NEXT_IDENTITY.fetch_add(1, Ordering::Relaxed);
    socket.write_all(&success_for(name, identity)).unwrap();
    GameStream {
        socket,
        pending: VecDeque::new(),
        login: false,
    }
}
