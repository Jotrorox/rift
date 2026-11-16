# Permissions for Pumpkin

`rift-permissions-pumpkin` (in [`adapters/pumpkin`](../adapters/pumpkin)) is a
native [Pumpkin](https://github.com/Pumpkin-MC/Pumpkin) plugin that hands a
backend's permission checks to Rift. It runs the
[Rust client](permissions-clients.md) and answers every
`player.has_permission(...)` from its replica of the proxy's data.

| Pumpkin source | Rift's answer |
| --- | --- |
| A node set for the player (directly, through a group, in matching contexts) | `true` or `false`, replacing Pumpkin's result |
| Nothing set anywhere | Pumpkin's own result: attachments, the registered default (`Allow`, `Deny`, `Op(level)`) against the player's op level |

## Build the plugin

> [!WARNING]
> A native plugin is a Rust `cdylib` that shares Rust types with the server.
> Build it from **the same Pumpkin revision and with the same rustc** as the
> server binary, and rebuild it whenever the server changes. Pumpkin checks
> only its `PLUGIN_API_VERSION`; any other mismatch is undefined behavior.

The adapter pins Pumpkin `ae36c3a56059a82147cf52ffd55bf6e256f81107` (plugin
API version 5) and Rift's toolchain (`rust-toolchain.toml`, 1.98.1).

1. Build the adapter on its own. It is not part of the root workspace,
   because Pumpkin is a large git dependency:

   ```sh
   cd adapters/pumpkin
   cargo build --release --locked
   # target/release/librift_permissions_pumpkin.so (Linux), .dylib (macOS),
   # rift_permissions_pumpkin.dll (Windows)
   ```

2. Build the server from the same revision and toolchain:

   ```sh
   git clone https://github.com/Pumpkin-MC/Pumpkin && cd Pumpkin
   git checkout ae36c3a56059a82147cf52ffd55bf6e256f81107
   cargo +1.98.1 build --release --locked
   ```

3. Copy the library into the server's `plugins/` directory.

To update Pumpkin, change the `rev` in `adapters/pumpkin/Cargo.toml` and
rebuild both, after two steps that keep the shared types identical:

- Reseed the adapter's `Cargo.lock` from Pumpkin's lockfile, so the libraries
  both sides share resolve to the same versions.
- Re-check the feature-parity list in that manifest against the server's
  `cargo tree`. The adapter enables every cargo feature the server binary
  enables on those libraries, because a feature can change a type's layout
  (smallvec's `union`, for one).

The built library statically links Pumpkin's crates, which are licensed
GPL-3.0. The adapter's own source is BSD-2-Clause like the rest of Rift, but
a distributed plugin binary is subject to Pumpkin's license as well.

## Proxy setup

The backend authenticates on the bus as its own principal, conventionally
`server.<name>`, with exactly the grants under
[principal permissions](permissions-protocol.md#principal-permissions). Give
the server process the principal's token in an environment variable and the
proxy's CA (or certificate) as a PEM file.

### Velocity modern forwarding

The proxy pushes each player's snapshot before it connects them to the
backend, keyed by the UUID it knows them by. Pumpkin knows the player by the
same UUID only when it reads it from Velocity modern forwarding, so enable it
on both sides with the same secret.

Rift sends forwarding only in online mode, and requires the two together:

```lua
rift.config.authentication = { online_mode = true, timeout_ms = 10000 }
rift.config.forwarding = { mode = "velocity", secret_env = "RIFT_FORWARDING_SECRET" }
```

In Pumpkin's `pumpkin.toml`, with `RIFT_FORWARDING_SECRET`'s value as the
secret:

```toml
[networking.proxy]
enabled = true

[networking.proxy.velocity]
enabled = true
secret = "the same secret as RIFT_FORWARDING_SECRET"

[networking.proxy.bungeecord]
enabled = false

[networking.proxy.vine]
enabled = false
```

With forwarding on:

- Pumpkin takes the player's UUID, name, skin properties and address from the
  proxy.
- Pumpkin ignores `[networking.java] online_mode` and `encryption` for
  logins, because Rift has already verified the account.
- Direct connections that bypass the proxy are kicked, as they cannot sign
  the forwarded data.
- Vine takes precedence over Velocity in Pumpkin's login, and Rift does not
  speak it.

Pumpkin also reads BungeeCord forwarding: enable
`[networking.proxy.bungeecord]` instead and set Rift's `forwarding.mode =
"bungeecord"`. BungeeCord forwarding is unsigned, so Pumpkin must then accept
connections from Rift alone. Prefer Velocity.

### Without forwarding

Pumpkin derives an offline UUID from the name: the first 16 bytes of its
SHA-256, not vanilla's `OfflinePlayer:` MD5 that an offline-mode Rift uses.
The pushed snapshot therefore goes unused, and each login waits to fetch one
(up to `join_timeout_ms`). Permissions still work, keyed by Pumpkin's UUID.

The plugin warns about this at load. That is expected behind an offline-mode
proxy such as `rift dev`, which cannot forward, and a setup error otherwise.
An enabled Velocity section with an empty secret gets its own warning, as
Pumpkin then rejects every forwarded login.

## Configuration

On first start the plugin writes a template to
`plugins/data/rift-permissions-pumpkin/config.toml` and refuses to start
until you fill it in; Pumpkin's own permissions stay in effect meanwhile. It
reads `config.json` when there is no `config.toml`. Both hold the client's
[configuration](permissions-clients.md#configure-the-rust-client), and
unknown keys are errors.

```toml
address = "proxy.internal:7443"
server_name = "proxy.internal"
ca = "ca.pem"
token_env = "RIFT_SURVIVAL_1_TOKEN"
server = "survival-1"
join_timeout_ms = 2000
request_timeout_ms = 5000

[offline_policy]
mode = "last_snapshot"
path = "replica.json"
```

| Key | Default | Meaning |
| --- | --- | --- |
| `address` | | `host:port` of the proxy's messaging listener |
| `server_name` | | Name the proxy's certificate is verified against |
| `ca` | | PEM file with the CA or the proxy's certificate |
| `token_env` | | Environment variable holding the principal's token |
| `server` | | This backend's name: sent in `hello` and used as the `server` context |
| `group` | none | Its service group, for an instance of one (`lobby` for `lobby-2`): the `group` context |
| `join_timeout_ms` | 2000 | How long a login waits for the player's snapshot |
| `request_timeout_ms` | 5000 | How long `/perms`, writes and group requests wait |
| `offline_policy` | `{mode = "deny"}` | `deny`, `last_snapshot` or `fallback_file` with a `path`; see [offline policies](permissions-clients.md#offline-policies) |

Relative paths (`ca`, the policy's `path`) are relative to the data folder.
The template uses `last_snapshot`, so players known before a restart keep
their permissions while the proxy is away.

On a server Rift manages with a `messaging` block, leave out `address`,
`server_name`, `ca`, `token_env`, `server` and `group`. They fall back to the
`RIFT_*` variables Rift sets for each instance (see
[bus logins for instances](bus-logins.md)).
With those variables and no configuration file at all, the plugin starts on
them alone instead of writing a template.

## What the plugin does

The plugin runs the Rust client, so reconnects, retries and eviction follow
the [client lifecycle](permissions-clients.md#client-lifecycle).

**Startup.** The plugin starts the client on a small tokio runtime of its own
and logs `started as server <name>; proxy <address>, offline policy <policy>`.
It warns when Pumpkin's [Velocity forwarding](#velocity-modern-forwarding) is
off or has no secret. When the proxy has not answered within the join
timeout, it logs
`permissions unavailable: ... applying the offline policy ...` and serves the
replica meanwhile.

**Logins.** A blocking `AsyncPlayerPreLoginEvent` handler waits up to the join
timeout for the player's snapshot, which the proxy already pushed before the
transfer when Velocity forwarding is on. Without one, the offline policy
applies to that player. A login that never reaches the join is released
after 60 s.

**Checks.** A blocking `PlayerPermissionCheckEvent` handler computes the
player's contexts, asks the replica, and overwrites `event.result` only for
`true` or `false`. Nodes are passed as Pumpkin spells them
(`minecraft:command.gamemode`), so a grant of `minecraft:command.*` covers
every vanilla command.

**Contexts.** Every check uses:

| Context | Value |
| --- | --- |
| `server`, `group` | From the configuration; `group` only when set |
| `world` | The world folder, such as `world_nether` |
| `dimension` | Such as `minecraft:the_nether` |
| `gamemode` | `survival`, `creative`, `adventure` or `spectator` |
| Custom keys | From [calculators](#use-the-service-from-another-plugin) other plugins register. Reported to the proxy on `rift.perms.context` whenever they change, and again after a reconnect |

**Registered permissions.** Pumpkin's permission registry cannot be listed,
so the plugin discovers it:

- At load it looks up `minecraft:command.<name>` and `pumpkin:command.<name>`
  for every registered command.
- Afterwards it looks up each node the first time it passes through a check.
  Pumpkin checks every command's permission when it sends a joining player
  the command tree, so the first join covers nearly everything.

It announces each registered node on `rift.perms.register`, with its
description, its default (`Allow` → `true`, `Deny` → `false`, `Op(n)` →
`op:n`) and the node's namespace as `plugin`. The full set is sent each time
it grows and after every reconnect. Pumpkin's `children` are not sent.

**`/perms`.** `/perms <arguments>` (alias `/rp`) needs `rift:perms` on the
backend (default: op level 3).

- The plugin forwards it to the proxy with the player as actor, or with no
  actor from the console (which needs a principal with `control = true`).
- The proxy checks the verb's `rift.perms.*` node, applies the change and
  replies with the text to show.
- Arguments split on spaces, with double quotes grouping words:
  `/perms group vip meta setprefix 100 "&6[VIP] "`.
- The command returns at once; the reply arrives when the proxy answers.

**Leave and unload.** `PlayerLeaveEvent` marks the player as gone, and the
snapshot is evicted 10 minutes later. On unload the client disconnects and
writes the replica under `last_snapshot`, and the plugin's runtime stops.

## Use the service from another plugin

Depend on the adapter as a library, without its plugin entry points, from
the same Pumpkin revision:

```toml
[dependencies]
pumpkin = { package = "pumpkin-core", git = "https://github.com/Pumpkin-MC/Pumpkin", rev = "ae36c3a56059a82147cf52ffd55bf6e256f81107" }
rift-permissions-pumpkin = { git = "https://github.com/Jotrorox/rift", tag = "v0.1.0", default-features = false }

[dev-dependencies]
rift-permissions-testing = { git = "https://github.com/Jotrorox/rift", tag = "v0.1.0" }
```

`player.has_permission(&server, node)` already includes Rift's answer. The
service is for meta values, groups, change events, custom contexts and
writes.

Look it up with `service(&context)`, which waits for the adapter to finish
loading. Pumpkin does not order native plugins, so look it up lazily (on
first use or a player's join) rather than only in `on_load`. It is `None`
when the adapter is missing, failed to start, or was built from another
version of this crate.

```rust,ignore
use std::sync::Arc;
use pumpkin::{entity::player::Player, plugin::Context};
use rift_permissions_pumpkin::{RiftPermissionsService, rift_permissions::Tristate, service};

async fn max_homes(context: &Context, player: &Arc<Player>) -> u32 {
    let Some(perms) = service(context).await else { return 1 };
    if perms.check_player(player, "homes:unlimited") == Tristate::True {
        return u32::MAX;
    }
    perms
        .meta_player(player)
        .and_then(|meta| meta.meta.get("homes").and_then(|n| n.parse().ok()))
        .unwrap_or(1)
}

fn track_regions(perms: &RiftPermissionsService) {
    perms
        .add_context_calculator("regions", |player| {
            vec![("region".to_owned(), region_of(player).to_owned())]
        })
        .forget();
}

fn region_of(_player: &Arc<Player>) -> &'static str {
    "spawn" // your region lookup
}
```

| Method | Returns |
| --- | --- |
| `check(uuid, node, &contexts)`, `meta`, `primary_group`, `groups`, `group_list`, `on_change`, `apply(actor, change)`, `revision` | The [`RiftPermissions`](permissions-clients.md#the-riftpermissions-trait) trait; import it to call these |
| `contexts(&player)` | The player's active contexts, as the check handler uses them |
| `check_player(&player, node)`, `meta_player`, `groups_player` | The same with the player's contexts filled in |
| `add_context_calculator(name, f)` | A `CalculatorHandle`: `f(&player)` returns `(key, value)` pairs until the handle drops (`forget` keeps it) |
| `refresh_contexts(&player)` | Recompute and report the player's custom contexts now, for example when they enter a region |
| `command(actor, args)` | Forward a `/perms` command line; `Result<CommandReply, Error>` |
| `is_online()` | Whether the client is connected and synchronized |
| `permissions()` | The service as `Arc<dyn RiftPermissions>`, so code tested with `TestPermissions` takes it unchanged |

Calculators run on every permission check, on the checking thread: keep them
cheap and non-blocking. `server`, `group`, `world`, `dimension` and
`gamemode` are reserved and ignored, and the proxy keeps only keys listed in
its external contexts. A panicking calculator is logged and contributes
nothing.

> [!WARNING]
> Change listeners, and the futures of `apply` and `command`, run on the
> adapter's runtime. Every native plugin links its own copy of tokio, so a
> listener must not call tokio functions (timers, `tokio::spawn`) directly.
> Send the event to your own runtime through a channel or a stored `Handle`.
> Awaiting `apply` from your own tasks is fine.

The service type is registered under `SERVICE_NAME` (`"rift-permissions"`)
and identified by name and crate version, as Pumpkin downcasts services by
name. Pumpkin cannot unregister services, so after the adapter unloads the
service keeps answering from the last replica and writes fail with
`Unavailable`.

## Differences from the design

Pumpkin at the pinned revision differs from what the design assumed:

- **No registry walk.** `PermissionManager::registry` is public, but its map
  is private and has no iterator, so the plugin discovers nodes through
  command names and checks.
- **Priority is not applied.** `register_event` takes an `EventPriority`, but
  Pumpkin runs blocking handlers in registration order. The check handler is
  registered as `Highest` and blocking; a plugin that registers a blocking
  check handler later can still override the result.
- **Login event.** The snapshot wait hooks `AsyncPlayerPreLoginEvent` (fired
  on every Java and Bedrock login, including proxy forwarding) rather than
  `PlayerJoinEvent`, which only records that the player arrived.
- **Service lookup** is by name and type: `get_service::<T>(name)`, not
  `get_service::<RiftPermissions>()`. The service is a concrete type,
  `RiftPermissionsService`, implementing the trait.
- **Entry points are written out** instead of using `#[plugin_impl]` and
  `#[plugin_method]`, which cannot be switched off for library users. A
  `plugin` cargo feature gates the same exported symbols
  (`PUMPKIN_API_VERSION`, `METADATA`, `plugin`).
- **`/perms` permission.** `Context::register_permission` only accepts the
  plugin's own namespace (`rift-permissions-pumpkin:`), so `rift:perms` is
  registered on the permission manager directly.

## Limitations

- A node registered by a plugin after the adapter first saw it checked is not
  announced until the next server start.
- WASM plugins go through the same `PermissionManager` and check event, so
  their checks are answered too; they cannot use the service.
