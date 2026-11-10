//! The recipe book across 1.21.2's redesign.
//!
//! A 1.21 server sends every recipe in full (`update_recipes`) and then the
//! identifiers the player has unlocked (`recipe`); the client places
//! recipes and marks them seen by identifier. A 1.21.2 server sends only
//! the item sets furnaces and smithing tables accept and the stonecutter's
//! recipes (`update_recipes`), then displays of the recipes the player knows
//! under per-connection display IDs (`recipe_book_add`, `recipe_book_remove`,
//! `recipe_book_settings`), and the client places recipes by display ID.
//!
//! Going up, [`UpBook`] numbers the 1.21 recipes in the order the server
//! sent them and keeps each one's display. Going down, [`DownBook`] keeps
//! the displays it was given, rebuilds a 1.21 recipe from each under the
//! identifier `rift:recipe/<display ID>` (`rift:stonecutter/<n>` for the
//! stonecutter's), and resends the client's full recipe list whenever the
//! book grows, since 1.21 can only unlock recipes it knows.
use super::{NEWER, OLDER, registry_id, stacks};
use crate::{
    Direction,
    codec::Shared,
    data,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::{
    collections::{BTreeMap, HashMap},
    io,
    sync::OnceLock,
};

/// The four recipe book types' "open" and "filtering" flags, in the same
/// order in both versions.
pub(super) type Settings = [bool; 8];

const STATE_INIT: i32 = 0;
const STATE_ADD: i32 = 1;
const STATE_REMOVE: i32 = 2;

const FLAG_NOTIFICATION: u8 = 1;
const FLAG_HIGHLIGHT: u8 = 2;

/// Registry IDs of the newer protocol's display types.
struct Ids {
    slot_empty: i32,
    slot_any_fuel: i32,
    slot_item: i32,
    slot_item_stack: i32,
    slot_tag: i32,
    slot_smithing_trim: i32,
    slot_with_remainder: i32,
    slot_composite: i32,
    display_shapeless: i32,
    display_shaped: i32,
    display_furnace: i32,
    display_stonecutter: i32,
    display_smithing: i32,
}

fn ids() -> &'static Ids {
    static IDS: OnceLock<Ids> = OnceLock::new();
    IDS.get_or_init(|| {
        let slot = |name| registry_id(NEWER, "slot_display", name);
        let display = |name| registry_id(NEWER, "recipe_display", name);
        Ids {
            slot_empty: slot("empty"),
            slot_any_fuel: slot("any_fuel"),
            slot_item: slot("item"),
            slot_item_stack: slot("item_stack"),
            slot_tag: slot("tag"),
            slot_smithing_trim: slot("smithing_trim"),
            slot_with_remainder: slot("with_remainder"),
            slot_composite: slot("composite"),
            display_shapeless: display("crafting_shapeless"),
            display_shaped: display("crafting_shaped"),
            display_furnace: display("furnace"),
            display_stonecutter: display("stonecutter"),
            display_smithing: display("smithing"),
        }
    })
}

fn serializer(id: i32) -> io::Result<&'static str> {
    data::version(OLDER)
        .expect_registry("recipe_serializer")
        .name(id)
        .ok_or_else(|| invalid("unknown recipe serializer"))
}

fn serializer_id(name: &str) -> i32 {
    registry_id(OLDER, "recipe_serializer", name)
}

fn category_id(name: &str) -> i32 {
    registry_id(NEWER, "recipe_book_category", name)
}

/// The block that crafts a recipe of each kind, shown by 1.21.2.
fn station(name: &str) -> i32 {
    registry_id(NEWER, "item", name)
}

fn read_settings(reader: &mut Reader<'_>) -> io::Result<Settings> {
    let mut settings = [false; 8];
    for flag in &mut settings {
        *flag = reader.bool()?;
    }
    Ok(settings)
}

fn write_settings(writer: &mut Writer, settings: &Settings) {
    for flag in settings {
        writer.bool(*flag);
    }
}

/// `count` strings of at most `max` characters.
fn read_strings<'a>(reader: &mut Reader<'a>, max: usize) -> io::Result<Vec<&'a str>> {
    let count = reader.count()?;
    (0..count).map(|_| reader.string(max)).collect()
}

/// A recipe identifier with the default namespace spelled out, as 1.21
/// clients write them back.
fn qualified(name: &str) -> String {
    if name.contains(':') {
        name.to_owned()
    } else {
        format!("minecraft:{name}")
    }
}

/// What a 1.21 `recipe` packet becomes after the settings: a
/// `recipe_book_add` or a `recipe_book_remove` body.
pub(super) enum BookChange {
    Add(Vec<u8>),
    Remove(Vec<u8>),
}

/// One 1.21 recipe as a 1.21.2 client sees it.
struct UpRecipe {
    name: String,
    /// The display, group, category and crafting requirements, as a
    /// `recipe_book_add` entry has them after the display ID; `None` for
    /// recipes the book does not show.
    entry: Option<Vec<u8>>,
    /// The display alone, for `place_ghost_recipe`.
    display: Option<Vec<u8>>,
    notification: bool,
}

/// The 1.21 recipes of a 1.21.2 client's connection.
#[derive(Default)]
pub(super) struct UpBook {
    /// By display ID: the position in the last `update_recipes`.
    recipes: Vec<UpRecipe>,
    by_name: HashMap<String, i32>,
    groups: HashMap<String, i32>,
}

/// Items of the property sets 1.21.2 sends with its recipes, in 1.21.2 IDs.
#[derive(Default)]
struct PropertySets {
    furnace: Vec<i32>,
    blast_furnace: Vec<i32>,
    smoker: Vec<i32>,
    campfire: Vec<i32>,
    smithing_template: Vec<i32>,
    smithing_base: Vec<i32>,
    smithing_addition: Vec<i32>,
}

/// A stonecutter recipe as 1.21.2's `update_recipes` lists it.
struct Stonecutting {
    input: Vec<i32>,
    result: Vec<u8>,
    /// What 1.21 sorts a stonecutter's options by.
    sort_key: String,
}

impl UpBook {
    /// Convert 1.21's `update_recipes` to 1.21.2's and remember the recipes.
    pub(super) fn update_recipes(
        &mut self,
        shared: &mut Shared,
        body: &[u8],
    ) -> io::Result<Vec<u8>> {
        let mut reader = Reader::new(body);
        let count = reader.count()?;
        self.recipes.clear();
        self.by_name.clear();
        let mut sets = PropertySets::default();
        let mut stonecutting = Vec::new();
        for _ in 0..count {
            let name = qualified(reader.string(32767)?);
            let recipe = self.convert(shared, &mut reader, name, &mut sets, &mut stonecutting)?;
            let id = self.recipes.len() as i32;
            self.by_name.insert(recipe.name.clone(), id);
            self.recipes.push(recipe);
        }
        reader.finish()?;

        let mut writer = Writer::new();
        let sets = [
            ("minecraft:blast_furnace_input", sets.blast_furnace),
            ("minecraft:campfire_input", sets.campfire),
            ("minecraft:furnace_input", sets.furnace),
            ("minecraft:smithing_addition", sets.smithing_addition),
            ("minecraft:smithing_base", sets.smithing_base),
            ("minecraft:smithing_template", sets.smithing_template),
            ("minecraft:smoker_input", sets.smoker),
        ];
        writer.varint(sets.len() as i32);
        for (key, mut items) in sets {
            items.sort_unstable();
            items.dedup();
            writer.string(key).varint(items.len() as i32);
            for item in items {
                writer.varint(item);
            }
        }
        // 1.21 servers list a stonecutter's options sorted by the result's
        // translation key and 1.21.2 clients keep the order they are given,
        // so the button the player clicks means the same on both sides.
        stonecutting.sort_by(|a: &Stonecutting, b| a.sort_key.cmp(&b.sort_key));
        writer.varint(stonecutting.len() as i32);
        for recipe in stonecutting {
            write_id_set(&mut writer, &recipe.input);
            writer.bytes(&recipe.result);
        }
        Ok(writer.into_inner())
    }

    fn group(&mut self, group: &str) -> Option<i32> {
        if group.is_empty() {
            return None;
        }
        let next = self.groups.len() as i32;
        Some(*self.groups.entry(group.to_owned()).or_insert(next))
    }

    fn convert(
        &mut self,
        shared: &mut Shared,
        reader: &mut Reader<'_>,
        name: String,
        sets: &mut PropertySets,
        stonecutting: &mut Vec<Stonecutting>,
    ) -> io::Result<UpRecipe> {
        let ids = ids();
        let kind = serializer(reader.varint()?)?;
        let mut recipe = UpRecipe {
            name,
            entry: None,
            display: None,
            notification: true,
        };
        let mut display = Writer::new();
        let (group, category, requirements) = match kind {
            "crafting_shaped" => {
                let group = reader.string(32767)?;
                let category = crafting_category(reader.varint()?)?;
                let width = reader.varint()?;
                let height = reader.varint()?;
                let cells = usize::try_from(width)
                    .ok()
                    .zip(usize::try_from(height).ok())
                    .and_then(|(w, h)| w.checked_mul(h))
                    .filter(|cells| *cells <= reader.remaining())
                    .ok_or_else(|| invalid("invalid shaped recipe size"))?;
                let ingredients = (0..cells)
                    .map(|_| read_ingredient(shared, reader))
                    .collect::<io::Result<Vec<_>>>()?;
                let result = read_result(shared, reader)?;
                recipe.notification = reader.bool()?;
                display
                    .varint(ids.display_shaped)
                    .varint(width)
                    .varint(height);
                display.varint(cells as i32);
                for ingredient in &ingredients {
                    write_item_display(&mut display, &ingredient.items);
                }
                (
                    group,
                    category,
                    finish(display, result, "crafting_table", &ingredients, &mut recipe),
                )
            }
            "crafting_shapeless" => {
                let group = reader.string(32767)?;
                let category = crafting_category(reader.varint()?)?;
                let count = reader.count()?;
                let ingredients = (0..count)
                    .map(|_| read_ingredient(shared, reader))
                    .collect::<io::Result<Vec<_>>>()?;
                let result = read_result(shared, reader)?;
                display.varint(ids.display_shapeless).varint(count as i32);
                for ingredient in &ingredients {
                    write_item_display(&mut display, &ingredient.items);
                }
                (
                    group,
                    category,
                    finish(display, result, "crafting_table", &ingredients, &mut recipe),
                )
            }
            "smelting" | "blasting" | "smoking" | "campfire_cooking" => {
                let group = reader.string(32767)?;
                let cooking = reader.varint()?;
                let ingredient = read_ingredient(shared, reader)?;
                let result = read_result(shared, reader)?;
                let experience = reader.f32()?;
                let time = reader.varint()?;
                let (category, set, block) = match (kind, cooking) {
                    ("smelting", 0) => ("furnace_food", &mut sets.furnace, "furnace"),
                    ("smelting", 1) => ("furnace_blocks", &mut sets.furnace, "furnace"),
                    ("smelting", _) => ("furnace_misc", &mut sets.furnace, "furnace"),
                    ("blasting", 1) => (
                        "blast_furnace_blocks",
                        &mut sets.blast_furnace,
                        "blast_furnace",
                    ),
                    ("blasting", _) => (
                        "blast_furnace_misc",
                        &mut sets.blast_furnace,
                        "blast_furnace",
                    ),
                    ("smoking", _) => ("smoker_food", &mut sets.smoker, "smoker"),
                    _ => ("campfire", &mut sets.campfire, "campfire"),
                };
                set.extend_from_slice(&ingredient.items);
                display.varint(ids.display_furnace);
                write_item_display(&mut display, &ingredient.items);
                display.varint(ids.slot_any_fuel);
                let requirements = match result {
                    Some(result) if ingredient.placeable() => {
                        display.bytes(&result);
                        display.varint(ids.slot_item).varint(station(block));
                        display.varint(time).f32(experience);
                        recipe.display = Some(display.into_inner());
                        Some(requirements_of(std::slice::from_ref(&ingredient)))
                    }
                    _ => None,
                };
                (group, category, requirements)
            }
            "stonecutting" => {
                let group = reader.string(32767)?;
                let ingredient = read_ingredient(shared, reader)?;
                let result = read_result(shared, reader)?;
                let requirements = match result {
                    Some(result) if ingredient.placeable() => {
                        stonecutting.push(Stonecutting {
                            input: ingredient.items.clone(),
                            result: result.clone(),
                            sort_key: translation_key(&result),
                        });
                        display.varint(ids.display_stonecutter);
                        write_item_display(&mut display, &ingredient.items);
                        display.bytes(&result);
                        display.varint(ids.slot_item).varint(station("stonecutter"));
                        recipe.display = Some(display.into_inner());
                        Some(requirements_of(std::slice::from_ref(&ingredient)))
                    }
                    _ => None,
                };
                (group, "stonecutter", requirements)
            }
            "smithing_transform" | "smithing_trim" => {
                let parts = [
                    read_ingredient(shared, reader)?,
                    read_ingredient(shared, reader)?,
                    read_ingredient(shared, reader)?,
                ];
                let [template, base, addition] = &parts;
                sets.smithing_template.extend_from_slice(&template.items);
                sets.smithing_base.extend_from_slice(&base.items);
                sets.smithing_addition.extend_from_slice(&addition.items);
                display.varint(ids.display_smithing);
                for part in &parts {
                    write_item_display(&mut display, &part.items);
                }
                let result = if kind == "smithing_transform" {
                    read_result(shared, reader)?
                } else {
                    let mut trim = Writer::new();
                    trim.varint(ids.slot_smithing_trim);
                    write_item_display(&mut trim, &base.items);
                    write_item_display(&mut trim, &addition.items);
                    write_item_display(&mut trim, &template.items);
                    Some(trim.into_inner())
                };
                let requirements = match result {
                    Some(result) if parts.iter().all(Ingredient::placeable) => {
                        display.bytes(&result);
                        display
                            .varint(ids.slot_item)
                            .varint(station("smithing_table"));
                        recipe.display = Some(display.into_inner());
                        Some(requirements_of(&parts))
                    }
                    _ => None,
                };
                ("", "smithing", requirements)
            }
            // Special crafting recipes (dyeing armour, cloning maps...) have
            // no display in either version's book.
            _ if kind.starts_with("crafting_") => {
                reader.varint()?;
                return Ok(recipe);
            }
            _ => return Err(invalid("unknown recipe serializer")),
        };
        if let (Some(display), Some(requirements)) = (&recipe.display, requirements) {
            let mut entry = Writer::new();
            entry.bytes(display);
            entry.varint(self.group(group).map_or(0, |group| group + 1));
            entry.varint(category_id(category));
            entry.bool(true).varint(requirements.len() as i32);
            for set in &requirements {
                write_id_set(&mut entry, set);
            }
            recipe.entry = Some(entry.into_inner());
        } else {
            recipe.display = None;
        }
        Ok(recipe)
    }

    /// Convert 1.21's `recipe` packet: settings first, then the displays it
    /// unlocks or the display IDs it locks again.
    pub(super) fn recipe(&self, body: &[u8]) -> io::Result<(Vec<u8>, BookChange)> {
        let mut reader = Reader::new(body);
        let state = reader.varint()?;
        let settings = read_settings(&mut reader)?;
        let names = read_strings(&mut reader, 32767)?;
        let highlight = if state == STATE_INIT {
            read_strings(&mut reader, 32767)?
        } else {
            Vec::new()
        };
        reader.finish()?;
        let mut settings_body = Writer::with_capacity(8);
        write_settings(&mut settings_body, &settings);
        let known = names
            .iter()
            .filter_map(|name| self.by_name.get(&qualified(name)).copied());
        let mut writer = Writer::new();
        let list = match state {
            STATE_REMOVE => {
                let ids: Vec<i32> = known.collect();
                writer.varint(ids.len() as i32);
                for id in ids {
                    writer.varint(id);
                }
                BookChange::Remove(writer.into_inner())
            }
            STATE_INIT | STATE_ADD => {
                let highlight: Vec<String> = highlight.iter().map(|name| qualified(name)).collect();
                let entries: Vec<(i32, &UpRecipe)> = known
                    .filter_map(|id| Some((id, self.recipes.get(id as usize)?)))
                    .filter(|(_, recipe)| recipe.entry.is_some())
                    .collect();
                writer.varint(entries.len() as i32);
                for (id, recipe) in entries {
                    writer
                        .varint(id)
                        .bytes(recipe.entry.as_deref().unwrap_or_default());
                    let flags = if state == STATE_ADD {
                        FLAG_HIGHLIGHT
                            | if recipe.notification {
                                FLAG_NOTIFICATION
                            } else {
                                0
                            }
                    } else if highlight.contains(&recipe.name) {
                        FLAG_HIGHLIGHT
                    } else {
                        0
                    };
                    writer.u8(flags);
                }
                writer.bool(state == STATE_INIT);
                BookChange::Add(writer.into_inner())
            }
            _ => return Err(invalid("unknown recipe book state")),
        };
        Ok((settings_body.into_inner(), list))
    }

    /// The display of the recipe `name`, for `place_ghost_recipe`.
    pub(super) fn display_of(&self, name: &str) -> Option<&[u8]> {
        let id = *self.by_name.get(&qualified(name))?;
        self.recipes.get(id as usize)?.display.as_deref()
    }

    /// The identifier behind a display ID the client sent.
    pub(super) fn name_of(&self, id: i32) -> Option<&str> {
        let recipe = self.recipes.get(usize::try_from(id).ok()?)?;
        Some(&recipe.name)
    }
}

/// The 1.21.2 recipe book category of a 1.21 crafting book category.
fn crafting_category(category: i32) -> io::Result<&'static str> {
    Ok(match category {
        0 => "crafting_building_blocks",
        1 => "crafting_redstone",
        2 => "crafting_equipment",
        3 => "crafting_misc",
        _ => return Err(invalid("unknown crafting book category")),
    })
}

/// A 1.21 ingredient's items, in 1.21.2 IDs.
struct Ingredient {
    items: Vec<i32>,
    /// Whether it had items before mapping: an empty slot of a shaped
    /// recipe, not one whose items 1.21.2 lacks.
    any: bool,
}

impl Ingredient {
    /// 1.21.2 cannot place a recipe with an ingredient it has no items for.
    fn placeable(&self) -> bool {
        !self.any || !self.items.is_empty()
    }
}

fn read_ingredient(shared: &mut Shared, reader: &mut Reader<'_>) -> io::Result<Ingredient> {
    let count = reader.count()?;
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        let item = stacks::read_item(shared, Direction::Clientbound, reader)?;
        if let Some(item) = stacks::map_item(shared, Direction::Clientbound, item)
            && !items.contains(&item)
        {
            items.push(item);
        }
    }
    Ok(Ingredient {
        items,
        any: count > 0,
    })
}

/// A recipe's result as a 1.21.2 item stack display; `None` when 1.21.2 has
/// no such item.
fn read_result(shared: &mut Shared, reader: &mut Reader<'_>) -> io::Result<Option<Vec<u8>>> {
    let mut rw = Rewriter::new(reader.peek());
    rw.w.varint(ids().slot_item_stack);
    let converted = stacks::transcode_required(shared, Direction::Clientbound, &mut rw)?;
    let consumed = reader.remaining() - rw.remaining();
    reader.take(consumed)?;
    Ok(converted.then_some(rw.w.buf))
}

/// Append a crafting recipe's result and station; returns its requirements
/// when the book can show it.
fn finish(
    mut display: Writer,
    result: Option<Vec<u8>>,
    block: &str,
    ingredients: &[Ingredient],
    recipe: &mut UpRecipe,
) -> Option<Vec<Vec<i32>>> {
    let result = result.filter(|_| ingredients.iter().all(Ingredient::placeable))?;
    display.bytes(&result);
    display.varint(ids().slot_item).varint(station(block));
    recipe.display = Some(display.into_inner());
    Some(requirements_of(ingredients))
}

fn requirements_of(ingredients: &[Ingredient]) -> Vec<Vec<i32>> {
    ingredients
        .iter()
        .filter(|ingredient| !ingredient.items.is_empty())
        .map(|ingredient| ingredient.items.clone())
        .collect()
}

/// A slot display showing any of `items`.
fn write_item_display(writer: &mut Writer, items: &[i32]) {
    let ids = ids();
    match items {
        [] => drop(writer.varint(ids.slot_empty)),
        [item] => drop(writer.varint(ids.slot_item).varint(*item)),
        items => {
            writer.varint(ids.slot_composite).varint(items.len() as i32);
            for item in items {
                writer.varint(ids.slot_item).varint(*item);
            }
        }
    }
}

/// A holder set of items given by ID.
fn write_id_set(writer: &mut Writer, items: &[i32]) {
    writer.varint(items.len() as i32 + 1);
    for item in items {
        writer.varint(*item);
    }
}

/// The translation key 1.21 sorts stonecutter results by, from an item stack
/// display: a block item's is the block's.
fn translation_key(display: &[u8]) -> String {
    let mut reader = Reader::new(display);
    let item = reader
        .varint()
        .and_then(|_| reader.varint())
        .and_then(|_| reader.varint())
        .unwrap_or(-1);
    let version = data::version(NEWER);
    let name = version
        .expect_registry("item")
        .name(item)
        .unwrap_or_default();
    let kind = if version.expect_registry("block").id(name).is_some() {
        "block"
    } else {
        "item"
    };
    format!("{kind}.minecraft.{name}")
}

/// One display the backend's book holds, as the 1.21 client is told it.
struct DownRecipe {
    /// The 1.21 recipe after its identifier: serializer and contents.
    recipe: Vec<u8>,
    /// The display as the backend sent it, to recognise ghost recipes.
    display: Vec<u8>,
    highlight: bool,
}

/// The 1.21.2 recipe book of a 1.21 client's connection.
#[derive(Default)]
pub(super) struct DownBook {
    known: BTreeMap<i32, DownRecipe>,
    /// The stonecutter's recipes as 1.21 recipe contents.
    stonecutter: Vec<Vec<u8>>,
    settings: Settings,
    /// Item tags by name, in 1.21.2 IDs, for tag slot displays.
    tags: HashMap<String, Vec<i32>>,
}

/// Packets the down book sends the client for one backend packet.
#[derive(Default, Debug)]
pub(super) struct DownOut {
    /// A full 1.21 `update_recipes` body.
    pub recipes: Option<Vec<u8>>,
    /// A 1.21 `recipe` body.
    pub book: Option<Vec<u8>>,
}

const RECIPE_PREFIX: &str = "rift:recipe/";
const STONECUTTER_PREFIX: &str = "rift:stonecutter/";

impl DownBook {
    /// Remember the item tags of an `update_tags` body (1.21.2 layout).
    pub(super) fn update_tags(&mut self, body: &[u8]) -> io::Result<()> {
        let mut reader = Reader::new(body);
        for _ in 0..reader.count()? {
            let registry = data::strip(reader.string(32767)?) == "item";
            if registry {
                self.tags.clear();
            }
            for _ in 0..reader.count()? {
                let name = data::strip(reader.string(32767)?).to_owned();
                let count = reader.count()?;
                let items = (0..count)
                    .map(|_| reader.varint())
                    .collect::<io::Result<Vec<_>>>()?;
                if registry {
                    self.tags.insert(name, items);
                }
            }
        }
        reader.finish()
    }

    /// 1.21.2's `update_recipes`: keep the stonecutter's recipes and resend
    /// the client's recipe list with them.
    pub(super) fn update_recipes(
        &mut self,
        shared: &mut Shared,
        body: &[u8],
    ) -> io::Result<DownOut> {
        let mut reader = Reader::new(body);
        for _ in 0..reader.count()? {
            reader.string(32767)?;
            for _ in 0..reader.count()? {
                reader.varint()?;
            }
        }
        let count = reader.count()?;
        let mut stonecutter = Vec::with_capacity(count);
        for _ in 0..count {
            let input = self.read_id_set(shared, &mut reader)?;
            let result = self.result(shared, &mut reader)?;
            if let Some(result) = result {
                let mut recipe = Writer::new();
                recipe.varint(serializer_id("stonecutting")).string("");
                write_ingredient(&mut recipe, &input);
                recipe.bytes(&result);
                stonecutter.push(recipe.into_inner());
            }
        }
        reader.finish()?;
        self.stonecutter = stonecutter;
        Ok(DownOut {
            recipes: Some(self.all_recipes()),
            book: None,
        })
    }

    pub(super) fn settings(&mut self, body: &[u8]) -> io::Result<()> {
        let mut reader = Reader::new(body);
        self.settings = read_settings(&mut reader)?;
        reader.finish()
    }

    /// 1.21.2's `recipe_book_add`: rebuild the recipes, resend the list and
    /// unlock them.
    pub(super) fn add(&mut self, shared: &mut Shared, body: &[u8]) -> io::Result<DownOut> {
        let mut reader = Reader::new(body);
        let count = reader.count()?;
        let mut added = Vec::with_capacity(count);
        for _ in 0..count {
            let id = reader.varint()?;
            let start = reader.peek();
            let recipe = self.convert(shared, &mut reader)?;
            let flags = reader.u8()?;
            if let Some(recipe) = recipe {
                added.push((id, recipe, flags, start));
            }
        }
        let replace = reader.bool()?;
        reader.finish()?;
        if replace {
            self.known.clear();
        }
        let mut names = Vec::with_capacity(added.len());
        for (id, (recipe, display_len), flags, start) in added {
            names.push(id);
            self.known.insert(
                id,
                DownRecipe {
                    recipe,
                    display: start[..display_len].to_vec(),
                    highlight: flags & FLAG_HIGHLIGHT != 0,
                },
            );
        }
        let mut book = Writer::new();
        book.varint(if replace { STATE_INIT } else { STATE_ADD });
        write_settings(&mut book, &self.settings);
        if replace {
            book.varint(self.known.len() as i32);
            for id in self.known.keys() {
                book.string(&format!("{RECIPE_PREFIX}{id}"));
            }
            let highlighted: Vec<_> = self
                .known
                .iter()
                .filter(|(_, recipe)| recipe.highlight)
                .collect();
            book.varint(highlighted.len() as i32);
            for (id, _) in highlighted {
                book.string(&format!("{RECIPE_PREFIX}{id}"));
            }
        } else {
            book.varint(names.len() as i32);
            for id in names {
                book.string(&format!("{RECIPE_PREFIX}{id}"));
            }
        }
        Ok(DownOut {
            recipes: Some(self.all_recipes()),
            book: Some(book.into_inner()),
        })
    }

    /// 1.21.2's `recipe_book_remove` as a 1.21 `recipe` body.
    pub(super) fn remove(&mut self, body: &[u8]) -> io::Result<Vec<u8>> {
        let mut reader = Reader::new(body);
        let count = reader.count()?;
        let mut removed = Vec::with_capacity(count);
        for _ in 0..count {
            let id = reader.varint()?;
            if self.known.remove(&id).is_some() {
                removed.push(id);
            }
        }
        reader.finish()?;
        let mut book = Writer::new();
        book.varint(STATE_REMOVE);
        write_settings(&mut book, &self.settings);
        book.varint(removed.len() as i32);
        for id in removed {
            book.string(&format!("{RECIPE_PREFIX}{id}"));
        }
        Ok(book.into_inner())
    }

    /// The identifier the client knows a ghost recipe's display by.
    pub(super) fn ghost(&self, display: &[u8]) -> Option<String> {
        self.known
            .iter()
            .find(|(_, recipe)| recipe.display == display)
            .map(|(id, _)| format!("{RECIPE_PREFIX}{id}"))
    }

    /// The display ID behind an identifier the client sent.
    pub(super) fn display_id(name: &str) -> Option<i32> {
        let id = qualified(name);
        let id = id.strip_prefix(RECIPE_PREFIX)?;
        if id.starts_with('+') || id.starts_with('-') {
            return None;
        }
        id.parse().ok()
    }

    /// A 1.21 `update_recipes` body with everything the client should know.
    fn all_recipes(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint((self.known.len() + self.stonecutter.len()) as i32);
        for (id, recipe) in &self.known {
            writer
                .string(&format!("{RECIPE_PREFIX}{id}"))
                .bytes(&recipe.recipe);
        }
        for (index, recipe) in self.stonecutter.iter().enumerate() {
            writer
                .string(&format!("{STONECUTTER_PREFIX}{index}"))
                .bytes(recipe);
        }
        writer.into_inner()
    }

    /// Rebuild one 1.21 recipe from a display entry (display, group,
    /// category, requirements); returns it with the display's length.
    /// `None` when 1.21 cannot express the result.
    fn convert(
        &self,
        shared: &mut Shared,
        reader: &mut Reader<'_>,
    ) -> io::Result<Option<(Vec<u8>, usize)>> {
        let ids = ids();
        let before = reader.remaining();
        let kind = reader.varint()?;
        let mut recipe = Writer::new();
        let mut parts = Vec::new();
        let result: Option<Vec<u8>>;
        let mut extra = None;
        if kind == ids.display_shaped {
            let width = reader.varint()?;
            let height = reader.varint()?;
            let count = reader.count()?;
            if i64::from(width) * i64::from(height) != count as i64 {
                return Err(invalid("shaped recipe display of the wrong size"));
            }
            for _ in 0..count {
                parts.push(self.ingredient(shared, reader)?);
            }
            result = self.result(shared, reader)?;
            self.skip_display(shared, reader)?;
            extra = Some((width, height));
        } else if kind == ids.display_shapeless {
            for _ in 0..reader.count()? {
                parts.push(self.ingredient(shared, reader)?);
            }
            result = self.result(shared, reader)?;
            self.skip_display(shared, reader)?;
        } else if kind == ids.display_furnace {
            parts.push(self.ingredient(shared, reader)?);
            self.skip_display(shared, reader)?; // Fuel.
            result = self.result(shared, reader)?;
            self.skip_display(shared, reader)?;
            let time = reader.varint()?;
            let experience = reader.f32()?;
            extra = Some((time, experience.to_bits() as i32));
        } else if kind == ids.display_stonecutter {
            parts.push(self.ingredient(shared, reader)?);
            result = self.result(shared, reader)?;
            self.skip_display(shared, reader)?;
        } else if kind == ids.display_smithing {
            for _ in 0..3 {
                parts.push(self.ingredient(shared, reader)?);
            }
            let trim = Reader::new(reader.peek()).varint()? == ids.slot_smithing_trim;
            if trim {
                self.skip_display(shared, reader)?;
                result = Some(Vec::new());
            } else {
                result = self.result(shared, reader)?;
            }
            self.skip_display(shared, reader)?;
            extra = Some((i32::from(trim), 0));
        } else {
            return Err(invalid("unknown recipe display"));
        }
        let display_len = before - reader.remaining();
        let group = match reader.varint()? {
            0 => String::new(),
            group => format!("rift:group/{}", group - 1),
        };
        let category = reader.varint()?;
        let category = data::version(NEWER)
            .expect_registry("recipe_book_category")
            .name(category)
            .ok_or_else(|| invalid("unknown recipe book category"))?;
        if reader.bool()? {
            for _ in 0..reader.count()? {
                self.read_id_set(shared, reader)?;
            }
        }
        let Some(result) = result else {
            return Ok(None);
        };
        if kind == ids.display_shaped || kind == ids.display_shapeless {
            let crafting = match category {
                "crafting_building_blocks" => 0,
                "crafting_redstone" => 1,
                "crafting_equipment" => 2,
                _ => 3,
            };
            if let Some((width, height)) = extra {
                recipe
                    .varint(serializer_id("crafting_shaped"))
                    .string(&group)
                    .varint(crafting);
                recipe.varint(width).varint(height);
                for part in &parts {
                    write_ingredient(&mut recipe, part);
                }
                recipe.bytes(&result).bool(true);
            } else {
                recipe
                    .varint(serializer_id("crafting_shapeless"))
                    .string(&group)
                    .varint(crafting);
                recipe.varint(parts.len() as i32);
                for part in &parts {
                    write_ingredient(&mut recipe, part);
                }
                recipe.bytes(&result);
            }
        } else if kind == ids.display_furnace {
            let (serializer, cooking) = match category {
                "furnace_food" => ("smelting", 0),
                "furnace_blocks" => ("smelting", 1),
                "blast_furnace_blocks" => ("blasting", 1),
                "blast_furnace_misc" => ("blasting", 2),
                "smoker_food" => ("smoking", 0),
                "campfire" => ("campfire_cooking", 0),
                _ => ("smelting", 2),
            };
            let (time, experience) = extra.unwrap_or_default();
            recipe
                .varint(serializer_id(serializer))
                .string(&group)
                .varint(cooking);
            write_ingredient(&mut recipe, &parts[0]);
            recipe
                .bytes(&result)
                .f32(f32::from_bits(experience as u32))
                .varint(time);
        } else if kind == ids.display_stonecutter {
            recipe.varint(serializer_id("stonecutting")).string(&group);
            write_ingredient(&mut recipe, &parts[0]);
            recipe.bytes(&result);
        } else if extra.is_some_and(|(trim, _)| trim == 1) {
            recipe.varint(serializer_id("smithing_trim"));
            for part in &parts {
                write_ingredient(&mut recipe, part);
            }
        } else {
            recipe.varint(serializer_id("smithing_transform"));
            for part in &parts {
                write_ingredient(&mut recipe, part);
            }
            recipe.bytes(&result);
        }
        Ok(Some((recipe.into_inner(), display_len)))
    }

    /// The items a slot display may show, in 1.21 IDs.
    fn ingredient(&self, shared: &mut Shared, reader: &mut Reader<'_>) -> io::Result<Vec<i32>> {
        let mut items = Vec::new();
        self.collect(shared, reader, &mut items, 0)?;
        let mut mapped = Vec::with_capacity(items.len());
        for item in items {
            if let Some(item) = stacks::map_item(shared, Direction::Clientbound, item)
                && !mapped.contains(&item)
            {
                mapped.push(item);
            }
        }
        Ok(mapped)
    }

    fn collect(
        &self,
        shared: &mut Shared,
        reader: &mut Reader<'_>,
        items: &mut Vec<i32>,
        depth: usize,
    ) -> io::Result<()> {
        if depth > crate::wire::MAX_DEPTH {
            return Err(invalid("slot display nesting limit exceeded"));
        }
        let ids = ids();
        let kind = reader.varint()?;
        if kind == ids.slot_empty || kind == ids.slot_any_fuel {
        } else if kind == ids.slot_item {
            items.push(reader.varint()?);
        } else if kind == ids.slot_item_stack {
            items.push(stacks::read_item(shared, Direction::Clientbound, reader)?);
        } else if kind == ids.slot_tag {
            let tag = data::strip(reader.string(32767)?);
            items.extend(self.tags.get(tag).into_iter().flatten());
        } else if kind == ids.slot_smithing_trim {
            for _ in 0..3 {
                self.collect(shared, reader, &mut Vec::new(), depth + 1)?;
            }
        } else if kind == ids.slot_with_remainder {
            self.collect(shared, reader, items, depth + 1)?;
            self.collect(shared, reader, &mut Vec::new(), depth + 1)?;
        } else if kind == ids.slot_composite {
            for _ in 0..reader.count()? {
                self.collect(shared, reader, items, depth + 1)?;
            }
        } else {
            return Err(invalid("unknown slot display"));
        }
        Ok(())
    }

    /// Skip a slot display: the item stacks inside still need reading to
    /// find its end.
    fn skip_display(&self, shared: &mut Shared, reader: &mut Reader<'_>) -> io::Result<()> {
        self.collect(shared, reader, &mut Vec::new(), 0)
    }

    /// A recipe result from a slot display, as a 1.21 item stack.
    fn result(&self, shared: &mut Shared, reader: &mut Reader<'_>) -> io::Result<Option<Vec<u8>>> {
        let ids = ids();
        let mut probe = *reader;
        let kind = probe.varint()?;
        if kind == ids.slot_item {
            let item = probe.varint()?;
            *reader = probe;
            let mut writer = Writer::new();
            return Ok(
                stacks::map_item(shared, Direction::Clientbound, item).map(|item| {
                    stacks::write_plain(&mut writer, 1, item);
                    writer.into_inner()
                }),
            );
        }
        if kind == ids.slot_item_stack {
            let mut rw = Rewriter::new(probe.peek());
            let converted = stacks::transcode_required(shared, Direction::Clientbound, &mut rw)?;
            let consumed = probe.remaining() - rw.remaining();
            probe.take(consumed)?;
            *reader = probe;
            return Ok(converted.then_some(rw.w.buf));
        }
        // Composite and other results: the first item they show.
        let mut items = Vec::new();
        self.collect(shared, reader, &mut items, 0)?;
        let Some(item) = items
            .into_iter()
            .find_map(|item| stacks::map_item(shared, Direction::Clientbound, item))
        else {
            return Ok(None);
        };
        let mut writer = Writer::new();
        stacks::write_plain(&mut writer, 1, item);
        Ok(Some(writer.into_inner()))
    }

    /// An ingredient holder set, resolved to 1.21 IDs.
    fn read_id_set(&self, shared: &mut Shared, reader: &mut Reader<'_>) -> io::Result<Vec<i32>> {
        let count = reader.varint()?;
        let items = if count == 0 {
            let tag = data::strip(reader.string(32767)?);
            self.tags.get(tag).cloned().unwrap_or_default()
        } else {
            let count = usize::try_from(count - 1)
                .ok()
                .filter(|count| *count <= reader.remaining())
                .ok_or_else(|| invalid("invalid holder set length"))?;
            (0..count)
                .map(|_| reader.varint())
                .collect::<io::Result<Vec<_>>>()?
        };
        let mut mapped = Vec::with_capacity(items.len());
        for item in items {
            if let Some(item) = stacks::map_item(shared, Direction::Clientbound, item)
                && !mapped.contains(&item)
            {
                mapped.push(item);
            }
        }
        Ok(mapped)
    }
}

/// A 1.21 ingredient: plain stacks of each item.
fn write_ingredient(writer: &mut Writer, items: &[i32]) {
    writer.varint(items.len() as i32);
    for item in items {
        stacks::write_plain(writer, 1, *item);
    }
}
