//! A 1.21.2 client learns recipes as displays in its recipe book, while
//! older backends send whole recipes once and then unlock them by name.
//! The table keeps a backend's recipes (in the client's terms) so that
//! `update_recipes`, `recipe` and `place_ghost_recipe` can be rebuilt in
//! the newer form.
use super::super::{
    ItemStack, Stack,
    convert::item_id,
    recipes::{
        BookAdd, BookEntry, IdSet, Ingredient, Recipe, RecipeBook, RecipeData, RecipeDisplay,
        RecipeIds, SlotDisplay, UpdateRecipes, write_book_remove, write_book_settings,
    },
};
use super::{encode, target_id};
use crate::{Direction, Packet, State, codec::Shared, data, step::Cx, wire::invalid};
use std::{collections::HashMap, io};

#[derive(Default)]
pub struct RecipeTable {
    /// In the order the backend sent them; the index is the display ID.
    recipes: Vec<Recipe>,
    by_name: HashMap<String, usize>,
    /// Recipe groups by name; a group's ID is its index.
    groups: Vec<String>,
    /// The client's protocol, once recipes arrived.
    protocol: Option<i32>,
}

const NOTIFICATION: u8 = 1;
const HIGHLIGHT: u8 = 2;

impl RecipeTable {
    pub fn new(recipes: Vec<Recipe>) -> Self {
        let by_name = recipes
            .iter()
            .enumerate()
            .map(|(index, recipe)| (recipe.id.clone(), index))
            .collect();
        let mut groups = Vec::new();
        for recipe in &recipes {
            if let Some(group) = group(&recipe.data)
                && !group.is_empty()
                && !groups.iter().any(|known| known == group)
            {
                groups.push(group.to_owned());
            }
        }
        Self {
            recipes,
            by_name,
            groups,
            protocol: None,
        }
    }

    /// The name of the recipe a 1.21.2 client refers to by display ID, for
    /// steps translating `place_recipe` and `recipe_book_seen_recipe`.
    pub fn name(&self, display: i32) -> Option<&str> {
        let index = usize::try_from(display).ok()?;
        Some(&self.recipes.get(index)?.id)
    }

    /// The display ID of a recipe name.
    pub fn display_id(&self, name: &str) -> Option<i32> {
        self.by_name.get(name).map(|index| *index as i32)
    }

    /// The display of the recipe `name`, if it has one.
    pub fn display(&self, name: &str) -> Option<RecipeDisplay> {
        let target = self.protocol?;
        display(target, &self.recipes[*self.by_name.get(name)?])
    }

    /// What a 1.21.2 client's `update_recipes` holds: the input sets of
    /// furnaces and smithing tables, and the stonecutter's recipes.
    pub fn update_recipes(&mut self, target: i32) -> io::Result<UpdateRecipes> {
        self.protocol = Some(target);
        let mut sets: Vec<(&str, Vec<i32>)> = [
            "blast_furnace_input",
            "campfire_input",
            "furnace_input",
            "smithing_addition",
            "smithing_base",
            "smithing_template",
            "smoker_input",
        ]
        .into_iter()
        .map(|name| (name, Vec::new()))
        .collect();
        let add = |sets: &mut Vec<(&str, Vec<i32>)>, name: &str, ingredient: &Ingredient| {
            if let Some((_, items)) = sets.iter_mut().find(|(known, _)| *known == name) {
                for stack in ingredient.iter().flatten() {
                    if !items.contains(&stack.id) {
                        items.push(stack.id);
                    }
                }
            }
        };
        let mut stonecutter = Vec::new();
        for recipe in &self.recipes {
            match (&recipe.data, recipe.serializer.as_str()) {
                (RecipeData::Cooking { ingredient, .. }, serializer) => {
                    let set = match serializer {
                        "smelting" => "furnace_input",
                        "blasting" => "blast_furnace_input",
                        "smoking" => "smoker_input",
                        _ => "campfire_input",
                    };
                    add(&mut sets, set, ingredient);
                }
                (
                    RecipeData::SmithingTransform {
                        template,
                        base,
                        addition,
                        ..
                    }
                    | RecipeData::SmithingTrim {
                        template,
                        base,
                        addition,
                    },
                    _,
                ) => {
                    add(&mut sets, "smithing_template", template);
                    add(&mut sets, "smithing_base", base);
                    add(&mut sets, "smithing_addition", addition);
                }
                (
                    RecipeData::Stonecutting {
                        ingredient,
                        result: Some(result),
                        ..
                    },
                    _,
                ) => {
                    let ids = ingredient.iter().flatten().map(|stack| stack.id).collect();
                    stonecutter.push((IdSet::Ids(ids), SlotDisplay::Stack(result.clone())));
                }
                _ => {}
            }
        }
        Ok(UpdateRecipes::Sets {
            property_sets: sets
                .into_iter()
                .map(|(name, items)| (format!("minecraft:{name}"), items))
                .collect(),
            stonecutter,
        })
    }
}

fn group(data: &RecipeData) -> Option<&str> {
    match data {
        RecipeData::Shapeless { group, .. }
        | RecipeData::Shaped { group, .. }
        | RecipeData::Cooking { group, .. }
        | RecipeData::Stonecutting { group, .. } => Some(group),
        _ => None,
    }
}

fn ingredient_display(ingredient: &Ingredient) -> SlotDisplay {
    let items: Vec<SlotDisplay> = ingredient
        .iter()
        .flatten()
        .map(|stack| SlotDisplay::Item(stack.id))
        .collect();
    match items.len() {
        0 => SlotDisplay::Empty,
        1 => items.into_iter().next().expect("one item"),
        _ => SlotDisplay::Composite(items),
    }
}

fn result_display(result: &Stack) -> SlotDisplay {
    result
        .as_ref()
        .map_or(SlotDisplay::Empty, |stack: &ItemStack| {
            SlotDisplay::Stack(stack.clone())
        })
}

fn station(protocol: i32, name: &str) -> SlotDisplay {
    item_id(protocol, name).map_or(SlotDisplay::Empty, SlotDisplay::Item)
}

/// The book display of a crafting or cooking recipe.
fn display(protocol: i32, recipe: &Recipe) -> Option<RecipeDisplay> {
    Some(match &recipe.data {
        RecipeData::Shapeless {
            ingredients,
            result,
            ..
        } => RecipeDisplay::Shapeless {
            ingredients: ingredients.iter().map(ingredient_display).collect(),
            result: result_display(result),
            station: station(protocol, "crafting_table"),
        },
        RecipeData::Shaped {
            width,
            height,
            ingredients,
            result,
            ..
        } => RecipeDisplay::Shaped {
            width: *width,
            height: *height,
            ingredients: ingredients.iter().map(ingredient_display).collect(),
            result: result_display(result),
            station: station(protocol, "crafting_table"),
        },
        RecipeData::Cooking {
            ingredient,
            result,
            xp,
            time,
            ..
        } => RecipeDisplay::Furnace {
            ingredient: ingredient_display(ingredient),
            fuel: SlotDisplay::AnyFuel,
            result: result_display(result),
            station: station(
                protocol,
                match recipe.serializer.as_str() {
                    "smelting" => "furnace",
                    "blasting" => "blast_furnace",
                    "smoking" => "smoker",
                    _ => "campfire",
                },
            ),
            duration: *time,
            xp: *xp,
        },
        _ => return None,
    })
}

/// The recipe book category of a recipe (`recipe_book_category`).
fn category(recipe: &Recipe) -> &'static str {
    const CRAFTING: [&str; 4] = [
        "crafting_building_blocks",
        "crafting_redstone",
        "crafting_equipment",
        "crafting_misc",
    ];
    match (&recipe.data, recipe.serializer.as_str()) {
        (RecipeData::Shapeless { category, .. } | RecipeData::Shaped { category, .. }, _) => {
            CRAFTING
                .get(usize::try_from(*category).unwrap_or(3))
                .copied()
                .unwrap_or("crafting_misc")
        }
        (RecipeData::Cooking { category, .. }, "smelting") => match category {
            0 => "furnace_food",
            1 => "furnace_blocks",
            _ => "furnace_misc",
        },
        (RecipeData::Cooking { category, .. }, "blasting") => match category {
            1 => "blast_furnace_blocks",
            _ => "blast_furnace_misc",
        },
        (RecipeData::Cooking { .. }, "smoking") => "smoker_food",
        (RecipeData::Cooking { .. }, _) => "campfire",
        _ => "crafting_misc",
    }
}

fn notification(recipe: &Recipe) -> bool {
    match recipe.data {
        RecipeData::Shaped { notification, .. } => notification,
        _ => true,
    }
}

fn requirements(recipe: &Recipe) -> Option<Vec<IdSet>> {
    match &recipe.data {
        RecipeData::Shapeless { ingredients, .. } | RecipeData::Shaped { ingredients, .. } => Some(
            ingredients
                .iter()
                .filter(|ingredient| ingredient.iter().any(Option::is_some))
                .map(|ingredient| {
                    IdSet::Ids(ingredient.iter().flatten().map(|stack| stack.id).collect())
                })
                .collect(),
        ),
        _ => None,
    }
}

/// The `recipe` packet of an older backend as a 1.21.2 client's settings
/// and book additions or removals.
pub(super) fn recipe_book(
    shared: &mut Shared,
    cx: &mut Cx<'_>,
    state: State,
    book: RecipeBook,
) -> io::Result<()> {
    let direction = Direction::Clientbound;
    let target = shared.target(direction);
    let settings = target_id(shared, direction, state, "recipe_book_settings")?;
    cx.to_client(
        state,
        Packet::new(
            settings,
            encode(|w| {
                write_book_settings(w, &book.flags);
                Ok(())
            })?,
        ),
    );
    let RecipeIds::Names(names) = book.recipes else {
        return Err(invalid("numbered recipes cannot reach a 1.21.2 client"));
    };
    let table = &shared.item.recipes;
    if book.action == 2 {
        let ids: Vec<i32> = names
            .iter()
            .filter_map(|name| table.display_id(name))
            .collect();
        let id = target_id(shared, direction, state, "recipe_book_remove")?;
        cx.to_client(
            state,
            Packet::new(
                id,
                encode(|w| {
                    write_book_remove(w, &ids);
                    Ok(())
                })?,
            ),
        );
        return Ok(());
    }
    let highlight: Vec<String> = match &book.highlight {
        Some(RecipeIds::Names(names)) => names.clone(),
        _ => Vec::new(),
    };
    let categories = data::version(target).registry("recipe_book_category");
    let mut entries = Vec::new();
    for name in &names {
        let Some(&index) = table.by_name.get(name) else {
            continue;
        };
        let recipe = &table.recipes[index];
        let Some(display) = display(target, recipe) else {
            continue;
        };
        let Some(category) = categories.and_then(|registry| registry.id(category(recipe))) else {
            continue;
        };
        let group = group(&recipe.data)
            .and_then(|group| table.groups.iter().position(|known| known == group))
            .map_or(0, |index| index as i32 + 1);
        let mut flags = 0;
        if book.action == 1 {
            if notification(recipe) {
                flags |= NOTIFICATION;
            }
            flags |= HIGHLIGHT;
        } else if highlight.contains(name) {
            flags |= HIGHLIGHT;
        }
        entries.push(BookEntry {
            id: index as i32,
            display,
            group,
            category,
            requirements: requirements(recipe),
            flags,
        });
    }
    let add = BookAdd {
        entries,
        replace: book.action == 0,
    };
    let id = target_id(shared, direction, state, "recipe_book_add")?;
    cx.to_client(state, Packet::new(id, encode(|w| add.write(target, w))?));
    Ok(())
}
