//! Villager trades: the payload of the `minecraft:trader_list` plugin
//! channel before 1.14 and the `merchant_offers` packet from 1.14.
//!
//! 1.14 sends each first cost at its base count with the price adjustments
//! (special price, and from 1.14.4 demand) beside it, and the client works
//! out the price; a 1.13 client shows the count it gets, so going down the
//! adjusted price is folded into the count. Going up the 1.13 price is
//! already final and the adjustments are zero.
use crate::{
    Direction,
    codec::{Shared, item},
    wire::{Reader, Rewriter, Writer},
};
use std::io;

pub(super) const TRADER_LIST: &str = "minecraft:trader_list";

/// Trade fields 1.14 added, with their meaning for a 1.13 trade.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Pricing {
    xp: i32,
    special_price: i32,
    multiplier: f32,
    demand: i32,
}

impl Pricing {
    /// The first cost's count the 1.14 client shows for a base count.
    fn adjusted(&self, count: i32) -> i32 {
        let demand = ((count * self.demand) as f32 * self.multiplier).floor() as i32;
        (count + demand.max(0) + self.special_price).clamp(1, 64)
    }
}

fn merchant_packet(protocol: i32) -> bool {
    protocol >= 477
}

/// Copy one item stack (the present-flag layout of 1.13.2 to 1.20.4),
/// converting it for the receiver, optionally with a new count.
fn stack(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
    count: impl FnOnce(i32) -> i32,
) -> io::Result<()> {
    let start = rw.r.peek();
    let mut r = Reader::new(start);
    let mut raw = Writer::new();
    if r.bool()? {
        let id = r.varint()?;
        let base = i32::from(r.i8()?);
        let tag = r.nbt_bytes(true)?;
        let count = count(base).clamp(i32::from(i8::MIN), i32::from(i8::MAX)) as i8;
        raw.bool(true).varint(id).i8(count).bytes(tag);
    } else {
        raw.bool(false);
    }
    rw.r.take(start.len() - r.remaining())?;
    let mut inner = Rewriter::new(&raw.buf);
    // INTEGRATION: the item codec converts the stack's ID and tag.
    item::transcode(shared, direction, &mut inner)?;
    rw.w.bytes(&inner.finish()?);
    Ok(())
}

/// Convert trades from the sender's to the receiver's layout. `data` is the
/// `merchant_offers` body (1.14 on) or the plugin channel's payload after
/// the channel name (before); so is the result.
pub(super) fn convert(
    shared: &mut Shared,
    direction: Direction,
    data: &[u8],
) -> io::Result<Vec<u8>> {
    let (from, to) = (shared.source(direction), shared.target(direction));
    let mut rw = Rewriter::new(data);
    let window = if merchant_packet(from) {
        rw.r.varint()?
    } else {
        rw.r.i32()?
    };
    if merchant_packet(to) {
        rw.w.varint(window);
    } else {
        rw.w.i32(window);
    }
    let trades = rw.u8()?;
    for _ in 0..trades {
        // The pricing follows the stacks, so the first cost is converted
        // after the rest of the trade is known.
        let first = rw.r.peek();
        skip_stack(&mut rw.r)?;
        let first = &first[..first.len() - rw.r.remaining()];
        let mut rest = Rewriter::new(rw.r.peek());
        stack(shared, direction, &mut rest, |count| count)?;
        if rest.bool()? {
            stack(shared, direction, &mut rest, |count| count)?;
        }
        rest.bool()?; // Disabled.
        rest.i32()?; // Uses.
        rest.i32()?; // Maximum uses.
        let mut pricing = Pricing::default();
        if merchant_packet(from) {
            pricing.xp = rest.r.i32()?;
            pricing.special_price = rest.r.i32()?;
            pricing.multiplier = rest.r.f32()?;
            if from >= 498 {
                pricing.demand = rest.r.i32()?;
            }
        }
        let consumed = rw.r.remaining() - rest.r.remaining();
        rw.r.take(consumed)?;
        let fold = merchant_packet(from) && !merchant_packet(to);
        let mut cost = Rewriter::new(first);
        stack(shared, direction, &mut cost, |count| {
            if fold { pricing.adjusted(count) } else { count }
        })?;
        rw.w.bytes(&cost.finish()?);
        rw.w.bytes(&rest.w.buf);
        if merchant_packet(to) {
            if !merchant_packet(from) {
                // A 1.13 price is final: no adjustment on top.
                pricing = Pricing::default();
            }
            rw.w.i32(pricing.xp)
                .i32(pricing.special_price)
                .f32(pricing.multiplier);
            if to >= 498 {
                rw.w.i32(pricing.demand);
            }
        }
    }
    // The villager's level and experience, whether it is a villager (rather
    // than a wandering trader) and from 1.14.3 whether it restocks.
    let (mut level, mut xp, mut regular, mut restock) = (0, 0, false, false);
    if merchant_packet(from) {
        level = rw.r.varint()?;
        xp = rw.r.varint()?;
        regular = rw.r.bool()?;
        restock = from >= 490 && rw.r.bool()?;
    }
    if merchant_packet(to) {
        rw.w.varint(level).varint(xp).bool(regular);
        if to >= 490 {
            rw.w.bool(restock);
        }
    }
    rw.finish()
}

fn skip_stack(r: &mut Reader<'_>) -> io::Result<()> {
    if r.bool()? {
        r.varint()?;
        r.i8()?;
        r.nbt_bytes(true)?;
    }
    Ok(())
}
