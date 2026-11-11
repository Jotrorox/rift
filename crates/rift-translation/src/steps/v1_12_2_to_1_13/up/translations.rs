//! Translation keys 1.13 renamed (`tile.stone.name` became
//! `block.minecraft.stone`, ...). `translations.json` pairs keys whose
//! English text is the same in both versions; `generate.py` builds it from
//! the two versions' own language files.
use std::{collections::HashMap, sync::OnceLock};

/// The 1.13 key for a 1.12 translation key that 1.13 renamed; `None` when
/// the key is the same in both versions or 1.13 has no equivalent.
/// The text codec applies it to components going from 1.12.2 to 1.13.
pub fn upgrade_translation_key(key: &str) -> Option<&'static str> {
    static KEYS: OnceLock<HashMap<String, String>> = OnceLock::new();
    KEYS.get_or_init(|| {
        serde_json::from_str(include_str!("translations.json"))
            .expect("translations.json is a JSON object of strings")
    })
    .get(key)
    .map(String::as_str)
}
