//! Default handlers for the packets the text codec owns. Each converter
//! reads its packet in the sender's layout, converts the components, and
//! writes the receiver's layout, including packets that split or merge
//! between two protocols. A converter is installed for a pair only when the
//! pair needs it (see [`Rule::needed`]).
mod chat;
mod misc;
mod players;
mod scoreboard;

use super::{Component, adapt, read, transcode, transcode_full, version as v, write};
use crate::{
    Direction, Packet, State as Phase,
    codec::{HasShared, Shared},
    step::{Cx, RouteBuilder},
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::{collections::HashMap, collections::VecDeque, io};

/// Per-connection state of the packet converters.
#[derive(Default)]
pub(crate) struct State {
    /// UUIDs of resource packs pushed to a client before 1.20.3 that has
    /// not answered yet, oldest first.
    resource_packs: VecDeque<u128>,
    /// The last decorations of each map sent to a client before 1.17,
    /// which cannot keep them when an update leaves them out.
    map_icons: HashMap<i32, Vec<misc::Icon>>,
    /// The next global chat index for clients from 1.21.5 on.
    chat_index: i32,
    /// Whether the backend enforces secure chat, which 1.20.5 moved from
    /// `server_data` into `login`.
    secure_chat: bool,
}

/// One packet being converted.
pub(super) struct Cv<'a> {
    pub shared: &'a mut Shared,
    pub direction: Direction,
    /// The sender's and receiver's protocols.
    pub from: i32,
    pub to: i32,
    /// Rewrite components even where they could be copied (for tests).
    force: bool,
    out: Emitted,
}

/// Packets a converter produced: the receiver's name and the body.
pub(super) type Emitted = Vec<(&'static str, Vec<u8>)>;

impl<'a> Cv<'a> {
    pub fn new(shared: &'a mut Shared, direction: Direction) -> Self {
        let (from, to) = (shared.source(direction), shared.target(direction));
        Self {
            shared,
            direction,
            from,
            to,
            force: false,
            out: Vec::new(),
        }
    }

    /// Copy one component, converting it.
    pub fn component(&mut self, rw: &mut Rewriter<'_>) -> io::Result<()> {
        if self.force {
            transcode_full(self.shared, self.direction, rw)
        } else {
            transcode(self.shared, self.direction, rw)
        }
    }

    /// Copy an optional component (a boolean, then the component).
    pub fn optional_component(&mut self, rw: &mut Rewriter<'_>) -> io::Result<()> {
        if rw.bool()? {
            self.component(rw)?;
        }
        Ok(())
    }

    /// Read a component in the sender's format, converted to the receiver's
    /// terms.
    pub fn read_component(&mut self, r: &mut Reader<'_>) -> io::Result<Component> {
        let mut component = read(self.from, r)?;
        adapt(self.shared, self.direction, &mut component);
        Ok(component)
    }

    pub fn read_optional_component(&mut self, r: &mut Reader<'_>) -> io::Result<Option<Component>> {
        if r.bool()? {
            Ok(Some(self.read_component(r)?))
        } else {
            Ok(None)
        }
    }

    pub fn write_component(&self, w: &mut Writer, component: &Component) {
        write(self.to, component, w);
    }

    pub fn write_optional_component(&self, w: &mut Writer, component: Option<&Component>) {
        w.bool(component.is_some());
        if let Some(component) = component {
            self.write_component(w, component);
        }
    }

    /// Send `body` on as the receiver's packet `name`.
    pub fn emit(&mut self, name: &'static str, body: Vec<u8>) {
        self.out.push((name, body));
    }

    pub fn text(&mut self) -> &mut State {
        &mut self.shared.text.packets
    }
}

/// Whether `boundary` (the first protocol with a change) lies between two
/// protocols.
pub(super) fn crosses(a: i32, b: i32, boundary: i32) -> bool {
    (a < boundary) != (b < boundary)
}

/// Whether the components of a pair must be converted.
pub(super) fn text_changes(from: i32, to: i32) -> bool {
    v::format_changes(from, to) || super::registry_names_change(from, to)
}

type Convert = fn(&mut Cv<'_>, &[u8]) -> io::Result<()>;

pub(super) struct Rule {
    pub direction: Direction,
    pub phases: &'static [Phase],
    /// The packet's name in the sender's protocol.
    pub name: &'static str,
    pub convert: Convert,
    /// Whether the pair (sender's protocol, receiver's protocol) needs it.
    pub needed: fn(i32, i32) -> bool,
}

/// Converters that emit packets of other names: the packets 1.17 split
/// (titles, combat), the 1.19.3 player list forms, and the 1.20.3 score
/// removal and resource pack removal.
const RENAMING: &[&str] = &[
    "set_titles",
    "set_title_text",
    "set_subtitle_text",
    "set_action_bar_text",
    "set_titles_animation",
    "clear_titles",
    "player_combat",
    "player_combat_kill",
    "player_combat_enter",
    "player_combat_end",
    "player_info",
    "player_info_update",
    "player_info_remove",
    "reset_score",
    "resource_pack_pop",
];

impl Rule {
    /// Whether the rule converts its packet in `phase` from protocol `from`
    /// to `to`: the sender has the packet, the pair needs it, and the
    /// receiver has the same packet unless the converter renames it. A
    /// packet that disappears otherwise is left to the step.
    fn applies(&self, phase: Phase, from: i32, to: i32) -> bool {
        let (source, target) = (crate::data::version(from), crate::data::version(to));
        source.packet_id(phase, self.direction, self.name).is_some()
            && (self.needed)(from, to)
            && (RENAMING.contains(&self.name)
                || target.packet_id(phase, self.direction, self.name).is_some())
    }
}

const PLAY: &[Phase] = &[Phase::Play];
const BOTH: &[Phase] = &[Phase::Configuration, Phase::Play];

macro_rules! rules {
    ($($direction:ident $phases:ident $name:literal => $convert:path, $needed:path;)*) => {
        pub(super) const RULES: &[Rule] = &[$(Rule {
            direction: Direction::$direction,
            phases: $phases,
            name: $name,
            convert: $convert,
            needed: $needed,
        }),*];
    };
}

rules! {
    Clientbound PLAY "chat" => chat::chat, chat::chat_needed;
    Clientbound PLAY "system_chat" => chat::system_chat, chat::system_chat_needed;
    Clientbound PLAY "disguised_chat" => chat::disguised_chat, chat::disguised_chat_needed;
    Clientbound PLAY "player_chat" => chat::player_chat, chat::player_chat_needed;
    Clientbound BOTH "disconnect" => misc::disconnect, text_changes;
    Clientbound PLAY "set_titles" => misc::set_titles, misc::titles_needed;
    Clientbound PLAY "set_title_text" => misc::set_title_text, misc::titles_needed;
    Clientbound PLAY "set_subtitle_text" => misc::set_subtitle_text, misc::titles_needed;
    Clientbound PLAY "set_action_bar_text" => misc::set_action_bar_text, misc::titles_needed;
    Clientbound PLAY "set_titles_animation" => misc::set_titles_animation, misc::titles_split;
    Clientbound PLAY "clear_titles" => misc::clear_titles, misc::titles_split;
    Clientbound PLAY "boss_event" => misc::boss_event, text_changes;
    Clientbound PLAY "tab_list" => misc::tab_list, text_changes;
    Clientbound PLAY "player_combat" => misc::player_combat, misc::combat_needed;
    Clientbound PLAY "player_combat_kill" => misc::player_combat_kill, misc::combat_kill_needed;
    Clientbound PLAY "player_combat_enter" => misc::player_combat_enter, misc::combat_split;
    Clientbound PLAY "player_combat_end" => misc::player_combat_end, misc::combat_split;
    Clientbound BOTH "resource_pack_push" => misc::resource_pack_push, misc::resource_pack_push_needed;
    Clientbound BOTH "resource_pack_pop" => misc::resource_pack_pop, misc::resource_pack_split;
    Serverbound BOTH "resource_pack" => misc::resource_pack, misc::resource_pack_split;
    Clientbound PLAY "server_data" => misc::server_data, misc::server_data_needed;
    Clientbound PLAY "map_item_data" => misc::map_item_data, misc::map_needed;
    Clientbound PLAY "command_suggestions" => misc::command_suggestions, misc::suggestions_needed;
    Clientbound PLAY "player_info" => players::player_info, players::player_info_needed;
    Clientbound PLAY "player_info_update" => players::player_info_update, players::player_info_update_needed;
    Clientbound PLAY "player_info_remove" => players::player_info_remove, players::player_info_remove_needed;
    Clientbound PLAY "set_objective" => scoreboard::set_objective, scoreboard::objective_needed;
    Clientbound PLAY "set_display_objective" => scoreboard::set_display_objective, scoreboard::display_needed;
    Clientbound PLAY "set_score" => scoreboard::set_score, scoreboard::score_needed;
    Clientbound PLAY "reset_score" => scoreboard::reset_score, scoreboard::score_needed;
    Clientbound PLAY "set_player_team" => scoreboard::set_player_team, scoreboard::team_needed;
}

/// Run rule `N` on one packet and send what it produced.
fn handle<S: HasShared, const N: usize>(
    step: &mut S,
    cx: &mut Cx<'_>,
    phase: Phase,
    packet: Packet,
) -> io::Result<()> {
    let rule = RULES.get(N).ok_or_else(|| invalid("no such text rule"))?;
    let mut cv = Cv::new(step.shared(), rule.direction);
    (rule.convert)(&mut cv, &packet.data)?;
    let target = cv.shared.target_data(rule.direction);
    for (name, body) in cv.out {
        let id = target
            .packet_id(phase, rule.direction, name)
            .ok_or_else(|| invalid(&format!("{} has no packet {name}", target.name)))?;
        match rule.direction {
            Direction::Clientbound => cx.to_client(phase, Packet::new(id, body)),
            Direction::Serverbound => cx.to_backend(phase, Packet::new(id, body)),
        }
    }
    Ok(())
}

/// Run a converter directly between `shared`'s protocols, rewriting every
/// component; returns the packets as (receiver's name, body). For tests.
#[cfg(test)]
pub(super) fn convert_forced(
    shared: &mut Shared,
    direction: Direction,
    phase: Phase,
    name: &str,
    body: &[u8],
) -> Option<io::Result<Emitted>> {
    let rule = RULES.iter().find(|rule| {
        rule.name == name && rule.direction == direction && rule.phases.contains(&phase)
    })?;
    let (from, to) = (shared.source(direction), shared.target(direction));
    // Within one protocol, a rule that converts that protocol's packet for
    // any other protocol reads and writes its layout.
    let applies = if from == to {
        crate::version::PROTOCOLS
            .iter()
            .any(|(other, _)| *other != from && rule.applies(phase, from, *other))
    } else {
        rule.applies(phase, from, to)
    };
    if !applies {
        return None;
    }
    let mut cv = Cv::new(shared, direction);
    cv.force = true;
    Some((rule.convert)(&mut cv, body).map(|()| cv.out))
}

macro_rules! install_rules {
    ($builder:ident, $($index:literal)*) => {{
        let mut builder = $builder;
        $(
            if let Some(rule) = RULES.get($index) {
                builder = install_one(builder, rule, handle::<S, $index>);
            }
        )*
        builder
    }};
}

fn install_one<S: HasShared + 'static>(
    builder: RouteBuilder<S>,
    rule: &Rule,
    handler: crate::step::Handler<S>,
) -> RouteBuilder<S> {
    let (from, to) = (
        builder.source(rule.direction).protocol,
        builder.target(rule.direction).protocol,
    );
    let mut builder = builder;
    for phase in rule.phases {
        if rule.applies(*phase, from, to) {
            builder = builder.default_handler(rule.direction, *phase, rule.name, handler);
        }
    }
    builder
}

pub(super) fn install<S: HasShared + 'static>(builder: RouteBuilder<S>) -> RouteBuilder<S> {
    const _: () = assert!(RULES.len() <= 40, "add indices to install_rules");
    let builder = install_rules!(builder, 0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39);
    let backend = builder.backend().protocol;
    if backend >= misc::SECURE_CHAT_MOVED && builder.client().protocol < misc::SECURE_CHAT_MOVED {
        builder.tap(Direction::Clientbound, Phase::Play, "login", login::<S>)
    } else {
        builder
    }
}

/// Follow the secure chat flag of a 1.20.5+ login for older clients'
/// `server_data`. It is the login packet's last field from 1.20.5 on.
fn login<S: HasShared>(step: &mut S, _: Phase, packet: &Packet) -> io::Result<()> {
    step.shared().text.packets.secure_chat = packet.data.last() == Some(&1);
    Ok(())
}

#[cfg(test)]
mod tests;
