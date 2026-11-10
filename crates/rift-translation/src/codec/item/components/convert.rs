//! Conversion of component patches between two adjacent protocols.
//!
//! A patch goes through three passes: the pair's explicit rules ([`rules`])
//! take the components that were split, merged, renamed or restructured;
//! every other component keeps its name (through the
//! `data_component_type` mapping) and is converted node by node by
//! [`convert_value`], which remaps registry IDs and enum values and applies
//! the coercions vanilla made between equivalent encodings (an either-holder
//! and a holder, a tag key and a holder set, a stack and a template); what
//! the receiver cannot express is dropped and stashed ([`super::stash`]).
use super::{
    Component, Components,
    layouts::{FIRST, Layout},
    rules,
    schema::{self, Node, Value},
    stash,
};
use crate::{
    Direction,
    codec::{Shared, item::ItemStack},
    data,
    nbt::Nbt,
    wire::{MAX_DEPTH, invalid},
};
use std::io;

/// Why a value could not be converted.
#[derive(Debug)]
pub enum Fail {
    /// The receiver cannot express it: drop the containing component.
    Lost,
    Error(io::Error),
}

impl From<io::Error> for Fail {
    fn from(error: io::Error) -> Self {
        Fail::Error(error)
    }
}

pub type Res<T> = Result<T, Fail>;

/// One conversion between the two protocols of a step.
pub struct Conv<'a> {
    pub shared: &'a mut Shared,
    pub direction: Direction,
    /// The sender's protocol.
    pub source: i32,
    /// The receiver's protocol.
    pub target: i32,
    pub depth: usize,
    /// Set when a conversion dropped part of a value (a list element, an
    /// optional value, an enum value, a holder set entry): the original is
    /// then stashed so it can come back.
    pub lossy: bool,
    /// Component types a tooltip display hides, for rules that turn it into
    /// per-component flags.
    pub hidden: Vec<&'static str>,
}

impl Conv<'_> {
    /// Whether the receiver is the newer protocol.
    pub fn up(&self) -> bool {
        self.target > self.source
    }

    /// The older protocol of the pair.
    pub fn older(&self) -> i32 {
        self.source.min(self.target)
    }

    /// A registry ID from the sender's numbering to the receiver's.
    pub fn map_id(&mut self, registry: &'static str, id: i32) -> Option<i32> {
        let (source, target) = (data::version(self.source), data::version(self.target));
        match (source.registry(registry), target.registry(registry)) {
            (Some(_), Some(_)) => self.shared.mapping(self.direction, registry).get(id),
            _ => synced(self, registry, id),
        }
    }

    /// The registry ID of the receiver's entry `name`, for values that
    /// name an entry instead of numbering it.
    pub fn target_id(&mut self, registry: &'static str, name: &str) -> Option<i32> {
        let name = data::strip(name);
        match data::version(self.target).registry(registry) {
            Some(registry) => registry.id(name),
            None => synced_by_name(self, registry, name),
        }
    }

    /// The receiver's name for the sender's component type `kind`.
    pub fn target_kind(&mut self, kind: &'static str) -> Option<&'static str> {
        let id = Layout::of(self.source).ok()?.id(kind)?;
        let id = self
            .shared
            .mapping(self.direction, "data_component_type")
            .get(id)?;
        Some(Layout::of(self.target).ok()?.get(id)?.0)
    }

    /// The layout of `kind` on the sending and the receiving side.
    pub fn layouts(&self, kind: &str) -> (Option<&'static Node>, Option<&'static Node>) {
        let find = |protocol| {
            Layout::of(protocol)
                .ok()
                .and_then(|layout| layout.find(kind))
                .map(|(_, node)| node)
        };
        (find(self.source), find(self.target))
    }

    /// Convert a text component between the two protocols.
    pub fn text(&mut self, bytes: &[u8]) -> io::Result<Vec<u8>> {
        #[cfg(test)]
        if tests::TEXT_PASSTHROUGH.with(|flag| flag.get()) {
            return Ok(bytes.to_vec());
        }
        let mut tag = Nbt::from_bytes(bytes, false)?.ok_or_else(|| invalid("missing text"))?;
        crate::codec::text::convert_nbt(self.shared, self.direction, &mut tag)?;
        Ok(tag.to_bytes(false))
    }

    /// A sound event holder for the receiver's built-in sound `name`, or an
    /// inline one when it has no such sound.
    pub fn sound(&mut self, name: &str) -> Value {
        match self.target_id("sound_event", name) {
            Some(id) => Value::Id(id),
            None => Value::Direct(Box::new(Value::Struct(vec![
                Value::String(format!("minecraft:{name}")),
                Value::none(),
            ]))),
        }
    }
}

/// Map an ID of a registry that at least one side synchronises from the
/// server instead of building it in.
// INTEGRATION: the registries codec should expose, per connection, the entry
// names of every synchronised registry on both sides of the step (from the
// `registry_data` it sees, or the known-pack defaults) and map by name, as
// the built-in mapping does. Until it does, such IDs pass unchanged.
fn synced(cx: &mut Conv<'_>, registry: &'static str, id: i32) -> Option<i32> {
    let _ = (cx, registry);
    Some(id)
}

/// The receiver's ID of the synchronised registry entry `name`.
// INTEGRATION: as for `synced`; until the registries codec exposes the
// receiver's synchronised entries, a value that names an entry the receiver
// numbers cannot be converted and its component is dropped.
fn synced_by_name(cx: &mut Conv<'_>, registry: &'static str, name: &str) -> Option<i32> {
    let _ = (cx, registry, name);
    None
}

/// The sender's name of a synchronised registry entry.
// INTEGRATION: as for `synced`; until the registries codec exposes the
// sender's synchronised entries, such names are unknown.
fn synced_name(cx: &mut Conv<'_>, registry: &'static str, id: i32) -> Option<String> {
    let _ = (cx, registry, id);
    None
}

/// Whether values of `source` can be carried over to `target` by
/// [`convert_value`] without an explicit rule.
pub fn compatible(source: &Node, target: &Node) -> bool {
    schema::compatible(source, target)
}

/// Convert `value` from the sender's `source` layout to the receiver's
/// `target` layout: registry IDs, enum values by name, text components,
/// nested stacks and components, and the coercions of [`compatible`].
pub fn convert_value(cx: &mut Conv<'_>, source: &Node, target: &Node, value: &Value) -> Res<Value> {
    use Node::*;
    if cx.depth > MAX_DEPTH {
        return Err(invalid("data component nesting limit exceeded").into());
    }
    cx.depth += 1;
    let result = convert_inner(cx, source, target, value);
    cx.depth -= 1;
    return result;

    fn convert_inner(cx: &mut Conv<'_>, source: &Node, target: &Node, value: &Value) -> Res<Value> {
        let mismatch = || Fail::Error(invalid(&format!("cannot convert {source:?} to {target:?}")));
        Ok(match (source, target, value) {
            (Ref(x), _, _) => convert_value(cx, x, target, value)?,
            (_, Ref(y), _) => convert_value(cx, source, y, value)?,
            (Text, Text, Value::Nbt(bytes)) => Value::Nbt(cx.text(bytes)?),
            (Id(registry), Id(_), Value::Id(id))
            | (Holder(registry, _), Holder(_, _), Value::Id(id)) => {
                Value::Id(cx.map_id(registry, *id).ok_or(Fail::Lost)?)
            }
            (Holder(_, x), Holder(_, y), Value::Direct(inner)) => {
                Value::Direct(Box::new(convert_value(cx, x, y, inner)?))
            }
            (HolderSet(registry), HolderSet(_), Value::Ids(ids)) => {
                let mut mapped = Vec::with_capacity(ids.len());
                for id in ids {
                    match cx.map_id(registry, *id) {
                        Some(id) => mapped.push(id),
                        None => cx.lossy = true,
                    }
                }
                Value::Ids(mapped)
            }
            (HolderSet(_), HolderSet(_), Value::Tag(tag)) => Value::Tag(tag.clone()),
            (Enum(_), Enum(names), Value::Enum(name)) => {
                // A value the receiver lacks becomes its first, neutral one.
                Value::Enum(
                    names
                        .iter()
                        .find(|known| *known == name)
                        .unwrap_or_else(|| {
                            cx.lossy = true;
                            &names[0]
                        }),
                )
            }
            (Optional(x), Optional(y), Value::Optional(inner)) => Value::Optional(match inner {
                // An optional value the receiver cannot express is absent.
                Some(inner) => match convert_value(cx, x, y, inner) {
                    Ok(value) => Some(Box::new(value)),
                    Err(Fail::Lost) => {
                        cx.lossy = true;
                        None
                    }
                    Err(error) => return Err(error),
                },
                None => None,
            }),
            (List(x, _), List(y, max), Value::List(items)) => {
                // Elements the receiver cannot express are left out.
                let mut converted = Vec::with_capacity(items.len());
                for item in items {
                    match convert_value(cx, x, y, item) {
                        Ok(value) => converted.push(value),
                        Err(Fail::Lost) => cx.lossy = true,
                        Err(error) => return Err(error),
                    }
                }
                if converted.len() > *max {
                    cx.lossy = true;
                    converted.truncate(*max);
                }
                Value::List(converted)
            }
            (Array(x, _), Array(y, _), Value::List(items)) => Value::List(
                items
                    .iter()
                    .map(|item| convert_value(cx, x, y, item))
                    .collect::<Res<_>>()?,
            ),
            (Struct(a), Struct(b), Value::Struct(values)) if a.len() == b.len() => Value::Struct(
                a.iter()
                    .zip(*b)
                    .zip(values)
                    .map(|(((_, x), (_, y)), value)| convert_value(cx, x, y, value))
                    .collect::<Res<_>>()?,
            ),
            (Either(l1, r1), Either(l2, r2), Value::Either(left, inner)) => {
                let (x, y) = if *left { (l1, l2) } else { (r1, r2) };
                Value::Either(*left, Box::new(convert_value(cx, x, y, inner)?))
            }
            // 26.3's tag display may list items, which a tag name cannot;
            // a composite of item displays shows the same.
            (
                Dispatch("slot_display", _),
                Dispatch("slot_display", ys),
                Value::Variant("tag", inner),
            ) if matches!(**inner, Value::Ids(_))
                && ys
                    .iter()
                    .any(|(name, node)| *name == "tag" && matches!(node, Identifier)) =>
            {
                let Value::Ids(ids) = &**inner else {
                    unreachable!("matched above");
                };
                // The same items, but not the same value coming back.
                cx.lossy = true;
                let mut items = Vec::with_capacity(ids.len());
                for id in ids {
                    match cx.map_id("item", *id) {
                        Some(id) => items.push(Value::Variant("item", Box::new(Value::Id(id)))),
                        None => cx.lossy = true,
                    }
                }
                Value::Variant("composite", Box::new(Value::List(items)))
            }
            (Dispatch(_, xs), Dispatch(_, ys), Value::Variant(name, inner))
            | (Union(xs), Union(ys), Value::Variant(name, inner)) => {
                let (_, x) = xs
                    .iter()
                    .find(|(known, _)| known == name)
                    .ok_or_else(mismatch)?;
                let converted = match ys.iter().find(|(known, _)| known == name) {
                    Some((name, y)) => convert_value(cx, x, y, inner)
                        .map(|inner| Value::Variant(name, Box::new(inner))),
                    None => Err(Fail::Lost),
                };
                match (converted, target) {
                    // A slot display the receiver cannot show shows nothing.
                    (Err(Fail::Lost), Dispatch("slot_display", _)) => {
                        cx.lossy = true;
                        Value::Variant("empty", Box::new(Value::Unit))
                    }
                    (converted, _) => converted?,
                }
            }
            (Stack | Template, Stack, Value::Stack(stack)) => {
                let converted = convert_stack(cx, stack.as_deref())?;
                if converted.is_none() && stack.is_some() {
                    cx.lossy = true;
                }
                Value::Stack(converted.map(Box::new))
            }
            (Stack | Template, Template, Value::Stack(stack)) => {
                let stack = convert_stack(cx, stack.as_deref())?.ok_or(Fail::Lost)?;
                Value::Stack(Some(Box::new(stack)))
            }
            (Kind, Kind, Value::Kind(kind)) => Value::Kind(cx.target_kind(kind).ok_or(Fail::Lost)?),
            (Typed, Typed, Value::Typed(component)) => Value::Typed(Box::new(
                convert_component(cx, component)?.ok_or(Fail::Lost)?,
            )),
            // Coercions.
            (Either(x, _), y, Value::Either(true, inner)) if !matches!(y, Either(..)) => {
                convert_value(cx, x, y, inner)?
            }
            (Either(_, _), y, Value::Either(false, key)) if !matches!(y, Either(..)) => {
                // An entry key becomes the receiver's ID of that entry; any
                // other right side has no equivalent.
                let (Some(registry), Some(name)) = (registry_of(y), key.as_str()) else {
                    return Err(Fail::Lost);
                };
                Value::Id(cx.target_id(registry, name).ok_or(Fail::Lost)?)
            }
            (x, Either(y, _), inner) if !matches!(x, Either(..)) => {
                Value::Either(true, Box::new(convert_value(cx, x, y, inner)?))
            }
            (Dispatch("slot_display", _), Holder("trim_pattern", _), display) => {
                display_to_pattern(cx, display)?
            }
            (Holder("trim_pattern", _), Dispatch("slot_display", _), pattern) => {
                pattern_to_display(cx, pattern)?
            }
            (Identifier, HolderSet(_), Value::String(tag)) => Value::Tag(tag.clone()),
            (HolderSet(_), Identifier, Value::Tag(tag)) => Value::String(tag.clone()),
            (HolderSet(_), Identifier, Value::Ids(_)) => return Err(Fail::Lost),
            (Stack, Optional(y), Value::Stack(stack)) => match stack {
                Some(_) => Value::some(convert_value(cx, &Stack, y, value)?),
                None => Value::none(),
            },
            (Optional(x), Stack, Value::Optional(inner)) => match inner {
                Some(inner) => convert_value(cx, x, &Stack, inner)?,
                None => Value::Stack(None),
            },
            (_, _, _) if leaf(source) && leaf(target) && same_leaf(source, target) => value.clone(),
            _ => return Err(mismatch()),
        })
    }
}

/// Vanilla names each trim pattern's smithing template after it.
const TEMPLATE_SUFFIX: &str = "_armor_trim_smithing_template";

/// A smithing trim display's pattern, from the template item it showed
/// before 1.21.5 to the pattern it names since.
fn display_to_pattern(cx: &mut Conv<'_>, display: &Value) -> Res<Value> {
    let Value::Variant("item", item) = display else {
        return Err(Fail::Lost);
    };
    let id = item.as_int().ok_or(Fail::Lost)?;
    let name = data::version(cx.source)
        .registry("item")
        .and_then(|registry| registry.name(id))
        .and_then(|name| name.strip_suffix(TEMPLATE_SUFFIX))
        .ok_or(Fail::Lost)?
        .to_owned();
    Ok(Value::Id(
        cx.target_id("trim_pattern", &name).ok_or(Fail::Lost)?,
    ))
}

/// The reverse of [`display_to_pattern`].
fn pattern_to_display(cx: &mut Conv<'_>, pattern: &Value) -> Res<Value> {
    let name = match pattern {
        Value::Id(id) => synced_name(cx, "trim_pattern", *id).ok_or(Fail::Lost)?,
        Value::Direct(inner) => {
            let asset = inner
                .fields()?
                .first()
                .and_then(Value::as_str)
                .ok_or(Fail::Lost)?;
            data::strip(asset).to_owned()
        }
        _ => return Err(Fail::Lost),
    };
    let item = cx
        .target_id("item", &format!("{name}{TEMPLATE_SUFFIX}"))
        .ok_or(Fail::Lost)?;
    Ok(Value::Variant("item", Box::new(Value::Id(item))))
}

/// The registry a holder or ID node refers to.
fn registry_of(node: &Node) -> Option<&'static str> {
    match node {
        Node::Id(registry) | Node::Holder(registry, _) => Some(registry),
        Node::Ref(inner) => registry_of(inner),
        _ => None,
    }
}

fn leaf(node: &Node) -> bool {
    use Node::*;
    matches!(
        node,
        Unit | Bool
            | Byte
            | VarInt
            | VarLong
            | Int
            | Long
            | Float
            | Double
            | Str(_)
            | Identifier
            | Uuid
            | Position
            | Tag
            | Compound
    )
}

fn same_leaf(source: &Node, target: &Node) -> bool {
    use Node::*;
    match (source, target) {
        (Str(_) | Identifier, Str(_) | Identifier) => true,
        _ => std::mem::discriminant(source) == std::mem::discriminant(target),
    }
}

/// Convert a nested stack; `None` when the receiver has no such item.
pub fn convert_stack(cx: &mut Conv<'_>, stack: Option<&ItemStack>) -> Res<Option<ItemStack>> {
    let Some(stack) = stack else {
        return Ok(None);
    };
    let Some(id) = cx.map_id("item", stack.id) else {
        return Ok(None);
    };
    let mut components = stack.components.clone().unwrap_or_default();
    patch(cx.shared, cx.direction, id, &mut components, cx.depth + 1)?;
    Ok(Some(ItemStack {
        id,
        count: stack.count,
        damage: 0,
        tag: None,
        components: Some(components),
    }))
}

/// Convert one component with no explicit rule; `None` when the receiver
/// lacks its type or cannot express its value.
pub fn convert_component(cx: &mut Conv<'_>, component: &Component) -> Res<Option<Component>> {
    let Some(kind) = cx.target_kind(component.kind) else {
        return Ok(None);
    };
    let (Some(source), Some(target)) = (cx.layouts(component.kind).0, cx.layouts(kind).1) else {
        return Ok(None);
    };
    match convert_value(cx, source, target, &component.value) {
        Ok(value) => Ok(Some(Component { kind, value })),
        Err(Fail::Lost) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The patch being built in the receiver's terms, and the sender's
/// components it could not express.
#[derive(Default)]
pub struct Out {
    pub components: Components,
    /// Sender's components to stash, with their values in its layout.
    pub lost: Vec<Component>,
    /// Sender's removed types the receiver lacks.
    pub lost_removed: Vec<&'static str>,
    /// Sender's types a rule's conversion back would set or remove although
    /// the sender's patch does neither.
    pub absent: Vec<&'static str>,
}

impl Out {
    pub fn push(&mut self, kind: &'static str, value: Value) {
        self.components.insert(kind, value);
    }
}

/// Convert `components` in place between the two protocols of the step.
pub fn patch(
    shared: &mut Shared,
    direction: Direction,
    item: i32,
    components: &mut Components,
    depth: usize,
) -> io::Result<()> {
    let (source, target) = (shared.source(direction), shared.target(direction));
    if source < FIRST || target < FIRST {
        return Err(invalid(
            "data components need 1.20.5 or newer on both sides",
        ));
    }
    if source == target {
        return Ok(());
    }
    if depth > MAX_DEPTH {
        return Err(invalid("data component nesting limit exceeded"));
    }
    let mut cx = Conv {
        shared,
        direction,
        source,
        target,
        depth,
        lossy: false,
        hidden: Vec::new(),
    };
    let restored = stash::take(&mut cx, std::mem::take(components))?;
    let mut input = restored.rest;
    let mut out = Out::default();
    rules::before(&mut cx, item, &mut input, &mut out)?;
    for component in std::mem::take(&mut input.added) {
        // The stash brings back the exact value of this one.
        let restoring = cx
            .target_kind(component.kind)
            .is_some_and(|kind| restored.components.iter().any(|known| known.kind == kind));
        if restoring {
            continue;
        }
        cx.lossy = false;
        let converted = match rules::component(&mut cx, item, &component, &mut out)? {
            true => None,
            false => match convert_component(&mut cx, &component) {
                Ok(Some(converted)) => Some(converted),
                Ok(None) | Err(Fail::Lost) => {
                    out.lost.push(component);
                    continue;
                }
                Err(Fail::Error(error)) => return Err(error),
            },
        };
        if let Some(converted) = converted {
            out.components.added.push(converted);
        }
        if cx.lossy {
            out.lost.push(component);
        }
    }
    for kind in std::mem::take(&mut input.removed) {
        if rules::removed(&mut cx, kind, &mut out) {
            continue;
        }
        match cx.target_kind(kind) {
            Some(kind) => {
                if !out.components.removed.contains(&kind) && out.components.get(kind).is_none() {
                    out.components.removed.push(kind);
                }
            }
            None => out.lost_removed.push(kind),
        }
    }
    for component in restored.components {
        out.components.insert(component.kind, component.value);
    }
    for kind in restored.absent {
        out.components.take(kind);
        out.components.removed.retain(|known| *known != kind);
    }
    for kind in restored.removed {
        out.components.take(kind);
        if !out.components.removed.contains(&kind) {
            out.components.removed.push(kind);
        }
    }
    stash::put(&mut cx, &mut out)?;
    *components = out.components;
    Ok(())
}

/// Run `f` with a conversion between the two protocols of the step, for
/// structures outside a component patch. `None` when `f` found something
/// the receiver cannot express at all.
pub fn with<T>(
    shared: &mut Shared,
    direction: Direction,
    f: impl FnOnce(&mut Conv<'_>) -> Res<T>,
) -> io::Result<Option<T>> {
    let (source, target) = (shared.source(direction), shared.target(direction));
    if source < FIRST || target < FIRST {
        return Err(invalid(
            "data components need 1.20.5 or newer on both sides",
        ));
    }
    let mut cx = Conv {
        shared,
        direction,
        source,
        target,
        depth: 0,
        lossy: false,
        hidden: Vec::new(),
    };
    match f(&mut cx) {
        Ok(value) => Ok(Some(value)),
        Err(Fail::Lost) => Ok(None),
        Err(Fail::Error(error)) => Err(error),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::cell::Cell;

    thread_local! {
        /// The text codec is not implemented yet; tests that convert named
        /// stacks pass text through unchanged instead.
        pub static TEXT_PASSTHROUGH: Cell<bool> = const { Cell::new(false) };
    }
}
