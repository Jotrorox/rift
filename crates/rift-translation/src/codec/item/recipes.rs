//! Recipes in every protocol's layout: `update_recipes` (whole recipes
//! until 1.21.2, then property sets and stonecutter recipes), the recipe
//! book (`recipe` until 1.21.2, then `recipe_book_add`, `recipe_book_remove`
//! and `recipe_book_settings`), `place_ghost_recipe`, and the slot and
//! recipe displays of 1.21.2 on.
use super::{ItemStack, Stack, Text, stack::read_template, stack::write_template, window};
use crate::{
    data,
    wire::{Reader, Writer, invalid},
};
use std::io;

const IDENTIFIER: usize = 32767;

/// Recipes become displays from 1.21.2 on.
pub fn displays(protocol: i32) -> bool {
    protocol >= 768
}

/// One ingredient: the stacks that match it.
pub type Ingredient = Vec<Stack>;

#[derive(Clone, Debug, PartialEq)]
pub enum RecipeData {
    Shapeless {
        group: String,
        /// The crafting book category, 1.19.3 on.
        category: i32,
        ingredients: Vec<Ingredient>,
        result: Stack,
    },
    Shaped {
        group: String,
        category: i32,
        width: i32,
        height: i32,
        ingredients: Vec<Ingredient>,
        result: Stack,
        /// 1.19.4 on.
        notification: bool,
    },
    /// Recipes the client computes itself; a category from 1.19.3 on.
    Special { category: i32 },
    Cooking {
        group: String,
        category: i32,
        ingredient: Ingredient,
        result: Stack,
        xp: f32,
        time: i32,
    },
    Stonecutting {
        group: String,
        ingredient: Ingredient,
        result: Stack,
    },
    /// 1.16 to 1.19.4.
    Smithing {
        base: Ingredient,
        addition: Ingredient,
        result: Stack,
    },
    /// 1.19.4 on.
    SmithingTransform {
        template: Ingredient,
        base: Ingredient,
        addition: Ingredient,
        result: Stack,
    },
    SmithingTrim {
        template: Ingredient,
        base: Ingredient,
        addition: Ingredient,
    },
}

/// One recipe of `update_recipes` before 1.21.2. `serializer` is the
/// serializer's name without namespace.
#[derive(Clone, Debug, PartialEq)]
pub struct Recipe {
    pub id: String,
    pub serializer: String,
    pub data: RecipeData,
}

#[derive(Clone, Debug, PartialEq)]
pub enum IdSet {
    Tag(String),
    Ids(Vec<i32>),
}

impl IdSet {
    pub fn read(r: &mut Reader<'_>) -> io::Result<Self> {
        let count = r.varint()?;
        if count == 0 {
            return Ok(Self::Tag(r.string(IDENTIFIER)?.to_owned()));
        }
        let count = usize::try_from(count - 1)
            .ok()
            .filter(|count| *count <= r.remaining())
            .ok_or_else(|| invalid("invalid ID set"))?;
        Ok(Self::Ids(
            (0..count).map(|_| r.varint()).collect::<io::Result<_>>()?,
        ))
    }

    pub fn write(&self, w: &mut Writer) {
        match self {
            Self::Tag(tag) => {
                w.varint(0).string(tag);
            }
            Self::Ids(ids) => {
                w.varint(ids.len() as i32 + 1);
                for id in ids {
                    w.varint(*id);
                }
            }
        }
    }
}

/// How a smithing trim display names its pattern: a display (1.21.2 to
/// 1.21.4), then a trim pattern registry entry or an inline pattern.
#[derive(Clone, Debug, PartialEq)]
pub enum TrimPattern {
    Display(Box<SlotDisplay>),
    Registered(i32),
    Inline {
        asset: String,
        description: Text,
        decal: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum SlotDisplay {
    Empty,
    AnyFuel,
    /// 26.1 on.
    WithAnyPotion(Box<SlotDisplay>),
    /// 26.1 on: the source and a data component type.
    OnlyWithComponent(Box<SlotDisplay>, i32),
    Item(i32),
    /// A non-empty stack (a stack template from 26.1 on).
    Stack(ItemStack),
    /// A tag name until 26.3, an item set after.
    Tag(String),
    Items(IdSet),
    /// 26.1 on: the dye and the target.
    Dyed(Box<SlotDisplay>, Box<SlotDisplay>),
    SmithingTrim {
        base: Box<SlotDisplay>,
        material: Box<SlotDisplay>,
        pattern: TrimPattern,
    },
    WithRemainder(Box<SlotDisplay>, Box<SlotDisplay>),
    Composite(Vec<SlotDisplay>),
}

/// The registry name of a slot display variant.
impl SlotDisplay {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::AnyFuel => "any_fuel",
            Self::WithAnyPotion(_) => "with_any_potion",
            Self::OnlyWithComponent(..) => "only_with_component",
            Self::Item(_) => "item",
            Self::Stack(_) => "item_stack",
            Self::Tag(_) | Self::Items(_) => "tag",
            Self::Dyed(..) => "dyed",
            Self::SmithingTrim { .. } => "smithing_trim",
            Self::WithRemainder(..) => "with_remainder",
            Self::Composite(_) => "composite",
        }
    }
}

/// Registry IDs of data-driven kinds by name, per protocol.
fn kind_id(protocol: i32, registry: &str, name: &str) -> io::Result<i32> {
    data::version(protocol)
        .registry(registry)
        .and_then(|registry| registry.id(name))
        .ok_or_else(|| {
            invalid(&format!(
                "{registry} {name} does not exist in this protocol"
            ))
        })
}

fn kind_name(protocol: i32, registry: &str, id: i32) -> io::Result<&'static str> {
    data::version(protocol)
        .registry(registry)
        .and_then(|registry| registry.name(id))
        .ok_or_else(|| invalid(&format!("unknown {registry} {id}")))
}

/// Nested displays share the wire depth budget.
const MAX_DISPLAY_DEPTH: usize = 32;

impl SlotDisplay {
    pub fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Self> {
        Self::read_nested(protocol, r, 0)
    }

    fn read_nested(protocol: i32, r: &mut Reader<'_>, depth: usize) -> io::Result<Self> {
        if depth > MAX_DISPLAY_DEPTH {
            return Err(invalid("slot displays nest too deeply"));
        }
        let nested = |r: &mut Reader<'_>| Self::read_nested(protocol, r, depth + 1).map(Box::new);
        let kind = kind_name(protocol, "slot_display", r.varint()?)?;
        Ok(match kind {
            "empty" => Self::Empty,
            "any_fuel" => Self::AnyFuel,
            "with_any_potion" => Self::WithAnyPotion(nested(r)?),
            "only_with_component" => {
                let source = nested(r)?;
                Self::OnlyWithComponent(source, r.varint()?)
            }
            "item" => Self::Item(r.varint()?),
            "item_stack" => Self::Stack(if protocol >= 775 {
                read_template(protocol, r)?
            } else {
                window::read_stack(protocol, r)?
                    .ok_or_else(|| invalid("an item stack display cannot be empty"))?
            }),
            "tag" if protocol >= 777 => Self::Items(IdSet::read(r)?),
            "tag" => Self::Tag(r.string(IDENTIFIER)?.to_owned()),
            "dyed" => {
                let dye = nested(r)?;
                Self::Dyed(dye, nested(r)?)
            }
            "smithing_trim" => {
                let base = nested(r)?;
                let material = nested(r)?;
                let pattern = if protocol < 770 {
                    TrimPattern::Display(nested(r)?)
                } else {
                    match r.varint()? {
                        0 => TrimPattern::Inline {
                            asset: r.string(IDENTIFIER)?.to_owned(),
                            description: Text::read(protocol, r)?,
                            decal: r.bool()?,
                        },
                        id => TrimPattern::Registered(id - 1),
                    }
                };
                Self::SmithingTrim {
                    base,
                    material,
                    pattern,
                }
            }
            "with_remainder" => {
                let input = nested(r)?;
                Self::WithRemainder(input, nested(r)?)
            }
            "composite" => {
                let count = r.count()?;
                Self::Composite(
                    (0..count)
                        .map(|_| Self::read_nested(protocol, r, depth + 1))
                        .collect::<io::Result<_>>()?,
                )
            }
            other => return Err(invalid(&format!("unknown slot display {other}"))),
        })
    }

    pub fn write(&self, protocol: i32, w: &mut Writer) -> io::Result<()> {
        if matches!(
            (self, protocol >= 777),
            (Self::Tag(_), true) | (Self::Items(_), false)
        ) {
            return Err(invalid("tag display does not fit the protocol"));
        }
        w.varint(kind_id(protocol, "slot_display", self.kind())?);
        match self {
            Self::Empty | Self::AnyFuel => {}
            Self::WithAnyPotion(base) => base.write(protocol, w)?,
            Self::OnlyWithComponent(source, component) => {
                source.write(protocol, w)?;
                w.varint(*component);
            }
            Self::Item(id) => {
                w.varint(*id);
            }
            Self::Stack(stack) => {
                if protocol >= 775 {
                    write_template(protocol, w, stack)?;
                } else {
                    window::write_stack(protocol, w, &Some(stack.clone()))?;
                }
            }
            Self::Tag(tag) => {
                w.string(tag);
            }
            Self::Items(items) => items.write(w),
            Self::Dyed(dye, target) => {
                dye.write(protocol, w)?;
                target.write(protocol, w)?;
            }
            Self::SmithingTrim {
                base,
                material,
                pattern,
            } => {
                base.write(protocol, w)?;
                material.write(protocol, w)?;
                match (pattern, protocol < 770) {
                    (TrimPattern::Display(display), true) => display.write(protocol, w)?,
                    (TrimPattern::Registered(id), false) => {
                        w.varint(id + 1);
                    }
                    (
                        TrimPattern::Inline {
                            asset,
                            description,
                            decal,
                        },
                        false,
                    ) => {
                        w.varint(0).string(asset);
                        description.write(protocol, w)?;
                        w.bool(*decal);
                    }
                    _ => return Err(invalid("trim pattern does not fit the protocol")),
                }
            }
            Self::WithRemainder(input, remainder) => {
                input.write(protocol, w)?;
                remainder.write(protocol, w)?;
            }
            Self::Composite(displays) => {
                w.varint(displays.len() as i32);
                for display in displays {
                    display.write(protocol, w)?;
                }
            }
        }
        Ok(())
    }

    /// Every display nested in this one, this one first, mutably.
    pub fn visit(
        &mut self,
        f: &mut dyn FnMut(&mut SlotDisplay) -> io::Result<()>,
    ) -> io::Result<()> {
        f(self)?;
        match self {
            Self::WithAnyPotion(inner) | Self::OnlyWithComponent(inner, _) => inner.visit(f),
            Self::Dyed(a, b) | Self::WithRemainder(a, b) => {
                a.visit(f)?;
                b.visit(f)
            }
            Self::SmithingTrim {
                base,
                material,
                pattern,
            } => {
                base.visit(f)?;
                material.visit(f)?;
                if let TrimPattern::Display(display) = pattern {
                    display.visit(f)?;
                }
                Ok(())
            }
            Self::Composite(displays) => {
                displays.iter_mut().try_for_each(|display| display.visit(f))
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RecipeDisplay {
    Shapeless {
        ingredients: Vec<SlotDisplay>,
        result: SlotDisplay,
        station: SlotDisplay,
    },
    Shaped {
        width: i32,
        height: i32,
        ingredients: Vec<SlotDisplay>,
        result: SlotDisplay,
        station: SlotDisplay,
    },
    Furnace {
        ingredient: SlotDisplay,
        fuel: SlotDisplay,
        result: SlotDisplay,
        station: SlotDisplay,
        duration: i32,
        xp: f32,
    },
    Stonecutter {
        input: SlotDisplay,
        result: SlotDisplay,
        station: SlotDisplay,
    },
    Smithing {
        template: SlotDisplay,
        base: SlotDisplay,
        addition: SlotDisplay,
        result: SlotDisplay,
        station: SlotDisplay,
    },
}

fn display_list(protocol: i32, r: &mut Reader<'_>) -> io::Result<Vec<SlotDisplay>> {
    let count = r.count()?;
    (0..count).map(|_| SlotDisplay::read(protocol, r)).collect()
}

fn write_display_list(protocol: i32, w: &mut Writer, displays: &[SlotDisplay]) -> io::Result<()> {
    w.varint(displays.len() as i32);
    displays
        .iter()
        .try_for_each(|display| display.write(protocol, w))
}

impl RecipeDisplay {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Shapeless { .. } => "crafting_shapeless",
            Self::Shaped { .. } => "crafting_shaped",
            Self::Furnace { .. } => "furnace",
            Self::Stonecutter { .. } => "stonecutter",
            Self::Smithing { .. } => "smithing",
        }
    }

    pub fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Self> {
        let slot = |r: &mut Reader<'_>| SlotDisplay::read(protocol, r);
        Ok(match kind_name(protocol, "recipe_display", r.varint()?)? {
            "crafting_shapeless" => Self::Shapeless {
                ingredients: display_list(protocol, r)?,
                result: slot(r)?,
                station: slot(r)?,
            },
            "crafting_shaped" => Self::Shaped {
                width: r.varint()?,
                height: r.varint()?,
                ingredients: display_list(protocol, r)?,
                result: slot(r)?,
                station: slot(r)?,
            },
            "furnace" => Self::Furnace {
                ingredient: slot(r)?,
                fuel: slot(r)?,
                result: slot(r)?,
                station: slot(r)?,
                duration: r.varint()?,
                xp: r.f32()?,
            },
            "stonecutter" => Self::Stonecutter {
                input: slot(r)?,
                result: slot(r)?,
                station: slot(r)?,
            },
            "smithing" => Self::Smithing {
                template: slot(r)?,
                base: slot(r)?,
                addition: slot(r)?,
                result: slot(r)?,
                station: slot(r)?,
            },
            other => return Err(invalid(&format!("unknown recipe display {other}"))),
        })
    }

    pub fn write(&self, protocol: i32, w: &mut Writer) -> io::Result<()> {
        w.varint(kind_id(protocol, "recipe_display", self.kind())?);
        match self {
            Self::Shapeless {
                ingredients,
                result,
                station,
            } => {
                write_display_list(protocol, w, ingredients)?;
                result.write(protocol, w)?;
                station.write(protocol, w)
            }
            Self::Shaped {
                width,
                height,
                ingredients,
                result,
                station,
            } => {
                w.varint(*width).varint(*height);
                write_display_list(protocol, w, ingredients)?;
                result.write(protocol, w)?;
                station.write(protocol, w)
            }
            Self::Furnace {
                ingredient,
                fuel,
                result,
                station,
                duration,
                xp,
            } => {
                for display in [ingredient, fuel, result, station] {
                    display.write(protocol, w)?;
                }
                w.varint(*duration).f32(*xp);
                Ok(())
            }
            Self::Stonecutter {
                input,
                result,
                station,
            } => [input, result, station]
                .into_iter()
                .try_for_each(|display| display.write(protocol, w)),
            Self::Smithing {
                template,
                base,
                addition,
                result,
                station,
            } => [template, base, addition, result, station]
                .into_iter()
                .try_for_each(|display| display.write(protocol, w)),
        }
    }

    /// Every slot display in this recipe display, nested ones included,
    /// mutably.
    pub fn visit(
        &mut self,
        f: &mut dyn FnMut(&mut SlotDisplay) -> io::Result<()>,
    ) -> io::Result<()> {
        self.slots()
            .into_iter()
            .try_for_each(|display| display.visit(f))
    }

    /// The slot displays this recipe display holds directly, mutably.
    pub fn slots(&mut self) -> Vec<&mut SlotDisplay> {
        match self {
            Self::Shapeless {
                ingredients,
                result,
                station,
            }
            | Self::Shaped {
                ingredients,
                result,
                station,
                ..
            } => ingredients.iter_mut().chain([result, station]).collect(),
            Self::Furnace {
                ingredient,
                fuel,
                result,
                station,
                ..
            } => vec![ingredient, fuel, result, station],
            Self::Stonecutter {
                input,
                result,
                station,
            } => vec![input, result, station],
            Self::Smithing {
                template,
                base,
                addition,
                result,
                station,
            } => vec![template, base, addition, result, station],
        }
    }
}

/// `update_recipes`.
#[derive(Clone, Debug, PartialEq)]
pub enum UpdateRecipes {
    /// Before 1.21.2.
    Recipes(Vec<Recipe>),
    /// From 1.21.2: item sets the client predicts inputs with, and the
    /// stonecutter's recipes.
    Sets {
        property_sets: Vec<(String, Vec<i32>)>,
        stonecutter: Vec<(IdSet, SlotDisplay)>,
    },
}

fn ingredient(protocol: i32, r: &mut Reader<'_>) -> io::Result<Ingredient> {
    let count = r.count()?;
    (0..count)
        .map(|_| window::read_stack(protocol, r))
        .collect()
}

fn write_ingredient(protocol: i32, w: &mut Writer, ingredient: &Ingredient) -> io::Result<()> {
    w.varint(ingredient.len() as i32);
    ingredient
        .iter()
        .try_for_each(|stack| window::write_stack(protocol, w, stack))
}

/// Recipe categories exist from 1.19.3 on.
fn categories(protocol: i32) -> bool {
    protocol >= 761
}

fn is_special(serializer: &str) -> bool {
    serializer.starts_with("crafting_special_")
        || matches!(serializer, "crafting_decorated_pot" | "crafting_transmute")
}

impl RecipeData {
    fn read(protocol: i32, serializer: &str, r: &mut Reader<'_>) -> io::Result<Self> {
        let category = |r: &mut Reader<'_>| -> io::Result<i32> {
            if categories(protocol) {
                r.varint()
            } else {
                Ok(0)
            }
        };
        let stack = |r: &mut Reader<'_>| window::read_stack(protocol, r);
        Ok(match serializer {
            "crafting_shapeless" => {
                let group = r.string(IDENTIFIER)?.to_owned();
                let category = category(r)?;
                let count = r.count()?;
                let ingredients = (0..count)
                    .map(|_| ingredient(protocol, r))
                    .collect::<io::Result<_>>()?;
                Self::Shapeless {
                    group,
                    category,
                    ingredients,
                    result: stack(r)?,
                }
            }
            "crafting_shaped" => {
                let (group, category, width, height);
                if protocol >= 765 {
                    group = r.string(IDENTIFIER)?.to_owned();
                    category = r.varint()?;
                    width = r.varint()?;
                    height = r.varint()?;
                } else {
                    width = r.varint()?;
                    height = r.varint()?;
                    group = r.string(IDENTIFIER)?.to_owned();
                    category = if categories(protocol) { r.varint()? } else { 0 };
                }
                let cells = usize::try_from(width)
                    .ok()
                    .zip(usize::try_from(height).ok())
                    .and_then(|(width, height)| width.checked_mul(height))
                    .filter(|cells| *cells <= r.remaining())
                    .ok_or_else(|| invalid("invalid shaped recipe size"))?;
                let ingredients = (0..cells)
                    .map(|_| ingredient(protocol, r))
                    .collect::<io::Result<_>>()?;
                let result = stack(r)?;
                let notification = if protocol >= 762 { r.bool()? } else { true };
                Self::Shaped {
                    group,
                    category,
                    width,
                    height,
                    ingredients,
                    result,
                    notification,
                }
            }
            "smelting" | "blasting" | "smoking" | "campfire_cooking" => Self::Cooking {
                group: r.string(IDENTIFIER)?.to_owned(),
                category: category(r)?,
                ingredient: ingredient(protocol, r)?,
                result: stack(r)?,
                xp: r.f32()?,
                time: r.varint()?,
            },
            "stonecutting" => Self::Stonecutting {
                group: r.string(IDENTIFIER)?.to_owned(),
                ingredient: ingredient(protocol, r)?,
                result: stack(r)?,
            },
            "smithing" => Self::Smithing {
                base: ingredient(protocol, r)?,
                addition: ingredient(protocol, r)?,
                result: stack(r)?,
            },
            "smithing_transform" => Self::SmithingTransform {
                template: ingredient(protocol, r)?,
                base: ingredient(protocol, r)?,
                addition: ingredient(protocol, r)?,
                result: stack(r)?,
            },
            "smithing_trim" => Self::SmithingTrim {
                template: ingredient(protocol, r)?,
                base: ingredient(protocol, r)?,
                addition: ingredient(protocol, r)?,
            },
            special if is_special(special) => Self::Special {
                category: category(r)?,
            },
            other => return Err(invalid(&format!("unknown recipe serializer {other}"))),
        })
    }

    fn write(&self, protocol: i32, w: &mut Writer) -> io::Result<()> {
        let category = |w: &mut Writer, category: i32| {
            if categories(protocol) {
                w.varint(category);
            }
        };
        let stack = |w: &mut Writer, stack: &Stack| window::write_stack(protocol, w, stack);
        match self {
            Self::Shapeless {
                group,
                category: book,
                ingredients,
                result,
            } => {
                w.string(group);
                category(w, *book);
                w.varint(ingredients.len() as i32);
                for ingredient in ingredients {
                    write_ingredient(protocol, w, ingredient)?;
                }
                stack(w, result)
            }
            Self::Shaped {
                group,
                category: book,
                width,
                height,
                ingredients,
                result,
                notification,
            } => {
                if ingredients.len() as i64 != i64::from(*width) * i64::from(*height) {
                    return Err(invalid("shaped recipe size does not match its ingredients"));
                }
                if protocol >= 765 {
                    w.string(group).varint(*book).varint(*width).varint(*height);
                } else {
                    w.varint(*width).varint(*height).string(group);
                    category(w, *book);
                }
                for ingredient in ingredients {
                    write_ingredient(protocol, w, ingredient)?;
                }
                stack(w, result)?;
                if protocol >= 762 {
                    w.bool(*notification);
                }
                Ok(())
            }
            Self::Special { category: book } => {
                category(w, *book);
                Ok(())
            }
            Self::Cooking {
                group,
                category: book,
                ingredient,
                result,
                xp,
                time,
            } => {
                w.string(group);
                category(w, *book);
                write_ingredient(protocol, w, ingredient)?;
                stack(w, result)?;
                w.f32(*xp).varint(*time);
                Ok(())
            }
            Self::Stonecutting {
                group,
                ingredient,
                result,
            } => {
                w.string(group);
                write_ingredient(protocol, w, ingredient)?;
                stack(w, result)
            }
            Self::Smithing {
                base,
                addition,
                result,
            } => {
                write_ingredient(protocol, w, base)?;
                write_ingredient(protocol, w, addition)?;
                stack(w, result)
            }
            Self::SmithingTransform {
                template,
                base,
                addition,
                result,
            } => {
                for ingredient in [template, base, addition] {
                    write_ingredient(protocol, w, ingredient)?;
                }
                stack(w, result)
            }
            Self::SmithingTrim {
                template,
                base,
                addition,
            } => [template, base, addition]
                .into_iter()
                .try_for_each(|ingredient| write_ingredient(protocol, w, ingredient)),
        }
    }

    /// Every stack in the recipe, mutably.
    pub fn stacks(&mut self) -> Vec<&mut Stack> {
        let mut stacks: Vec<&mut Stack> = Vec::new();
        match self {
            Self::Shapeless {
                ingredients,
                result,
                ..
            }
            | Self::Shaped {
                ingredients,
                result,
                ..
            } => {
                stacks.extend(ingredients.iter_mut().flatten());
                stacks.push(result);
            }
            Self::Special { .. } => {}
            Self::Cooking {
                ingredient, result, ..
            }
            | Self::Stonecutting {
                ingredient, result, ..
            } => {
                stacks.extend(ingredient.iter_mut());
                stacks.push(result);
            }
            Self::Smithing {
                base,
                addition,
                result,
            } => {
                stacks.extend(base.iter_mut().chain(addition.iter_mut()));
                stacks.push(result);
            }
            Self::SmithingTransform {
                template,
                base,
                addition,
                result,
            } => {
                stacks.extend(
                    template
                        .iter_mut()
                        .chain(base.iter_mut())
                        .chain(addition.iter_mut()),
                );
                stacks.push(result);
            }
            Self::SmithingTrim {
                template,
                base,
                addition,
            } => stacks.extend(
                template
                    .iter_mut()
                    .chain(base.iter_mut())
                    .chain(addition.iter_mut()),
            ),
        }
        stacks
    }
}

impl UpdateRecipes {
    pub fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Self> {
        let count = r.count()?;
        if displays(protocol) {
            let mut property_sets = Vec::with_capacity(count);
            for _ in 0..count {
                let name = r.string(IDENTIFIER)?.to_owned();
                let items = r.count()?;
                let items = (0..items).map(|_| r.varint()).collect::<io::Result<_>>()?;
                property_sets.push((name, items));
            }
            let count = r.count()?;
            let stonecutter = (0..count)
                .map(|_| Ok((IdSet::read(r)?, SlotDisplay::read(protocol, r)?)))
                .collect::<io::Result<_>>()?;
            return Ok(Self::Sets {
                property_sets,
                stonecutter,
            });
        }
        let mut recipes = Vec::with_capacity(count);
        for _ in 0..count {
            let (id, serializer) = if protocol >= 766 {
                let id = r.string(IDENTIFIER)?.to_owned();
                let serializer = kind_name(protocol, "recipe_serializer", r.varint()?)?;
                (id, serializer.to_owned())
            } else if protocol >= 477 {
                let serializer = r.string(IDENTIFIER)?;
                (r.string(IDENTIFIER)?.to_owned(), serializer.to_owned())
            } else {
                let id = r.string(IDENTIFIER)?.to_owned();
                (id, r.string(IDENTIFIER)?.to_owned())
            };
            let serializer = data::strip(&serializer).to_owned();
            let data = RecipeData::read(protocol, &serializer, r)?;
            recipes.push(Recipe {
                id,
                serializer,
                data,
            });
        }
        Ok(Self::Recipes(recipes))
    }

    pub fn write(&self, protocol: i32, w: &mut Writer) -> io::Result<()> {
        match (self, displays(protocol)) {
            (
                Self::Sets {
                    property_sets,
                    stonecutter,
                },
                true,
            ) => {
                w.varint(property_sets.len() as i32);
                for (name, items) in property_sets {
                    w.string(name).varint(items.len() as i32);
                    for item in items {
                        w.varint(*item);
                    }
                }
                w.varint(stonecutter.len() as i32);
                for (input, display) in stonecutter {
                    input.write(w);
                    display.write(protocol, w)?;
                }
                Ok(())
            }
            (Self::Recipes(recipes), false) => {
                w.varint(recipes.len() as i32);
                for recipe in recipes {
                    if protocol >= 766 {
                        w.string(&recipe.id).varint(kind_id(
                            protocol,
                            "recipe_serializer",
                            &recipe.serializer,
                        )?);
                    } else if protocol >= 477 {
                        w.string(&format!("minecraft:{}", recipe.serializer))
                            .string(&recipe.id);
                    } else {
                        w.string(&recipe.id).string(&recipe.serializer);
                    }
                    recipe.data.write(protocol, w)?;
                }
                Ok(())
            }
            _ => Err(invalid("recipes do not fit the protocol")),
        }
    }
}

/// Recipe IDs in the recipe book: numbers in 1.12, names until 1.21.2.
#[derive(Clone, Debug, PartialEq)]
pub enum RecipeIds {
    Numeric(Vec<i32>),
    Names(Vec<String>),
}

impl RecipeIds {
    fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Self> {
        let count = r.count()?;
        Ok(if version_flattened(protocol) {
            Self::Names(
                (0..count)
                    .map(|_| Ok(r.string(IDENTIFIER)?.to_owned()))
                    .collect::<io::Result<_>>()?,
            )
        } else {
            Self::Numeric((0..count).map(|_| r.varint()).collect::<io::Result<_>>()?)
        })
    }

    fn write(&self, protocol: i32, w: &mut Writer) -> io::Result<()> {
        match (self, version_flattened(protocol)) {
            (Self::Names(names), true) => {
                w.varint(names.len() as i32);
                for name in names {
                    w.string(name);
                }
            }
            (Self::Numeric(ids), false) => {
                w.varint(ids.len() as i32);
                for id in ids {
                    w.varint(*id);
                }
            }
            _ => return Err(invalid("recipe IDs do not fit the protocol")),
        }
        Ok(())
    }
}

fn version_flattened(protocol: i32) -> bool {
    crate::version::flattened(protocol)
}

/// How many recipe book flags (open, filtering) pairs a protocol has:
/// crafting; furnace from 1.13; blast furnace and smoker from 1.16.2.
pub fn book_flags(protocol: i32) -> usize {
    if protocol >= 751 {
        8
    } else if protocol >= 393 {
        4
    } else {
        2
    }
}

/// The `recipe` packet, before 1.21.2. `action` 0 initialises the book (and
/// lists the highlighted recipes too), 1 adds and 2 removes.
#[derive(Clone, Debug, PartialEq)]
pub struct RecipeBook {
    pub action: i32,
    /// Open and filtering per book: crafting, furnace, blast furnace, smoker.
    pub flags: [bool; 8],
    pub recipes: RecipeIds,
    pub highlight: Option<RecipeIds>,
}

impl RecipeBook {
    pub fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Self> {
        let action = r.varint()?;
        let mut flags = [false; 8];
        for flag in flags.iter_mut().take(book_flags(protocol)) {
            *flag = r.bool()?;
        }
        let recipes = RecipeIds::read(protocol, r)?;
        let highlight = if action == 0 {
            Some(RecipeIds::read(protocol, r)?)
        } else {
            None
        };
        Ok(Self {
            action,
            flags,
            recipes,
            highlight,
        })
    }

    pub fn write(&self, protocol: i32, w: &mut Writer) -> io::Result<()> {
        w.varint(self.action);
        for flag in self.flags.iter().take(book_flags(protocol)) {
            w.bool(*flag);
        }
        self.recipes.write(protocol, w)?;
        match (&self.highlight, self.action == 0) {
            (Some(highlight), true) => highlight.write(protocol, w),
            (None, false) => Ok(()),
            _ => Err(invalid(
                "recipe book highlights only come with initialisation",
            )),
        }
    }
}

/// One `recipe_book_add` entry (1.21.2 on).
#[derive(Clone, Debug, PartialEq)]
pub struct BookEntry {
    pub id: i32,
    pub display: RecipeDisplay,
    /// The group plus one, zero for none.
    pub group: i32,
    pub category: i32,
    pub requirements: Option<Vec<IdSet>>,
    /// 1: notification, 2: highlight.
    pub flags: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BookAdd {
    pub entries: Vec<BookEntry>,
    pub replace: bool,
}

impl BookAdd {
    pub fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Self> {
        let count = r.count()?;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let id = r.varint()?;
            let display = RecipeDisplay::read(protocol, r)?;
            let group = r.varint()?;
            let category = r.varint()?;
            let requirements = if r.bool()? {
                let count = r.count()?;
                Some(
                    (0..count)
                        .map(|_| IdSet::read(r))
                        .collect::<io::Result<_>>()?,
                )
            } else {
                None
            };
            entries.push(BookEntry {
                id,
                display,
                group,
                category,
                requirements,
                flags: r.u8()?,
            });
        }
        Ok(Self {
            entries,
            replace: r.bool()?,
        })
    }

    pub fn write(&self, protocol: i32, w: &mut Writer) -> io::Result<()> {
        w.varint(self.entries.len() as i32);
        for entry in &self.entries {
            w.varint(entry.id);
            entry.display.write(protocol, w)?;
            w.varint(entry.group).varint(entry.category);
            w.bool(entry.requirements.is_some());
            if let Some(requirements) = &entry.requirements {
                w.varint(requirements.len() as i32);
                for set in requirements {
                    set.write(w);
                }
            }
            w.u8(entry.flags);
        }
        w.bool(self.replace);
        Ok(())
    }
}

/// `place_ghost_recipe`: a numeric recipe in 1.12, a recipe name until
/// 1.21.2, then a display.
#[derive(Clone, Debug, PartialEq)]
pub enum GhostRecipe {
    Numeric(i32),
    Name(String),
    Display(Box<RecipeDisplay>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlaceGhostRecipe {
    pub container: i32,
    pub recipe: GhostRecipe,
}

impl PlaceGhostRecipe {
    pub fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Self> {
        let container = if displays(protocol) {
            r.varint()?
        } else {
            i32::from(r.i8()?)
        };
        let recipe = if displays(protocol) {
            GhostRecipe::Display(Box::new(RecipeDisplay::read(protocol, r)?))
        } else if version_flattened(protocol) {
            GhostRecipe::Name(r.string(IDENTIFIER)?.to_owned())
        } else {
            GhostRecipe::Numeric(r.varint()?)
        };
        Ok(Self { container, recipe })
    }

    pub fn write(&self, protocol: i32, w: &mut Writer) -> io::Result<()> {
        if displays(protocol) {
            w.varint(self.container);
        } else {
            w.i8(i8::try_from(self.container).map_err(|_| invalid("container ID out of range"))?);
        }
        match (
            &self.recipe,
            displays(protocol),
            version_flattened(protocol),
        ) {
            (GhostRecipe::Display(display), true, _) => display.write(protocol, w),
            (GhostRecipe::Name(name), false, true) => {
                w.string(name);
                Ok(())
            }
            (GhostRecipe::Numeric(id), false, false) => {
                w.varint(*id);
                Ok(())
            }
            _ => Err(invalid("ghost recipe does not fit the protocol")),
        }
    }
}

/// `recipe_book_remove` (1.21.2 on): display IDs.
pub fn read_book_remove(r: &mut Reader<'_>) -> io::Result<Vec<i32>> {
    let count = r.count()?;
    (0..count).map(|_| r.varint()).collect()
}

pub fn write_book_remove(w: &mut Writer, ids: &[i32]) {
    w.varint(ids.len() as i32);
    for id in ids {
        w.varint(*id);
    }
}

/// `recipe_book_settings` (1.21.2 on): open and filtering per book.
pub fn read_book_settings(r: &mut Reader<'_>) -> io::Result<[bool; 8]> {
    let mut flags = [false; 8];
    for flag in &mut flags {
        *flag = r.bool()?;
    }
    Ok(flags)
}

pub fn write_book_settings(w: &mut Writer, flags: &[bool; 8]) {
    for flag in flags {
        w.bool(*flag);
    }
}
