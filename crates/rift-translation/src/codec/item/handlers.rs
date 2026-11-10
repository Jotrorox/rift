//! Default handlers for the item-carrying packets, and which pairs need them.
use super::{
    ItemStack, Stack,
    advancements::Advancements,
    components::{self, Components},
    convert,
    convert::{item_id, item_name, map_item},
    merchant::Offers,
    recipes::{
        self, BookAdd, GhostRecipe, PlaceGhostRecipe, RecipeBook, RecipeData, SlotDisplay,
        UpdateRecipes,
    },
    window::{
        self, Click, ClickSlot, Cooldown, CooldownKey, CreativeSlot, EditBook, HashedStack,
        SetContent, SetEquipment, SetPlayerInventory, SetSlot,
    },
};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    data, mapping,
    nbt::{Compound, Nbt},
    step::{Cx, RouteBuilder},
    version,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::io;

pub(super) use table::RecipeTable;

mod table;

/// Whether `a` and `b` lie on different sides of any of `boundaries` (the
/// first protocol of each new layout).
fn crosses(a: i32, b: i32, boundaries: &[i32]) -> bool {
    let (low, high) = (a.min(b), a.max(b));
    boundaries
        .iter()
        .any(|boundary| low < *boundary && *boundary <= high)
}

/// Stack layouts change at 1.13, 1.13.2, 1.20.2 and 1.20.5.
const STACK_LAYOUTS: [i32; 4] = [393, 404, 764, 766];

fn registry_changes(source: i32, target: i32, registry: &str) -> bool {
    mapping::try_mapping(source, target, registry).is_none_or(|mapping| !mapping.is_identity())
}

/// Pairs whose item tags change beyond item IDs (see `tags` and `flatten`).
fn tags_change(low: i32, high: i32) -> bool {
    matches!(
        (low, high),
        (340, 393) | (404, 477) | (578, 735) | (754, 755) | (763, 764) | (764, 765)
    )
}

/// INTEGRATION: the component codec should say which pairs change
/// component layouts or IDs; until it does, a pair is assumed to when its
/// component type registries differ or its items do.
fn components_change(source: i32, target: i32) -> bool {
    version::item_components(source.min(target))
        && registry_changes(source, target, "data_component_type")
}

/// Whether stacks sent from `source` to `target` need converting.
fn stacks_change(source: i32, target: i32) -> bool {
    crosses(source, target, &STACK_LAYOUTS)
        || registry_changes(source, target, "item")
        || tags_change(source.min(target), source.max(target))
        || components_change(source, target)
}

/// INTEGRATION: the text codec should say which pairs it converts text
/// components for; these are the pairs known to change them (1.13
/// translation keys, 1.16 colours and hover contents, 1.20.3 NBT text,
/// 1.21.5 snake-case events).
fn text_changes(source: i32, target: i32) -> bool {
    let pair = (source.min(target), source.max(target));
    matches!(pair, (340, 393) | (578, 735) | (764, 765) | (769, 770))
}

/// Defaults for packets the handler emits under the same name: none for a
/// pair whose receiver lacks the packet, which its step converts.
trait Paired<S> {
    fn default_handler_paired(
        self,
        direction: Direction,
        state: State,
        name: &'static str,
        handler: crate::step::Handler<S>,
    ) -> Self;
}

impl<S> Paired<S> for RouteBuilder<S> {
    fn default_handler_paired(
        self,
        direction: Direction,
        state: State,
        name: &'static str,
        handler: crate::step::Handler<S>,
    ) -> Self {
        if self
            .target(direction)
            .packet_id(state, direction, name)
            .is_none()
        {
            return self;
        }
        self.default_handler(direction, state, name, handler)
    }
}

pub(super) fn install<S: HasShared + 'static>(builder: RouteBuilder<S>) -> RouteBuilder<S> {
    use crate::State::{Configuration, Play};
    use Direction::{Clientbound, Serverbound};
    let (client, backend) = (builder.client().protocol, builder.backend().protocol);
    let mut builder = builder;
    // Clientbound packets go from backend to client, serverbound the reverse.
    let down = stacks_change(backend, client);
    let up = stacks_change(client, backend);
    if down || crosses(backend, client, &[756, 768]) {
        builder = builder
            .default_handler_paired(Clientbound, Play, "container_set_content", set_content::<S>)
            .default_handler(Clientbound, Play, "container_set_slot", set_slot::<S>);
    }
    if down {
        builder = builder
            .default_handler(Clientbound, Play, "set_cursor_item", set_cursor_item::<S>)
            .default_handler(
                Clientbound,
                Play,
                "set_player_inventory",
                set_player_inventory::<S>,
            );
    }
    if down || crosses(backend, client, &[735, 766, 770]) {
        builder = builder
            .default_handler(Clientbound, Play, "set_equipment", set_equipment::<S>)
            .default_handler(Clientbound, Play, "set_equipped_item", set_equipment::<S>);
    }
    if registry_changes(backend, client, "item") || crosses(backend, client, &[768]) {
        builder = builder.default_handler_paired(Clientbound, Play, "cooldown", cooldown::<S>);
    }
    if down || crosses(backend, client, &[490, 498, 759, 766]) {
        builder = builder.default_handler_paired(
            Clientbound,
            Play,
            "merchant_offers",
            merchant_offers::<S>,
        );
    }
    if down
        || text_changes(backend, client)
        || crosses(backend, client, &[763, 764, 765, 770, 775, 777])
    {
        builder = builder.default_handler_paired(
            Clientbound,
            Play,
            "update_advancements",
            advancements::<S>,
        );
    }
    let recipes_change =
        down || crosses(
            backend,
            client,
            &[477, 761, 762, 763, 765, 766, 768, 770, 775, 777],
        ) || registry_changes(backend, client, "recipe_serializer")
            || registry_changes(backend, client, "slot_display")
            || registry_changes(backend, client, "recipe_display")
            || registry_changes(backend, client, "recipe_book_category");
    // The flattening renumbers recipes (1.12 numbers them, 1.13 names them):
    // only the 1.12.2-1.13 step can know how.
    let numbered = crosses(backend, client, &[393]);
    if recipes_change && !numbered {
        for state in [Configuration, Play] {
            builder = builder.default_handler_paired(
                Clientbound,
                state,
                "update_recipes",
                update_recipes::<S>,
            );
        }
        builder = builder
            .default_handler(Clientbound, Play, "recipe_book_add", recipe_book_add::<S>)
            .default_handler_paired(
                Clientbound,
                Play,
                "place_ghost_recipe",
                place_ghost_recipe::<S>,
            );
    }
    if crosses(backend, client, &[751, 768]) && !numbered {
        builder = builder.default_handler(Clientbound, Play, "recipe", recipe_book::<S>);
    }
    if crosses(backend, client, &[768]) {
        builder = builder
            .default_handler(
                Clientbound,
                Play,
                "recipe_book_settings",
                recipe_book_settings::<S>,
            )
            .default_handler(
                Clientbound,
                Play,
                "recipe_book_remove",
                recipe_book_remove::<S>,
            );
    }
    if up || crosses(client, backend, &[755, 756, 768, 770]) {
        builder = builder.default_handler_paired(
            Serverbound,
            Play,
            "container_click",
            container_click::<S>,
        );
    }
    if up || crosses(client, backend, &[770]) {
        builder = builder.default_handler_paired(
            Serverbound,
            Play,
            "set_creative_mode_slot",
            creative_slot::<S>,
        );
    }
    if up || crosses(client, backend, &[401, 754, 756]) {
        builder = builder.default_handler_paired(Serverbound, Play, "edit_book", edit_book::<S>);
    }
    if crosses(client, backend, &[754]) {
        // Books are edited by inventory slot from 1.16.4 on; follow the
        // selected slot for clients that name the hand instead.
        builder = builder
            .tap(
                Serverbound,
                Play,
                "set_carried_item",
                held_slot_serverbound::<S>,
            )
            .tap(
                Clientbound,
                Play,
                "set_held_slot",
                held_slot_clientbound::<S>,
            );
    }
    builder
}

/// Read a packet completely in `protocol`'s layout.
fn parse<T>(
    protocol: i32,
    data: &[u8],
    read: fn(i32, &mut Reader<'_>) -> io::Result<T>,
) -> io::Result<T> {
    let mut reader = Reader::new(data);
    let value = read(protocol, &mut reader)?;
    reader.finish()?;
    Ok(value)
}

fn encode(write: impl FnOnce(&mut Writer) -> io::Result<()>) -> io::Result<Vec<u8>> {
    let mut writer = Writer::new();
    write(&mut writer)?;
    Ok(writer.buf)
}

/// The receiver's ID of the packet `name`, for packets the receiver calls
/// differently.
fn target_id(shared: &Shared, direction: Direction, state: State, name: &str) -> io::Result<i32> {
    shared
        .target_data(direction)
        .packet_id(state, direction, name)
        .ok_or_else(|| invalid(&format!("the receiving protocol has no {name} packet")))
}

fn convert_all<'a>(
    shared: &mut Shared,
    direction: Direction,
    stacks: impl IntoIterator<Item = &'a mut Stack>,
) -> io::Result<()> {
    stacks
        .into_iter()
        .try_for_each(|stack| convert::stack(shared, direction, stack))
}

/// Remember what the backend put where, for clients that report hashed
/// stacks in clicks (1.21.5 on) to a backend that wants them whole.
fn tracks_slots(shared: &Shared) -> bool {
    window::hashed_clicks(shared.client) && !window::hashed_clicks(shared.backend)
}

fn track(shared: &mut Shared, container: i32, slot: i16, stack: &Stack) {
    match stack {
        Some(stack) => {
            shared.item.slots.insert((container, slot), stack.clone());
        }
        None => {
            shared.item.slots.remove(&(container, slot));
        }
    }
}

/// The cursor is tracked as container -1, slot -1.
const CURSOR: (i32, i16) = (-1, -1);

fn set_content<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let (source, target) = (shared.source(direction), shared.target(direction));
    let mut content = parse(source, &packet.data, SetContent::read)?;
    if window::has_state_ids(source) {
        shared.item.state_id = content.state_id;
    }
    if tracks_slots(shared) {
        shared
            .item
            .slots
            .retain(|(container, _), _| *container != content.container);
        for (slot, stack) in content.items.iter().enumerate() {
            track(shared, content.container, slot as i16, stack);
        }
        if window::has_state_ids(source) {
            track(shared, CURSOR.0, CURSOR.1, &content.carried);
        }
    }
    convert_all(shared, direction, content.items.iter_mut())?;
    convert::stack(shared, direction, &mut content.carried)?;
    let separate_cursor = window::has_state_ids(source) && !window::has_state_ids(target);
    let carried = if separate_cursor {
        std::mem::take(&mut content.carried)
    } else {
        None
    };
    if window::has_state_ids(target) && !window::has_state_ids(source) {
        // A 1.17 backend sends the cursor separately; use the last one.
        content.carried = shared.item.cursor.clone();
    }
    cx.to_client(
        state,
        Packet::new(packet.id, encode(|w| content.write(target, w))?),
    );
    if separate_cursor {
        // A 1.17 client learns its cursor from the cursor slot.
        let cursor = SetSlot {
            container: CURSOR.0,
            state_id: 0,
            slot: CURSOR.1,
            item: carried,
        };
        let id = target_id(shared, direction, state, "container_set_slot")?;
        cx.to_client(state, Packet::new(id, encode(|w| cursor.write(target, w))?));
    }
    Ok(())
}

fn set_slot<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let (source, target) = (shared.source(direction), shared.target(direction));
    let mut slot = parse(source, &packet.data, SetSlot::read)?;
    if window::has_state_ids(source) {
        shared.item.state_id = slot.state_id;
    }
    if tracks_slots(shared) {
        track(shared, slot.container, slot.slot, &slot.item);
    }
    convert::stack(shared, direction, &mut slot.item)?;
    if (slot.container, slot.slot) == CURSOR {
        shared.item.cursor = slot.item.clone();
    }
    let separate = recipes::displays(target) && !recipes::displays(source);
    match slot.container {
        // 1.21.2 has packets of its own for the cursor and the inventory.
        -1 if separate => {
            let id = target_id(shared, direction, state, "set_cursor_item")?;
            let data = encode(|w| window::write_stack(target, w, &slot.item))?;
            cx.to_client(state, Packet::new(id, data));
        }
        -2 if separate => {
            let inventory = SetPlayerInventory {
                slot: i32::from(slot.slot),
                item: slot.item,
            };
            let id = target_id(shared, direction, state, "set_player_inventory")?;
            cx.to_client(
                state,
                Packet::new(id, encode(|w| inventory.write(target, w))?),
            );
        }
        _ => {
            let data = encode(|w| slot.write(target, w))?;
            cx.to_client(state, Packet::new(packet.id, data));
        }
    }
    Ok(())
}

fn set_cursor_item<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let (source, target) = (shared.source(direction), shared.target(direction));
    let mut stack = parse(source, &packet.data, window::read_stack)?;
    if tracks_slots(shared) {
        track(shared, CURSOR.0, CURSOR.1, &stack);
    }
    convert::stack(shared, direction, &mut stack)?;
    if recipes::displays(target) {
        cx.to_client(
            state,
            Packet::new(
                packet.id,
                encode(|w| window::write_stack(target, w, &stack))?,
            ),
        );
        return Ok(());
    }
    let slot = SetSlot {
        container: -1,
        state_id: shared.item.state_id,
        slot: -1,
        item: stack,
    };
    let id = target_id(shared, direction, state, "container_set_slot")?;
    cx.to_client(state, Packet::new(id, encode(|w| slot.write(target, w))?));
    Ok(())
}

fn set_player_inventory<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let (source, target) = (shared.source(direction), shared.target(direction));
    let mut inventory = parse(source, &packet.data, SetPlayerInventory::read)?;
    convert::stack(shared, direction, &mut inventory.item)?;
    if recipes::displays(target) {
        cx.to_client(
            state,
            Packet::new(packet.id, encode(|w| inventory.write(target, w))?),
        );
        return Ok(());
    }
    let slot = SetSlot {
        container: -2,
        state_id: shared.item.state_id,
        slot: i16::try_from(inventory.slot).map_err(|_| invalid("inventory slot out of range"))?,
        item: inventory.item,
    };
    let id = target_id(shared, direction, state, "container_set_slot")?;
    cx.to_client(state, Packet::new(id, encode(|w| slot.write(target, w))?));
    Ok(())
}

fn set_equipment<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let (source, target) = (shared.source(direction), shared.target(direction));
    let mut equipment = parse(source, &packet.data, SetEquipment::read)?;
    // Slots the receiver does not know (body armour, saddles) are left out.
    let slots = window::equipment_slots(target);
    equipment
        .slots
        .retain(|(slot, _)| (0..slots).contains(slot));
    if equipment.slots.is_empty() {
        return Ok(());
    }
    convert_all(
        shared,
        direction,
        equipment.slots.iter_mut().map(|(_, stack)| stack),
    )?;
    let name = if window::equipment_list(target) {
        "set_equipment"
    } else {
        "set_equipped_item"
    };
    let id = target_id(shared, direction, state, name)?;
    let packets = if window::equipment_list(target) {
        vec![equipment]
    } else {
        equipment.split()
    };
    for equipment in packets {
        cx.to_client(
            state,
            Packet::new(id, encode(|w| equipment.write(target, w))?),
        );
    }
    Ok(())
}

fn cooldown<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let (source, target) = (shared.source(direction), shared.target(direction));
    let mut cooldown = parse(source, &packet.data, Cooldown::read)?;
    let item = match &cooldown.key {
        CooldownKey::Item(id) => Some(*id),
        // Groups named after an item (all of vanilla's) map to that item.
        CooldownKey::Group(group) => item_id(source, group),
    };
    let mapped = match item {
        Some(id) => {
            Some(map_item(shared, direction, id)?).filter(|mapped| mapped.stand_in_for.is_none())
        }
        None => None,
    };
    cooldown.key = match (window::cooldown_groups(target), mapped, &cooldown.key) {
        (false, Some(mapped), _) => CooldownKey::Item(mapped.id),
        // An item the older client lacks, or a custom group: nothing to show.
        (false, None, _) => return Ok(()),
        (true, Some(mapped), _) => CooldownKey::Group(format!(
            "minecraft:{}",
            item_name(target, mapped.id).unwrap_or("stone")
        )),
        (true, None, CooldownKey::Group(group)) => CooldownKey::Group(group.clone()),
        (true, None, CooldownKey::Item(_)) => return Ok(()),
    };
    cx.to_client(
        state,
        Packet::new(packet.id, encode(|w| cooldown.write(target, w))?),
    );
    Ok(())
}

/// Convert the stacks of villager trades between the step's protocols.
/// Before 1.14 trades travel in a plugin channel, which the 1.12.2-1.13 and
/// 1.13.2-1.14 steps own; they read and write [`Offers`] in either layout
/// and convert them with this.
pub fn convert_offers(
    shared: &mut Shared,
    direction: Direction,
    offers: &mut Offers,
) -> io::Result<()> {
    let target = shared.target(direction);
    for offer in &mut offers.offers {
        convert_all(
            shared,
            direction,
            [&mut offer.cost, &mut offer.result, &mut offer.second_cost],
        )?;
    }
    if version::item_components(target) {
        // Costs cannot be empty from 1.20.5 on.
        offers.offers.retain(|offer| offer.cost.is_some());
    }
    Ok(())
}

/// Copy a trade list plugin payload (after the channel name, protocols
/// before 1.14 on both sides) from `rw.r` to `rw.w`, converting its stacks.
pub fn transcode_trade_list(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
) -> io::Result<()> {
    let mut offers = Offers::read(shared.source(direction), &mut rw.r)?;
    convert_offers(shared, direction, &mut offers)?;
    offers.write(shared.target(direction), &mut rw.w)
}

fn merchant_offers<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let (source, target) = (shared.source(direction), shared.target(direction));
    let mut offers = parse(source, &packet.data, Offers::read)?;
    convert_offers(shared, direction, &mut offers)?;
    cx.to_client(
        state,
        Packet::new(packet.id, encode(|w| offers.write(target, w))?),
    );
    Ok(())
}

fn advancements<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let (source, target) = (shared.source(direction), shared.target(direction));
    let mut advancements = parse(source, &packet.data, Advancements::read)?;
    let convert_text = text_changes(source, target) || crosses(source, target, &[765]);
    for advancement in &mut advancements.added {
        if let Some(display) = &mut advancement.display {
            if convert_text {
                display.title.convert(shared, direction)?;
                display.description.convert(shared, direction)?;
            }
            convert::stack(shared, direction, &mut display.icon)?;
            if display.icon.is_none() && version::item_components(target) {
                // Icons cannot be empty in stack templates.
                display.icon = Some(stone(target, 1)?);
            }
        }
    }
    if target < 764 {
        advancements.fill_criteria();
    }
    cx.to_client(
        state,
        Packet::new(packet.id, encode(|w| advancements.write(target, w))?),
    );
    Ok(())
}

fn stone(protocol: i32, count: i32) -> io::Result<ItemStack> {
    Ok(ItemStack {
        id: item_id(protocol, "stone").ok_or_else(|| invalid("no stone"))?,
        count,
        damage: 0,
        tag: None,
        components: version::item_components(protocol).then(Components::default),
    })
}

/// Serializers of 1.13 (protocol 393 to 404); later protocols have the
/// `recipe_serializer` registry.
const SERIALIZERS_1_13: &[&str] = &[
    "crafting_shaped",
    "crafting_shapeless",
    "crafting_special_armordye",
    "crafting_special_bookcloning",
    "crafting_special_mapcloning",
    "crafting_special_mapextending",
    "crafting_special_firework_rocket",
    "crafting_special_firework_star",
    "crafting_special_firework_star_fade",
    "crafting_special_repairitem",
    "crafting_special_tippedarrow",
    "crafting_special_bannerduplicate",
    "crafting_special_banneraddpattern",
    "crafting_special_shielddecoration",
    "crafting_special_shulkerboxcoloring",
    "smelting",
];

fn has_serializer(protocol: i32, name: &str) -> bool {
    if protocol < 477 {
        return SERIALIZERS_1_13.contains(&name);
    }
    data::version(protocol)
        .registry("recipe_serializer")
        .is_some_and(|registry| registry.id(name).is_some())
}

/// The crafting and cooking book categories' "misc" entries (1.19.3 on).
const CRAFTING_MISC: i32 = 3;
const COOKING_MISC: i32 = 2;

/// Convert one recipe; `false` when the receiver cannot have it.
fn convert_recipe(
    shared: &mut Shared,
    direction: Direction,
    recipe: &mut recipes::Recipe,
) -> io::Result<bool> {
    let (source, target) = (shared.source(direction), shared.target(direction));
    if !has_serializer(target, &recipe.serializer) {
        return Ok(false);
    }
    for stack in recipe.data.stacks() {
        convert::stack(shared, direction, stack)?;
    }
    if !(761..).contains(&source) && target >= 761 {
        match &mut recipe.data {
            RecipeData::Shapeless { category, .. }
            | RecipeData::Shaped { category, .. }
            | RecipeData::Special { category } => *category = CRAFTING_MISC,
            RecipeData::Cooking { category, .. } => *category = COOKING_MISC,
            _ => {}
        }
    }
    if version::item_components(target) {
        // Ingredients hold no empty stacks and results are never empty.
        let mut complete = true;
        let ingredients: Vec<&mut recipes::Ingredient> = match &mut recipe.data {
            RecipeData::Shapeless {
                ingredients,
                result,
                ..
            }
            | RecipeData::Shaped {
                ingredients,
                result,
                ..
            } => {
                complete &= result.is_some();
                ingredients.iter_mut().collect()
            }
            RecipeData::Cooking {
                ingredient, result, ..
            }
            | RecipeData::Stonecutting {
                ingredient, result, ..
            } => {
                complete &= result.is_some();
                vec![ingredient]
            }
            RecipeData::SmithingTransform {
                template,
                base,
                addition,
                result,
            } => {
                complete &= result.is_some();
                vec![template, base, addition]
            }
            RecipeData::SmithingTrim {
                template,
                base,
                addition,
            } => vec![template, base, addition],
            RecipeData::Smithing {
                base,
                addition,
                result,
            } => {
                complete &= result.is_some();
                vec![base, addition]
            }
            RecipeData::Special { .. } => Vec::new(),
        };
        for ingredient in ingredients {
            ingredient.retain(Option::is_some);
        }
        if !complete {
            return Ok(false);
        }
    }
    Ok(true)
}

fn update_recipes<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let (source, target) = (shared.source(direction), shared.target(direction));
    let recipes = parse(source, &packet.data, UpdateRecipes::read)?;
    let converted = match recipes {
        UpdateRecipes::Recipes(mut list) if !recipes::displays(target) => {
            let mut kept = Vec::with_capacity(list.len());
            for mut recipe in list.drain(..) {
                if convert_recipe(shared, direction, &mut recipe)? {
                    kept.push(recipe);
                }
            }
            UpdateRecipes::Recipes(kept)
        }
        UpdateRecipes::Recipes(list) => {
            // A 1.21.2 client learns recipes from the book; remember them.
            let mut converted = Vec::with_capacity(list.len());
            for mut recipe in list {
                if convert_recipe(shared, direction, &mut recipe)?
                    || recipe.serializer.starts_with("crafting_special_")
                {
                    converted.push(recipe);
                }
            }
            shared.item.recipes = RecipeTable::new(converted);
            shared.item.recipes.update_recipes(target)?
        }
        UpdateRecipes::Sets {
            mut property_sets,
            mut stonecutter,
        } if recipes::displays(target) => {
            let mapping = shared.mapping(direction, "item");
            for (_, items) in &mut property_sets {
                items.retain_mut(|item| match mapping.get(*item) {
                    Some(mapped) => {
                        *item = mapped;
                        true
                    }
                    None => false,
                });
            }
            for (input, display) in &mut stonecutter {
                map_id_set(shared, direction, input);
                convert_display(shared, direction, display)?;
            }
            UpdateRecipes::Sets {
                property_sets,
                stonecutter,
            }
        }
        UpdateRecipes::Sets { .. } => {
            // INTEGRATION: an older client needs whole recipes, which a
            // 1.21.2 backend only describes as displays in the recipe book;
            // until recipes are synthesised from displays it gets none.
            UpdateRecipes::Recipes(Vec::new())
        }
    };
    cx.to_client(
        state,
        Packet::new(packet.id, encode(|w| converted.write(target, w))?),
    );
    Ok(())
}

fn map_id_set(shared: &mut Shared, direction: Direction, set: &mut recipes::IdSet) {
    if let recipes::IdSet::Ids(ids) = set {
        let mapping = shared.mapping(direction, "item");
        ids.retain_mut(|id| match mapping.get(*id) {
            Some(mapped) => {
                *id = mapped;
                true
            }
            None => false,
        });
    }
}

/// Convert a slot display: items, stacks, and kinds the receiver lacks
/// (which become empty).
fn convert_display(
    shared: &mut Shared,
    direction: Direction,
    display: &mut SlotDisplay,
) -> io::Result<()> {
    let target = shared.target(direction);
    let has_kind = |display: &SlotDisplay| {
        data::version(target)
            .registry("slot_display")
            .is_some_and(|registry| registry.id(display.kind()).is_some())
    };
    display.visit(&mut |display| {
        let replace = match display {
            _ if !has_kind(display) => Some(SlotDisplay::Empty),
            SlotDisplay::Item(id) => Some(match map_item(shared, direction, *id)? {
                mapped if mapped.stand_in_for.is_none() => SlotDisplay::Item(mapped.id),
                _ => SlotDisplay::Empty,
            }),
            SlotDisplay::Stack(stack) => {
                let mut converted = Some(stack.clone());
                convert::stack(shared, direction, &mut converted)?;
                Some(converted.map_or(SlotDisplay::Empty, SlotDisplay::Stack))
            }
            SlotDisplay::Items(set) => {
                map_id_set(shared, direction, set);
                None
            }
            SlotDisplay::Tag(_) if target >= 777 => Some(SlotDisplay::Empty),
            SlotDisplay::SmithingTrim { pattern, .. } => {
                // The pattern changed from a display to a trim pattern in
                // 1.21.5; neither tells the other.
                let display_pattern = matches!(pattern, recipes::TrimPattern::Display(_));
                (display_pattern != (target < 770)).then_some(SlotDisplay::Empty)
            }
            SlotDisplay::OnlyWithComponent(_, component) => {
                let mapping = shared.mapping(direction, "data_component_type");
                match mapping.get(*component) {
                    Some(mapped) => {
                        *component = mapped;
                        None
                    }
                    None => Some(SlotDisplay::Empty),
                }
            }
            _ => None,
        };
        if let Some(replace) = replace {
            *display = replace;
        }
        Ok(())
    })
}

fn convert_recipe_display(
    shared: &mut Shared,
    direction: Direction,
    display: &mut recipes::RecipeDisplay,
) -> io::Result<()> {
    // `convert_display` visits what each slot nests.
    display
        .slots()
        .into_iter()
        .try_for_each(|slot| convert_display(shared, direction, slot))
}

fn recipe_book_add<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let (source, target) = (shared.source(direction), shared.target(direction));
    if !recipes::displays(target) {
        // INTEGRATION: see `update_recipes`: no recipes for older clients
        // from a 1.21.2 backend yet.
        return Ok(());
    }
    let mut book = parse(source, &packet.data, BookAdd::read)?;
    let categories = shared.mapping(direction, "recipe_book_category");
    book.entries
        .retain_mut(|entry| match categories.get(entry.category) {
            Some(category) => {
                entry.category = category;
                true
            }
            None => false,
        });
    for entry in &mut book.entries {
        convert_recipe_display(shared, direction, &mut entry.display)?;
        for set in entry.requirements.iter_mut().flatten() {
            map_id_set(shared, direction, set);
        }
    }
    cx.to_client(
        state,
        Packet::new(packet.id, encode(|w| book.write(target, w))?),
    );
    Ok(())
}

fn place_ghost_recipe<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let (source, target) = (shared.source(direction), shared.target(direction));
    let mut ghost = parse(source, &packet.data, PlaceGhostRecipe::read)?;
    ghost.recipe = match ghost.recipe {
        GhostRecipe::Display(mut display) if recipes::displays(target) => {
            convert_recipe_display(shared, direction, &mut display)?;
            GhostRecipe::Display(display)
        }
        GhostRecipe::Name(name) if recipes::displays(target) => {
            match shared.item.recipes.display(&name) {
                Some(display) => GhostRecipe::Display(Box::new(display)),
                None => return Ok(()),
            }
        }
        // INTEGRATION: a display cannot name a recipe of an older client.
        GhostRecipe::Display(_) => return Ok(()),
        other => other,
    };
    cx.to_client(
        state,
        Packet::new(packet.id, encode(|w| ghost.write(target, w))?),
    );
    Ok(())
}

fn recipe_book<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let (source, target) = (shared.source(direction), shared.target(direction));
    let book = parse(source, &packet.data, RecipeBook::read)?;
    if recipes::displays(target) {
        return table::recipe_book(shared, cx, state, book);
    }
    let mut book = book;
    for flag in book.flags.iter_mut().skip(recipes::book_flags(target)) {
        *flag = false;
    }
    cx.to_client(
        state,
        Packet::new(packet.id, encode(|w| book.write(target, w))?),
    );
    Ok(())
}

fn recipe_book_settings<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let target = shared.target(direction);
    let flags = parse(shared.source(direction), &packet.data, |_, r| {
        recipes::read_book_settings(r)
    })?;
    if recipes::displays(target) {
        cx.to_client(state, packet);
        return Ok(());
    }
    // An older client sets its book flags with the recipe packet.
    let book = RecipeBook {
        action: 1,
        flags,
        recipes: recipes::RecipeIds::Names(Vec::new()),
        highlight: None,
    };
    let id = target_id(shared, direction, state, "recipe")?;
    cx.to_client(state, Packet::new(id, encode(|w| book.write(target, w))?));
    Ok(())
}

fn recipe_book_remove<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let ids = parse(shared.source(direction), &packet.data, |_, r| {
        recipes::read_book_remove(r)
    })?;
    if recipes::displays(shared.target(direction)) {
        cx.to_client(
            state,
            Packet::new(
                packet.id,
                encode(|w| {
                    recipes::write_book_remove(w, &ids);
                    Ok(())
                })?,
            ),
        );
    }
    // INTEGRATION: an older client's book holds no recipes from a 1.21.2
    // backend yet (see `update_recipes`), so there is nothing to remove.
    Ok(())
}

/// A hashed stack (1.21.5 on) for a backend that wants the whole stack:
/// the one it last put there when item and count still match.
fn unhash(
    shared: &mut Shared,
    direction: Direction,
    key: (i32, i16),
    hashed: &Option<HashedStack>,
) -> io::Result<Stack> {
    let Some(hashed) = hashed else {
        return Ok(None);
    };
    let mapped = map_item(shared, direction, hashed.item)?;
    if let Some(known) = shared.item.slots.get(&key)
        && known.id == mapped.id
    {
        let mut known = known.clone();
        known.count = hashed.count;
        return Ok(Some(known));
    }
    let target = shared.target(direction);
    Ok(Some(ItemStack {
        id: mapped.id,
        count: hashed.count,
        damage: 0,
        tag: None,
        components: version::item_components(target).then(Components::default),
    }))
}

/// A whole stack for a backend that wants hashes (1.21.5 on): converted,
/// then hashed the way the receiver's client hashes; components the
/// component codec cannot hash get a wrong hash and the backend corrects
/// the client.
fn hash(
    shared: &mut Shared,
    direction: Direction,
    stack: &Stack,
) -> io::Result<Option<HashedStack>> {
    let mut stack = stack.clone();
    convert::stack(shared, direction, &mut stack)?;
    let target = shared.target(direction);
    let Some(hashed) = components::hashed::from_stack(target, stack.as_ref())? else {
        return Ok(None);
    };
    let layout = components::layouts::Layout::of(target)?;
    let id = |kind: &str| {
        layout
            .id(kind)
            .ok_or_else(|| invalid(&format!("protocol {target} has no {kind} component")))
    };
    Ok(Some(HashedStack {
        item: hashed.item,
        count: hashed.count,
        components: hashed
            .added
            .iter()
            .map(|(kind, hash)| Ok((id(kind)?, *hash)))
            .collect::<io::Result<_>>()?,
        removed: hashed
            .removed
            .iter()
            .map(|kind| id(kind))
            .collect::<io::Result<_>>()?,
    }))
}

fn click_slot(
    shared: &mut Shared,
    direction: Direction,
    key: (i32, i16),
    slot: ClickSlot,
) -> io::Result<ClickSlot> {
    let (source, target) = (shared.source(direction), shared.target(direction));
    Ok(match (slot, window::hashed_clicks(target)) {
        (ClickSlot::Full(mut stack), false) => {
            convert::stack(shared, direction, &mut stack)?;
            ClickSlot::Full(stack)
        }
        (ClickSlot::Full(stack), true) => ClickSlot::Hashed(hash(shared, direction, &stack)?),
        (ClickSlot::Hashed(hashed), false) => {
            ClickSlot::Full(unhash(shared, direction, key, &hashed)?)
        }
        (ClickSlot::Hashed(Some(mut hashed)), true) => {
            hashed.item = map_item(shared, direction, hashed.item)?.id;
            if source != target {
                let types = shared.mapping(direction, "data_component_type");
                hashed
                    .components
                    .retain_mut(|(kind, _)| match types.get(*kind) {
                        Some(mapped) => {
                            *kind = mapped;
                            true
                        }
                        None => false,
                    });
                hashed.removed.retain_mut(|kind| match types.get(*kind) {
                    Some(mapped) => {
                        *kind = mapped;
                        true
                    }
                    None => false,
                });
            }
            ClickSlot::Hashed(Some(hashed))
        }
        (ClickSlot::Hashed(None), true) => ClickSlot::Hashed(None),
    })
}

/// Click modes: 0 pick up, 1 quick move (shift), 5 quick craft (drag).
const QUICK_MOVE: i32 = 1;
const QUICK_CRAFT: i32 = 5;

fn container_click<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Serverbound;
    let (source, target) = (shared.source(direction), shared.target(direction));
    let mut click = parse(source, &packet.data, Click::read)?;
    if window::has_state_ids(target) && !window::has_state_ids(source) {
        click.state_id = shared.item.state_id;
    }
    match (
        window::has_changed_slots(source),
        window::has_changed_slots(target),
    ) {
        (true, true) | (false, false) => {}
        (true, false) => {
            // A pre-1.17 backend wants an action number and the clicked
            // stack; the carried stack stands in for it except where the
            // backend expects nothing.
            shared.item.action = shared.item.action.wrapping_add(1).max(1);
            click.action = shared.item.action;
            let carried = std::mem::replace(&mut click.carried, ClickSlot::Full(None));
            click.clicked = match carried {
                _ if matches!(click.mode, QUICK_MOVE | QUICK_CRAFT) => None,
                ClickSlot::Full(stack) => stack,
                ClickSlot::Hashed(_) => None,
            };
            click.changed.clear();
        }
        (false, true) => {
            // A 1.17 backend wants the slots the click changed: say the
            // clicked one emptied, and let the backend correct the rest.
            let clicked = std::mem::take(&mut click.clicked);
            if click.slot >= 0 && clicked.is_some() {
                click.changed = vec![(click.slot, ClickSlot::Full(None))];
            }
            let picked_up = click.mode == 0 && click.button == 0;
            click.carried = ClickSlot::Full(if picked_up { clicked } else { None });
        }
    }
    convert::stack(shared, direction, &mut click.clicked)?;
    let container = click.container;
    let changed = std::mem::take(&mut click.changed);
    for (slot, stack) in changed {
        let converted = click_slot(shared, direction, (container, slot), stack)?;
        click.changed.push((slot, converted));
    }
    let carried = std::mem::replace(&mut click.carried, ClickSlot::Full(None));
    click.carried = click_slot(shared, direction, CURSOR, carried)?;
    cx.to_backend(
        state,
        Packet::new(packet.id, encode(|w| click.write(target, w))?),
    );
    Ok(())
}

fn creative_slot<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Serverbound;
    let (source, target) = (shared.source(direction), shared.target(direction));
    let mut slot = parse(source, &packet.data, CreativeSlot::read)?;
    convert::stack(shared, direction, &mut slot.item)?;
    cx.to_backend(
        state,
        Packet::new(packet.id, encode(|w| slot.write(target, w))?),
    );
    Ok(())
}

/// The off hand's inventory slot, which books name from 1.16.4 on.
const OFF_HAND_SLOT: i32 = 40;

fn edit_book<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Serverbound;
    let (source, target) = (shared.source(direction), shared.target(direction));
    let edit = parse(source, &packet.data, EditBook::read)?;
    // The book's inventory slot from the sender's hand or slot, and back.
    let held = shared.item.held_slot;
    let slot_of = |hand: i32| match (window::book_slot(source), hand) {
        (true, slot) => slot,
        (false, 1) => OFF_HAND_SLOT,
        (false, _) => held,
    };
    let hand_for = |slot: i32| {
        if window::book_slot(target) {
            slot
        } else {
            i32::from(slot == OFF_HAND_SLOT)
        }
    };
    let edit = match (edit, window::book_pages(target)) {
        (
            EditBook::Stack {
                mut book,
                signing,
                hand,
            },
            false,
        ) => {
            convert::stack(shared, direction, &mut book)?;
            EditBook::Stack {
                book,
                signing,
                hand: hand_for(slot_of(hand)),
            }
        }
        (EditBook::Pages { slot, pages, title }, true) => EditBook::Pages { slot, pages, title },
        (
            EditBook::Stack {
                book,
                signing,
                hand,
            },
            true,
        ) => {
            let tag = book
                .as_ref()
                .and_then(|book| book.tag.as_ref())
                .and_then(Nbt::as_compound);
            let pages = tag
                .and_then(|tag| tag.list("pages"))
                .unwrap_or_default()
                .iter()
                .filter_map(Nbt::as_str)
                .map(str::to_owned)
                .collect();
            let title = signing.then(|| {
                tag.and_then(|tag| tag.str("title"))
                    .unwrap_or_default()
                    .to_owned()
            });
            EditBook::Pages {
                slot: slot_of(hand),
                pages,
                title,
            }
        }
        (EditBook::Pages { slot, pages, title }, false) => {
            let mut tag = Compound::new();
            tag.insert(
                "pages",
                pages.into_iter().map(Nbt::String).collect::<Vec<_>>(),
            );
            if let Some(title) = &title {
                tag.insert("title", title.as_str());
            }
            let book = ItemStack {
                id: item_id(target, "writable_book").ok_or_else(|| invalid("no writable book"))?,
                count: 1,
                damage: 0,
                tag: Some(Nbt::Compound(tag)),
                components: None,
            };
            EditBook::Stack {
                book: Some(book),
                signing: title.is_some(),
                hand: hand_for(slot),
            }
        }
    };
    cx.to_backend(
        state,
        Packet::new(packet.id, encode(|w| edit.write(target, w))?),
    );
    Ok(())
}

fn held_slot_serverbound<S: HasShared>(step: &mut S, _: State, packet: &Packet) -> io::Result<()> {
    let slot = Reader::new(&packet.data).i16()?;
    step.shared().item.held_slot = i32::from(slot);
    Ok(())
}

fn held_slot_clientbound<S: HasShared>(step: &mut S, _: State, packet: &Packet) -> io::Result<()> {
    let shared = step.shared();
    let backend = shared.backend;
    let mut reader = Reader::new(&packet.data);
    // A byte until 1.21.2, a VarInt after.
    let slot = if backend >= 768 {
        reader.varint()?
    } else {
        i32::from(reader.i8()?)
    };
    shared.item.held_slot = slot;
    Ok(())
}
