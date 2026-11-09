//! The service other Pumpkin plugins use: `RiftPermissions` plus the
//! Pumpkin-specific parts (a player's contexts, context calculators,
//! `/perms` forwarding).
//!
//! The service holds a trait object created inside this plugin, so every call
//! runs this plugin's code on this plugin's tokio runtime, whichever plugin
//! makes it. That matters for native plugins: each one links its own copy of
//! tokio, and a future that uses the client's timers or sockets only works on
//! the runtime of the copy that created it.

use std::{any::Any, sync::Arc};

use pumpkin::{
    entity::player::Player,
    plugin::{Context, Payload},
};
use rift_permissions::{
    Applied, Change, ChangeEvent, ContextSet, MetaView, Tristate, Uuid,
    wire::{CommandReply, GroupInfo},
};
use rift_permissions_client::{BoxFuture, Error, Listener, RiftPermissions};

use crate::{
    check::rift_uuid,
    contexts::{CalculatorFn, CalculatorHandle},
};

/// The plugin's name, as Pumpkin reports it (`plugins/data/<name>`).
pub const PLUGIN_NAME: &str = env!("CARGO_PKG_NAME");

/// The name the service is registered under with `Context::register_service`.
pub const SERVICE_NAME: &str = "rift-permissions";

/// Pumpkin downcasts services by this name, so it carries the crate version:
/// a plugin built against another version of this crate gets `None` from
/// `get_service` instead of a type with a different layout.
const SERVICE_TYPE: &str = concat!(
    "rift_permissions_pumpkin::RiftPermissionsService@",
    env!("CARGO_PKG_VERSION")
);

/// What the plugin implements behind [`RiftPermissionsService`].
pub trait PumpkinPermissions: RiftPermissions {
    /// The player's active contexts: `server`, `world`, `dimension`,
    /// `gamemode` and every calculator's custom keys.
    fn contexts(&self, player: &Arc<Player>) -> ContextSet;

    /// Register a custom context calculator until the handle drops.
    fn add_calculator(
        &self,
        name: &str,
        calculate: Box<CalculatorFn<Arc<Player>>>,
    ) -> CalculatorHandle;

    /// Recompute the player's custom contexts now and report them to the
    /// proxy if they changed, rather than at the next permission check.
    fn refresh_contexts(&self, player: &Arc<Player>);

    /// Forward a `/perms` command line, split into arguments.
    fn command(
        &self,
        actor: Option<Uuid>,
        args: Vec<String>,
    ) -> BoxFuture<'_, Result<CommandReply, Error>>;

    /// Whether the client is connected and synchronised with the proxy.
    fn is_online(&self) -> bool;
}

/// Rift permissions for other Pumpkin plugins. Obtain it with [`service`] or
/// `context.get_service::<RiftPermissionsService>(SERVICE_NAME)`.
///
/// It implements [`RiftPermissions`] (import the trait to call `check`,
/// `meta`, `primary_group`, `groups`, `group_list`, `on_change`, `apply` and
/// `revision`), and adds methods that take a Pumpkin player and fill in their
/// contexts.
///
/// Change listeners and `apply` run on this plugin's runtime: a listener must
/// not call tokio functions of the calling plugin's own copy of tokio (hand
/// the event to your own runtime with a channel or `Handle::spawn` instead).
pub struct RiftPermissionsService {
    inner: Arc<dyn PumpkinPermissions>,
}

impl RiftPermissionsService {
    pub fn new(inner: Arc<dyn PumpkinPermissions>) -> Self {
        Self { inner }
    }

    /// The service as a plain [`RiftPermissions`] trait object, for code
    /// shared with tests that use `rift_permissions_testing::TestPermissions`.
    pub fn permissions(&self) -> Arc<dyn RiftPermissions> {
        self.inner.clone()
    }

    /// See [`PumpkinPermissions::contexts`].
    pub fn contexts(&self, player: &Arc<Player>) -> ContextSet {
        self.inner.contexts(player)
    }

    /// `check` with the player's current contexts. `Undefined` means Rift has
    /// no answer and the plugin's own default applies; `player.has_permission`
    /// already includes Pumpkin's default.
    pub fn check_player(&self, player: &Arc<Player>, node: &str) -> Tristate {
        let contexts = self.inner.contexts(player);
        self.inner
            .check(rift_uuid(&player.gameprofile.id), node, &contexts)
    }

    /// `meta` with the player's current contexts.
    pub fn meta_player(&self, player: &Arc<Player>) -> Option<MetaView> {
        let contexts = self.inner.contexts(player);
        self.inner
            .meta(rift_uuid(&player.gameprofile.id), &contexts)
    }

    /// `groups` with the player's current contexts.
    pub fn groups_player(&self, player: &Arc<Player>) -> Vec<String> {
        let contexts = self.inner.contexts(player);
        self.inner
            .groups(rift_uuid(&player.gameprofile.id), &contexts)
    }

    /// Add a custom context calculator, for example a region plugin's
    /// `region` key. It runs on every permission check of every player, on
    /// the checking thread: keep it cheap and non-blocking. Keys `server`,
    /// `world`, `dimension` and `gamemode` are reserved and ignored. The
    /// result is reported to the proxy (`rift.perms.context`) when it
    /// changes; the proxy keeps only keys listed in its external contexts.
    pub fn add_context_calculator(
        &self,
        name: &str,
        calculate: impl Fn(&Arc<Player>) -> Vec<(String, String)> + Send + Sync + 'static,
    ) -> CalculatorHandle {
        self.inner.add_calculator(name, Box::new(calculate))
    }

    /// See [`PumpkinPermissions::refresh_contexts`].
    pub fn refresh_contexts(&self, player: &Arc<Player>) {
        self.inner.refresh_contexts(player);
    }

    /// Forward a `/perms` command line on behalf of `actor` (a player who
    /// holds the verb's `rift.perms.*` node on the proxy), or of this backend
    /// when `None`.
    pub fn command(
        &self,
        actor: Option<Uuid>,
        args: Vec<String>,
    ) -> BoxFuture<'_, Result<CommandReply, Error>> {
        self.inner.command(actor, args)
    }

    /// Whether the client is connected and synchronised with the proxy.
    pub fn is_online(&self) -> bool {
        self.inner.is_online()
    }
}

impl RiftPermissions for RiftPermissionsService {
    fn check(&self, uuid: Uuid, node: &str, contexts: &ContextSet) -> Tristate {
        self.inner.check(uuid, node, contexts)
    }

    fn meta(&self, uuid: Uuid, contexts: &ContextSet) -> Option<MetaView> {
        self.inner.meta(uuid, contexts)
    }

    fn primary_group(&self, uuid: Uuid) -> Option<String> {
        self.inner.primary_group(uuid)
    }

    fn groups(&self, uuid: Uuid, contexts: &ContextSet) -> Vec<String> {
        self.inner.groups(uuid, contexts)
    }

    fn group_list(&self) -> Arc<Vec<GroupInfo>> {
        self.inner.group_list()
    }

    fn on_change(&self, listener: Box<dyn Fn(&ChangeEvent) + Send + Sync>) -> Listener {
        self.inner.on_change(listener)
    }

    fn apply(&self, actor: Option<Uuid>, change: Change) -> BoxFuture<'_, Result<Applied, Error>> {
        self.inner.apply(actor, change)
    }

    fn revision(&self) -> u64 {
        self.inner.revision()
    }
}

impl Payload for RiftPermissionsService {
    fn get_name_static() -> &'static str {
        SERVICE_TYPE
    }

    fn get_name(&self) -> &'static str {
        SERVICE_TYPE
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// The service, once this plugin has loaded. `None` when it is not
/// installed, failed to load (for example without a configuration), was
/// built from another version of this crate, or has not started loading yet:
/// Pumpkin does not order native plugins, so look it up lazily (on first use
/// or first player join) rather than only in your `on_load`.
pub async fn service(context: &Context) -> Option<Arc<RiftPermissionsService>> {
    context
        .plugin_manager
        .wait_for_plugin(PLUGIN_NAME)
        .await
        .ok()?;
    context
        .get_service::<RiftPermissionsService>(SERVICE_NAME)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_permissions::{Node, SubjectRef};
    use rift_permissions_testing::TestPermissions;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// The testing crate's store behind the Pumpkin trait; the player-based
    /// methods need a running server and are not exercised here.
    struct InMemory(TestPermissions);

    impl RiftPermissions for InMemory {
        fn check(&self, uuid: Uuid, node: &str, contexts: &ContextSet) -> Tristate {
            self.0.check(uuid, node, contexts)
        }
        fn meta(&self, uuid: Uuid, contexts: &ContextSet) -> Option<MetaView> {
            self.0.meta(uuid, contexts)
        }
        fn primary_group(&self, uuid: Uuid) -> Option<String> {
            self.0.primary_group(uuid)
        }
        fn groups(&self, uuid: Uuid, contexts: &ContextSet) -> Vec<String> {
            self.0.groups(uuid, contexts)
        }
        fn group_list(&self) -> Arc<Vec<GroupInfo>> {
            self.0.group_list()
        }
        fn on_change(&self, listener: Box<dyn Fn(&ChangeEvent) + Send + Sync>) -> Listener {
            self.0.on_change(listener)
        }
        fn apply(
            &self,
            actor: Option<Uuid>,
            change: Change,
        ) -> BoxFuture<'_, Result<Applied, Error>> {
            self.0.apply(actor, change)
        }
        fn revision(&self) -> u64 {
            self.0.revision()
        }
    }

    impl PumpkinPermissions for InMemory {
        fn contexts(&self, _player: &Arc<Player>) -> ContextSet {
            ContextSet::new()
        }
        fn add_calculator(
            &self,
            _name: &str,
            _calculate: Box<CalculatorFn<Arc<Player>>>,
        ) -> CalculatorHandle {
            crate::contexts::Calculators::<u8>::default().add("unused", Box::new(|_| Vec::new()))
        }
        fn refresh_contexts(&self, _player: &Arc<Player>) {}
        fn command(
            &self,
            _actor: Option<Uuid>,
            _args: Vec<String>,
        ) -> BoxFuture<'_, Result<CommandReply, Error>> {
            Box::pin(async { Err(Error::Unavailable) })
        }
        fn is_online(&self) -> bool {
            true
        }
    }

    #[test]
    fn the_service_is_a_thin_wrapper() {
        let alice = Uuid::from_u128(1);
        let perms = TestPermissions::builder()
            .group("vip", |g| {
                g.weight(10)
                    .meta("homes", "5")
                    .permission("homes.many", true)
            })
            .user(alice, |u| u.parent("vip"))
            .server("survival")
            .build();
        let service = RiftPermissionsService::new(Arc::new(InMemory(perms)));
        let none = ContextSet::new();
        assert_eq!(service.check(alice, "homes.many", &none), Tristate::True);
        assert_eq!(
            service.check(alice, "homes.other", &none),
            Tristate::Undefined
        );
        assert_eq!(
            service
                .meta(alice, &none)
                .unwrap()
                .meta
                .get("homes")
                .map(String::as_str),
            Some("5")
        );
        assert_eq!(service.primary_group(alice).as_deref(), Some("vip"));
        assert_eq!(service.groups(alice, &none), ["vip", "default"]);
        assert!(service.group_list().iter().any(|group| group.name == "vip"));
        assert!(service.is_online());

        let seen = Arc::new(AtomicU64::new(0));
        let counter = seen.clone();
        let listener = service.on_change(Box::new(move |event| {
            counter.store(event.revision, Ordering::Relaxed);
        }));
        let change = Change::SetNode {
            subject: SubjectRef::User(alice),
            node: Node::permission("homes.other", false).unwrap(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let applied = runtime.block_on(service.apply(None, change)).unwrap();
        assert_eq!(seen.load(Ordering::Relaxed), applied.revision);
        assert_eq!(service.revision(), applied.revision);
        assert_eq!(service.check(alice, "homes.other", &none), Tristate::False);
        let plain = service.permissions();
        assert_eq!(plain.check(alice, "homes.other", &none), Tristate::False);
        drop(listener);
        assert!(matches!(
            runtime.block_on(service.command(None, vec!["info".into()])),
            Err(Error::Unavailable)
        ));
    }

    #[test]
    fn the_payload_name_carries_the_version() {
        assert!(RiftPermissionsService::get_name_static().ends_with(env!("CARGO_PKG_VERSION")));
    }
}
