//! Block states a 1.12 client computes from neighbouring blocks when it
//! draws them, and a 1.13 client expects the server to send: fence, pane and
//! wall connections, stairs shapes, redstone wire sides, chest halves, the
//! hidden halves of doors and tall plants, attached stems, chorus plants,
//! fire on walls, tripwires, snowy grass, vines hanging from blocks and fence
//! gates in walls.
//!
//! The rules follow vanilla 1.12's (`getActualState` of each block): a face
//! is "solid" when the block's collision shape covers it completely
//! (`faces.json`), and the blocks vanilla excludes from attaching (glass,
//! leaves, ice, ...) do not attach. Not covered: repeaters locked by a
//! powered neighbour (needs redstone power, which the client does not get).
use crate::data::{self, Block, BlockStates};
use serde_json::Value;
use std::sync::OnceLock;

/// Faces and directions: down, up, north, south, west, east.
pub const DOWN: usize = 0;
pub const UP: usize = 1;
const NORTH: usize = 2;
const SOUTH: usize = 3;
const WEST: usize = 4;
const EAST: usize = 5;
pub const OFFSETS: [(i32, i32, i32); 6] = [
    (0, -1, 0),
    (0, 1, 0),
    (0, 0, -1),
    (0, 0, 1),
    (-1, 0, 0),
    (1, 0, 0),
];
const NAMES: [&str; 6] = ["down", "up", "north", "south", "west", "east"];
/// Horizontal directions in vanilla's iteration order.
const HORIZONTAL: [usize; 4] = [NORTH, EAST, SOUTH, WEST];

fn opposite(direction: usize) -> usize {
    direction ^ 1
}

/// Clockwise seen from above.
fn clockwise(direction: usize) -> usize {
    match direction {
        NORTH => EAST,
        EAST => SOUTH,
        SOUTH => WEST,
        WEST => NORTH,
        other => other,
    }
}

fn counter_clockwise(direction: usize) -> usize {
    match direction {
        NORTH => WEST,
        WEST => SOUTH,
        SOUTH => EAST,
        EAST => NORTH,
        other => other,
    }
}

fn axis(direction: usize) -> usize {
    direction / 2
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Nothing about the block depends on its neighbours or matters to
    /// them beyond its faces and flags.
    Plain,
    WoodFence,
    NetherFence,
    Pane,
    Wall,
    Gate,
    Stairs,
    Wire,
    Repeater,
    Observer,
    Door,
    TallPlant,
    Chest,
    TrappedChest,
    MelonStem,
    PumpkinStem,
    ChorusPlant,
    ChorusFlower,
    Fire,
    Snowy,
    Tripwire,
    TripwireHook,
    Vine,
}

impl Kind {
    /// Whether the block's own state depends on its neighbours.
    fn connects(self) -> bool {
        !matches!(
            self,
            Self::Plain | Self::Repeater | Self::Observer | Self::ChorusFlower | Self::TripwireHook
        )
    }
}

/// Flags of a state, as the rules ask about neighbours.
const FULL: u16 = 1;
/// Its material is opaque (glass, leaves, ice, TNT... are not).
const OPAQUE: u16 = 2;
/// It is a redstone power source wire connects to from any side.
const POWER: u16 = 4;
/// Vanilla 1.12 does not let fences, panes and walls attach to it.
const EXCEPTED: u16 = 8;
const FLAMMABLE: u16 = 16;
const AIR: u16 = 32;
const SNOW: u16 = 64;
const GLOWSTONE: u16 = 128;
const MELON: u16 = 256;
const PUMPKIN: u16 = 512;
const END_STONE: u16 = 1024;

#[derive(Clone, Copy, Debug)]
pub struct Info {
    pub kind: Kind,
    /// Full faces, by direction bit.
    faces: u8,
    flags: u16,
    /// The `facing` property as a direction, if any.
    facing: Option<u8>,
    /// `half=top`/`upper`.
    top: bool,
    /// The state the store keeps for this one.
    pub stored: u16,
}

impl Info {
    fn face(&self, direction: usize) -> bool {
        self.faces & 1 << direction != 0
    }

    fn has(&self, flag: u16) -> bool {
        self.flags & flag != 0
    }

    /// vanilla's `isNormalCube`: an opaque full cube that is not a power
    /// source.
    fn normal_cube(&self) -> bool {
        self.has(FULL) && self.has(OPAQUE) && !self.has(POWER)
    }

    /// vanilla's `isTopSolid`.
    fn top_solid(&self) -> bool {
        self.face(UP) && self.has(OPAQUE)
    }
}

const TRANSPARENT: &[&str] = &[
    "glass",
    "ice",
    "frosted_ice",
    "glowstone",
    "sea_lantern",
    "beacon",
    "tnt",
    "spawner",
];
const EXCEPTIONS: &[&str] = &[
    "beacon",
    "cauldron",
    "glass",
    "glowstone",
    "ice",
    "sea_lantern",
    "piston",
    "sticky_piston",
    "piston_head",
    "moving_piston",
];
const POWER_SOURCES: &[&str] = &[
    "redstone_torch",
    "redstone_wall_torch",
    "lever",
    "detector_rail",
    "daylight_detector",
    "redstone_block",
    "trapped_chest",
    "tripwire_hook",
    "comparator",
    "light_weighted_pressure_plate",
    "heavy_weighted_pressure_plate",
];
const FLAMMABLES: &[&str] = &[
    "bookshelf",
    "tnt",
    "grass",
    "fern",
    "dead_bush",
    "sunflower",
    "lilac",
    "rose_bush",
    "peony",
    "tall_grass",
    "large_fern",
    "dandelion",
    "poppy",
    "blue_orchid",
    "allium",
    "azure_bluet",
    "oxeye_daisy",
    "vine",
    "coal_block",
    "hay_block",
];
const WOODS: [&str; 6] = ["oak", "spruce", "birch", "jungle", "acacia", "dark_oak"];
const TALL_PLANTS: [&str; 6] = [
    "sunflower",
    "lilac",
    "tall_grass",
    "large_fern",
    "rose_bush",
    "peony",
];

fn kind_of(name: &str) -> Kind {
    match name {
        "nether_brick_fence" => Kind::NetherFence,
        "glass_pane" | "iron_bars" => Kind::Pane,
        "cobblestone_wall" | "mossy_cobblestone_wall" => Kind::Wall,
        "redstone_wire" => Kind::Wire,
        "repeater" => Kind::Repeater,
        "observer" => Kind::Observer,
        "chest" => Kind::Chest,
        "trapped_chest" => Kind::TrappedChest,
        "melon_stem" | "attached_melon_stem" => Kind::MelonStem,
        "pumpkin_stem" | "attached_pumpkin_stem" => Kind::PumpkinStem,
        "chorus_plant" => Kind::ChorusPlant,
        "chorus_flower" => Kind::ChorusFlower,
        "fire" => Kind::Fire,
        "grass_block" | "podzol" | "mycelium" => Kind::Snowy,
        "tripwire" => Kind::Tripwire,
        "tripwire_hook" => Kind::TripwireHook,
        "vine" => Kind::Vine,
        _ if name.ends_with("_stained_glass_pane") => Kind::Pane,
        _ if name.ends_with("_fence_gate") => Kind::Gate,
        _ if name.ends_with("_fence") => Kind::WoodFence,
        _ if name.ends_with("_stairs") => Kind::Stairs,
        _ if name.ends_with("_door") => Kind::Door,
        _ if TALL_PLANTS.contains(&name) => Kind::TallPlant,
        _ => Kind::Plain,
    }
}

fn flags_of(name: &str, faces: u8) -> u16 {
    let mut flags = 0;
    if faces == 63 {
        flags |= FULL;
    }
    let transparent = TRANSPARENT.contains(&name)
        || name.ends_with("_stained_glass")
        || name.ends_with("_leaves")
        || name.ends_with("shulker_box");
    if !transparent {
        flags |= OPAQUE;
    }
    if EXCEPTIONS.contains(&name)
        || name.ends_with("_stained_glass")
        || name.ends_with("_leaves")
        || name.ends_with("shulker_box")
    {
        flags |= EXCEPTED;
    }
    if POWER_SOURCES.contains(&name)
        || name.ends_with("_button")
        || name.ends_with("_pressure_plate")
    {
        flags |= POWER;
    }
    let wooden = WOODS
        .iter()
        .any(|wood| name.starts_with(&format!("{wood}_")) || name.starts_with("stripped_"));
    if FLAMMABLES.contains(&name)
        || name.ends_with("_wool")
        || name.ends_with("_carpet")
        || name.ends_with("_tulip")
        || name.ends_with("_leaves")
        || (wooden
            && [
                "_planks",
                "_slab",
                "_fence_gate",
                "_fence",
                "_stairs",
                "_log",
                "_wood",
            ]
            .iter()
            .any(|suffix| name.ends_with(suffix)))
    {
        flags |= FLAMMABLE;
    }
    match name {
        "air" | "cave_air" | "void_air" => flags |= AIR,
        "snow" | "snow_block" => flags |= SNOW,
        "glowstone" => flags |= GLOWSTONE,
        "melon" => flags |= MELON,
        "pumpkin" | "carved_pumpkin" => flags |= PUMPKIN,
        "end_stone" => flags |= END_STONE,
        _ => {}
    }
    flags
}

pub fn states() -> &'static BlockStates {
    &data::version(super::super::super::NEWER).block_states
}

/// Every 1.13 state's info, by state ID.
pub fn infos() -> &'static [Info] {
    static INFOS: OnceLock<Vec<Info>> = OnceLock::new();
    INFOS.get_or_init(|| {
        let faces: serde_json::Map<String, Value> =
            serde_json::from_str(include_str!("../faces.json")).expect("faces.json is an object");
        let states = states();
        let mut infos = Vec::with_capacity(states.len() as usize);
        // Stored representatives of plain states by (faces, flags).
        let mut representatives: Vec<((u8, u16), u16)> = Vec::new();
        for block in &states.blocks {
            let kind = kind_of(&block.name);
            let name = &*block.name;
            for offset in 0..block.state_count() {
                let state = block.first_state + offset;
                let mut face = match faces.get(name) {
                    Some(Value::Array(masks)) => masks.get(offset as usize).and_then(Value::as_u64),
                    Some(mask) => mask.as_u64(),
                    None => None,
                }
                .unwrap_or(0) as u8;
                if name == "soul_sand" {
                    // Vanilla 1.12 treats it as a full block despite its
                    // lowered top.
                    face = 63;
                }
                let flags = flags_of(name, face);
                let values = block.values(offset);
                let value = |property: &str| {
                    values
                        .iter()
                        .find(|(name, _)| *name == property)
                        .map(|(_, value)| *value)
                };
                let facing = value("facing")
                    .and_then(|facing| NAMES.iter().position(|name| *name == facing))
                    .map(|facing| facing as u8);
                let top = matches!(value("half"), Some("top" | "upper"));
                let stored = if kind == Kind::Plain {
                    let signature = (face, flags);
                    match representatives
                        .iter()
                        .find(|(known, _)| *known == signature)
                    {
                        Some((_, representative)) => *representative,
                        None => {
                            representatives.push((signature, state as u16));
                            state as u16
                        }
                    }
                } else {
                    state as u16
                };
                infos.push(Info {
                    kind,
                    faces: face,
                    flags,
                    facing,
                    top,
                    stored,
                });
            }
        }
        infos
    })
}

pub fn info(state: u16) -> &'static Info {
    let infos = infos();
    infos.get(usize::from(state)).unwrap_or(&infos[0])
}

/// Whether a state depends on its neighbours.
pub fn connects(state: u16) -> bool {
    info(state).kind.connects()
}

/// Whether a state depends on its neighbours' faces, which for stairs
/// depend on their shape: these connect after everything else.
pub fn face_dependent(state: u16) -> bool {
    matches!(
        info(state).kind,
        Kind::WoodFence | Kind::NetherFence | Kind::Pane | Kind::Wall
    )
}

/// Neighbouring blocks, as the client has them; `None` is unknown (not
/// loaded), which the rules treat as air.
pub trait Blocks {
    fn state(&self, x: i32, y: i32, z: i32) -> Option<u16>;
}

struct Around<'a, B: Blocks> {
    blocks: &'a B,
    x: i32,
    y: i32,
    z: i32,
    /// Whether a block the rule looked at is unknown.
    unknown: std::cell::Cell<bool>,
}

impl<B: Blocks> Around<'_, B> {
    fn at(&self, dx: i32, dy: i32, dz: i32) -> u16 {
        self.blocks
            .state(self.x + dx, self.y + dy, self.z + dz)
            .unwrap_or_else(|| {
                self.unknown.set(true);
                0
            })
    }

    fn side(&self, direction: usize) -> u16 {
        let (dx, dy, dz) = OFFSETS[direction];
        self.at(dx, dy, dz)
    }

    fn info(&self, direction: usize) -> &'static Info {
        info(self.side(direction))
    }
}

/// The state of `block` with `state`'s values and `changes`.
fn with(block: &Block, offset: i32, changes: &[(&str, &str)]) -> Option<i32> {
    let mut values = block.values(offset);
    for (property, value) in changes {
        if let Some(slot) = values.iter_mut().find(|(name, _)| name == property) {
            slot.1 = value;
        }
    }
    Some(block.first_state + block.offset(&values)?)
}

fn flag(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

/// The state a block at (x, y, z) has once connected to its neighbours.
/// Unknown neighbours count as air, unless `keep_on_unknown`: then a block
/// that depends on one keeps its state (the client still has the
/// neighbour, only the step forgot it).
pub fn connect<B: Blocks>(
    blocks: &B,
    x: i32,
    y: i32,
    z: i32,
    state: u16,
    keep_on_unknown: bool,
) -> u16 {
    let info = info(state);
    if !info.kind.connects() {
        return state;
    }
    let states = states();
    let Some((block, offset)) = states.locate(i32::from(state)) else {
        return state;
    };
    let around = Around {
        blocks,
        x,
        y,
        z,
        unknown: std::cell::Cell::new(false),
    };
    let connected = match info.kind {
        Kind::WoodFence | Kind::NetherFence | Kind::Pane | Kind::Wall => {
            let mut sides = [false; 6];
            for direction in HORIZONTAL {
                sides[direction] = attaches(info.kind, around.info(direction), direction);
            }
            let mut changes: Vec<(&str, &str)> = HORIZONTAL
                .iter()
                .map(|direction| (NAMES[*direction], flag(sides[*direction])))
                .collect();
            if info.kind == Kind::Wall {
                let straight = (sides[NORTH] && sides[SOUTH] && !sides[EAST] && !sides[WEST])
                    || (sides[EAST] && sides[WEST] && !sides[NORTH] && !sides[SOUTH]);
                changes.push(("up", flag(!straight || !around.info(UP).has(AIR))));
            }
            with(block, offset, &changes)
        }
        Kind::Gate => {
            let facing = usize::from(info.facing.unwrap_or(NORTH as u8));
            let wall = |direction| around.info(direction).kind == Kind::Wall;
            let in_wall = if axis(facing) == axis(NORTH) {
                wall(WEST) || wall(EAST)
            } else {
                wall(NORTH) || wall(SOUTH)
            };
            with(block, offset, &[("in_wall", flag(in_wall))])
        }
        Kind::Stairs => with(block, offset, &[("shape", stairs_shape(&around, info))]),
        Kind::Wire => {
            let sides: Vec<(&str, &str)> = HORIZONTAL
                .iter()
                .map(|direction| (NAMES[*direction], wire_side(&around, *direction)))
                .collect();
            with(block, offset, &sides)
        }
        Kind::Door => door(&around, block, offset, info),
        Kind::TallPlant => {
            if !info.top {
                return state;
            }
            let below = around.side(DOWN);
            let below_info = info_of(below);
            match states.locate(i32::from(below)) {
                Some((plant, _)) if below_info.kind == Kind::TallPlant && !below_info.top => plant
                    .offset(&[("half", "upper")])
                    .map(|offset| plant.first_state + offset),
                _ => None,
            }
        }
        Kind::Chest | Kind::TrappedChest => {
            let facing = usize::from(info.facing.unwrap_or(NORTH as u8));
            let partner = |direction: usize| {
                let other = around.info(direction);
                other.kind == info.kind && other.facing == info.facing
            };
            let kind = if partner(clockwise(facing)) {
                "left"
            } else if partner(counter_clockwise(facing)) {
                "right"
            } else {
                "single"
            };
            with(block, offset, &[("type", kind)])
        }
        Kind::MelonStem | Kind::PumpkinStem => stem(&around, block, offset, info),
        Kind::ChorusPlant => {
            let changes: Vec<(&str, &str)> = (0..6)
                .map(|direction| {
                    let other = around.info(direction);
                    let joins = matches!(other.kind, Kind::ChorusPlant | Kind::ChorusFlower)
                        || (direction == DOWN && other.has(END_STONE));
                    (NAMES[direction], flag(joins))
                })
                .collect();
            with(block, offset, &changes)
        }
        Kind::Fire => {
            let below = around.info(DOWN);
            let grounded = below.top_solid() || below.has(FLAMMABLE);
            let changes: Vec<(&str, &str)> = [NORTH, EAST, SOUTH, WEST, UP]
                .iter()
                .map(|direction| {
                    (
                        NAMES[*direction],
                        flag(!grounded && around.info(*direction).has(FLAMMABLE)),
                    )
                })
                .collect();
            with(block, offset, &changes)
        }
        Kind::Snowy => with(block, offset, &[("snowy", flag(around.info(UP).has(SNOW)))]),
        Kind::Tripwire => {
            let changes: Vec<(&str, &str)> = HORIZONTAL
                .iter()
                .map(|direction| {
                    let other = around.info(*direction);
                    let joins = other.kind == Kind::Tripwire
                        || (other.kind == Kind::TripwireHook
                            && other.facing == Some(opposite(*direction) as u8));
                    (NAMES[*direction], flag(joins))
                })
                .collect();
            with(block, offset, &changes)
        }
        Kind::Vine => with(block, offset, &[("up", flag(around.info(UP).face(DOWN)))]),
        Kind::Plain | Kind::Repeater | Kind::Observer | Kind::ChorusFlower | Kind::TripwireHook => {
            None
        }
    };
    if keep_on_unknown && around.unknown.get() {
        return state;
    }
    connected.map_or(state, |state| state as u16)
}

fn info_of(state: u16) -> &'static Info {
    info(state)
}

/// Whether a fence, pane or wall attaches to `other`, which lies in
/// `direction`: the same family, a fence gate across its path (not panes),
/// or a full face that is not excepted.
fn attaches(kind: Kind, other: &Info, direction: usize) -> bool {
    let solid = other.face(opposite(direction)) && !other.has(EXCEPTED);
    let gate = other.kind == Kind::Gate
        && other
            .facing
            .is_some_and(|facing| axis(usize::from(facing)) != axis(direction));
    solid
        || match kind {
            Kind::WoodFence | Kind::NetherFence => other.kind == kind || gate,
            Kind::Pane => other.kind == Kind::Pane,
            Kind::Wall => other.kind == Kind::Wall || gate,
            _ => false,
        }
}

fn stairs_shape<B: Blocks>(around: &Around<'_, B>, info: &Info) -> &'static str {
    let facing = usize::from(info.facing.unwrap_or(NORTH as u8));
    let stairs = |direction: usize| {
        let other = around.info(direction);
        (other.kind == Kind::Stairs && other.top == info.top).then_some(other)
    };
    // Whether the block in `direction` is not stairs like this one.
    let different = |direction: usize| {
        let other = around.info(direction);
        other.kind != Kind::Stairs || other.facing != info.facing || other.top != info.top
    };
    if let Some(back) = stairs(facing) {
        let back_facing = usize::from(back.facing.unwrap_or(NORTH as u8));
        if axis(back_facing) != axis(facing) && different(opposite(back_facing)) {
            return if back_facing == counter_clockwise(facing) {
                "outer_left"
            } else {
                "outer_right"
            };
        }
    }
    if let Some(front) = stairs(opposite(facing)) {
        let front_facing = usize::from(front.facing.unwrap_or(NORTH as u8));
        if axis(front_facing) != axis(facing) && different(front_facing) {
            return if front_facing == counter_clockwise(facing) {
                "inner_left"
            } else {
                "inner_right"
            };
        }
    }
    "straight"
}

/// Whether wire connects to `other` seen in `direction` (`None`: from
/// above or below, where only wire counts).
fn wire_connects(other: &Info, direction: Option<usize>) -> bool {
    match other.kind {
        Kind::Wire => true,
        Kind::Repeater => direction.is_some_and(|direction| {
            other
                .facing
                .is_some_and(|facing| axis(usize::from(facing)) == axis(direction))
        }),
        Kind::Observer => direction.is_some_and(|direction| other.facing == Some(direction as u8)),
        _ => other.has(POWER) && direction.is_some(),
    }
}

/// A wire's side: it runs to wire or a power source beside it, or down to
/// wire below a non-solid neighbour; failing that, up a solid neighbour to
/// wire on top of it unless a solid block above cuts it off.
fn wire_side<B: Blocks>(around: &Around<'_, B>, direction: usize) -> &'static str {
    let (dx, _, dz) = OFFSETS[direction];
    let neighbour = info(around.at(dx, 0, dz));
    if wire_connects(neighbour, Some(direction))
        || (!neighbour.normal_cube() && wire_connects(info(around.at(dx, -1, dz)), None))
    {
        return "side";
    }
    if !info(around.at(0, 1, 0)).normal_cube()
        && (neighbour.top_solid() || neighbour.has(GLOWSTONE))
        && wire_connects(info(around.at(dx, 1, dz)), None)
    {
        return if neighbour.has(FULL) { "up" } else { "side" };
    }
    "none"
}

/// Each door half carries the other half's properties in 1.13: facing and
/// open from the lower half, hinge and powered from the upper.
fn door<B: Blocks>(around: &Around<'_, B>, block: &Block, offset: i32, info: &Info) -> Option<i32> {
    let states = states();
    let other = around.side(if info.top { DOWN } else { UP });
    let (other_block, other_offset) = states.locate(i32::from(other))?;
    if other_block.name != block.name || info_of(other).top == info.top {
        return None;
    }
    let values = other_block.values(other_offset);
    let value = |property: &str| {
        values
            .iter()
            .find(|(name, _)| *name == property)
            .map(|(_, value)| *value)
    };
    let copied: &[&str] = if info.top {
        &["facing", "open"]
    } else {
        &["hinge", "powered"]
    };
    let changes: Vec<(&str, &str)> = copied
        .iter()
        .filter_map(|property| Some((*property, value(property)?)))
        .collect();
    with(block, offset, &changes)
}

/// A fully grown stem bends towards an adjacent melon or pumpkin (the first
/// in vanilla's order) and becomes the attached stem; an attached stem
/// whose fruit is gone is a grown stem again.
fn stem<B: Blocks>(around: &Around<'_, B>, block: &Block, offset: i32, info: &Info) -> Option<i32> {
    let states = states();
    let (stem, attached, crop) = if info.kind == Kind::MelonStem {
        ("melon_stem", "attached_melon_stem", MELON)
    } else {
        ("pumpkin_stem", "attached_pumpkin_stem", PUMPKIN)
    };
    let grown = &*block.name == attached
        || block
            .values(offset)
            .iter()
            .any(|(name, value)| *name == "age" && *value == "7");
    if !grown {
        return None;
    }
    for direction in HORIZONTAL {
        if around.info(direction).has(crop) {
            let target = states.block(attached)?;
            return Some(target.first_state + target.offset(&[("facing", NAMES[direction])])?);
        }
    }
    let target = states.block(stem)?;
    Some(target.first_state + target.offset(&[("age", "7")])?)
}
