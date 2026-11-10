//! Tags, statistics, command trees, sounds and menus.
//!
//! Each packet's handler is installed only for the pairs whose two
//! protocols lay it out differently or number a registry it refers to
//! differently; elsewhere it passes through untouched.
pub mod commands;
pub mod registry;
pub mod screen;
pub mod sound;
pub mod stats;
pub mod tags;

use super::HasShared;
use crate::{Direction, State as Phase, step::RouteBuilder};

/// What this codec keeps for one step of one connection.
#[derive(Default)]
pub struct State {
    /// Drives the seeds given to sounds that gain one (1.19 and later).
    seed: u64,
}

impl State {
    /// A seed for a sound whose sender did not give one (SplitMix64).
    pub fn next_seed(&mut self) -> i64 {
        self.seed = self.seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.seed;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (value ^ (value >> 31)) as i64
    }
}

/// Install this codec's default routes for the builder's pair.
pub fn install<S: HasShared + 'static>(builder: RouteBuilder<S>) -> RouteBuilder<S> {
    use Direction::Clientbound;
    let (source, target) = (builder.backend().protocol, builder.client().protocol);
    let mut builder = builder;
    if tags::needed(source, target) {
        for phase in [Phase::Configuration, Phase::Play] {
            builder =
                builder.default_handler(Clientbound, phase, "update_tags", tags::update_tags::<S>);
        }
    }
    if commands::needed(source, target) {
        builder = builder.default_handler(
            Clientbound,
            Phase::Play,
            "commands",
            commands::commands::<S>,
        );
    }
    if stats::needed(source, target) {
        builder = builder.default_handler(
            Clientbound,
            Phase::Play,
            "award_stats",
            stats::award_stats::<S>,
        );
    }
    for (kind, name, handler) in [
        (
            sound::Kind::Sound,
            "sound",
            sound::sound::<S> as crate::step::Handler<S>,
        ),
        (
            sound::Kind::CustomSound,
            "custom_sound",
            sound::custom_sound::<S>,
        ),
        (
            sound::Kind::SoundEntity,
            "sound_entity",
            sound::sound_entity::<S>,
        ),
    ] {
        if sound::needed(kind, source, target) {
            builder = builder.default_handler(Clientbound, Phase::Play, name, handler);
        }
    }
    if source >= 393 && sound::stop_needed(source, target) {
        builder = builder.default_handler(
            Clientbound,
            Phase::Play,
            "stop_sound",
            sound::stop_sound::<S>,
        );
    }
    if screen::open_needed(source, target) {
        builder = builder.default_handler(
            Clientbound,
            Phase::Play,
            "open_screen",
            screen::open_screen::<S>,
        );
    }
    if source >= 477 && screen::mount_needed(source, target) {
        builder = builder.default_handler(
            Clientbound,
            Phase::Play,
            "mount_screen_open",
            screen::mount_screen_open::<S>,
        );
    }
    builder
}
