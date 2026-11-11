//! The protocols translation can connect, oldest first.

/// Every protocol from 1.12 to 26.3 with the newest release that speaks it.
/// Adjacent entries are one translation step apart.
pub const PROTOCOLS: &[(i32, &str)] = &[
    (335, "1.12"),
    (338, "1.12.1"),
    (340, "1.12.2"),
    (393, "1.13"),
    (401, "1.13.1"),
    (404, "1.13.2"),
    (477, "1.14"),
    (480, "1.14.1"),
    (485, "1.14.2"),
    (490, "1.14.3"),
    (498, "1.14.4"),
    (573, "1.15"),
    (575, "1.15.1"),
    (578, "1.15.2"),
    (735, "1.16"),
    (736, "1.16.1"),
    (751, "1.16.2"),
    (753, "1.16.3"),
    (754, "1.16.5"),
    (755, "1.17"),
    (756, "1.17.1"),
    (757, "1.18.1"),
    (758, "1.18.2"),
    (759, "1.19"),
    (760, "1.19.2"),
    (761, "1.19.3"),
    (762, "1.19.4"),
    (763, "1.20.1"),
    (764, "1.20.2"),
    (765, "1.20.4"),
    (766, "1.20.6"),
    (767, "1.21.1"),
    (768, "1.21.3"),
    (769, "1.21.4"),
    (770, "1.21.5"),
    (771, "1.21.6"),
    (772, "1.21.8"),
    (773, "1.21.10"),
    (774, "1.21.11"),
    (775, "26.1.2"),
    (776, "26.2"),
    (777, "26.3"),
];

/// The position of `protocol` in [`PROTOCOLS`].
pub fn index(protocol: i32) -> Option<usize> {
    PROTOCOLS
        .binary_search_by_key(&protocol, |(protocol, _)| *protocol)
        .ok()
}

/// The newest release name of `protocol`, for messages.
pub fn release(protocol: i32) -> &'static str {
    index(protocol).map_or("unknown", |index| PROTOCOLS[index].1)
}

/// The configuration phase exists from 1.20.2 on.
pub fn has_configuration(protocol: i32) -> bool {
    protocol >= 764
}

/// Network NBT has a nameless root from 1.20.2 on.
pub fn nameless_nbt(protocol: i32) -> bool {
    protocol >= 764
}

/// Text components are NBT instead of JSON strings from 1.20.3 on (outside
/// login and status).
pub fn nbt_components(protocol: i32) -> bool {
    protocol >= 765
}

/// Blocks and items are flattened (no numeric ID plus metadata) from 1.13 on.
pub fn flattened(protocol: i32) -> bool {
    protocol >= 393
}

/// Block positions pack as x, z, y from 1.14 on (x, y, z before).
pub fn modern_positions(protocol: i32) -> bool {
    protocol >= 477
}

/// Items carry data components instead of an NBT tag from 1.20.5 on.
pub fn item_components(protocol: i32) -> bool {
    protocol >= 766
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocols_are_sorted_and_unique() {
        assert!(PROTOCOLS.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert_eq!(index(335), Some(0));
        assert_eq!(index(777), Some(PROTOCOLS.len() - 1));
        assert_eq!(index(47), None);
        assert_eq!(release(774), "1.21.11");
    }
}
