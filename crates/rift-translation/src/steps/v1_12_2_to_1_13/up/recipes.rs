//! Recipes: 1.12 numbers them in registration order and sends no recipe
//! list (its clients load the same recipes from their own jar); 1.13 names
//! them and gets the list from the server.
//!
//! `recipes.json` (see `generate.py`) has 1.12.2's recipes in its order; the
//! client gets the crafting recipes, flattened, named
//! `minecraft:<1.12 name>`, and every packet naming a recipe is translated
//! by that name. The built-in special recipes (armour dyeing, map cloning,
//! ...) never appear in the recipe book and are left out. 1.12 has no
//! furnace recipe book: its state is always closed and toggling it is not
//! passed on.
use super::Up;
use crate::{
    Packet, State, data,
    legacy::{self, legacy},
    step::{Cx, RouteBuilder},
    wire::{Reader, Writer, invalid},
};
use serde::Deserialize;
use std::{collections::HashMap, io, sync::OnceLock};

const NAMESPACE: &str = "minecraft:";

#[derive(Deserialize)]
struct Raw {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    group: String,
    #[serde(default)]
    width: i32,
    #[serde(default)]
    height: i32,
    #[serde(default)]
    ingredients: Vec<Vec<String>>,
    /// Item and count.
    #[serde(default)]
    result: Option<(String, i8)>,
}

struct Recipes {
    /// 1.12 names by 1.12 ID, and whether the client knows the recipe.
    names: Vec<(String, bool)>,
    ids: HashMap<String, i32>,
    /// The body of `update_recipes`.
    declared: Vec<u8>,
}

/// The 1.13 items a 1.12 item spec stands for: `name:damage`, or `name:*`
/// for every variant.
fn items(spec: &str) -> io::Result<Vec<i32>> {
    let (name, damage) = spec
        .rsplit_once(':')
        .ok_or_else(|| invalid("recipe item without damage"))?;
    let registry = data::version(legacy::LEGACY).expect_registry("item");
    let id = registry
        .id(name)
        .ok_or_else(|| invalid("recipe names an unknown 1.12 item"))?;
    let damages: Vec<i16> = match damage {
        "*" => (0..16).collect(),
        damage => vec![
            damage
                .parse()
                .map_err(|_| invalid("recipe item damage is not a number"))?,
        ],
    };
    let mut found = Vec::new();
    for damage in damages {
        if let Some(item) = legacy().flatten_item(id, damage)
            && !found.contains(&item.item)
        {
            found.push(item.item);
        }
    }
    Ok(found)
}

/// A 1.13 stack without NBT: item ID, count, TAG_End.
fn stack(writer: &mut Writer, item: i32, count: i8) {
    writer.i16(item as i16).i8(count).u8(0);
}

/// Write one recipe's 1.13 body, or `None` when an ingredient or the result
/// has no 1.13 item.
fn recipe(raw: &Raw) -> io::Result<Option<Vec<u8>>> {
    let Some((result, count)) = &raw.result else {
        return Ok(None);
    };
    let Some(&result) = items(result)?.first() else {
        return Ok(None);
    };
    let mut ingredients = Vec::with_capacity(raw.ingredients.len());
    for alternatives in &raw.ingredients {
        let mut ids = Vec::new();
        for spec in alternatives {
            for item in items(spec)? {
                if !ids.contains(&item) {
                    ids.push(item);
                }
            }
        }
        if ids.is_empty() && !alternatives.is_empty() {
            return Ok(None);
        }
        ingredients.push(ids);
    }
    let mut writer = Writer::new();
    writer
        .string(&format!("{NAMESPACE}{}", raw.name))
        .string(&raw.kind);
    match raw.kind.as_str() {
        "crafting_shaped" => {
            if ingredients.len() != (raw.width * raw.height) as usize {
                return Err(invalid("shaped recipe size does not match"));
            }
            writer
                .varint(raw.width)
                .varint(raw.height)
                .string(&raw.group);
        }
        "crafting_shapeless" => {
            writer.string(&raw.group).varint(ingredients.len() as i32);
        }
        _ => return Err(invalid("unknown recipe type")),
    }
    for ids in ingredients {
        writer.varint(ids.len() as i32);
        for id in ids {
            stack(&mut writer, id, 1);
        }
    }
    stack(&mut writer, result, *count);
    Ok(Some(writer.buf))
}

fn load() -> io::Result<Recipes> {
    let raw: Vec<Raw> = serde_json::from_str(include_str!("recipes.json"))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut names = Vec::with_capacity(raw.len());
    let mut ids = HashMap::with_capacity(raw.len());
    let mut bodies = Vec::new();
    for (id, raw) in raw.iter().enumerate() {
        let body = if raw.kind == "special" {
            None
        } else {
            recipe(raw)?
        };
        names.push((raw.name.clone(), body.is_some()));
        ids.insert(raw.name.clone(), id as i32);
        bodies.extend(body);
    }
    let mut declared = Writer::with_capacity(bodies.iter().map(Vec::len).sum::<usize>() + 3);
    declared.varint(bodies.len() as i32);
    for body in bodies {
        declared.bytes(&body);
    }
    Ok(Recipes {
        names,
        ids,
        declared: declared.buf,
    })
}

fn recipes() -> io::Result<&'static Recipes> {
    static RECIPES: OnceLock<Result<Recipes, String>> = OnceLock::new();
    RECIPES
        .get_or_init(|| load().map_err(|error| error.to_string()))
        .as_ref()
        .map_err(|error| invalid(error))
}

/// The body of `update_recipes`: every 1.12.2 crafting recipe.
pub fn declare() -> io::Result<Vec<u8>> {
    Ok(recipes()?.declared.clone())
}

/// The 1.13 name of a 1.12 recipe ID, when the client knows that recipe.
fn name(id: i32) -> io::Result<Option<String>> {
    let recipes = recipes()?;
    Ok(usize::try_from(id)
        .ok()
        .and_then(|id| recipes.names.get(id))
        .filter(|(_, declared)| *declared)
        .map(|(name, _)| format!("{NAMESPACE}{name}")))
}

/// The 1.12 ID of a recipe the client names.
fn id(name: &str) -> io::Result<Option<i32>> {
    let recipes = recipes()?;
    Ok(name
        .strip_prefix(NAMESPACE)
        .and_then(|name| recipes.ids.get(name))
        .copied()
        .filter(|id| recipes.names[*id as usize].1))
}

pub fn install(builder: RouteBuilder<Up>) -> RouteBuilder<Up> {
    builder
        .clientbound(State::Play, "recipe", unlock)
        .clientbound(State::Play, "place_ghost_recipe", ghost)
        .serverbound(State::Play, "place_recipe", place)
        .serverbound(State::Play, "recipe_book_update", book)
}

/// A VarInt list of 1.12 IDs as a list of the names the client knows.
fn names(reader: &mut Reader<'_>, writer: &mut Writer) -> io::Result<()> {
    let count = reader.count()?;
    let mut found = Vec::with_capacity(count);
    for _ in 0..count {
        found.extend(name(reader.varint()?)?);
    }
    writer.varint(found.len() as i32);
    for name in found {
        writer.string(&name);
    }
    Ok(())
}

/// Unlock recipes: action (0 init, 1 add, 2 remove), the crafting book's
/// state, the recipes; on init also those to highlight. 1.13 adds the
/// furnace book's state.
fn unlock(_: &mut Up, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let action = reader.varint()?;
    if !(0..3).contains(&action) {
        return Err(invalid("unknown recipe unlock action"));
    }
    let open = reader.bool()?;
    let filtering = reader.bool()?;
    let mut writer = Writer::with_capacity(packet.data.len() * 8);
    writer
        .varint(action)
        .bool(open)
        .bool(filtering)
        .bool(false)
        .bool(false);
    names(&mut reader, &mut writer)?;
    if action == 0 {
        names(&mut reader, &mut writer)?;
    }
    reader.finish()?;
    cx.to_client(state, Packet::new(packet.id, writer.buf));
    Ok(())
}

/// The recipe the server put into the grid for the player to fill.
fn ghost(_: &mut Up, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let window = reader.i8()?;
    let recipe = reader.varint()?;
    reader.finish()?;
    let Some(name) = name(recipe)? else {
        return Ok(());
    };
    let mut writer = Writer::new();
    writer.i8(window).string(&name);
    cx.to_client(state, Packet::new(packet.id, writer.buf));
    Ok(())
}

/// A recipe book click: fill the grid with a recipe.
fn place(_: &mut Up, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let window = reader.i8()?;
    let recipe = reader.str()?;
    let all = reader.bool()?;
    reader.finish()?;
    let Some(recipe) = id(recipe)? else {
        return Ok(());
    };
    let mut writer = Writer::new();
    writer.i8(window).varint(recipe).bool(all);
    cx.to_backend(state, Packet::new(packet.id, writer.buf));
    Ok(())
}

/// Recipe book data: 0 marks a recipe as seen (an int ID in 1.12), 1 the
/// books' state (1.13 adds the furnace book's two flags).
fn book(_: &mut Up, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let kind = reader.varint()?;
    let mut writer = Writer::new();
    writer.varint(kind);
    match kind {
        0 => {
            let Some(recipe) = id(reader.str()?)? else {
                return Ok(());
            };
            writer.i32(recipe);
        }
        1 => {
            writer.bool(reader.bool()?).bool(reader.bool()?);
            reader.bool()?;
            reader.bool()?;
        }
        _ => return Err(invalid("unknown recipe book data type")),
    }
    reader.finish()?;
    cx.to_backend(state, Packet::new(packet.id, writer.buf));
    Ok(())
}
