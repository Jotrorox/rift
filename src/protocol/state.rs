use super::{Packet, PacketKind, ProtocolVersion, invalid, read_varint};
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Handshake,
    Status,
    Login,
    Configuration,
    Play,
    Closed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Serverbound,
    Clientbound,
}

/// Read and write phases can differ while an acknowledgement is in flight.
/// A client connection and a backend connection each own one of these.
#[derive(Clone, Debug)]
pub struct ConnectionState {
    serverbound: State,
    clientbound: State,
    login_started: bool,
    login_finished: bool,
}

impl Default for ConnectionState {
    fn default() -> Self {
        Self::new(State::Handshake)
    }
}
impl ConnectionState {
    pub fn new(state: State) -> Self {
        Self {
            serverbound: state,
            clientbound: state,
            login_started: false,
            login_finished: false,
        }
    }
    pub fn accept_handshake(&mut self, handshake: &super::Handshake) -> io::Result<()> {
        if !self.settled(State::Handshake) {
            return Err(invalid("duplicate handshake"));
        }
        let state = match handshake.next_state {
            super::NextState::Status => State::Status,
            _ => State::Login,
        };
        self.serverbound = state;
        self.clientbound = state;
        Ok(())
    }

    pub fn phase(&self, direction: Direction) -> State {
        match direction {
            Direction::Serverbound => self.serverbound,
            Direction::Clientbound => self.clientbound,
        }
    }
    pub fn settled(&self, state: State) -> bool {
        self.serverbound == state && self.clientbound == state
    }
    pub fn close(&mut self) {
        self.serverbound = State::Closed;
        self.clientbound = State::Closed;
    }
    pub fn observe(
        &mut self,
        version: ProtocolVersion,
        direction: Direction,
        packet: &Packet,
    ) -> io::Result<PacketKind> {
        use Direction::*;
        use PacketKind::*;
        let phase = self.phase(direction);
        let kind = version.kind(phase, direction, packet.id);
        if phase == State::Closed {
            return Err(invalid("packet on closed connection"));
        }
        if matches!(
            kind,
            LoginAcknowledged
                | FinishConfiguration
                | StartConfiguration
                | ConfigurationAcknowledged
        ) && !packet.data.is_empty()
        {
            return Err(invalid("unexpected control packet payload"));
        }
        match kind {
            Handshake => self.accept_handshake(&super::Handshake::decode(packet)?)?,
            LoginStart => {
                if self.login_started {
                    return Err(invalid("duplicate login start"));
                }
                super::start_identity(version, packet)?;
                self.login_started = true;
            }
            LoginSuccess => {
                if !self.login_started || self.login_finished {
                    return Err(invalid("unexpected login success"));
                }
                super::login_identity(version, packet)?;
                self.login_finished = true;
                if version.has_configuration() {
                    self.clientbound = State::Configuration;
                } else {
                    self.clientbound = State::Play;
                    self.serverbound = State::Play;
                }
            }
            LoginAcknowledged => {
                if !self.login_finished || self.clientbound != State::Configuration {
                    return Err(invalid("unexpected login acknowledgement"));
                }
                self.serverbound = State::Configuration;
            }
            FinishConfiguration if direction == Clientbound => {
                if self.serverbound != State::Configuration {
                    return Err(invalid("configuration is not acknowledged"));
                }
                self.clientbound = State::Play;
            }
            FinishConfiguration => {
                if self.clientbound != State::Play {
                    return Err(invalid("unexpected configuration finish acknowledgement"));
                }
                self.serverbound = State::Play;
            }
            StartConfiguration => {
                if self.serverbound != State::Play {
                    return Err(invalid("play is not acknowledged"));
                }
                self.clientbound = State::Configuration;
            }
            ConfigurationAcknowledged => {
                if self.clientbound != State::Configuration {
                    return Err(invalid("unexpected configuration acknowledgement"));
                }
                self.serverbound = State::Configuration;
            }
            SetCompression => {
                let mut bytes = packet.data.as_slice();
                read_varint(&mut bytes)?;
                if !bytes.is_empty() {
                    return Err(invalid("trailing compression data"));
                }
            }
            Disconnect => self.close(),
            Unknown if phase == State::Login || phase == State::Handshake => {
                return Err(invalid("unexpected packet in protocol state"));
            }
            _ => {}
        }
        Ok(kind)
    }
}
