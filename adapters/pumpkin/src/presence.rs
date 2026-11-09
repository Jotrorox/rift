//! Which players are logging in or online, so a login that never completes
//! (kicked by another plugin, a ban, a closed connection) still releases its
//! snapshot.

use std::collections::HashSet;

use rift_permissions::Uuid;

#[derive(Debug, Default)]
pub struct Presence {
    logging_in: HashSet<Uuid>,
    online: HashSet<Uuid>,
}

impl Presence {
    /// The player passed pre-login and their snapshot was requested.
    pub fn logging_in(&mut self, uuid: Uuid) {
        self.logging_in.insert(uuid);
    }

    /// The player joined. Returns whether they came through pre-login here,
    /// which is false when the plugin loaded while they were connecting.
    pub fn joined(&mut self, uuid: Uuid) -> bool {
        self.online.insert(uuid);
        self.logging_in.remove(&uuid)
    }

    pub fn left(&mut self, uuid: Uuid) {
        self.online.remove(&uuid);
        self.logging_in.remove(&uuid);
    }

    /// True once for a player who passed pre-login but never joined; the
    /// caller then releases their snapshot.
    pub fn abandoned(&mut self, uuid: Uuid) -> bool {
        !self.online.contains(&uuid) && self.logging_in.remove(&uuid)
    }

    pub fn is_online(&self, uuid: Uuid) -> bool {
        self.online.contains(&uuid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logins_that_never_join_are_released_once() {
        let mut presence = Presence::default();
        let (alice, bob, carol) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        presence.logging_in(alice);
        presence.logging_in(bob);
        assert!(presence.joined(alice));
        assert!(presence.is_online(alice));
        assert!(!presence.abandoned(alice));
        assert!(presence.abandoned(bob));
        assert!(!presence.abandoned(bob));
        // Joined without a pre-login seen here.
        assert!(!presence.joined(carol));
        presence.left(alice);
        assert!(!presence.is_online(alice));
        assert!(!presence.abandoned(alice));
    }
}
