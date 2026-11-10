//! Translation keys 1.13 renamed (`tile.stone.name` became
//! `block.minecraft.stone`, ...). `translations.json` pairs keys whose
//! English text is the same in both versions; `generate.py` builds it from
//! the two versions' own language files.
use std::{collections::HashMap, sync::OnceLock};

/// The 1.13 key for a 1.12 translation key that 1.13 renamed; `None` when
/// the key is the same in both versions or 1.13 has no equivalent.
// INTEGRATION: the text codec should apply this to `translate` keys of
// components travelling from 340 to 393; `crate::steps` keeps its pair
// modules private, so it needs a path to reach it first.
#[allow(dead_code)]
pub fn upgrade_translation_key(key: &str) -> Option<&'static str> {
    static KEYS: OnceLock<HashMap<String, String>> = OnceLock::new();
    KEYS.get_or_init(|| {
        serde_json::from_str(include_str!("translations.json"))
            .expect("translations.json is a JSON object of strings")
    })
    .get(key)
    .map(String::as_str)
}
