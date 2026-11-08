//! The permission-check decision and UUID conversion.

use rift_permissions::{Tristate, Uuid};

/// The result a `PlayerPermissionCheckEvent` ends with: the replica's answer
/// when it has one, otherwise Pumpkin's own (attachments, the registered
/// default, the op level), which arrives as `pumpkin_result`.
pub const fn decide(answer: Tristate, pumpkin_result: bool) -> bool {
    match answer {
        Tristate::True => true,
        Tristate::False => false,
        Tristate::Undefined => pumpkin_result,
    }
}

/// A Pumpkin player UUID (the `uuid` crate's) as Rift's.
pub const fn rift_uuid(uuid: &uuid::Uuid) -> Uuid {
    Uuid::from_u128(uuid.as_u128())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_defined_answer_replaces_pumpkins() {
        for pumpkin in [true, false] {
            assert!(decide(Tristate::True, pumpkin));
            assert!(!decide(Tristate::False, pumpkin));
            assert_eq!(decide(Tristate::Undefined, pumpkin), pumpkin);
        }
    }

    #[test]
    fn uuids_keep_their_bits() {
        let pumpkin = uuid::Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap();
        assert_eq!(
            rift_uuid(&pumpkin).to_string(),
            "0123456789abcdef0123456789abcdef"
        );
    }
}
