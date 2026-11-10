//! One translation step between two adjacent protocols, and the routing
//! tables most steps are built from.
//!
//! A step sits between a *client side* and a *backend side* protocol. It
//! receives backend packets in the backend-side format and passes them on in
//! the client-side format, and the reverse for client packets. A step may
//! pass a packet on, drop it, split it, or answer it in the other direction.
use crate::{
    Direction, Packet, State,
    data::{self, VersionData},
};
use std::{collections::VecDeque, io};

/// Converts packets between two adjacent protocols for one connection.
/// A step owns whatever per-connection state it needs.
pub trait Step: Send + Sync {
    /// A packet from the backend, in the backend-side protocol.
    fn clientbound(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()>;

    /// A packet from the client, in the client-side protocol.
    fn serverbound(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()>;
}

/// A packet a step sent on while handling another.
pub struct Emitted {
    pub direction: Direction,
    pub state: State,
    pub packet: Packet,
}

/// What a step may do while handling one packet.
pub struct Cx<'a> {
    pub emitted: &'a mut VecDeque<Emitted>,
    /// The protocol the player's client speaks.
    pub client_protocol: i32,
    /// The protocol the backend server speaks.
    pub backend_protocol: i32,
}

impl Cx<'_> {
    /// Send `packet` on towards the client, in this step's client-side
    /// protocol. `state` is the protocol phase the packet belongs to.
    pub fn to_client(&mut self, state: State, packet: Packet) {
        self.emitted.push_back(Emitted {
            direction: Direction::Clientbound,
            state,
            packet,
        });
    }

    /// Send `packet` on towards the backend, in this step's backend-side
    /// protocol. `state` is the protocol phase the packet belongs to.
    pub fn to_backend(&mut self, state: State, packet: Packet) {
        self.emitted.push_back(Emitted {
            direction: Direction::Serverbound,
            state,
            packet,
        });
    }
}

/// Handles one packet kind. The packet arrives with its ID already replaced
/// by the same-named packet's ID on the receiving side, when there is one.
pub type Handler<S> = fn(&mut S, &mut Cx<'_>, State, Packet) -> io::Result<()>;

/// Looks at one packet kind before its route runs, without changing it. The
/// packet is in the sender's format with the sender's ID. Several parts of a
/// step may tap the same packet, for example to follow the world a `login`
/// or `respawn` puts the player in.
pub type Tap<S> = fn(&mut S, State, &Packet) -> io::Result<()>;

enum Route<S> {
    /// The source protocol has no packet with this ID.
    Unknown,
    /// Same layout on both sides: only the ID changes.
    Pass(i32),
    Drop,
    /// The ID is the same-named packet on the receiving side, or -1.
    Handle(Handler<S>, i32),
    /// The receiving side has no such packet and the step declares no rule.
    Unmapped,
}

impl<S> Clone for Route<S> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<S> Copy for Route<S> {}

const STATES: [State; 3] = [State::Login, State::Configuration, State::Play];

fn state_index(state: State) -> Option<usize> {
    STATES.iter().position(|known| *known == state)
}

/// The routing table of a step type: what happens to every packet of both
/// protocols. Built once per step type with [`RouteBuilder`].
pub struct Routes<S> {
    client: &'static VersionData,
    backend: &'static VersionData,
    /// Indexed by state, then by backend-side packet ID.
    clientbound: [Vec<Entry<S>>; 3],
    /// Indexed by state, then by client-side packet ID.
    serverbound: [Vec<Entry<S>>; 3],
    problems: Vec<String>,
}

struct Entry<S> {
    route: Route<S>,
    taps: Box<[Tap<S>]>,
}

impl<S> Routes<S> {
    /// The client-side protocol's data.
    pub fn client(&self) -> &'static VersionData {
        self.client
    }

    /// The backend-side protocol's data.
    pub fn backend(&self) -> &'static VersionData {
        self.backend
    }

    /// Packets without a rule and rules naming packets that do not exist.
    /// Empty for a complete step; checked by the crate's tests.
    pub fn problems(&self) -> &[String] {
        &self.problems
    }

    fn entry(&self, direction: Direction, state: State, id: i32) -> Option<&Entry<S>> {
        let tables = match direction {
            Direction::Clientbound => &self.clientbound,
            Direction::Serverbound => &self.serverbound,
        };
        tables[state_index(state)?].get(usize::try_from(id).ok()?)
    }

    /// Apply the table to one packet. Steps built on routes call this from
    /// their [`Step`] implementation (see [`routed_step!`]).
    pub fn dispatch(
        &self,
        step: &mut S,
        cx: &mut Cx<'_>,
        direction: Direction,
        state: State,
        mut packet: Packet,
    ) -> io::Result<()> {
        let entry = self.entry(direction, state, packet.id);
        for tap in entry.map_or(&[][..], |entry| &entry.taps) {
            tap(step, state, &packet)?;
        }
        match entry.map_or(Route::Unknown, |entry| entry.route) {
            Route::Pass(id) => {
                packet.id = id;
                match direction {
                    Direction::Clientbound => cx.to_client(state, packet),
                    Direction::Serverbound => cx.to_backend(state, packet),
                }
                Ok(())
            }
            Route::Drop => Ok(()),
            Route::Handle(handler, id) => {
                if id >= 0 {
                    packet.id = id;
                }
                handler(step, cx, state, packet)
            }
            Route::Unknown | Route::Unmapped => {
                let (source, target) = match direction {
                    Direction::Clientbound => (self.backend, self.client),
                    Direction::Serverbound => (self.client, self.backend),
                };
                let name = source
                    .packet_name(state, direction, packet.id)
                    .unwrap_or("unknown");
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "no translation from {} to {} for {state:?} {direction:?} packet {:#04x} ({name})",
                        source.name, target.name, packet.id
                    ),
                ))
            }
        }
    }
}

/// Declares the routes of a step between `client` and `backend` protocols.
/// Packets with the same name on both sides pass through with their new ID
/// unless a rule says otherwise; a packet the receiving side lacks needs a
/// rule, or [`Routes::problems`] reports it.
pub struct RouteBuilder<S> {
    client: &'static VersionData,
    backend: &'static VersionData,
    rules: Vec<(Direction, State, &'static str, Rule<S>)>,
    /// Rules shared codecs install; a step's own rule for the same packet
    /// replaces them, and one naming a packet the pair lacks is ignored.
    defaults: Vec<(Direction, State, &'static str, Rule<S>)>,
    taps: Vec<(Direction, State, &'static str, Tap<S>)>,
}

enum Rule<S> {
    Handle(Handler<S>),
    Rename(&'static str),
    Drop,
}

impl<S> RouteBuilder<S> {
    pub fn new(client_protocol: i32, backend_protocol: i32) -> Self {
        Self {
            client: data::version(client_protocol),
            backend: data::version(backend_protocol),
            rules: Vec::new(),
            defaults: Vec::new(),
            taps: Vec::new(),
        }
    }

    /// The client-side protocol's data.
    pub fn client(&self) -> &'static VersionData {
        self.client
    }

    /// The backend-side protocol's data.
    pub fn backend(&self) -> &'static VersionData {
        self.backend
    }

    /// The protocol packets travelling in `direction` arrive in.
    pub fn source(&self, direction: Direction) -> &'static VersionData {
        match direction {
            Direction::Clientbound => self.backend,
            Direction::Serverbound => self.client,
        }
    }

    /// The protocol packets travelling in `direction` leave in.
    pub fn target(&self, direction: Direction) -> &'static VersionData {
        match direction {
            Direction::Clientbound => self.client,
            Direction::Serverbound => self.backend,
        }
    }

    /// Handle the packet `name` (in the sender's protocol) with `handler`
    /// unless the step declares its own rule for it. For shared codecs: no
    /// problem is reported when the sender's protocol has no such packet.
    pub fn default_handler(
        mut self,
        direction: Direction,
        state: State,
        name: &'static str,
        handler: Handler<S>,
    ) -> Self {
        self.defaults
            .push((direction, state, name, Rule::Handle(handler)));
        self
    }

    /// Run `tap` on the packet `name` (in the sender's protocol) before its
    /// route. Ignored when the sender's protocol has no such packet.
    pub fn tap(
        mut self,
        direction: Direction,
        state: State,
        name: &'static str,
        tap: Tap<S>,
    ) -> Self {
        self.taps.push((direction, state, name, tap));
        self
    }

    /// Handle the backend-side packet `name` with `handler`.
    pub fn clientbound(mut self, state: State, name: &'static str, handler: Handler<S>) -> Self {
        self.rules
            .push((Direction::Clientbound, state, name, Rule::Handle(handler)));
        self
    }

    /// Handle the client-side packet `name` with `handler`.
    pub fn serverbound(mut self, state: State, name: &'static str, handler: Handler<S>) -> Self {
        self.rules
            .push((Direction::Serverbound, state, name, Rule::Handle(handler)));
        self
    }

    /// Pass the backend-side packet `name` on unchanged as the client-side
    /// packet `to`.
    pub fn rename_clientbound(
        mut self,
        state: State,
        name: &'static str,
        to: &'static str,
    ) -> Self {
        self.rules
            .push((Direction::Clientbound, state, name, Rule::Rename(to)));
        self
    }

    /// Pass the client-side packet `name` on unchanged as the backend-side
    /// packet `to`.
    pub fn rename_serverbound(
        mut self,
        state: State,
        name: &'static str,
        to: &'static str,
    ) -> Self {
        self.rules
            .push((Direction::Serverbound, state, name, Rule::Rename(to)));
        self
    }

    /// Discard the backend-side packet `name`.
    pub fn drop_clientbound(mut self, state: State, name: &'static str) -> Self {
        self.rules
            .push((Direction::Clientbound, state, name, Rule::Drop));
        self
    }

    /// Discard the client-side packet `name`.
    pub fn drop_serverbound(mut self, state: State, name: &'static str) -> Self {
        self.rules
            .push((Direction::Serverbound, state, name, Rule::Drop));
        self
    }

    pub fn build(self) -> Routes<S> {
        let mut problems = Vec::new();
        let mut used = vec![false; self.rules.len()];
        let mut table = |direction: Direction| -> [Vec<Entry<S>>; 3] {
            let (source, target) = match direction {
                Direction::Clientbound => (self.backend, self.client),
                Direction::Serverbound => (self.client, self.backend),
            };
            STATES.map(|state| {
                source
                    .packets(state, direction)
                    .names()
                    .iter()
                    .map(|name| {
                        let matches = |(d, s, n, _): &(Direction, State, &str, _)| {
                            *d == direction && *s == state && *n == &**name
                        };
                        let taps = self
                            .taps
                            .iter()
                            .filter(|(d, s, n, _)| *d == direction && *s == state && *n == &**name)
                            .map(|(_, _, _, tap)| *tap)
                            .collect();
                        let same = target.packet_id(state, direction, name);
                        let own: Vec<usize> = (0..self.rules.len())
                            .filter(|index| matches(&self.rules[*index]))
                            .collect();
                        if own.len() > 1 {
                            problems.push(format!(
                                "{state:?} {direction:?} {name} has {} rules",
                                own.len()
                            ));
                        }
                        let rule = match own.first() {
                            Some(index) => {
                                own.iter().for_each(|index| used[*index] = true);
                                Some(&self.rules[*index].3)
                            }
                            // The codec installed last wins: later installs refine.
                            None => self
                                .defaults
                                .iter()
                                .rev()
                                .find(|rule| matches(rule))
                                .map(|rule| &rule.3),
                        };
                        let route = match rule {
                            None => same.map_or_else(
                                || {
                                    problems.push(format!(
                                        "{state:?} {direction:?} {name} ({}) has no counterpart in {} and no rule",
                                        source.name, target.name
                                    ));
                                    Route::Unmapped
                                },
                                Route::Pass,
                            ),
                            Some(Rule::Handle(handler)) => {
                                Route::Handle(*handler, same.unwrap_or(-1))
                            }
                            Some(Rule::Drop) => Route::Drop,
                            Some(Rule::Rename(to)) => {
                                match target.packet_id(state, direction, to) {
                                    Some(id) => Route::Pass(id),
                                    None => {
                                        problems.push(format!(
                                            "{state:?} {direction:?} {name} is renamed to {to}, which {} lacks",
                                            target.name
                                        ));
                                        Route::Unmapped
                                    }
                                }
                            }
                        };
                        Entry { route, taps }
                    })
                    .collect()
            })
        };
        let clientbound = table(Direction::Clientbound);
        let serverbound = table(Direction::Serverbound);
        for (index, (direction, state, name, _)) in self.rules.iter().enumerate() {
            if !used[index] {
                problems.push(format!(
                    "rule for {state:?} {direction:?} {name} matches no packet"
                ));
            }
        }
        Routes {
            client: self.client,
            backend: self.backend,
            clientbound,
            serverbound,
            problems,
        }
    }
}

/// Implement [`Step`] for a type whose `routes()` associated function
/// returns its `&'static Routes<Self>`.
#[macro_export]
macro_rules! routed_step {
    ($ty:ty) => {
        impl $crate::step::Step for $ty {
            fn clientbound(
                &mut self,
                cx: &mut $crate::step::Cx<'_>,
                state: $crate::State,
                packet: $crate::Packet,
            ) -> ::std::io::Result<()> {
                <$ty>::routes().dispatch(self, cx, $crate::Direction::Clientbound, state, packet)
            }

            fn serverbound(
                &mut self,
                cx: &mut $crate::step::Cx<'_>,
                state: $crate::State,
                packet: $crate::Packet,
            ) -> ::std::io::Result<()> {
                <$ty>::routes().dispatch(self, cx, $crate::Direction::Serverbound, state, packet)
            }
        }
    };
}

/// A step between two protocols that differ only in their version number:
/// every packet passes through with its ID unchanged.
pub struct Identity;

impl Step for Identity {
    fn clientbound(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        cx.to_client(state, packet);
        Ok(())
    }

    fn serverbound(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        cx.to_backend(state, packet);
        Ok(())
    }
}

pub fn identity() -> Box<dyn Step> {
    Box::new(Identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct Probe {
        seen: Vec<&'static str>,
    }

    impl Probe {
        fn tap(&mut self, _: State, _: &Packet) -> io::Result<()> {
            self.seen.push("tap");
            Ok(())
        }

        fn fallback(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
            self.seen.push("fallback");
            cx.to_client(state, packet);
            Ok(())
        }

        fn own(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
            self.seen.push("own");
            cx.to_client(state, packet);
            cx.to_backend(state, Packet::empty(0));
            Ok(())
        }
    }

    fn run(routes: &Routes<Probe>, name: &str) -> (Vec<&'static str>, usize, usize) {
        let mut probe = Probe::default();
        let mut emitted = VecDeque::new();
        let mut cx = Cx {
            emitted: &mut emitted,
            client_protocol: 772,
            backend_protocol: 771,
        };
        let id = routes
            .backend()
            .expect_packet(State::Play, Direction::Clientbound, name);
        routes
            .dispatch(
                &mut probe,
                &mut cx,
                Direction::Clientbound,
                State::Play,
                Packet::empty(id),
            )
            .unwrap();
        let to_client = emitted
            .iter()
            .filter(|emitted| emitted.direction == Direction::Clientbound)
            .count();
        (probe.seen, to_client, emitted.len() - to_client)
    }

    #[test]
    fn own_rules_replace_defaults_and_taps_run_first() {
        use Direction::Clientbound;
        let routes = RouteBuilder::<Probe>::new(772, 771)
            .default_handler(Clientbound, State::Play, "sound", Probe::fallback)
            .default_handler(Clientbound, State::Play, "explode", Probe::fallback)
            .default_handler(Clientbound, State::Play, "not_a_packet", Probe::fallback)
            .tap(Clientbound, State::Play, "sound", Probe::tap)
            .tap(Clientbound, State::Play, "sound", Probe::tap)
            .clientbound(State::Play, "sound", Probe::own)
            .drop_clientbound(State::Play, "set_health")
            .build();
        assert_eq!(routes.problems(), &[] as &[String]);
        assert_eq!(run(&routes, "sound"), (vec!["tap", "tap", "own"], 1, 1));
        assert_eq!(run(&routes, "explode"), (vec!["fallback"], 1, 0));
        assert_eq!(run(&routes, "set_health"), (vec![], 0, 0));
        assert_eq!(run(&routes, "keep_alive"), (vec![], 1, 0));
    }

    #[test]
    fn unknown_names_duplicates_and_unknown_ids_are_reported() {
        let routes = RouteBuilder::<Probe>::new(772, 771)
            .clientbound(State::Play, "sound", Probe::own)
            .clientbound(State::Play, "sound", Probe::own)
            .drop_serverbound(State::Play, "no_such_packet")
            .rename_clientbound(State::Play, "explode", "no_such_target")
            .build();
        let problems = routes.problems().join("\n");
        assert!(problems.contains("sound has 2 rules"), "{problems}");
        assert!(
            problems.contains("no_such_packet matches no packet"),
            "{problems}"
        );
        assert!(problems.contains("no_such_target"), "{problems}");
        let mut emitted = VecDeque::new();
        let mut cx = Cx {
            emitted: &mut emitted,
            client_protocol: 772,
            backend_protocol: 771,
        };
        let error = routes
            .dispatch(
                &mut Probe::default(),
                &mut cx,
                Direction::Clientbound,
                State::Play,
                Packet::empty(9999),
            )
            .unwrap_err();
        assert!(error.to_string().contains("no translation from"), "{error}");
    }
}
