# Rift permissions on Pumpkin

`rift-permissions-pumpkin` (in [`adapters/pumpkin`](../adapters/pumpkin)) is a
native [Pumpkin](https://github.com/Pumpkin-MC/Pumpkin) plugin that hands a
backend's permission checks to Rift. It runs the
[Rust client](permissions-clients.md#rust-client), keeps a replica of the
proxy's data, and answers every `player.has_permission(...)` from it; `/perms`
typed on the backend is executed by the proxy. Other Pumpkin plugins get the
same data through a service.

| Pumpkin source | Rift's answer |
| --- | --- |
| A node set for the player (directly, through a group, in matching contexts) | `true` or `false`, replacing Pumpkin's result |
| Nothing set anywhere | Pumpkin's own result: attachments, the registered default (`Allow`, `Deny`, `Op(level)`) against the player's op level |

## Building

A native plugin is a Rust `cdylib` that shares Rust types with the server, so
it must be built from **the same Pumpkin revision and with the same rustc** as
the server binary; Pumpkin checks only its `PLUGIN_API_VERSION`, and any other
mismatch is undefined behaviour. The adapter pins Pumpkin
`ae36c3a56059a82147cf52ffd55bf6e256f81107` (plugin API version 5) and Rift's
toolchain (`rust-toolchain.toml`, 1.98.1). Its `Cargo.lock` started from
Pumpkin's lockfile at that revision, so the libraries both sides share resolve
to the same versions, and its manifest enables every cargo feature the server
binary enables on those libraries, because a feature can change a type's
layout (smallvec's `union`, for one).

The adapter is not part of the root workspace (Pumpkin is a large git
dependency); build it on its own:

```sh
cd adapters/pumpkin
cargo build --release --locked
# target/release/librift_permissions_pumpkin.so (Linux), .dylib (macOS),
# rift_permissions_pumpkin.dll (Windows)
```

And the server from the same revision and toolchain:

```sh
git clone https://github.com/Pumpkin-MC/Pumpkin && cd Pumpkin
git checkout ae36c3a56059a82147cf52ffd55bf6e256f81107
cargo +1.98.1 build --release --locked
```

The built library statically links Pumpkin's crates, which are licensed
GPL-3.0; the adapter's own source is BSD-2-Clause like the rest of Rift, but
a distributed plugin binary is subject to Pumpkin's license as well.

Copy the library into the server's `plugins/` directory. Updating Pumpkin
means changing the `rev` in `adapters/pumpkin/Cargo.toml`, reseeding the
lockfile from Pumpkin's, re-checking the feature-parity list in that manifest
against the server's `cargo tree`, and rebuilding both.

## Proxy setup

The backend authenticates on the bus as its own principal, conventionally
`server.<name>`, with exactly the grants under
[principal permissions](permissions-protocol.md#principal-permissions). Give
the server process the principal's token in an environment variable and the
proxy's CA (or certificate) as a PEM file.

### Velocity modern forwarding

The proxy pushes each player's snapshot keyed by the UUID it knows them by,
before it connects them to the backend. Pumpkin knows the player by the same
UUID only when it reads it from Velocity modern forwarding, so enable it on
both sides with the same secret. Rift sends forwarding only in online mode,
and requires the two together:

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

Pumpkin then takes the player's UUID, name, skin properties and address from
the proxy, and ignores `[networking.java] online_mode` and `encryption` for
logins (Rift has already verified the account). Direct connections that
bypass the proxy are kicked, as they cannot sign the forwarded data. Vine
takes precedence over Velocity in Pumpkin's login and Rift does not speak it.

Pumpkin also reads BungeeCord forwarding: enable
`[networking.proxy.bungeecord]` instead and set Rift's `forwarding.mode =
"bungeecord"`. BungeeCord forwarding is unsigned, so Pumpkin must then accept
connections from Rift alone; prefer Velocity.

Without forwarding Pumpkin derives an offline UUID from the name
(the first 16 bytes of its SHA-256, not vanilla's `OfflinePlayer:` MD5 that an
offline-mode Rift uses), so the pushed snapshot goes unused and each login
waits to fetch one (up to `join_timeout_ms`). Permissions still work, keyed by
Pumpkin's UUID. The plugin warns about this at load: expected behind an
offline-mode proxy such as `rift dev`, which cannot forward, and a setup error
otherwise. An enabled Velocity section with an empty secret gets its own
warning, as Pumpkin then rejects every forwarded login.

## Configuration

On first start the plugin writes a template to
`plugins/data/rift-permissions-pumpkin/config.toml` and refuses to start
(Pumpkin's own permissions stay in effect) until it is filled in. It also
reads `config.json` when there is no `config.toml`. Both hold the client's
[configuration](permissions-clients.md#configuration); unknown keys are
errors.

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
| `join_timeout_ms` | 2000 | How long a login waits for the player's snapshot |
| `request_timeout_ms` | 5000 | How long `/perms`, writes and group requests wait |
| `offline_policy` | `{mode = "deny"}` | `deny`, `last_snapshot` or `fallback_file` with a `path`; see [offline policies](permissions-clients.md#offline-policies) |

Relative paths (`ca`, the policy's `path`) are relative to the data folder.
The template uses `last_snapshot`, so players known before a restart keep
their permissions while the proxy is away.

## What the plugin does

**Startup.** It starts the client on a small tokio runtime of its own, logs
`started as server <name>; proxy <address>, offline policy <policy>`, warns
when Pumpkin's [Velocity forwarding](#velocity-modern-forwarding) is off or
has no secret, and, when the proxy has not answered within the join
timeout, `permissions unavailable: ... applying the offline policy ...`. The
client keeps reconnecting with backoff and serves the replica meanwhile.

**Checks.** A blocking `PlayerPermissionCheckEvent` handler computes the
player's contexts, asks the replica, and overwrites `event.result` only for
`true` or `false`. Nodes are passed as Pumpkin spells them
(`minecraft:command.gamemode`), so a grant of `minecraft:command.*` covers
every vanilla command.

**Contexts.** Every check uses `server` (from the configuration), `world`
(the world folder, such as `world_nether`), `dimension` (such as
`minecraft:the_nether`) and `gamemode` (`survival`, `creative`, `adventure`,
`spectator`), plus custom keys from calculators other plugins register (see
below). Custom keys are reported to the proxy on `rift.perms.context`
whenever they change, and again after a reconnect.

**Logins.** A blocking `AsyncPlayerPreLoginEvent` handler waits up to the join
timeout for the player's snapshot (already pushed by the proxy before the
transfer when [Velocity forwarding](#velocity-modern-forwarding) is on);
without one the offline policy applies to that player and the client retries
every 5 s. `PlayerLeaveEvent` marks the player as gone, and the
snapshot is evicted 10 minutes later. A login that never reaches the join is
released after 60 s.

**Registered permissions.** Pumpkin's permission registry cannot be listed,
so the plugin discovers it: at load it looks up `minecraft:command.<name>` and
`pumpkin:command.<name>` for every registered command, and afterwards it looks
up each node the first time it passes through a check. Pumpkin checks every
command's permission when it sends a joining player the command tree, so the
first join covers nearly everything. Each registered node, with its
description and default (`Allow` → `true`, `Deny` → `false`, `Op(n)` →
`op:n`), is announced on `rift.perms.register` with the node's namespace as
`plugin`; the full set is sent each time it grows and after every reconnect.
Pumpkin's `children` are not sent.

**`/perms`.** `/perms <arguments>` (alias `/rp`) needs `rift:perms` on the
backend (default: op level 3) and is forwarded to the proxy with the player
as actor, or with no actor from the console (which needs a principal with
`control = true`). The proxy checks the verb's `rift.perms.*` node, applies
the change and replies with the text to show. Arguments split on spaces, with
double quotes grouping words: `/perms group vip meta setprefix 100 "&6[VIP] "`.
The command returns at once; the reply arrives when the proxy answers.

**Unload.** The client disconnects, writes the replica under `last_snapshot`,
and the plugin's runtime stops.

## For plugin authors

Depend on the adapter as a library, without its plugin entry points, from
the same Pumpkin revision:

```toml
[dependencies]
pumpkin = { package = "pumpkin-core", git = "https://github.com/Pumpkin-MC/Pumpkin", rev = "ae36c3a56059a82147cf52ffd55bf6e256f81107" }
rift-permissions-pumpkin = { git = "https://github.com/Jotrorox/rift", tag = "v0.1.0", default-features = false }

[dev-dependencies]
rift-permissions-testing = { git = "https://github.com/Jotrorox/rift", tag = "v0.1.0" }
```

`player.has_permission(&server, node)` already includes Rift's answer; the
service is for meta values, groups, change events, custom contexts and
writes. Look it up with `service(&context)`, which waits for the adapter to
finish loading. Pumpkin does not order native plugins, so look it up lazily
(on first use or a player's join) rather than only in `on_load`; it is `None`
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
| `is_online()` | Whether the client is connected and synchronised |
| `permissions()` | The service as `Arc<dyn RiftPermissions>`, so code tested with `TestPermissions` takes it unchanged |

Calculators run on every permission check, on the checking thread: keep them
cheap and non-blocking. `server`, `world`, `dimension` and `gamemode` are
reserved and ignored; the proxy keeps only keys listed in its external
contexts. A panicking calculator is logged and contributes nothing.

Change listeners, and the futures of `apply` and `command`, run on the
adapter's runtime. Every native plugin links its own copy of tokio, so a
listener must not call tokio functions (timers, `tokio::spawn`) directly: send
the event to your own runtime through a channel or a stored `Handle`. Awaiting
`apply` from your own tasks is fine.

The service type is registered under `SERVICE_NAME` (`"rift-permissions"`)
and identified by name and crate version, as Pumpkin downcasts services by
name. Pumpkin cannot unregister services, so after the adapter unloads the
service keeps answering from the last replica and writes fail with
`Unavailable`.

## Differences from the design

Pumpkin at the pinned revision differs from what the design assumed:

- **No registry walk.** `PermissionManager::registry` is public, but its map
  is private and has no iterator, so the plugin discovers registered nodes
  through command names and checks instead of walking the registry.
- **Priority is not applied.** `register_event` takes an `EventPriority`, but
  Pumpkin runs blocking handlers in registration order. The check handler is
  registered as `Highest` and blocking; another plugin that registers a
  blocking check handler later can still override the result.
- **Login event.** The snapshot wait hooks `AsyncPlayerPreLoginEvent` (fired
  on every Java and Bedrock login, including proxy forwarding) rather than
  `PlayerJoinEvent`, which only records that the player arrived.
- **Services are looked up by name and type**: `get_service::<T>(name)`, not
  `get_service::<RiftPermissions>()`. The service is a concrete type,
  `RiftPermissionsService`, implementing the trait.
- **Entry points are written out** instead of using `#[plugin_impl]` and
  `#[plugin_method]`, which keep methods in proc-macro global state, expect
  the plugin struct at the crate root and cannot be switched off for library
  users; the exported symbols (`PUMPKIN_API_VERSION`, `METADATA`, `plugin`)
  are the same, and a `plugin` cargo feature gates them.
- **`/perms` permission.** `Context::register_permission` only accepts the
  plugin's own namespace (`rift-permissions-pumpkin:`), so `rift:perms` is
  registered on the permission manager directly.

## Limitations

- A node registered by a plugin after the adapter first saw it checked is not
  announced until the next server start.
- Pumpkin's native plugin ABI is the server's Rust ABI: rebuild the adapter
  whenever the server changes, and keep both on one toolchain.
- WASM plugins go through the same `PermissionManager` and check event, so
  their checks are answered too; they cannot use the service.
