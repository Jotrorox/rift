//! IDs of the packets the step emits under another name than it received.
use crate::{Direction, State, data};
use std::sync::OnceLock;

pub struct Ids {
    pub update_recipes: i32,
    pub update_tags: i32,
    pub commands: i32,
    pub stop_sound: i32,
    pub block_update: i32,
    pub chunk_blocks_update: i32,
    pub level_chunk: i32,
    /// The 1.12 serverbound plugin message.
    pub custom_payload: i32,
}

pub fn ids() -> &'static Ids {
    static IDS: OnceLock<Ids> = OnceLock::new();
    IDS.get_or_init(|| {
        let (client, backend) = (
            data::version(super::super::NEWER),
            data::version(super::super::OLDER),
        );
        let to_client = |name| client.expect_packet(State::Play, Direction::Clientbound, name);
        Ids {
            update_recipes: to_client("update_recipes"),
            update_tags: to_client("update_tags"),
            commands: to_client("commands"),
            stop_sound: to_client("stop_sound"),
            block_update: to_client("block_update"),
            chunk_blocks_update: to_client("chunk_blocks_update"),
            level_chunk: to_client("level_chunk"),
            custom_payload: backend.expect_packet(
                State::Play,
                Direction::Serverbound,
                "custom_payload",
            ),
        }
    })
}
