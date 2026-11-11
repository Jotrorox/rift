//! 1.12 items (numeric ID and damage) by their 1.13 names and back, exact
//! counterparts only, over [`crate::legacy`]: the flattening code decides
//! itself what stands in where 1.12 had no such item.
use crate::{data, legacy};

/// The 1.13 name of the 1.12 item `id` with data value `damage`.
pub fn name(id: i16, damage: i16) -> Option<&'static str> {
    let item = legacy::legacy().flatten_item_exact(i32::from(id), damage)?;
    data::version(legacy::FLATTENED)
        .registry("item")?
        .name(item)
}

/// The 1.12 item and data value for a 1.13 item name, when 1.12 had it.
pub fn legacy(name: &str) -> Option<(i16, i16)> {
    let item = data::version(legacy::FLATTENED)
        .registry("item")?
        .id(data::strip(name))?;
    let (id, damage) = legacy::legacy().unflatten_item(item)?;
    // `unflatten_item` falls back to a related item; only an exact source
    // is this item's 1.12 form.
    (legacy::legacy().flatten_item_exact(id, damage) == Some(item))
        .then(|| Some((i16::try_from(id).ok()?, damage)))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_items_only() {
        assert_eq!(name(35, 14), Some("red_wool"));
        assert_eq!(name(276, 0), Some("diamond_sword"));
        assert_eq!(name(35, 99), None);
        assert_eq!(legacy("red_wool"), Some((35, 14)));
        assert_eq!(legacy("music_disc_wait"), Some((2267, 0)));
        // 1.12 had one spawn egg, one button and no wood blocks.
        assert_eq!(legacy("cow_spawn_egg"), None);
        assert_eq!(legacy("acacia_button"), None);
        assert_eq!(legacy("oak_wood"), None);
        assert_eq!(legacy("kelp"), None);
    }
}
