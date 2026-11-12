//! Villager trades: the `merchant_offers` packet (1.14 on) and, before
//! 1.14, the trade list that travels in the `MC|TrList` (1.12) or
//! `minecraft:trader_list` (1.13) plugin channel.
use super::{ItemStack, Stack, components, window::read_stack, window::write_stack};
use crate::wire::{Reader, Writer, invalid};
use std::io;

/// One trade. Costs are full stacks before 1.20.5 and item costs (item,
/// count, exact component matches) after; both are held as stacks, the
/// matches as the stack's component patch.
#[derive(Clone, Debug, PartialEq)]
pub struct Offer {
    pub cost: Stack,
    pub result: Stack,
    pub second_cost: Stack,
    pub disabled: bool,
    pub uses: i32,
    pub max_uses: i32,
    /// 1.14 on.
    pub xp: i32,
    pub special_price: i32,
    pub price_multiplier: f32,
    /// 1.14.4 on.
    pub demand: i32,
}

/// The trades and, from 1.14, the villager's progress.
#[derive(Clone, Debug, PartialEq)]
pub struct Offers {
    /// The window (an int in the plugin channel, a VarInt after).
    pub container: i32,
    pub offers: Vec<Offer>,
    pub level: i32,
    pub xp: i32,
    pub regular: bool,
    /// 1.14.3 on.
    pub can_restock: bool,
}

/// The plugin-channel layout of protocols before 1.14.
pub fn legacy(protocol: i32) -> bool {
    protocol < 477
}

fn item_costs(protocol: i32) -> bool {
    protocol >= 766
}

/// The trade count is a VarInt and the second cost a plain (possibly
/// empty) stack from 1.19 on.
fn modern_list(protocol: i32) -> bool {
    protocol >= 759
}

impl Offers {
    /// Read the packet body (1.14 on) or the plugin channel's payload
    /// after the channel name (before 1.14).
    pub fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Self> {
        let container = if legacy(protocol) {
            r.i32()?
        } else {
            r.varint()?
        };
        let count = if modern_list(protocol) {
            r.count()?
        } else {
            usize::from(r.u8()?)
        };
        let mut offers = Vec::with_capacity(count.min(r.remaining()));
        for _ in 0..count {
            let cost = read_cost(protocol, r)?;
            let result = read_stack(protocol, r)?;
            let second_cost = if item_costs(protocol) {
                if r.bool()? {
                    read_cost(protocol, r)?
                } else {
                    None
                }
            } else if modern_list(protocol) || r.bool()? {
                read_stack(protocol, r)?
            } else {
                None
            };
            let disabled = r.bool()?;
            let uses = r.i32()?;
            let max_uses = r.i32()?;
            let (xp, special_price, price_multiplier) = if legacy(protocol) {
                (0, 0, 0.0)
            } else {
                (r.i32()?, r.i32()?, r.f32()?)
            };
            let demand = if protocol >= 498 { r.i32()? } else { 0 };
            offers.push(Offer {
                cost,
                result,
                second_cost,
                disabled,
                uses,
                max_uses,
                xp,
                special_price,
                price_multiplier,
                demand,
            });
        }
        let (level, xp, regular, can_restock) = if legacy(protocol) {
            (0, 0, false, false)
        } else {
            let level = r.varint()?;
            let xp = r.varint()?;
            let regular = r.bool()?;
            let can_restock = if protocol >= 490 { r.bool()? } else { false };
            (level, xp, regular, can_restock)
        };
        Ok(Self {
            container,
            offers,
            level,
            xp,
            regular,
            can_restock,
        })
    }

    pub fn write(&self, protocol: i32, w: &mut Writer) -> io::Result<()> {
        if legacy(protocol) {
            w.i32(self.container);
        } else {
            w.varint(self.container);
        }
        if modern_list(protocol) {
            w.varint(self.offers.len() as i32);
        } else {
            w.u8(u8::try_from(self.offers.len()).map_err(|_| invalid("too many trades"))?);
        }
        for offer in &self.offers {
            write_cost(protocol, w, &offer.cost)?;
            write_stack(protocol, w, &offer.result)?;
            if item_costs(protocol) {
                w.bool(offer.second_cost.is_some());
                if offer.second_cost.is_some() {
                    write_cost(protocol, w, &offer.second_cost)?;
                }
            } else if modern_list(protocol) {
                write_stack(protocol, w, &offer.second_cost)?;
            } else {
                w.bool(offer.second_cost.is_some());
                if offer.second_cost.is_some() {
                    write_stack(protocol, w, &offer.second_cost)?;
                }
            }
            w.bool(offer.disabled).i32(offer.uses).i32(offer.max_uses);
            if !legacy(protocol) {
                w.i32(offer.xp)
                    .i32(offer.special_price)
                    .f32(offer.price_multiplier);
            }
            if protocol >= 498 {
                w.i32(offer.demand);
            }
        }
        if !legacy(protocol) {
            w.varint(self.level).varint(self.xp).bool(self.regular);
            if protocol >= 490 {
                w.bool(self.can_restock);
            }
        }
        Ok(())
    }
}

/// A cost: a stack, or from 1.20.5 an item cost, which cannot be empty.
fn read_cost(protocol: i32, r: &mut Reader<'_>) -> io::Result<Stack> {
    if !item_costs(protocol) {
        return read_stack(protocol, r);
    }
    let id = r.varint()?;
    let count = r.varint()?;
    if id < 0 || count <= 0 {
        return Err(invalid("invalid item cost"));
    }
    let components = read_exact_components(protocol, r)?;
    Ok(Some(ItemStack {
        id,
        count,
        damage: 0,
        tag: None,
        components: Some(components),
    }))
}

fn write_cost(protocol: i32, w: &mut Writer, cost: &Stack) -> io::Result<()> {
    if !item_costs(protocol) {
        return write_stack(protocol, w, cost);
    }
    let Some(cost) = cost else {
        return Err(invalid("an item cost cannot be empty"));
    };
    w.varint(cost.id).varint(cost.count.max(1));
    write_exact_components(protocol, w, &cost.components.clone().unwrap_or_default())
}

/// Exact component matches: a count and that many typed components, the
/// added half of a component patch. Read through the patch codec by
/// putting a patch header (added, none removed) in front of the input.
pub fn read_exact_components(
    protocol: i32,
    r: &mut Reader<'_>,
) -> io::Result<components::Components> {
    let count = r.count()?;
    let mut header = Writer::new();
    header.varint(count as i32).varint(0);
    let mut patched = header.buf.clone();
    patched.extend_from_slice(r.peek());
    let mut inner = Reader::new(&patched);
    let components = components::read(protocol, &mut inner)?;
    let consumed = patched.len() - inner.remaining();
    let body = consumed
        .checked_sub(header.len())
        .ok_or_else(|| invalid("component patch shorter than its header"))?;
    r.take(body)?;
    Ok(components)
}

/// The reverse of [`read_exact_components`]: the patch without its removed
/// half, which must be empty.
pub fn write_exact_components(
    protocol: i32,
    w: &mut Writer,
    components: &components::Components,
) -> io::Result<()> {
    let mut patch = Writer::new();
    components::write(protocol, components, &mut patch)?;
    let mut r = Reader::new(&patch.buf);
    let added = r.varint()?;
    let removed = r.varint()?;
    if removed != 0 {
        return Err(invalid("exact component matches cannot remove components"));
    }
    w.varint(added).bytes(r.rest());
    Ok(())
}
