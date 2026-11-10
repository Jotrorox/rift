//! The Pumpkin plugin: the exported entry points, the event handlers,
//! `/perms`, and the backend behind [`RiftPermissionsService`].
//!
//! Everything that touches the network runs on the plugin's own tokio
//! runtime. A native plugin links its own copy of tokio, whose timers and
//! sockets do not work on the server's runtime, so futures that need them are
//! spawned there and only their `JoinHandle`s (which work on any executor)
//! are awaited from Pumpkin's tasks.

use std::{
    io,
    sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, RwLock},
    time::Duration,
};

use pumpkin::{
    command::{
        CommandSender,
        argument_builder::{ArgumentBuilder, CommandArgumentBuilder, argument, command},
        argument_types::core::string::StringArgumentType,
        context::command_context::CommandContext,
        node::{CommandExecutor, CommandExecutorResult},
    },
    entity::player::Player,
    plugin::{
        BoxFuture as PumpkinFuture, Cancellable, Context, EventHandler, EventPriority,
        PLUGIN_API_VERSION, Plugin, PluginFuture, PluginMetadata,
        player::{
            async_player_pre_login::AsyncPlayerPreLoginEvent, player_join::PlayerJoinEvent,
            player_leave::PlayerLeaveEvent, player_permission_check::PlayerPermissionCheckEvent,
        },
    },
    server::Server,
};
use pumpkin_util::{
    PermissionLvl,
    permission::{Permission, PermissionDefault, PermissionManager},
    text::{TextComponent, color::NamedColor},
};
use rift_permissions::{
    Applied, Change, ChangeEvent, ContextSet, MetaView, Tristate, Uuid,
    wire::{CommandReply, GroupInfo},
};
use rift_permissions_client::{BoxFuture, Client, Error, Listener, RiftPermissions};
use tokio::{
    runtime::{Handle, Runtime},
    task::JoinHandle,
    time::sleep,
};

use crate::{
    check::{decide, rift_uuid},
    command::{error_message, plain_text, split_args},
    config,
    contexts::{CalculatorFn, CalculatorHandle, Calculators, PlayerState, Reported, base_contexts},
    forwarding::{self, ProxySettings},
    logging,
    presence::Presence,
    registry::{self, Registrations},
    service::{PLUGIN_NAME, PumpkinPermissions, RiftPermissionsService, SERVICE_NAME},
};

/// The permission `/perms` requires on this backend. The proxy additionally
/// checks the verb's `rift.perms.*` node for the player.
pub const PERMS_PERMISSION: &str = "rift:perms";
const PERMS_ARGS: &str = "arguments";
/// How long a player may take from pre-login to joining before their
/// snapshot is released.
const LOGIN_GRACE: Duration = Duration::from_secs(60);
/// How often the maintenance task looks for reconnects.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(5);
/// How long newly discovered permissions are collected before announcing.
const REGISTER_DEBOUNCE: Duration = Duration::from_secs(1);

/// Checked by Pumpkin's loader against its own `PLUGIN_API_VERSION`.
#[unsafe(no_mangle)]
pub static PUMPKIN_API_VERSION: u32 = PLUGIN_API_VERSION;

/// Read by Pumpkin's loader before `plugin()` is called.
#[unsafe(no_mangle)]
pub static METADATA: LazyLock<PluginMetadata> = LazyLock::new(|| PluginMetadata {
    name: PLUGIN_NAME.to_owned(),
    version: env!("CARGO_PKG_VERSION").to_owned(),
    authors: Vec::new(),
    description: env!("CARGO_PKG_DESCRIPTION").to_owned(),
    dependencies: Vec::new(),
    permissions: Vec::new(),
});

/// The plugin factory Pumpkin's loader calls.
#[unsafe(no_mangle)]
pub fn plugin() -> Box<dyn Plugin> {
    Box::new(RiftPermissionsPlugin::default())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The plugin instance Pumpkin holds.
#[derive(Default)]
pub struct RiftPermissionsPlugin {
    running: Mutex<Option<Running>>,
}

struct Running {
    runtime: Runtime,
    backend: Arc<Backend>,
}

impl Plugin for RiftPermissionsPlugin {
    fn on_load(&self, context: Arc<Context>) -> PluginFuture<'_, Result<(), String>> {
        Box::pin(async move {
            self.load(&context).await.inspect_err(|error| {
                log::error!("rift-permissions: not started: {error}");
            })
        })
    }

    fn on_unload(&self, _context: Arc<Context>) -> PluginFuture<'_, Result<(), String>> {
        Box::pin(async move {
            self.unload();
            Ok(())
        })
    }
}

impl RiftPermissionsPlugin {
    async fn load(&self, context: &Arc<Context>) -> Result<(), String> {
        context.init_log();
        logging::install(log::LevelFilter::Info);

        let config = config::load(&context.get_data_folder()).map_err(|e| e.to_string())?;
        let address = config.address.clone();
        let server = config.server.clone();
        let group = config.group.clone();
        let join_timeout = config.join_timeout;
        let policy = config::describe_policy(&config.offline_policy);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("rift-permissions")
            .enable_all()
            .build()
            .map_err(|e| format!("starting the runtime: {e}"))?;
        let client = {
            let _runtime = runtime.enter();
            Client::start(config).map_err(|e| e.to_string())?
        };
        let backend = Arc::new(Backend {
            client,
            runtime: runtime.handle().clone(),
            server: server.clone(),
            group,
            calculators: Calculators::default(),
            reported: Reported::default(),
            registrations: RwLock::default(),
            discovered: tokio::sync::Notify::new(),
            permission_manager: context.permission_manager.clone(),
            presence: Mutex::default(),
        });

        // Registered on the manager directly: `Context::register_permission`
        // insists on the `rift-permissions-pumpkin:` namespace.
        let _already = context
            .permission_manager
            .register_permission(Permission::new(
                PERMS_PERMISSION,
                "Use /perms, which the Rift proxy executes",
                PermissionDefault::Op(PermissionLvl::Three),
            ));

        // Pumpkin runs blocking handlers in registration order and does not
        // sort by priority yet; `Highest` documents the intent.
        context.register_event::<PlayerPermissionCheckEvent, _>(
            Arc::new(CheckHandler(backend.clone())),
            EventPriority::Highest,
            true,
        );
        context.register_event::<AsyncPlayerPreLoginEvent, _>(
            Arc::new(PreLoginHandler(backend.clone())),
            EventPriority::Highest,
            true,
        );
        context.register_event::<PlayerJoinEvent, _>(
            Arc::new(JoinHandler(backend.clone())),
            EventPriority::Highest,
            false,
        );
        context.register_event::<PlayerLeaveEvent, _>(
            Arc::new(LeaveHandler(backend.clone())),
            EventPriority::Lowest,
            false,
        );
        context.register_command_with_aliases(
            perms_command(&backend),
            &["rp".to_owned()],
            PERMS_PERMISSION,
        );
        context
            .register_service(
                SERVICE_NAME,
                Arc::new(RiftPermissionsService::new(backend.clone())),
            )
            .await;

        backend.observe(PERMS_PERMISSION);
        let commands: Vec<String> = context
            .server
            .command_dispatcher
            .load()
            .get_all_commands()
            .keys()
            .map(|name| (*name).to_owned())
            .collect();
        for name in commands {
            for node in registry::command_candidates(&name) {
                backend.observe(&node);
            }
        }
        // Players already online when the plugin loads late.
        for player in context.server.get_all_players() {
            let uuid = rift_uuid(&player.gameprofile.id);
            lock(&backend.presence).joined(uuid);
            backend.spawn_join(uuid);
        }

        runtime.spawn(backend.clone().maintain());
        runtime.spawn(
            backend
                .clone()
                .report_status(join_timeout, address.clone(), policy.clone()),
        );
        log::info!(
            "rift-permissions: started as server {server}; proxy {address}, offline policy {policy}"
        );
        let proxy = &context.server.advanced_config.networking.proxy;
        if let Some(warning) = forwarding::warning(ProxySettings {
            enabled: proxy.enabled,
            velocity: proxy.velocity.enabled,
            velocity_secret: !proxy.velocity.secret.is_empty(),
            bungeecord: proxy.bungeecord.enabled,
        }) {
            log::warn!("rift-permissions: {warning}");
        }
        *lock(&self.running) = Some(Running { runtime, backend });
        Ok(())
    }

    fn unload(&self) {
        let Some(Running { runtime, backend }) = lock(&self.running).take() else {
            return;
        };
        if let Err(error) = backend.client.shutdown() {
            log::warn!("rift-permissions: {error}");
        }
        drop(backend);
        // Wait for the runtime's threads before Pumpkin unloads the library.
        runtime.shutdown_timeout(Duration::from_secs(1));
        log::info!("rift-permissions: stopped");
    }
}

/// The client plus the Pumpkin-side state; implements the service.
struct Backend {
    client: Client,
    runtime: Handle,
    server: String,
    /// The `group` context, for an instance of a service group.
    group: Option<String>,
    calculators: Calculators<Arc<Player>>,
    reported: Reported,
    registrations: RwLock<Registrations>,
    /// Woken when a new registered permission is discovered.
    discovered: tokio::sync::Notify,
    permission_manager: Arc<PermissionManager>,
    presence: Mutex<Presence>,
}

impl Backend {
    /// The player's active contexts; reports changed custom contexts.
    fn player_contexts(&self, player: &Arc<Player>) -> ContextSet {
        let world = player.world();
        let state = PlayerState {
            world: world.get_world_name(),
            dimension: world.dimension.minecraft_name,
            gamemode: player.gamemode.load(),
        };
        let base = base_contexts(&self.server, self.group.as_deref(), &state);
        let custom = self.calculators.calculate(player);
        let uuid = rift_uuid(&player.gameprofile.id);
        if self.reported.update(uuid, &custom) {
            let client = self.client.clone();
            let report = custom.clone();
            self.runtime.spawn(async move {
                if let Err(error) = client.report_context(uuid, report).await {
                    log::debug!("rift-permissions: reporting contexts for {uuid}: {error}");
                }
            });
        }
        if custom.is_empty() {
            base
        } else {
            base.union(&custom)
        }
    }

    /// Look `node` up in Pumpkin's registry the first time it is seen and
    /// queue it for `rift.perms.register`.
    fn observe(&self, node: &str) {
        let registrations = || {
            self.registrations
                .write()
                .unwrap_or_else(PoisonError::into_inner)
        };
        if self
            .registrations
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .was_seen(node)
            || registrations().seen(node)
        {
            return;
        }
        let Some(registration) = self
            .permission_manager
            .get_permission(node)
            .and_then(|permission| registry::registration(&permission))
        else {
            return;
        };
        if registrations().add(registration) {
            self.discovered.notify_one();
        }
    }

    /// Fetch a player's snapshot in the background.
    fn spawn_join(&self, uuid: Uuid) -> JoinHandle<bool> {
        let client = self.client.clone();
        self.runtime.spawn(async move { client.join(uuid).await })
    }

    /// Release the snapshot of a login that does not reach the join.
    fn watch_login(self: &Arc<Self>, uuid: Uuid) {
        let backend = self.clone();
        self.runtime.spawn(async move {
            sleep(LOGIN_GRACE).await;
            if lock(&backend.presence).abandoned(uuid) {
                backend.client.leave(uuid);
            }
        });
    }

    /// Announce registered permissions when they change or after a
    /// reconnect, and repeat custom context reports after a reconnect.
    async fn maintain(self: Arc<Self>) {
        let mut online = false;
        loop {
            tokio::select! {
                () = self.discovered.notified() => sleep(REGISTER_DEBOUNCE).await,
                () = sleep(MAINTENANCE_INTERVAL) => {}
            }
            let now_online = self.client.is_online();
            let reconnected = now_online && !online;
            online = now_online;
            if !online {
                continue;
            }
            if reconnected {
                self.registrations
                    .write()
                    .unwrap_or_else(PoisonError::into_inner)
                    .mark_dirty();
                for (uuid, contexts) in self.reported.all() {
                    if let Err(error) = self.client.report_context(uuid, contexts).await {
                        log::debug!("rift-permissions: reporting contexts for {uuid}: {error}");
                    }
                }
            }
            let batch = self
                .registrations
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .take_batch();
            if let Some(batch) = batch {
                match self.client.register(&batch).await {
                    Ok(()) => log::info!(
                        "rift-permissions: announced {} registered permissions to the proxy",
                        batch.len()
                    ),
                    Err(error) => {
                        log::warn!("rift-permissions: announcing registered permissions: {error}");
                        self.registrations
                            .write()
                            .unwrap_or_else(PoisonError::into_inner)
                            .mark_dirty();
                    }
                }
            }
        }
    }

    /// Say once at startup whether the proxy answered within the join
    /// timeout; the client keeps logging while it stays unreachable.
    async fn report_status(self: Arc<Self>, wait: Duration, address: String, policy: String) {
        if self.client.wait_online(wait).await {
            log::info!(
                "rift-permissions: synchronised with the proxy at revision {}",
                self.client.revision()
            );
        } else {
            log::warn!(
                "rift-permissions: permissions unavailable: the proxy at {address} did not answer within {} ms; \
                 serving the replica (revision {}) and applying the offline policy {policy} to players without a snapshot",
                wait.as_millis(),
                self.client.revision()
            );
        }
    }

    /// Run a client future on the plugin's runtime and await its result from
    /// any executor.
    fn on_runtime<T: Send + 'static>(
        &self,
        future: impl Future<Output = Result<T, Error>> + Send + 'static,
    ) -> BoxFuture<'_, Result<T, Error>> {
        let task = self.runtime.spawn(future);
        Box::pin(async move {
            task.await
                .unwrap_or_else(|error| Err(Error::Transport(io::Error::other(error))))
        })
    }
}

impl RiftPermissions for Backend {
    fn check(&self, uuid: Uuid, node: &str, contexts: &ContextSet) -> Tristate {
        self.client.check(uuid, node, contexts)
    }

    fn meta(&self, uuid: Uuid, contexts: &ContextSet) -> Option<MetaView> {
        self.client.meta(uuid, contexts)
    }

    fn primary_group(&self, uuid: Uuid) -> Option<String> {
        self.client.primary_group(uuid)
    }

    fn groups(&self, uuid: Uuid, contexts: &ContextSet) -> Vec<String> {
        self.client.groups(uuid, contexts)
    }

    fn group_list(&self) -> Arc<Vec<GroupInfo>> {
        self.client.group_list()
    }

    fn on_change(&self, listener: Box<dyn Fn(&ChangeEvent) + Send + Sync>) -> Listener {
        self.client.on_change(listener)
    }

    fn apply(&self, actor: Option<Uuid>, change: Change) -> BoxFuture<'_, Result<Applied, Error>> {
        let client = self.client.clone();
        self.on_runtime(async move { client.apply(actor, change).await })
    }

    fn revision(&self) -> u64 {
        self.client.revision()
    }
}

impl PumpkinPermissions for Backend {
    fn contexts(&self, player: &Arc<Player>) -> ContextSet {
        self.player_contexts(player)
    }

    fn add_calculator(
        &self,
        name: &str,
        calculate: Box<CalculatorFn<Arc<Player>>>,
    ) -> CalculatorHandle {
        self.calculators.add(name, calculate)
    }

    fn refresh_contexts(&self, player: &Arc<Player>) {
        self.player_contexts(player);
    }

    fn command(
        &self,
        actor: Option<Uuid>,
        args: Vec<String>,
    ) -> BoxFuture<'_, Result<CommandReply, Error>> {
        let client = self.client.clone();
        self.on_runtime(async move { client.command(actor, args).await })
    }

    fn is_online(&self) -> bool {
        self.client.is_online()
    }
}

/// Answers every permission check from the replica.
struct CheckHandler(Arc<Backend>);

impl EventHandler<PlayerPermissionCheckEvent> for CheckHandler {
    fn handle_blocking<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a mut PlayerPermissionCheckEvent,
    ) -> PumpkinFuture<'a, ()> {
        let backend = &self.0;
        let contexts = backend.player_contexts(&event.player);
        let uuid = rift_uuid(&event.player.gameprofile.id);
        let answer = backend.client.check(uuid, &event.permission, &contexts);
        event.result = decide(answer, event.result);
        backend.observe(&event.permission);
        Box::pin(std::future::ready(()))
    }
}

/// Makes the login wait (up to the join timeout) for the player's snapshot.
struct PreLoginHandler(Arc<Backend>);

impl EventHandler<AsyncPlayerPreLoginEvent> for PreLoginHandler {
    fn handle_blocking<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a mut AsyncPlayerPreLoginEvent,
    ) -> PumpkinFuture<'a, ()> {
        let uuid = rift_uuid(&event.player_uuid);
        let cancelled = event.cancelled();
        Box::pin(async move {
            if cancelled {
                return;
            }
            let backend = &self.0;
            lock(&backend.presence).logging_in(uuid);
            // `false` (no snapshot in time) is logged by the client, which
            // applies the offline policy and keeps retrying.
            let _snapshot = backend.spawn_join(uuid).await;
            backend.watch_login(uuid);
        })
    }
}

struct JoinHandler(Arc<Backend>);

impl EventHandler<PlayerJoinEvent> for JoinHandler {
    fn handle<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a PlayerJoinEvent,
    ) -> PumpkinFuture<'a, ()> {
        let backend = &self.0;
        let uuid = rift_uuid(&event.player.gameprofile.id);
        if !lock(&backend.presence).joined(uuid) {
            // No pre-login seen here (the plugin loaded meanwhile).
            drop(backend.spawn_join(uuid));
        }
        backend.player_contexts(&event.player);
        Box::pin(std::future::ready(()))
    }
}

struct LeaveHandler(Arc<Backend>);

impl EventHandler<PlayerLeaveEvent> for LeaveHandler {
    fn handle<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a PlayerLeaveEvent,
    ) -> PumpkinFuture<'a, ()> {
        let backend = &self.0;
        let uuid = rift_uuid(&event.player.gameprofile.id);
        lock(&backend.presence).left(uuid);
        backend.client.leave(uuid);
        backend.reported.forget(uuid);
        Box::pin(std::future::ready(()))
    }
}

/// `/perms [arguments...]`, forwarded to the proxy.
fn perms_command(backend: &Arc<Backend>) -> CommandArgumentBuilder {
    command("perms", "Manage Rift permissions (executed by the proxy)")
        .executes(PermsCommand {
            backend: backend.clone(),
            with_args: false,
        })
        .then(
            argument(PERMS_ARGS, StringArgumentType::GreedyPhrase).executes(PermsCommand {
                backend: backend.clone(),
                with_args: true,
            }),
        )
}

struct PermsCommand {
    backend: Arc<Backend>,
    with_args: bool,
}

impl CommandExecutor for PermsCommand {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        let args = if self.with_args {
            split_args(StringArgumentType::get(context, PERMS_ARGS)?)
        } else {
            Vec::new()
        };
        let source = context.source.clone();
        let actor = match &source.output {
            CommandSender::Player(player) => Some(rift_uuid(&player.gameprofile.id)),
            CommandSender::Console => None,
            _ => {
                source.send_error(TextComponent::text(
                    "/perms can only be used by players and the console",
                ));
                return Ok(0);
            }
        };
        let client = self.backend.client.clone();
        // The reply arrives later; the command returns at once so the
        // server thread never waits on the network.
        self.backend.runtime.spawn(async move {
            let message = match client.command(actor, args).await {
                Ok(reply) => reply_component(&reply),
                Err(error) => {
                    TextComponent::text(error_message(&error)).color_named(NamedColor::Red)
                }
            };
            source.send_message(message);
        });
        Ok(1)
    }
}

/// The proxy's Adventure component as Pumpkin text, or its plain text when
/// Pumpkin cannot read it.
fn reply_component(reply: &CommandReply) -> TextComponent {
    serde_json::from_value::<TextComponent>(reply.component.clone())
        .unwrap_or_else(|_| TextComponent::text(plain_text(&reply.component)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exports_match_pumpkins_loader() {
        assert_eq!(PUMPKIN_API_VERSION, PLUGIN_API_VERSION);
        assert_eq!(METADATA.name, "rift-permissions-pumpkin");
        assert!(METADATA.dependencies.is_empty());
        // No WASI host permissions: loading must not prompt the operator.
        assert!(METADATA.permissions.is_empty());
    }

    #[test]
    fn proxy_replies_become_pumpkin_text() {
        let reply = CommandReply {
            revision: 3,
            component: json!({"text": "Set fly to true for Alice", "color": "green"}),
        };
        assert_eq!(
            reply_component(&reply).get_text(),
            "Set fly to true for Alice"
        );
        let odd = CommandReply {
            revision: 3,
            component: json!({"text": "Done", "color": 7}),
        };
        assert_eq!(reply_component(&odd).get_text(), "Done");
    }
}
