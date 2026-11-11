//! A minimal translating proxy for testing steps against real servers:
//!
//! ```sh
//! cargo run -p rift-translation --example proxy -- \
//!     --listen 127.0.0.1:25565 --backend 127.0.0.1:25566 --backend-protocol 340
//! ```
//!
//! Clients of any supported version connect to `--listen`; the backend is an
//! offline-mode server speaking `--backend-protocol`. Status pings are relayed
//! unchanged. With `--trace`, every packet is logged to stderr with its name.
//! This is a test tool: Rift itself embeds the translator in its sessions.
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use rift_translation::{Direction, Output, Packet, Sent, State, Translator, data, wire::Reader};
use std::{
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
};

const MAX_FRAME: usize = 1 << 23;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

fn write_varint(mut value: u32, out: &mut Vec<u8>) {
    while value > 127 {
        out.push(value as u8 & 127 | 128);
        value >>= 7;
    }
    out.push(value as u8);
}

/// One frame body, or `None` at a clean end of stream.
fn read_frame(stream: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut length = 0usize;
    for shift in (0..35).step_by(7) {
        let mut byte = [0];
        match stream.read(&mut byte)? {
            0 if shift == 0 => return Ok(None),
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            _ => {}
        }
        length |= usize::from(byte[0] & 127) << shift;
        if byte[0] & 128 == 0 {
            if length > MAX_FRAME {
                return Err(invalid("frame too large"));
            }
            let mut body = vec![0; length];
            stream.read_exact(&mut body)?;
            return Ok(Some(body));
        }
    }
    Err(invalid("invalid frame length"))
}

fn decode(frame: &[u8], threshold: Option<usize>) -> io::Result<Packet> {
    let mut reader = Reader::new(frame);
    let body;
    let mut reader = if threshold.is_some() {
        let size = reader.varint()?;
        if size == 0 {
            reader
        } else {
            if size < 0 || size as usize > MAX_FRAME {
                return Err(invalid("invalid uncompressed size"));
            }
            let mut inflated = Vec::with_capacity(size as usize);
            ZlibDecoder::new(reader.rest())
                .take(MAX_FRAME as u64)
                .read_to_end(&mut inflated)?;
            body = inflated;
            Reader::new(&body)
        }
    } else {
        reader
    };
    let id = reader.varint()?;
    Ok(Packet::new(id, reader.rest().to_vec()))
}

fn encode(packet: &Packet, threshold: Option<usize>) -> io::Result<Vec<u8>> {
    let mut body = Vec::with_capacity(packet.data.len() + 5);
    write_varint(packet.id as u32, &mut body);
    body.extend_from_slice(&packet.data);
    let mut payload = Vec::with_capacity(body.len() + 5);
    match threshold {
        None => payload = body,
        Some(threshold) if body.len() < threshold => {
            payload.push(0);
            payload.extend_from_slice(&body);
        }
        Some(_) => {
            write_varint(body.len() as u32, &mut payload);
            let mut encoder = ZlibEncoder::new(payload, Compression::fast());
            encoder.write_all(&body)?;
            payload = encoder.finish()?;
        }
    }
    let mut frame = Vec::with_capacity(payload.len() + 5);
    write_varint(payload.len() as u32, &mut frame);
    frame.extend_from_slice(&payload);
    Ok(frame)
}

struct Shared {
    translator: Translator,
    client: TcpStream,
    backend: TcpStream,
    client_threshold: Option<usize>,
    backend_threshold: Option<usize>,
    trace: bool,
    label: String,
}

impl Shared {
    fn trace(
        &self,
        side: &str,
        protocol: i32,
        direction: Direction,
        state: State,
        packet: &Packet,
    ) {
        if !self.trace {
            return;
        }
        let name = data::version(protocol)
            .packet_name(state, direction, packet.id)
            .unwrap_or("?");
        eprintln!(
            "[{}] {side} {state:?} {} {:#04x} {name} ({} bytes)",
            self.label,
            match direction {
                Direction::Clientbound => "<-",
                Direction::Serverbound => "->",
            },
            packet.id,
            packet.data.len()
        );
    }

    /// Write what a translation call produced. Compression starts on the
    /// client side right after the client is told about it.
    fn send(&mut self, out: Output, compression: Option<i32>) -> io::Result<()> {
        let (client, backend) = (
            self.translator.client_protocol(),
            self.translator.backend_protocol(),
        );
        for Sent { state, packet } in out.backend {
            self.trace("backend", backend, Direction::Serverbound, state, &packet);
            let frame = encode(&packet, self.backend_threshold)?;
            self.backend.write_all(&frame)?;
        }
        let set_compression = data::version(client).packet_id(
            State::Login,
            Direction::Clientbound,
            "login_compression",
        );
        for Sent { state, packet } in out.client {
            self.trace("client", client, Direction::Clientbound, state, &packet);
            let frame = encode(&packet, self.client_threshold)?;
            self.client.write_all(&frame)?;
            if let Some(threshold) = compression
                && state == State::Login
                && Some(packet.id) == set_compression
                && self.client_threshold.is_none()
            {
                self.client_threshold = (threshold >= 0).then_some(threshold as usize);
            }
        }
        Ok(())
    }
}

fn pump(shared: Arc<Mutex<Shared>>, mut stream: TcpStream, from_backend: bool) -> io::Result<()> {
    loop {
        let Some(frame) = read_frame(&mut stream)? else {
            return Ok(());
        };
        let mut shared = shared.lock().unwrap();
        let threshold = if from_backend {
            shared.backend_threshold
        } else {
            shared.client_threshold
        };
        let packet = decode(&frame, threshold)?;
        let mut out = Output::default();
        let mut compression = None;
        if from_backend {
            let protocol = shared.translator.backend_protocol();
            let state = shared.translator.backend_phase();
            shared.trace("backend", protocol, Direction::Clientbound, state, &packet);
            if state == State::Login
                && data::version(protocol).packet_name(state, Direction::Clientbound, packet.id)
                    == Some("login_compression")
            {
                let threshold = Reader::new(&packet.data).varint()?;
                shared.backend_threshold = (threshold >= 0).then_some(threshold as usize);
                compression = Some(threshold);
            }
            shared.translator.clientbound(packet, &mut out)?;
        } else {
            let protocol = shared.translator.client_protocol();
            let state = shared.translator.client_phase();
            shared.trace("client", protocol, Direction::Serverbound, state, &packet);
            shared.translator.serverbound(packet, &mut out)?;
        }
        shared.send(out, compression)?;
    }
}

fn relay(mut from: TcpStream, mut to: TcpStream) {
    let _ = io::copy(&mut from, &mut to);
    let _ = to.shutdown(Shutdown::Both);
}

fn serve(
    mut client: TcpStream,
    backend_address: &str,
    backend_protocol: i32,
    trace: bool,
) -> io::Result<()> {
    client.set_nodelay(true)?;
    let handshake = read_frame(&mut client)?.ok_or_else(|| invalid("missing handshake"))?;
    let mut reader = Reader::new(&handshake);
    if reader.varint()? != 0 {
        return Err(invalid("expected handshake"));
    }
    let client_protocol = reader.varint()?;
    let address = reader.string(255)?.to_owned();
    let port = reader.u16()?;
    let next = reader.varint()?;
    let mut backend = TcpStream::connect(backend_address)?;
    backend.set_nodelay(true)?;
    if next == 1 {
        // Status: relayed as is, so a ping shows the backend's own version.
        let mut frame = Vec::new();
        write_varint(handshake.len() as u32, &mut frame);
        frame.extend_from_slice(&handshake);
        backend.write_all(&frame)?;
        let (client_read, backend_write) = (client.try_clone()?, backend.try_clone()?);
        thread::spawn(move || relay(client_read, backend_write));
        relay(backend, client);
        return Ok(());
    }
    let label = format!("{client_protocol}->{backend_protocol}");
    let Some(translator) = Translator::new(client_protocol, backend_protocol)? else {
        // Same protocol: plain relay.
        let mut frame = Vec::new();
        write_varint(handshake.len() as u32, &mut frame);
        frame.extend_from_slice(&handshake);
        backend.write_all(&frame)?;
        let (client_read, backend_write) = (client.try_clone()?, backend.try_clone()?);
        thread::spawn(move || relay(client_read, backend_write));
        relay(backend, client);
        return Ok(());
    };
    let mut body = Vec::new();
    write_varint(backend_protocol as u32, &mut body);
    write_varint(address.len() as u32, &mut body);
    body.extend_from_slice(address.as_bytes());
    body.extend_from_slice(&port.to_be_bytes());
    write_varint(next as u32, &mut body);
    backend.write_all(&encode(&Packet::new(0, body), None)?)?;
    let shared = Arc::new(Mutex::new(Shared {
        translator,
        client: client.try_clone()?,
        backend: backend.try_clone()?,
        client_threshold: None,
        backend_threshold: None,
        trace,
        label: label.clone(),
    }));
    let close = |shared: &Arc<Mutex<Shared>>| {
        let shared = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = shared.client.shutdown(Shutdown::Both);
        let _ = shared.backend.shutdown(Shutdown::Both);
    };
    let upstream = {
        let (shared, backend, label) = (shared.clone(), backend.try_clone()?, label.clone());
        thread::spawn(move || {
            if let Err(error) = pump(shared.clone(), backend, true) {
                eprintln!("[{label}] backend to client failed: {error}");
            }
            close(&shared);
        })
    };
    if let Err(error) = pump(shared.clone(), client, false) {
        // A reset after the other side closed is the normal end of a session.
        if error.kind() != io::ErrorKind::ConnectionReset {
            eprintln!("[{label}] client to backend failed: {error}");
        }
    }
    close(&shared);
    let _ = upstream.join();
    Ok(())
}

fn main() -> io::Result<()> {
    let mut listen = "127.0.0.1:25565".to_owned();
    let mut backend = "127.0.0.1:25566".to_owned();
    let mut protocol = None;
    let mut trace = std::env::var_os("RIFT_TRANSLATION_TRACE").is_some();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| invalid("missing argument value"));
        match arg.as_str() {
            "--listen" => listen = value()?,
            "--backend" => backend = value()?,
            "--backend-protocol" => {
                protocol = Some(
                    value()?
                        .parse::<i32>()
                        .map_err(|_| invalid("invalid protocol"))?,
                )
            }
            "--trace" => trace = true,
            _ => {
                return Err(invalid(
                    "usage: proxy --listen ADDR --backend ADDR --backend-protocol N [--trace]",
                ));
            }
        }
    }
    let protocol = protocol.ok_or_else(|| invalid("--backend-protocol is required"))?;
    let listener = TcpListener::bind(&listen)?;
    eprintln!("translating {listen} to {backend} (protocol {protocol})");
    for client in listener.incoming() {
        let client = client?;
        let backend = backend.clone();
        thread::spawn(move || {
            if let Err(error) = serve(client, &backend, protocol, trace) {
                eprintln!("connection failed: {error}");
            }
        });
    }
    Ok(())
}
