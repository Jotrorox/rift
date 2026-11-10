//! What the steps from 1.14.4 to 1.16.5 share: a step made of the shared
//! codecs alone, and a harness for testing steps packet by packet.

/// Declare `$name`, a step between `$client` and `$backend` that is nothing
/// but the shared codecs' defaults: for pairs whose packets differ only in
/// what those codecs convert (often nothing but the protocol number).
macro_rules! defaults_step {
    ($name:ident, $client:expr, $backend:expr) => {
        pub struct $name {
            shared: $crate::codec::Shared,
        }

        impl $crate::codec::HasShared for $name {
            fn shared(&mut self) -> &mut $crate::codec::Shared {
                &mut self.shared
            }
        }

        impl $name {
            fn create() -> Box<dyn $crate::step::Step> {
                Box::new(Self {
                    shared: $crate::codec::Shared::new($client, $backend),
                })
            }

            pub fn routes() -> &'static $crate::step::Routes<Self> {
                static ROUTES: ::std::sync::OnceLock<$crate::step::Routes<$name>> =
                    ::std::sync::OnceLock::new();
                ROUTES.get_or_init(|| {
                    $crate::step::RouteBuilder::new($client, $backend)
                        .defaults()
                        .build()
                })
            }
        }

        $crate::routed_step!($name);

        pub const STEP: Option<$crate::steps::Make> = Some($name::create);

        #[cfg(test)]
        mod tests {
            #[test]
            fn every_packet_has_a_route() {
                assert_eq!(super::$name::routes().problems(), &[] as &[String]);
            }
        }
    };
}
pub(in crate::steps) use defaults_step;

#[cfg(test)]
pub(in crate::steps) mod harness {
    use crate::{
        Direction, Packet, State,
        codec::testing::corpus,
        data,
        step::{Cx, Emitted, Step},
        wire::Writer,
    };
    use std::{collections::VecDeque, io};

    /// A packet a step emitted, named in the protocol it is in.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Out {
        pub direction: Direction,
        pub state: State,
        pub name: String,
        pub data: Vec<u8>,
    }

    /// One step instance between `client` and `backend`, fed by packet name.
    pub struct Harness {
        pub step: Box<dyn Step>,
        pub client: i32,
        pub backend: i32,
    }

    impl Harness {
        pub fn new(step: Box<dyn Step>, client: i32, backend: i32) -> Self {
            Self {
                step,
                client,
                backend,
            }
        }

        /// Send the packet `name` of the sender's protocol through the step.
        pub fn send(
            &mut self,
            direction: Direction,
            state: State,
            name: &str,
            body: &[u8],
        ) -> io::Result<Vec<Out>> {
            let source = match direction {
                Direction::Clientbound => self.backend,
                Direction::Serverbound => self.client,
            };
            let id = data::version(source).expect_packet(state, direction, name);
            self.send_id(direction, state, id, body)
        }

        pub fn send_id(
            &mut self,
            direction: Direction,
            state: State,
            id: i32,
            body: &[u8],
        ) -> io::Result<Vec<Out>> {
            let mut emitted = VecDeque::new();
            let mut cx = Cx {
                emitted: &mut emitted,
                client_protocol: self.client,
                backend_protocol: self.backend,
            };
            let packet = Packet::new(id, body.to_vec());
            match direction {
                Direction::Clientbound => self.step.clientbound(&mut cx, state, packet)?,
                Direction::Serverbound => self.step.serverbound(&mut cx, state, packet)?,
            }
            Ok(emitted
                .into_iter()
                .map(
                    |Emitted {
                         direction,
                         state,
                         packet,
                     }| {
                        let protocol = match direction {
                            Direction::Clientbound => self.client,
                            Direction::Serverbound => self.backend,
                        };
                        Out {
                            direction,
                            state,
                            name: data::version(protocol)
                                .packet_name(state, direction, packet.id)
                                .unwrap_or("?")
                                .to_owned(),
                            data: packet.data,
                        }
                    },
                )
                .collect())
        }

        pub fn clientbound(&mut self, name: &str, body: &[u8]) -> io::Result<Vec<Out>> {
            self.send(Direction::Clientbound, State::Play, name, body)
        }

        pub fn serverbound(&mut self, name: &str, body: &[u8]) -> io::Result<Vec<Out>> {
            self.send(Direction::Serverbound, State::Play, name, body)
        }

        /// Exactly one packet comes out, named `name`; its body.
        pub fn one(&mut self, direction: Direction, name: &str, body: &[u8], out: &str) -> Vec<u8> {
            let sent = self
                .send(direction, State::Play, name, body)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(sent.len(), 1, "{name} became {sent:?}");
            assert_eq!(sent[0].name, out, "{name}");
            sent[0].data.clone()
        }
    }

    /// Whether `error` comes from a shared codec helper that is still a stub
    /// (item stacks, text components, particles), not from the step.
    pub fn unimplemented(error: &io::Error) -> bool {
        error.to_string().ends_with("are not implemented")
    }

    /// Feed every packet of `protocol`'s recorded session that travels from
    /// the step's sender side through `harness`, asserting none fails except
    /// where a stubbed codec helper does (those are named on stderr).
    /// Returns how many went through, or `None` without a corpus.
    pub fn replay(harness: &mut Harness, protocol: i32, direction: Direction) -> Option<usize> {
        let packets = corpus(protocol)?;
        let mut count = 0;
        for packet in packets
            .iter()
            .filter(|packet| packet.direction == direction)
        {
            match harness.send_id(direction, packet.state, packet.id, &packet.data) {
                Ok(_) => count += 1,
                Err(error) if unimplemented(&error) => eprintln!(
                    "{protocol} {}: waits for a codec: {error}",
                    packet.name(protocol)
                ),
                Err(error) => panic!(
                    "{protocol} {:?} {}: {error}",
                    packet.state,
                    packet.name(protocol)
                ),
            }
        }
        Some(count)
    }

    pub fn hex(text: &str) -> Vec<u8> {
        let text: String = text.split_whitespace().collect();
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect()
    }

    /// Build a body with a [`Writer`].
    pub fn body(build: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut writer = Writer::new();
        build(&mut writer);
        writer.buf
    }

    /// Every malformed prefix of `body` fails without panicking.
    pub fn truncations_fail(harness: &mut Harness, direction: Direction, name: &str, body: &[u8]) {
        for size in 0..body.len() {
            assert!(
                harness
                    .send(direction, State::Play, name, &body[..size])
                    .is_err(),
                "{name} truncated to {size} bytes was accepted"
            );
        }
        let mut longer = body.to_vec();
        longer.push(0);
        assert!(
            harness.send(direction, State::Play, name, &longer).is_err(),
            "{name} with a trailing byte was accepted"
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::{Direction, State, data, mapping};

    /// The pairs built with [`defaults_step!`] differ in nothing but their
    /// protocol number: same packet tables and identical registries.
    #[test]
    fn identical_protocols() {
        for (older, newer) in [(573, 575), (575, 578), (735, 736), (751, 753)] {
            let (a, b) = (data::version(older), data::version(newer));
            for state in [State::Login, State::Configuration, State::Play] {
                for direction in [Direction::Clientbound, Direction::Serverbound] {
                    assert_eq!(
                        a.packets(state, direction).names(),
                        b.packets(state, direction).names(),
                        "{older}-{newer} {state:?} {direction:?}"
                    );
                }
            }
            for registry in mapping::shared_registries(older, newer) {
                for (from, to) in [(older, newer), (newer, older)] {
                    let map = mapping::mapping(from, to, &registry);
                    let target = data::version(to);
                    let size = match registry.as_str() {
                        "block_state" => target.block_states.len() as usize,
                        name => target.expect_registry(name).len(),
                    };
                    assert!(
                        map.is_identity() && map.len() == size,
                        "{from}->{to} {registry} is not the identity"
                    );
                }
            }
        }
    }
}
