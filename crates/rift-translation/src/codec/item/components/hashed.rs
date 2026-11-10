//! Hashed stacks (1.21.5+): what a client sends in `container_click` for the
//! slots it predicts changed, instead of whole stacks. Each component is a
//! CRC32C hash of its persistent codec's encoding (`HashOps`), which names
//! registry entries rather than numbering them, so remapping IDs never
//! changes a hash.
//!
//! Converting a hashed stack maps the item and component type IDs and keeps
//! every hash. Where a component's persistent encoding differs between the
//! two protocols, the kept hash no longer matches what the receiving server
//! computes for its own stack; the server then treats the slot as out of
//! sync and sends it again in full, which is safe (vanilla does the same
//! for any misprediction). Recomputing such a hash would need the value,
//! which a hashed stack does not carry. The one exact recomputation is a
//! pure rename: 26.3 split `swing_animation` into `attack_animation` and
//! `interact_animation` with the same encoding, so both get its hash and
//! going back keeps the attack one. Types the receiver lacks are dropped
//! (again a resync at worst), and a stack whose item the receiver lacks
//! becomes empty.
use super::{convert::Conv, layouts::Layout};
use crate::{
    Direction,
    codec::Shared,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::io;

/// Vanilla's limit on hashed components and removed types per stack.
const LIMIT: usize = 256;

/// One non-empty hashed stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HashedStack {
    pub item: i32,
    pub count: i32,
    /// Component types set, with their hashes.
    pub added: Vec<(&'static str, i32)>,
    /// Component types removed.
    pub removed: Vec<&'static str>,
}

fn kind(layout: &Layout, r: &mut Reader<'_>) -> io::Result<&'static str> {
    let id = r.varint()?;
    layout
        .get(id)
        .map(|(kind, _)| kind)
        .ok_or_else(|| invalid(&format!("unknown data component type {id}")))
}

/// `HashedStack.STREAM_CODEC`; `None` is the empty stack.
pub fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Option<HashedStack>> {
    if !r.bool()? {
        return Ok(None);
    }
    let layout = Layout::of(protocol)?;
    let item = r.varint()?;
    let count = r.varint()?;
    let added_count = r.count()?;
    if added_count > LIMIT {
        return Err(invalid("too many hashed components"));
    }
    let mut added = Vec::with_capacity(added_count);
    for _ in 0..added_count {
        added.push((kind(layout, r)?, r.i32()?));
    }
    let removed_count = r.count()?;
    if removed_count > LIMIT {
        return Err(invalid("too many removed components"));
    }
    let mut removed = Vec::with_capacity(removed_count);
    for _ in 0..removed_count {
        removed.push(kind(layout, r)?);
    }
    Ok(Some(HashedStack {
        item,
        count,
        added,
        removed,
    }))
}

pub fn write(protocol: i32, stack: Option<&HashedStack>, w: &mut Writer) -> io::Result<()> {
    let Some(stack) = stack else {
        w.bool(false);
        return Ok(());
    };
    let layout = Layout::of(protocol)?;
    let id = |kind: &str| {
        layout
            .id(kind)
            .ok_or_else(|| invalid(&format!("protocol {protocol} has no {kind} component")))
    };
    w.bool(true).varint(stack.item).varint(stack.count);
    w.varint(stack.added.len() as i32);
    for (kind, hash) in &stack.added {
        w.varint(id(kind)?).i32(*hash);
    }
    w.varint(stack.removed.len() as i32);
    for kind in &stack.removed {
        w.varint(id(kind)?);
    }
    Ok(())
}

/// The receiver's types for the sender's type `kind`: usually one, none
/// when it lacks the type, two for a split.
fn kinds(cx: &mut Conv<'_>, kind: &'static str) -> Vec<&'static str> {
    let renamed: &[&str] = match (cx.older(), cx.up(), kind) {
        (776, true, "swing_animation") => &["attack_animation", "interact_animation"],
        (776, false, "attack_animation") => &["swing_animation"],
        (776, false, "interact_animation") => &[],
        _ => return cx.target_kind(kind).into_iter().collect(),
    };
    let layout = Layout::of(cx.target).ok();
    renamed
        .iter()
        .filter_map(|name| {
            let layout = layout?;
            Some(layout.get(layout.id(name)?)?.0)
        })
        .collect()
}

/// Convert a hashed stack between the two protocols of the step.
pub fn convert(
    shared: &mut Shared,
    direction: Direction,
    stack: &mut Option<HashedStack>,
) -> io::Result<()> {
    let Some(hashed) = stack.take() else {
        return Ok(());
    };
    *stack = super::convert::with(shared, direction, |cx| {
        let Some(item) = cx.map_id("item", hashed.item) else {
            return Ok(None);
        };
        let mut added = Vec::with_capacity(hashed.added.len());
        for (kind, hash) in &hashed.added {
            for kind in kinds(cx, kind) {
                if !added.iter().any(|(known, _)| *known == kind) {
                    added.push((kind, *hash));
                }
            }
        }
        let mut removed = Vec::with_capacity(hashed.removed.len());
        for kind in &hashed.removed {
            for kind in kinds(cx, kind) {
                if !removed.contains(&kind) {
                    removed.push(kind);
                }
            }
        }
        Ok(Some(HashedStack {
            item,
            count: hashed.count,
            added,
            removed,
        }))
    })?
    .flatten();
    Ok(())
}

/// Copy one hashed stack from `rw.r` to `rw.w`, converting it.
pub fn transcode(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
) -> io::Result<()> {
    let mut stack = read(shared.source(direction), &mut rw.r)?;
    convert(shared, direction, &mut stack)?;
    write(shared.target(direction), stack.as_ref(), &mut rw.w)
}
