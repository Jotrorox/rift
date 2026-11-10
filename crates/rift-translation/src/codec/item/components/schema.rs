//! A small tree of typed nodes that describes a data component value's wire
//! layout, and the one engine that reads, writes and walks values by it.
//!
//! Every layout in [`super::layouts`] is such a tree, so reading, writing,
//! registry ID remapping and conversion all follow the same description; a
//! new protocol only adds table rows.
use super::{Component, Components, layouts};
use crate::{
    codec::item::ItemStack,
    data,
    nbt::Nbt,
    wire::{MAX_DEPTH, Reader, Writer, invalid},
};
use std::io;

/// One node of a layout. Registry names are those of `data::VersionData`
/// (without the `minecraft:` namespace).
pub enum Node {
    /// `StreamCodec.unit`: nothing on the wire.
    Unit,
    Bool,
    Byte,
    VarInt,
    VarLong,
    Int,
    Long,
    Float,
    Double,
    /// A UTF-8 string of at most this many UTF-16 units.
    Str(usize),
    /// `Identifier.STREAM_CODEC`, also used for tag and resource keys.
    Identifier,
    Uuid,
    /// `BlockPos.STREAM_CODEC`: a packed long.
    Position,
    /// Any NBT tag but TAG_End: `ByteBufCodecs.TAG` and every component
    /// without a stream codec of its own, which vanilla sends through its
    /// persistent codec as NBT.
    Tag,
    /// `ByteBufCodecs.COMPOUND_TAG`.
    Compound,
    /// `ComponentSerialization.STREAM_CODEC`: a text component as NBT.
    Text,
    /// `ByteBufCodecs.registry`/`holderRegistry`: a plain registry ID.
    Id(&'static str),
    /// `ByteBufCodecs.holder`: registry ID + 1, or 0 and an inline value.
    Holder(&'static str, &'static Node),
    /// `ByteBufCodecs.holderSet`: 0 and a tag name, or count + 1 plain IDs.
    HolderSet(&'static str),
    /// `ByteBufCodecs.idMapper` over an enum whose values change between
    /// protocols; converted by name. Stable enums are plain `VarInt`s.
    Enum(&'static [&'static str]),
    /// `ByteBufCodecs.optional`: a boolean, then the value when true.
    Optional(&'static Node),
    /// A VarInt count (at most the given number), then the elements.
    List(&'static Node, usize),
    /// `ByteBufCodecs.fixedSizeList`: exactly this many elements, no count.
    Array(&'static Node, usize),
    /// Fields in wire order, named after Mojang's record components.
    Struct(&'static [(&'static str, Node)]),
    /// `ByteBufCodecs.either`: a boolean, then the left value when true.
    Either(&'static Node, &'static Node),
    /// A registry ID selecting one of these layouts by entry name.
    Dispatch(&'static str, &'static [(&'static str, Node)]),
    /// A VarInt index selecting one of these layouts (an `idMapper` enum
    /// dispatch); converted by name like `Dispatch`.
    Union(&'static [(&'static str, Node)]),
    /// `ItemStack.OPTIONAL_STREAM_CODEC`: count, then item and patch when
    /// the count is positive.
    Stack,
    /// `ItemStackTemplate.STREAM_CODEC` (26.1+): item, count and patch.
    Template,
    /// `DataComponentType.STREAM_CODEC`: a component type ID.
    Kind,
    /// Another node, by reference: for recursive layouts. No wire effect.
    Ref(&'static Node),
    /// `TypedDataComponent.STREAM_CODEC`: a component type and its value.
    Typed,
}

/// Layouts are recursive through [`Node::Ref`], which prints as `Ref`.
impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use Node::*;
        match self {
            Str(max) => write!(f, "Str({max})"),
            Id(registry) => write!(f, "Id({registry})"),
            Holder(registry, inline) => write!(f, "Holder({registry}, {inline:?})"),
            HolderSet(registry) => write!(f, "HolderSet({registry})"),
            Enum(names) => write!(f, "Enum({names:?})"),
            Optional(inner) => write!(f, "Optional({inner:?})"),
            List(element, _) => write!(f, "List({element:?})"),
            Array(element, count) => write!(f, "Array({element:?}, {count})"),
            Struct(fields) => {
                let names: Vec<_> = fields.iter().map(|(name, _)| *name).collect();
                write!(f, "Struct({names:?})")
            }
            Either(left, right) => write!(f, "Either({left:?}, {right:?})"),
            Dispatch(registry, _) => write!(f, "Dispatch({registry})"),
            Union(variants) => {
                let names: Vec<_> = variants.iter().map(|(name, _)| *name).collect();
                write!(f, "Union({names:?})")
            }
            Ref(_) => write!(f, "Ref"),
            Unit => write!(f, "Unit"),
            Bool => write!(f, "Bool"),
            Byte => write!(f, "Byte"),
            VarInt => write!(f, "VarInt"),
            VarLong => write!(f, "VarLong"),
            Int => write!(f, "Int"),
            Long => write!(f, "Long"),
            Float => write!(f, "Float"),
            Double => write!(f, "Double"),
            Identifier => write!(f, "Identifier"),
            Uuid => write!(f, "Uuid"),
            Position => write!(f, "Position"),
            Tag => write!(f, "Tag"),
            Compound => write!(f, "Compound"),
            Text => write!(f, "Text"),
            Stack => write!(f, "Stack"),
            Template => write!(f, "Template"),
            Kind => write!(f, "Kind"),
            Typed => write!(f, "Typed"),
        }
    }
}

/// A value read by a [`Node`], independent of any protocol's numbering
/// where it can be: registry entries of dispatched types, enums and
/// component types are kept by name; registry IDs are the sender's until
/// converted.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Unit,
    Bool(bool),
    Byte(i8),
    /// `VarInt` and `Int`.
    Int(i32),
    /// `VarLong`, `Long` and `Position`.
    Long(i64),
    Float(f32),
    Double(f64),
    /// `Str` and `Identifier`.
    String(String),
    Uuid(u128),
    /// One complete nameless NBT tag as on the wire, kept as bytes so an
    /// untouched tag goes back out unchanged and costs no parsing.
    Nbt(Vec<u8>),
    /// A registry ID, or a holder that refers to one.
    Id(i32),
    /// A holder with an inline value.
    Direct(Box<Value>),
    /// A holder set by tag name.
    Tag(String),
    /// A holder set as plain IDs.
    Ids(Vec<i32>),
    Enum(&'static str),
    Optional(Option<Box<Value>>),
    /// `List` and `Array`.
    List(Vec<Value>),
    Struct(Vec<Value>),
    /// `true` for the left side.
    Either(bool, Box<Value>),
    Variant(&'static str, Box<Value>),
    /// `Stack` (`None` is the empty stack) and `Template` (always `Some`).
    Stack(Option<Box<ItemStack>>),
    Kind(&'static str),
    Typed(Box<Component>),
}

impl Value {
    /// An NBT value from a parsed tag.
    pub fn nbt(tag: &Nbt) -> Self {
        Value::Nbt(tag.to_bytes(false))
    }

    /// The parsed tag of an NBT value.
    pub fn as_nbt(&self) -> io::Result<Option<Nbt>> {
        match self {
            Value::Nbt(bytes) => Nbt::from_bytes(bytes, false),
            _ => Err(invalid("value is not NBT")),
        }
    }

    pub fn some(value: Value) -> Self {
        Value::Optional(Some(Box::new(value)))
    }

    pub fn none() -> Self {
        Value::Optional(None)
    }

    /// An int, or a registry ID.
    pub fn as_int(&self) -> Option<i32> {
        match self {
            Value::Int(value) | Value::Id(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(value) => Some(value),
            _ => None,
        }
    }

    /// The fields of a struct value.
    pub fn fields(&self) -> io::Result<&[Value]> {
        match self {
            Value::Struct(fields) => Ok(fields),
            _ => Err(invalid("value is not a struct")),
        }
    }

    pub fn fields_mut(&mut self) -> io::Result<&mut Vec<Value>> {
        match self {
            Value::Struct(fields) => Ok(fields),
            _ => Err(invalid("value is not a struct")),
        }
    }

    pub fn into_fields(self) -> io::Result<Vec<Value>> {
        match self {
            Value::Struct(fields) => Ok(fields),
            _ => Err(invalid("value is not a struct")),
        }
    }

    /// The elements of a list value.
    pub fn items(&self) -> io::Result<&[Value]> {
        match self {
            Value::List(items) => Ok(items),
            _ => Err(invalid("value is not a list")),
        }
    }

    pub fn into_items(self) -> io::Result<Vec<Value>> {
        match self {
            Value::List(items) => Ok(items),
            _ => Err(invalid("value is not a list")),
        }
    }

    /// The content of an optional value.
    pub fn option(&self) -> io::Result<Option<&Value>> {
        match self {
            Value::Optional(value) => Ok(value.as_deref()),
            _ => Err(invalid("value is not optional")),
        }
    }

    pub fn into_option(self) -> io::Result<Option<Value>> {
        match self {
            Value::Optional(value) => Ok(value.map(|value| *value)),
            _ => Err(invalid("value is not optional")),
        }
    }
}

fn kind_id(protocol: i32, kind: &str) -> io::Result<i32> {
    layouts::Layout::of(protocol)?
        .id(kind)
        .ok_or_else(|| invalid(&format!("protocol {protocol} has no {kind} component")))
}

/// Read one value of `node` in `protocol`'s layout.
pub fn read(protocol: i32, node: &Node, r: &mut Reader, depth: usize) -> io::Result<Value> {
    if depth > MAX_DEPTH {
        return Err(invalid("data component nesting limit exceeded"));
    }
    Ok(match node {
        Node::Unit => Value::Unit,
        Node::Bool => Value::Bool(r.bool()?),
        Node::Byte => Value::Byte(r.i8()?),
        Node::VarInt => Value::Int(r.varint()?),
        Node::VarLong => Value::Long(r.varlong()?),
        Node::Int => Value::Int(r.i32()?),
        Node::Long | Node::Position => Value::Long(r.i64()?),
        Node::Float => Value::Float(r.f32()?),
        Node::Double => Value::Double(r.f64()?),
        Node::Str(max) => Value::String(r.string(*max)?.to_owned()),
        Node::Identifier => Value::String(r.str()?.to_owned()),
        Node::Uuid => Value::Uuid(r.uuid()?),
        Node::Tag | Node::Text => {
            let bytes = r.nbt_bytes(false)?;
            if bytes == [0] {
                return Err(invalid("missing NBT tag"));
            }
            Value::Nbt(bytes.to_vec())
        }
        Node::Compound => {
            let bytes = r.nbt_bytes(false)?;
            if bytes[0] != 10 {
                return Err(invalid("NBT tag is not a compound"));
            }
            Value::Nbt(bytes.to_vec())
        }
        Node::Id(_) => {
            let id = r.varint()?;
            if id < 0 {
                return Err(invalid("negative registry ID"));
            }
            Value::Id(id)
        }
        Node::Holder(_, inline) => match r.varint()? {
            0 => Value::Direct(Box::new(read(protocol, inline, r, depth + 1)?)),
            id if id > 0 => Value::Id(id - 1),
            _ => return Err(invalid("negative holder ID")),
        },
        Node::HolderSet(_) => {
            let size = r.varint()?;
            if size == 0 {
                Value::Tag(r.str()?.to_owned())
            } else if size < 0 || size as usize - 1 > r.remaining() {
                return Err(invalid("invalid holder set size"));
            } else {
                let mut ids = Vec::with_capacity(size as usize - 1);
                for _ in 1..size {
                    let id = r.varint()?;
                    if id < 0 {
                        return Err(invalid("negative registry ID"));
                    }
                    ids.push(id);
                }
                Value::Ids(ids)
            }
        }
        Node::Enum(names) => {
            let id = r.varint()?;
            let name = usize::try_from(id)
                .ok()
                .and_then(|id| names.get(id))
                .ok_or_else(|| invalid("enum value out of range"))?;
            Value::Enum(name)
        }
        Node::Optional(inner) => Value::Optional(if r.bool()? {
            Some(Box::new(read(protocol, inner, r, depth + 1)?))
        } else {
            None
        }),
        Node::List(element, max) => {
            let count = r.count()?;
            if count > *max {
                return Err(invalid("list exceeds its limit"));
            }
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                items.push(read(protocol, element, r, depth + 1)?);
            }
            Value::List(items)
        }
        Node::Array(element, count) => Value::List(
            (0..*count)
                .map(|_| read(protocol, element, r, depth + 1))
                .collect::<io::Result<_>>()?,
        ),
        Node::Struct(fields) => Value::Struct(
            fields
                .iter()
                .map(|(_, field)| read(protocol, field, r, depth + 1))
                .collect::<io::Result<_>>()?,
        ),
        Node::Either(left, right) => {
            let is_left = r.bool()?;
            let side = if is_left { left } else { right };
            Value::Either(is_left, Box::new(read(protocol, side, r, depth + 1)?))
        }
        Node::Dispatch(registry, variants) => {
            let id = r.varint()?;
            let name = data::version(protocol)
                .registry(registry)
                .and_then(|registry| registry.name(id))
                .ok_or_else(|| invalid(&format!("unknown {registry} ID {id}")))?;
            let (name, variant) = variants
                .iter()
                .find(|(known, _)| *known == name)
                .ok_or_else(|| invalid(&format!("no layout for {registry} {name}")))?;
            Value::Variant(name, Box::new(read(protocol, variant, r, depth + 1)?))
        }
        Node::Union(variants) => {
            let id = r.varint()?;
            let (name, variant) = usize::try_from(id)
                .ok()
                .and_then(|id| variants.get(id))
                .ok_or_else(|| invalid("union variant out of range"))?;
            Value::Variant(name, Box::new(read(protocol, variant, r, depth + 1)?))
        }
        Node::Stack => Value::Stack(read_stack(protocol, r, depth + 1)?.map(Box::new)),
        Node::Template => Value::Stack(Some(Box::new(read_template(protocol, r, depth + 1)?))),
        Node::Kind => Value::Kind(read_kind(protocol, r)?.0),
        Node::Ref(inner) => read(protocol, inner, r, depth + 1)?,
        Node::Typed => {
            let (kind, node) = read_kind(protocol, r)?;
            let value = read(protocol, node, r, depth + 1)?;
            Value::Typed(Box::new(Component { kind, value }))
        }
    })
}

fn read_kind(protocol: i32, r: &mut Reader) -> io::Result<(&'static str, &'static Node)> {
    let id = r.varint()?;
    layouts::Layout::of(protocol)?
        .get(id)
        .ok_or_else(|| invalid(&format!("unknown data component type {id}")))
}

/// `ItemStack.OPTIONAL_STREAM_CODEC` from 1.20.5 on.
pub fn read_stack(protocol: i32, r: &mut Reader, depth: usize) -> io::Result<Option<ItemStack>> {
    let count = r.varint()?;
    if count <= 0 {
        return Ok(None);
    }
    let id = r.varint()?;
    if id < 0 {
        return Err(invalid("negative item ID"));
    }
    let components = read_patch(protocol, r, depth, false)?;
    Ok(Some(ItemStack {
        id,
        count,
        damage: 0,
        tag: None,
        components: Some(components),
    }))
}

/// `ItemStackTemplate.STREAM_CODEC` (26.1+).
pub fn read_template(protocol: i32, r: &mut Reader, depth: usize) -> io::Result<ItemStack> {
    let id = r.varint()?;
    if id < 0 {
        return Err(invalid("negative item ID"));
    }
    let count = r.varint()?;
    let components = read_patch(protocol, r, depth, false)?;
    Ok(ItemStack {
        id,
        count,
        damage: 0,
        tag: None,
        components: Some(components),
    })
}

/// `DataComponentPatch.STREAM_CODEC`, or `DELIMITED_STREAM_CODEC` (each
/// value length-prefixed) as creative clients send it.
pub fn read_patch(
    protocol: i32,
    r: &mut Reader,
    depth: usize,
    delimited: bool,
) -> io::Result<Components> {
    if depth > MAX_DEPTH {
        return Err(invalid("data component nesting limit exceeded"));
    }
    let layout = layouts::Layout::of(protocol)?;
    let added = r.count()?;
    let removed = r.count()?;
    let mut components = Components::default();
    components.added.reserve(added);
    for _ in 0..added {
        let (kind, node) = read_kind(protocol, r)?;
        let value = if delimited {
            let mut inner = Reader::new(r.byte_array()?);
            let value = read(protocol, node, &mut inner, depth + 1)?;
            inner.finish()?;
            value
        } else {
            read(protocol, node, r, depth + 1)?
        };
        components.added.push(Component { kind, value });
    }
    for _ in 0..removed {
        let id = r.varint()?;
        let (kind, _) = layout
            .get(id)
            .ok_or_else(|| invalid(&format!("unknown data component type {id}")))?;
        components.removed.push(kind);
    }
    Ok(components)
}

fn mismatch(node: &Node) -> io::Error {
    invalid(&format!("value does not match its layout ({node:?})"))
}

/// Write one value of `node` in `protocol`'s layout.
pub fn write(protocol: i32, node: &Node, value: &Value, w: &mut Writer) -> io::Result<()> {
    match (node, value) {
        (Node::Unit, Value::Unit) => {}
        (Node::Bool, Value::Bool(value)) => drop(w.bool(*value)),
        (Node::Byte, Value::Byte(value)) => drop(w.i8(*value)),
        (Node::VarInt, Value::Int(value)) => drop(w.varint(*value)),
        (Node::VarLong, Value::Long(value)) => drop(w.varlong(*value)),
        (Node::Int, Value::Int(value)) => drop(w.i32(*value)),
        (Node::Long | Node::Position, Value::Long(value)) => drop(w.i64(*value)),
        (Node::Float, Value::Float(value)) => drop(w.f32(*value)),
        (Node::Double, Value::Double(value)) => drop(w.f64(*value)),
        (Node::Str(max), Value::String(value)) => {
            if value.len() > *max && value.encode_utf16().count() > *max {
                return Err(invalid("string exceeds its limit"));
            }
            w.string(value);
        }
        (Node::Identifier, Value::String(value)) => drop(w.string(value)),
        (Node::Uuid, Value::Uuid(value)) => drop(w.uuid(*value)),
        (Node::Tag | Node::Text | Node::Compound, Value::Nbt(bytes)) => drop(w.bytes(bytes)),
        (Node::Id(_), Value::Id(id)) => drop(w.varint(*id)),
        (Node::Holder(_, _), Value::Id(id)) => drop(w.varint(id + 1)),
        (Node::Holder(_, inline), Value::Direct(value)) => {
            w.varint(0);
            write(protocol, inline, value, w)?;
        }
        (Node::HolderSet(_), Value::Tag(tag)) => drop(w.varint(0).string(tag)),
        (Node::HolderSet(_), Value::Ids(ids)) => {
            w.varint(ids.len() as i32 + 1);
            for id in ids {
                w.varint(*id);
            }
        }
        (Node::Enum(names), Value::Enum(name)) => {
            let id = names
                .iter()
                .position(|known| known == name)
                .ok_or_else(|| invalid(&format!("enum value {name} does not exist here")))?;
            w.varint(id as i32);
        }
        (Node::Optional(inner), Value::Optional(value)) => {
            w.bool(value.is_some());
            if let Some(value) = value {
                write(protocol, inner, value, w)?;
            }
        }
        (Node::List(element, max), Value::List(items)) => {
            if items.len() > *max {
                return Err(invalid("list exceeds its limit"));
            }
            w.varint(items.len() as i32);
            for item in items {
                write(protocol, element, item, w)?;
            }
        }
        (Node::Array(element, count), Value::List(items)) => {
            if items.len() != *count {
                return Err(mismatch(node));
            }
            for item in items {
                write(protocol, element, item, w)?;
            }
        }
        (Node::Struct(fields), Value::Struct(values)) => {
            if fields.len() != values.len() {
                return Err(mismatch(node));
            }
            for ((_, field), value) in fields.iter().zip(values) {
                write(protocol, field, value, w)?;
            }
        }
        (Node::Either(left, right), Value::Either(is_left, value)) => {
            w.bool(*is_left);
            write(protocol, if *is_left { left } else { right }, value, w)?;
        }
        (Node::Dispatch(registry, variants), Value::Variant(name, value)) => {
            let id = data::version(protocol)
                .registry(registry)
                .and_then(|registry| registry.id(name))
                .ok_or_else(|| invalid(&format!("protocol {protocol} has no {registry} {name}")))?;
            let (_, variant) = variants
                .iter()
                .find(|(known, _)| known == name)
                .ok_or_else(|| invalid(&format!("no layout for {registry} {name}")))?;
            w.varint(id);
            write(protocol, variant, value, w)?;
        }
        (Node::Union(variants), Value::Variant(name, value)) => {
            let id = variants
                .iter()
                .position(|(known, _)| known == name)
                .ok_or_else(|| invalid(&format!("union variant {name} does not exist here")))?;
            w.varint(id as i32);
            write(protocol, &variants[id].1, value, w)?;
        }
        (Node::Stack, Value::Stack(stack)) => write_stack(protocol, stack.as_deref(), w)?,
        (Node::Template, Value::Stack(Some(stack))) => write_template(protocol, stack, w)?,
        (Node::Kind, Value::Kind(kind)) => drop(w.varint(kind_id(protocol, kind)?)),
        (Node::Ref(inner), value) => write(protocol, inner, value, w)?,
        (Node::Typed, Value::Typed(component)) => write_component(protocol, component, w)?,
        _ => return Err(mismatch(node)),
    }
    Ok(())
}

pub fn write_component(protocol: i32, component: &Component, w: &mut Writer) -> io::Result<()> {
    let layout = layouts::Layout::of(protocol)?;
    let (id, node) = layout.find(component.kind).ok_or_else(|| {
        invalid(&format!(
            "protocol {protocol} has no {} component",
            component.kind
        ))
    })?;
    w.varint(id);
    write(protocol, node, &component.value, w)
}

pub fn write_stack(protocol: i32, stack: Option<&ItemStack>, w: &mut Writer) -> io::Result<()> {
    match stack {
        Some(stack) if stack.count > 0 => {
            w.varint(stack.count).varint(stack.id);
            write_patch(protocol, stack_components(stack), w, false)
        }
        _ => {
            w.varint(0);
            Ok(())
        }
    }
}

pub fn write_template(protocol: i32, stack: &ItemStack, w: &mut Writer) -> io::Result<()> {
    w.varint(stack.id).varint(stack.count);
    write_patch(protocol, stack_components(stack), w, false)
}

fn stack_components(stack: &ItemStack) -> &Components {
    static EMPTY: Components = Components {
        added: Vec::new(),
        removed: Vec::new(),
    };
    stack.components.as_ref().unwrap_or(&EMPTY)
}

pub fn write_patch(
    protocol: i32,
    components: &Components,
    w: &mut Writer,
    delimited: bool,
) -> io::Result<()> {
    let layout = layouts::Layout::of(protocol)?;
    w.varint(components.added.len() as i32);
    w.varint(components.removed.len() as i32);
    for component in &components.added {
        if delimited {
            let (id, node) = layout.find(component.kind).ok_or_else(|| {
                invalid(&format!(
                    "protocol {protocol} has no {} component",
                    component.kind
                ))
            })?;
            let mut inner = Writer::new();
            write(protocol, node, &component.value, &mut inner)?;
            w.varint(id).byte_array(&inner.buf);
        } else {
            write_component(protocol, component, w)?;
        }
    }
    for kind in &components.removed {
        w.varint(kind_id(protocol, kind)?);
    }
    Ok(())
}

/// Whether values of `source` can be carried over to `target` node by node,
/// remapping registry IDs and enum values by name and coercing between
/// equivalent encodings; anything else needs an explicit conversion.
pub fn compatible(source: &Node, target: &Node) -> bool {
    compatible_in(source, target, &mut Vec::new())
}

/// `seen` holds the reference pairs being compared further up, so that
/// recursive layouts end: a pair met again is assumed compatible.
fn compatible_in(source: &Node, target: &Node, seen: &mut Vec<(usize, usize)>) -> bool {
    use Node::*;
    match (source, target) {
        (Ref(x), Ref(y)) => {
            let pair = (*x as *const Node as usize, *y as *const Node as usize);
            if pair.0 == pair.1 || seen.contains(&pair) {
                return true;
            }
            seen.push(pair);
            let result = compatible_in(x, y, seen);
            seen.pop();
            result
        }
        (Ref(x), _) => compatible_in(x, target, seen),
        (_, Ref(y)) => compatible_in(source, y, seen),
        (Holder(a, x), Holder(b, y)) => a == b && compatible_in(x, y, seen),
        (Enum(_), Enum(_)) => true,
        (Optional(x), Optional(y)) => compatible_in(x, y, seen),
        (List(x, _), List(y, _)) | (Array(x, _), Array(y, _)) => compatible_in(x, y, seen),
        (Struct(a), Struct(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(*b)
                    .all(|((_, x), (_, y))| compatible_in(x, y, seen))
        }
        (Either(a, b), Either(c, d)) => compatible_in(a, c, seen) && compatible_in(b, d, seen),
        (Dispatch(a, x), Dispatch(b, y)) => a == b && variants_compatible(x, y, seen),
        (Union(x), Union(y)) => variants_compatible(x, y, seen),
        (Stack | Template, Stack | Template) => true,
        // Encodings vanilla switched between without changing the meaning:
        // an either-holder (a holder, or an entry key) and a plain holder,
        // a tag key and a holder set, a stack and an optional template.
        (Either(x, _), y) | (y, Either(x, _)) => compatible_in(x, y, seen),
        (Identifier, HolderSet(_)) | (HolderSet(_), Identifier) => true,
        // 1.21.5's smithing trim display names its pattern instead of
        // showing the template item.
        (Dispatch("slot_display", _), Holder("trim_pattern", _))
        | (Holder("trim_pattern", _), Dispatch("slot_display", _)) => true,
        (Stack, Optional(inner)) | (Optional(inner), Stack) => {
            matches!(inner, Template | Stack)
        }
        (Id(a), Id(b)) | (HolderSet(a), HolderSet(b)) => a == b,
        (Str(_), Str(_)) => true,
        (Unit, Unit)
        | (Bool, Bool)
        | (Byte, Byte)
        | (VarInt, VarInt)
        | (VarLong, VarLong)
        | (Int, Int)
        | (Long, Long)
        | (Float, Float)
        | (Double, Double)
        | (Identifier, Identifier)
        | (Uuid, Uuid)
        | (Position, Position)
        | (Tag, Tag)
        | (Compound, Compound)
        | (Text, Text)
        | (Kind, Kind)
        | (Typed, Typed) => true,
        _ => false,
    }
}

/// Variants both sides name must be compatible; the others cannot be
/// carried over and are dropped by conversion.
fn variants_compatible(
    source: &[(&str, Node)],
    target: &[(&str, Node)],
    seen: &mut Vec<(usize, usize)>,
) -> bool {
    source.iter().all(|(name, x)| {
        target
            .iter()
            .find(|(other, _)| other == name)
            .is_none_or(|(_, y)| compatible_in(x, y, seen))
    })
}

/// Call `visit` with the registry name of every registry ID `node` can hold
/// (possibly more than once).
pub fn registries(node: &Node, visit: &mut dyn FnMut(&'static str)) {
    registries_in(node, visit, &mut Vec::new());
}

fn registries_in(node: &Node, visit: &mut dyn FnMut(&'static str), seen: &mut Vec<usize>) {
    match node {
        Node::Ref(inner) => {
            let address = *inner as *const Node as usize;
            if !seen.contains(&address) {
                seen.push(address);
                registries_in(inner, visit, seen);
            }
        }
        Node::Id(registry) | Node::HolderSet(registry) => visit(registry),
        Node::Holder(registry, inline) => {
            visit(registry);
            registries_in(inline, visit, seen);
        }
        Node::Optional(inner) | Node::List(inner, _) | Node::Array(inner, _) => {
            registries_in(inner, visit, seen)
        }
        Node::Struct(fields) => fields
            .iter()
            .for_each(|(_, field)| registries_in(field, visit, seen)),
        Node::Either(left, right) => {
            registries_in(left, visit, seen);
            registries_in(right, visit, seen);
        }
        Node::Dispatch(_, variants) | Node::Union(variants) => variants
            .iter()
            .for_each(|(_, variant)| registries_in(variant, visit, seen)),
        Node::Stack | Node::Template => visit("item"),
        _ => {}
    }
}
