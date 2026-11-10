//! What changes inside a component between two protocols beyond its
//! format: item and entity type names in hover events, the item stacks
//! they show, and sprite atlases.
use super::{
    component::{Arg, Content, EntityInfo, HoverEvent, ItemInfo, ObjectInfo, primitive_text},
    legacy::Part,
    version as v,
};
use crate::{
    Direction,
    codec::{Shared, item},
    data,
    nbt::Nbt,
    steps,
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
        if from < v::FLATTENING && to >= v::FLATTENING {
            upgrade_translation(component);
        } else if from >= v::FLATTENING && to < v::FLATTENING {
            legacy_translation(component);
        }
    });
}

/// 1.13 renamed most translation keys; a client shows a key it lacks as
/// the key itself.
fn upgrade_translation(component: &mut super::Component) {
    if let Content::Translate { key, .. } = &mut component.content
        && let Some(renamed) = steps::v1_12_2_to_1_13::upgrade_translation_key(key)
    {
        *key = renamed.to_owned();
    }
}

/// The 1.12 key with the same English text, or else the 1.13 English text
/// as literal text with the arguments in place of its placeholders.
fn legacy_translation(component: &mut super::Component) {
    use steps::v1_12_2_to_1_13::{Legacy, legacy_translation};
    let Content::Translate { key, args, .. } = &mut component.content else {
        return;
    };
    let template = match legacy_translation(key) {
        None => return,
        Some(Legacy::Key(old)) => {
            *key = old.to_owned();
            return;
        }
        Some(Legacy::Text(template)) => template,
    };
    let args = std::mem::take(args);
    let mut parts = Vec::new();
    super::legacy::substitute_parts(template, args.len(), |part| match part {
        Part::Literal("") => {}
        Part::Literal(text) => parts.push(super::Component::text(text)),
        Part::Arg(index) => parts.push(match &args[index] {
            Arg::Component(arg) => (**arg).clone(),
            Arg::Primitive(value) => super::Component::text(primitive_text(value)),
        }),
    });
    component.content = Content::Text(String::new());
    parts.append(&mut component.extra);
    component.extra = parts;
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
