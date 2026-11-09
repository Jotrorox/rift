//! Whether Pumpkin knows players by the proxy's UUIDs.
//!
//! The proxy pushes each player's snapshot keyed by the UUID it knows them
//! by. Pumpkin uses that UUID only when it reads it from Velocity modern
//! forwarding; otherwise it derives its own offline UUID from the name (the
//! first 16 bytes of SHA-256, not vanilla's `OfflinePlayer:` MD5), the pushed
//! snapshot is never used, and every login waits to fetch one instead.

/// The parts of Pumpkin's `[networking.proxy]` settings that decide which
/// UUID a player gets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProxySettings {
    /// `[networking.proxy] enabled`
    pub enabled: bool,
    /// `[networking.proxy.velocity] enabled`
    pub velocity: bool,
    /// `[networking.proxy.velocity] secret` is not empty.
    pub velocity_secret: bool,
    /// `[networking.proxy.bungeecord] enabled`
    pub bungeecord: bool,
}

const FIX: &str = "set [networking.proxy] enabled = true and [networking.proxy.velocity] \
                   enabled = true with secret = Rift's forwarding secret (Rift needs online \
                   mode and forwarding.mode = \"velocity\")";

/// The warning to log at load, or `None` when Pumpkin takes players' UUIDs
/// from Velocity forwarding.
pub fn warning(proxy: ProxySettings) -> Option<String> {
    let reason = if !proxy.enabled {
        "[networking.proxy] enabled = false"
    } else if proxy.velocity && !proxy.velocity_secret {
        return Some(format!(
            "Pumpkin's Velocity forwarding has an empty secret, so Pumpkin rejects every \
             forwarded login; {FIX}"
        ));
    } else if proxy.velocity {
        return None;
    } else if proxy.bungeecord {
        "BungeeCord forwarding is selected, but Rift sends only Velocity modern forwarding"
    } else {
        "[networking.proxy.velocity] enabled = false"
    };
    Some(format!(
        "Pumpkin's Velocity forwarding is off ({reason}): Pumpkin gives players its own \
         offline UUIDs, which do not match the proxy's, so the snapshots the proxy pushes \
         go unused and each login fetches one instead (waiting up to join_timeout_ms). \
         This is expected behind an offline-mode proxy such as `rift dev`; otherwise {FIX}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORWARDING: ProxySettings = ProxySettings {
        enabled: true,
        velocity: true,
        velocity_secret: true,
        bungeecord: false,
    };

    #[test]
    fn velocity_with_a_secret_is_quiet() {
        assert_eq!(warning(FORWARDING), None);
        // Velocity wins over BungeeCord in Pumpkin's login.
        let both = ProxySettings {
            bungeecord: true,
            ..FORWARDING
        };
        assert_eq!(warning(both), None);
    }

    #[test]
    fn disabled_forwarding_warns_about_uuids() {
        for (proxy, reason) in [
            (
                ProxySettings::default(),
                "[networking.proxy] enabled = false",
            ),
            (
                ProxySettings {
                    enabled: false,
                    ..FORWARDING
                },
                "[networking.proxy] enabled = false",
            ),
            (
                ProxySettings {
                    velocity: false,
                    ..FORWARDING
                },
                "[networking.proxy.velocity] enabled = false",
            ),
            (
                ProxySettings {
                    velocity: false,
                    bungeecord: true,
                    ..FORWARDING
                },
                "Rift sends only Velocity modern forwarding",
            ),
        ] {
            let warning = warning(proxy).expect("a warning");
            assert!(warning.contains(reason), "{proxy:?}: {warning}");
            assert!(warning.contains("do not match the proxy's"), "{warning}");
            assert!(warning.contains("[networking.proxy.velocity]"), "{warning}");
        }
    }

    #[test]
    fn an_empty_velocity_secret_warns_about_rejected_logins() {
        let proxy = ProxySettings {
            velocity_secret: false,
            ..FORWARDING
        };
        let warning = warning(proxy).expect("a warning");
        assert!(warning.contains("empty secret"), "{warning}");
        assert!(
            warning.contains("rejects every forwarded login"),
            "{warning}"
        );
    }
}
