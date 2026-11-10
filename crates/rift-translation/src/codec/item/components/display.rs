//! Item structures outside a stack's patch that packets carry from 1.20.5 on:
//! slot and recipe displays (1.21.2+), trade costs with exact component
//! matches, component matchers and consume effects. Each `transcode_*`
//! function copies one value from `rw.r` to `rw.w`, converting it between
//! the two protocols of the step, so packet code stays layout-only.
use super::{
    convert::{self, Conv, Res, convert_value},
    layouts,
    schema::{self, Node, Value},
};
use crate::{
    Direction,
    codec::Shared,
    wire::{Rewriter, invalid},
};
use std::io;

/// `ItemCost.STREAM_CODEC`, the same in every protocol from 1.20.5 on:
/// item, count and the components the offered stack must have exactly.
pub static ITEM_COST: Node = Node::Struct(&[
    ("item", Node::Id("item")),
    ("count", Node::VarInt),
    ("components", Node::List(&Node::Typed, usize::MAX)),
]);

/// `DataComponentExactPredicate.STREAM_CODEC`: typed components.
pub static EXACT_PREDICATE: Node = Node::List(&Node::Typed, usize::MAX);

/// `SlotDisplay.STREAM_CODEC` of `protocol` (1.21.2+).
pub fn slot_display(protocol: i32) -> Option<&'static Node> {
    Some(match protocol {
        ..768 => return None,
        768..770 => &layouts::SLOT_DISPLAY_768,
        770..775 => &layouts::SLOT_DISPLAY_770,
        775..777 => &layouts::SLOT_DISPLAY_775,
        _ => &layouts::SLOT_DISPLAY_777,
    })
}

/// `RecipeDisplay.STREAM_CODEC` of `protocol` (1.21.2+).
pub fn recipe_display(protocol: i32) -> Option<&'static Node> {
    Some(match protocol {
        ..768 => return None,
        768..770 => &layouts::RECIPE_DISPLAY_768,
        770..775 => &layouts::RECIPE_DISPLAY_770,
        775..777 => &layouts::RECIPE_DISPLAY_775,
        _ => &layouts::RECIPE_DISPLAY_777,
    })
}

/// `DataComponentMatchers.STREAM_CODEC` of `protocol` (1.21.5+), as block
/// and item predicates carry it.
pub fn matchers(protocol: i32) -> Option<&'static Node> {
    Some(match protocol {
        ..770 => return None,
        770..774 => &layouts::MATCHERS_770,
        _ => &layouts::MATCHERS_774,
    })
}

/// `ConsumeEffect.STREAM_CODEC` of `protocol` (1.21.2+).
pub fn consume_effect(protocol: i32) -> Option<&'static Node> {
    Some(match protocol {
        ..768 => return None,
        768..777 => &layouts::CONSUME_EFFECT_768,
        _ => &layouts::CONSUME_EFFECT_777,
    })
}

fn layouts_for(
    shared: &Shared,
    direction: Direction,
    layout: fn(i32) -> Option<&'static Node>,
    what: &str,
) -> io::Result<(&'static Node, &'static Node)> {
    let (source, target) = (shared.source(direction), shared.target(direction));
    match (layout(source), layout(target)) {
        (Some(source), Some(target)) => Ok((source, target)),
        _ => Err(invalid(&format!(
            "{what} needs a newer protocol than {source} or {target}"
        ))),
    }
}

/// Copy one value of `source` (sender's layout) to `target` (receiver's),
/// converting it. `Ok(false)` means the receiver cannot express it at all:
/// nothing was written and the caller drops what contains it.
pub fn transcode_node(
    shared: &mut Shared,
    direction: Direction,
    source: &'static Node,
    target: &'static Node,
    rw: &mut Rewriter<'_>,
) -> io::Result<bool> {
    let protocol = shared.source(direction);
    let value = schema::read(protocol, source, &mut rw.r, 0)?;
    let converted = convert::with(shared, direction, |cx| {
        convert_value(cx, source, target, &value)
    })?;
    let target_protocol = shared.target(direction);
    match converted {
        Some(value) => {
            schema::write(target_protocol, target, &value, &mut rw.w)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// A slot display; one the receiver cannot show becomes `empty`.
pub fn transcode_slot_display(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
) -> io::Result<()> {
    let (source, target) = layouts_for(shared, direction, slot_display, "a slot display")?;
    transcode_node(shared, direction, source, target, rw).map(drop)
}

/// A recipe display; `Ok(false)` when the receiver has no such display
/// type (nothing written: drop the recipe).
pub fn transcode_recipe_display(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
) -> io::Result<bool> {
    let (source, target) = layouts_for(shared, direction, recipe_display, "a recipe display")?;
    transcode_node(shared, direction, source, target, rw)
}

/// A trade's item cost; `Ok(false)` when the receiver lacks the item
/// (nothing written: drop the offer). Exact components the receiver cannot
/// express are left out, which only makes the cost easier to meet.
pub fn transcode_item_cost(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
) -> io::Result<bool> {
    transcode_node(shared, direction, &ITEM_COST, &ITEM_COST, rw)
}

/// An exact component predicate (`DataComponentExactPredicate`).
pub fn transcode_exact_predicate(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
) -> io::Result<()> {
    transcode_node(shared, direction, &EXACT_PREDICATE, &EXACT_PREDICATE, rw).map(drop)
}

/// Component matchers (exact components and partial predicates, 1.21.5+).
pub fn transcode_matchers(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
) -> io::Result<()> {
    let (source, target) = layouts_for(shared, direction, matchers, "component matchers")?;
    transcode_node(shared, direction, source, target, rw).map(drop)
}

/// One consume effect; `Ok(false)` when the receiver lacks its type.
pub fn transcode_consume_effect(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
) -> io::Result<bool> {
    let (source, target) = layouts_for(shared, direction, consume_effect, "a consume effect")?;
    if !std::ptr::eq(source, target) {
        // 26.3 changed the random teleport; convert through the rule that
        // knows it.
        let protocol = shared.source(direction);
        let value = schema::read(protocol, source, &mut rw.r, 0)?;
        let converted = convert::with(shared, direction, |cx| one_effect(cx, &value))?;
        return match converted {
            Some(value) => {
                schema::write(shared.target(direction), target, &value, &mut rw.w)?;
                Ok(true)
            }
            None => Ok(false),
        };
    }
    transcode_node(shared, direction, source, target, rw)
}

fn one_effect(cx: &mut Conv<'_>, value: &Value) -> Res<Value> {
    let converted = super::rules::consume_effects_777(cx, &Value::List(vec![value.clone()]))?;
    converted
        .into_items()?
        .into_iter()
        .next()
        .ok_or(convert::Fail::Lost)
}
