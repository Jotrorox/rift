//! What changes inside a component between two protocols beyond its
//! format: item and entity type names in hover events, the item stacks
//! they show, and sprite atlases.
use super::{
    component::{Content, EntityInfo, HoverEvent, ItemInfo, ObjectInfo},
    version as v,
};
use crate::{
    Direction,
    codec::{Shared, item},
    data,
    nbt::Nbt,
};

/// Convert what `component` refers to from the sender's protocol to the
/// receiver's. Hover events whose item or entity the receiver lacks (and
/// has no stand-in for) are dropped: newer clients reject unknown IDs.
pub fn adapt(shared: &mut Shared, direction: Direction, component: &mut super::Component) {
    let (from, to) = (shared.source(direction), shared.target(direction));
    component.walk_mut(&mut |component| {
        let keep = match &mut component.style.hover {
            Some(HoverEvent::ShowItem(item)) => convert_item(shared, direction, item),
            Some(HoverEvent::ShowEntity(EntityInfo { kind, .. })) => {
                match map_name(shared, direction, "entity_type", kind) {
                    Some(name) => {
                        *kind = name;
                        true
                    }
                    None => false,
                }
            }
            _ => true,
        };
        if !keep {
            component.style.hover = None;
        }
        if let Content::Object {
            object: ObjectInfo::Atlas { atlas, sprite },
            ..
        } = &mut component.content
        {
            adapt_atlas(from, to, atlas, sprite);
        }
    });
}

/// 1.21.11 moved item sprites from the block atlas into their own.
fn adapt_atlas(from: i32, to: i32, atlas: &mut Option<String>, sprite: &str) {
    let blocks = atlas
        .as_deref()
        .is_none_or(|atlas| data::strip(atlas) == "blocks");
    if from < v::ITEMS_ATLAS && to >= v::ITEMS_ATLAS && blocks {
        let path = data::strip(sprite);
        if path.starts_with("item/") || path.starts_with("trims/items/") {
            *atlas = Some("minecraft:items".to_owned());
        }
    } else if from >= v::ITEMS_ATLAS
        && to < v::ITEMS_ATLAS
        && atlas
            .as_deref()
            .is_some_and(|atlas| data::strip(atlas) == "items")
    {
        *atlas = None;
    }
}

/// The receiver's name for the sender's `name` in `registry`, through the
/// step's ID mapping (with its stand-ins). A name the sender's registry
/// does not know is kept when the receiver knows it; other namespaces are
/// kept as they are.
pub fn map_name(
    shared: &mut Shared,
    direction: Direction,
    registry: &'static str,
    name: &str,
) -> Option<String> {
    let namespaced = |name: &str| format!("minecraft:{name}");
    let path = match name.split_once(':') {
        Some(("minecraft", path)) => path,
        Some(_) => return Some(name.to_owned()),
        None => name,
    };
    let (source, target) = (shared.source_data(direction), shared.target_data(direction));
    let (Some(source_registry), Some(target_registry)) =
        (source.registry(registry), target.registry(registry))
    else {
        return Some(name.to_owned());
    };
    let Some(id) = source_registry.id(path) else {
        return target_registry.id(path).map(|_| namespaced(path));
    };
    let mapped = shared.mapping(direction, registry).get(id)?;
    target_registry.name(mapped).map(namespaced)
}

/// Convert the stack a `show_item` event shows; `false` drops the event.
fn convert_item(shared: &mut Shared, direction: Direction, item: &mut ItemInfo) -> bool {
    let (from, to) = (shared.source(direction), shared.target(direction));
    if from < v::ITEM_COMPONENTS
        && to < v::ITEM_COMPONENTS
        && through_item_codec(shared, direction, item)
    {
        return true;
    }
    if convert_components(shared, direction, item) {
        return true;
    }
    // Degrade: the receiver gets the item and count; data it cannot be
    // trusted to read is left out.
    let Some(id) = map_name(shared, direction, "item", &item.id) else {
        return false;
    };
    item.id = id;
    if (from < v::ITEM_COMPONENTS) != (to < v::ITEM_COMPONENTS) {
        item.tag = None;
    }
    item.components = None;
    if to >= v::FLATTENING {
        item.damage = None;
    }
    true
}

/// Convert a tag-era stack with the item codec, as inventory stacks are.
/// `false` when the item codec cannot (yet) convert it.
fn through_item_codec(shared: &mut Shared, direction: Direction, info: &mut ItemInfo) -> bool {
    let source = shared.source_data(direction);
    let target = shared.target_data(direction);
    let path = data::strip(&info.id);
    let Some(id) = source
        .registry("item")
        .and_then(|registry| registry.id(path))
    else {
        return false;
    };
    let mut stack = Some(item::ItemStack {
        id,
        count: info.count,
        damage: info.damage.unwrap_or(0),
        tag: info.tag.clone().map(Nbt::Compound),
        components: None,
    });
    if item::convert(shared, direction, &mut stack).is_err() {
        return false;
    }
    let Some(stack) = stack else {
        return false;
    };
    let Some(name) = target
        .registry("item")
        .and_then(|registry| registry.name(stack.id))
    else {
        return false;
    };
    info.id = format!("minecraft:{name}");
    info.count = stack.count;
    info.damage = (shared.target(direction) < v::FLATTENING).then_some(stack.damage);
    info.tag = match stack.tag {
        Some(Nbt::Compound(tag)) => Some(tag),
        _ => None,
    };
    true
}

// INTEGRATION: the item codec should provide a conversion of a stack in its
// NBT/codec form (item ID, count, and the NBT form of its data component
// patch; across 1.20.5 the old `tag` to components and back), for example
// `item::convert_nbt_stack(shared, direction, &mut id, &mut count, &mut tag,
// &mut components) -> io::Result<()>`. Until then `show_item` events that
// cross a protocol pair needing a rewrite keep only their item and count
// from 1.20.5 on.
fn convert_components(_shared: &mut Shared, _direction: Direction, _item: &mut ItemInfo) -> bool {
    false
}
