# Rift permissions clients

Backends answer permission checks from a local replica of the proxy's data;
[`permissions-protocol.md`](permissions-protocol.md) specifies the bus
messages that keep it current. This page covers the three libraries that
implement it, so a plugin never speaks the protocol itself:

| Library | For | Contents |
| --- | --- | --- |
| `rift-permissions-client` (Rust) | Pumpkin plugins, Rust services | Bus connection, handshake, replica, offline policy, writes, change events |
| `rift-permissions-testing` (Rust) | Plugin unit tests | The same API in memory, with fixture builders |
| `librift_perms` (C ABI) | Go, Python, Fabric and other hosts | The client behind ten C functions with JSON at the edges |

All three sit on `rift-permissions` (the model and the resolver) and
`rift-messaging` (the QUIC client of the [messaging protocol](messaging-protocol.md)).
Rust crates are consumed as git dependencies pinned to a release tag:

```toml
[dependencies]
rift-permissions = { git = "https://github.com/Jotrorox/rift", tag = "v0.1.0" }
rift-permissions-client = { git = "https://github.com/Jotrorox/rift", tag = "v0.1.0" }

[dev-dependencies]
rift-permissions-testing = { git = "https://github.com/Jotrorox/rift", tag = "v0.1.0" }
```

## Proxy setup

Each backend authenticates as its own principal `server.<name>` with the
`backend` preset (the grants listed under
[principal permissions](permissions-protocol.md#principal-permissions)),
and gets the proxy's CA (or certificate) and its token through an
environment variable. For local development, `rift dev` starts a proxy with
such a principal and prints its token, a client configuration and the
dashboard URL (see [`rift dev`](permissions.md#rift-dev)).

## Rust client

### Configuration

`Config` deserialises from a plugin's configuration file, or is built with
`Config::new(address, server_name, ca, token_env, server)`:

```json
{"address": "proxy.internal:7443", "server_name": "proxy.internal", "ca": "certs/ca.pem",
 "token_env": "RIFT_SURVIVAL_1_TOKEN", "server": "survival-1",
 "join_timeout_ms": 2000, "request_timeout_ms": 5000,
 "offline_policy": {"mode": "last_snapshot", "path": "plugins/rift/replica.json"}}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `address` | | `host:port` of the proxy's messaging listener, resolved on every connection attempt |
| `server_name` | | Name the proxy's certificate is verified against |
| `ca` | | PEM file with the CA or the proxy's certificate |
| `token_env` | | Environment variable holding the principal's token (`Client::start_with_token` takes it directly instead) |
| `server` | | This backend's name: sent in `hello` and used as the `server` context |
| `group` | none | This backend's service group, for an instance of one: filled in as the `group` context with `server` (`Config::with_group`) |
| `join_timeout_ms` | 2000 | How long a join waits for a missing snapshot |
| `request_timeout_ms` | 5000 | How long writes, commands and group requests wait |
| `offline_policy` | `{"mode": "deny"}` | See [offline policies](#offline-policies) |

### Lifecycle

`Client::start(config)` must run inside a tokio runtime. It fails only on
unusable configuration (missing token variable, unreadable CA, invalid
fallback file); an unreachable proxy is retried in the background and the
client serves from its replica meanwhile.

In the background the client connects, subscribes `rift.perms.update`,
`rift.perms.update.>` and `rift.events.permissions`, then sends `hello`. A
replica revision that differs from the proxy's refetches every stored
snapshot with `subject.batch` (at most `limits.batch` per request); the group
table is fetched on every connection. On a lost connection it reconnects with
backoff from 0.5 s to 30 s and repeats the handshake. While disconnected it
logs `permissions unavailable` with the stale revision once a minute. A
`/perms sync` (`rift.perms.update.resync`) repeats the handshake and refetches
every stored snapshot.

Updates follow the protocol's ordering rules: a pushed snapshot replaces the
stored one only when newer; a change event at the next revision refetches the
players it lists unless their push arrives within 250 ms; a skipped revision
refetches everything. A refetch that fails is retried every 5 s.

| Call | When |
| --- | --- |
| `client.join(uuid).await` | At login. Returns at once when a push already brought the snapshot, otherwise fetches it with the join timeout. `false` means the offline policy applies to this player and the fetch is retried every 5 s |
| `client.leave(uuid)` | At logout. The snapshot is evicted 10 minutes later unless the player returns |
| `client.shutdown()` | On plugin disable. Disconnects and, with `last_snapshot`, writes the replica |

Snapshots pushed for players who have not joined this backend (the proxy
pushes ahead of a transfer) are kept for 10 minutes, so the following join
needs no request.

### The `RiftPermissions` trait

Plugins program against `RiftPermissions`, which `Client` and
`TestPermissions` implement and which works as a trait object. Reads come
from memory and never block on the network:

| Method | Returns |
| --- | --- |
| `check(uuid, node, &contexts)` | `Tristate`: `Undefined` when nothing matches or the player has no data, so the platform's default applies |
| `meta(uuid, &contexts)` | `Option<MetaView>`: prefix, suffix, meta map, primary group |
| `primary_group(uuid)` | `Option<String>` |
| `groups(uuid, &contexts)` | The player's groups in rank order |
| `group_list()` | Every group with weight, display name and decorations |
| `on_change(Box<dyn Fn(&ChangeEvent)>)` | A `Listener`; the callback runs until it is dropped (`Listener::forget` keeps it) |
| `apply(actor, change).await` | `Result<Applied, Error>` once the proxy commits; `actor` is the player acting, `None` the backend itself |
| `revision()` | The newest revision the replica has seen |

`contexts` is the player's active set; the client adds its `server` (and
`group`, when configured) when the set names none. `Client` also has `command` (forward `/perms`), `register`
(plugin permissions with defaults), `report_context` (custom contexts),
`fetch_groups` (groups with permission nodes), `on_snapshot` (a player's
snapshot changed) and `replica()` for direct reads.

Change listeners receive `rift.events.permissions` as it arrives; the
replica updates from the separate `rift.perms.update` path, so a listener
may run moments before the new snapshot of a player in `changed` is stored.
Use `on_snapshot` to react to a player's data actually changing. Listeners
run on the client's task: keep them short.

`Error` tells apart `Unavailable` (no connection, or nothing answers on the
bus), `Timeout` (the write may still have been applied: compare revisions
before retrying), `Rejected` (the proxy's error reply), `Config` and
`Transport`.

### A Pumpkin plugin

The [Pumpkin adapter](permissions-pumpkin.md) wires the client into
Pumpkin's permission events, so `player.has_permission` already answers from
the replica, and offers the trait to other plugins as a service. A plugin that wants
meta values, groups or writes holds the trait:

```rust,ignore
use std::sync::Arc;
use rift_permissions::{Change, ContextSet, Node, SubjectRef, Tristate, Uuid};
use rift_permissions_client::{Client, Config, Error, RiftPermissions};

pub struct Homes {
    pub perms: Arc<dyn RiftPermissions>,
}

impl Homes {
    /// On plugin load, inside Pumpkin's tokio runtime.
    pub fn load(config: Config) -> Result<(Self, Client), Error> {
        let client = Client::start(config)?;
        client
            .on_change(Box::new(|event| log::info!("permissions at revision {}", event.revision)))
            .forget();
        Ok((Self { perms: Arc::new(client.clone()) }, client))
    }

    /// Called with the player's contexts, for example
    /// `ContextSet::from_pairs([("world", world_name)])`.
    pub fn max_homes(&self, player: Uuid, contexts: &ContextSet) -> u32 {
        if self.perms.check(player, "homes.unlimited", contexts) == Tristate::True {
            return u32::MAX;
        }
        self.perms
            .meta(player, contexts)
            .and_then(|meta| meta.meta.get("homes").and_then(|n| n.parse().ok()))
            .unwrap_or(1)
    }

    /// `/homes vip <player>`: the proxy checks that `admin` may do this.
    pub async fn grant_vip(&self, admin: Uuid, target: Uuid) -> Result<u64, Error> {
        let change = Change::SetNode {
            subject: SubjectRef::User(target),
            node: Node::inherit("vip", true).unwrap(),
        };
        Ok(self.perms.apply(Some(admin), change).await?.revision)
    }
}
```

Call `client.join(uuid)` in the login event and `client.leave(uuid)` on
quit, and `client.shutdown()` when the plugin unloads.

### Offline policies

The policy decides what a player gets when no snapshot came from the proxy,
because it is unreachable or did not answer within the join timeout. Players
whose snapshot is already in the replica keep being served from it.

`{"mode": "deny"}` answers `Undefined` for everything, so plugin defaults and
op levels apply.

`{"mode": "last_snapshot", "path": "replica.json"}` writes the replica on
`shutdown()` and loads it on start, so players known before a restart keep
their permissions while the proxy is away. The file is the
`subject.batch` reply shape, `{"revision": 412, "subjects": [<compiled
subject>, ...]}`, written atomically. A missing or unreadable file starts
empty. Loaded snapshots are not evicted before the first successful
connection.

`{"mode": "fallback_file", "path": "fallback.json"}` loads a file the
operator writes for emergencies. It holds groups and users in the
[typed node](permissions-protocol.md#typed-node) format and is compiled
locally exactly as the proxy compiles, so inheritance, weights, contexts,
prefixes and meta all work. A player not listed gets `default`. The file is
validated on start, and a snapshot from the proxy always replaces a fallback
one.

```json
{"groups": [
   {"name": "default", "nodes": [{"kind": "permission", "key": "essentials.spawn", "value": true}]},
   {"name": "staff", "weight": 100, "nodes": [
      {"kind": "permission", "key": "essentials.*", "value": true},
      {"kind": "prefix", "priority": 100, "text": "&c[Staff] "}]}],
 "users": [
   {"uuid": "0123456789abcdef0123456789abcdef",
    "nodes": [{"kind": "inherit", "group": "staff", "value": true}]}]}
```

| Member | Fields |
| --- | --- |
| `groups[]` | `name`, `weight` (default 0), `display_name`?, `nodes` (default `[]`) |
| `users[]` | `uuid`, `primary_group`?, `nodes` (default `[]`) |

The Java client and RiftPerms read and write both files in these formats
([Java offline policy](permissions-java.md#offline-policy)).

## Testing crate

`TestPermissions` is an in-memory store with the same trait. It applies
`Change`s with the proxy's semantics (one revision per effective change,
before/after ops, atomic batches, track moves, bulk edits) and notifies
`on_change` listeners, so a plugin's logic is tested without a proxy:

```rust,ignore
use rift_permissions::{ContextSet, Uuid};
use rift_permissions_testing::TestPermissions;

#[test]
fn vip_players_get_five_homes() {
    let alice = Uuid::from_u128(1);
    let perms = TestPermissions::builder()
        .group("vip", |g| g.weight(10).meta("homes", "5").prefix(100, "&6[VIP] "))
        .user(alice, |u| u.parent("vip"))
        .track("ranks", &["default", "vip"])
        .server("survival")
        .build();
    let homes = Homes { perms: std::sync::Arc::new(perms) };
    assert_eq!(homes.max_homes(alice, &ContextSet::new()), 5);
    assert_eq!(homes.max_homes(Uuid::from_u128(2), &ContextSet::new()), 1);
}
```

Builders take `permission`, `parent`, `prefix`, `suffix`, `meta` and any
`node(Node)`, plus `weight` and `display_name` for groups and
`primary_group` for users; invalid input panics, as a broken fixture should.
`server(name)` fills in the `server` context like a backend client, and
`service_group(name)` the `group` context with it.
`apply_now(actor, change)` is the synchronous form of `apply`, and
`compiled(uuid)` returns the compiled subject the proxy would send. Every
UUID compiles, a stranger to just `default`, as on the proxy.

## C ABI

`cargo build --release -p rift-permissions-ffi` builds `librift_perms` (`.so`,
`.dylib` or `rift_perms.dll`) in `target/release`; the header is
[`crates/rift-permissions-ffi/include/rift_perms.h`](../crates/rift-permissions-ffi/include/rift_perms.h),
generated by `cbindgen` (`cbindgen --config cbindgen.toml --output
include/rift_perms.h` in that directory) and checked against the exported
symbols by the crate's tests.

```c
typedef struct rift_perms rift_perms;
rift_perms *rift_perms_connect(const char *config_json, char **err);
void        rift_perms_close(rift_perms *perms);
int         rift_perms_check(rift_perms *perms, const uint8_t *uuid, const char *node, const char *ctx_json);
char       *rift_perms_meta(rift_perms *perms, const uint8_t *uuid, const char *ctx_json);
uint64_t    rift_perms_revision(rift_perms *perms);
int         rift_perms_apply(rift_perms *perms, const char *actor, const char *change_json, char **reply_json);
void        rift_perms_subscribe(rift_perms *perms, void (*cb)(const uint8_t *uuid, uint64_t revision, void *user), void *user);
int         rift_perms_join(rift_perms *perms, const uint8_t *uuid);
void        rift_perms_leave(rift_perms *perms, const uint8_t *uuid);
void        rift_perms_free(char *text);
```

| Function | Behaviour |
| --- | --- |
| `connect` | Takes the [configuration](#configuration) as JSON; the token comes from `token_env`. Returns null on unusable configuration and stores a message in `*err` (when `err` is not null; `*err` is set to null on success) |
| `close` | Disconnects, writes the last snapshot under that policy, frees the handle |
| `check` | `uuid` points to the 16 UUID bytes (big-endian, as Java's most then least significant bits); `ctx_json` is a context object such as `{"world": "nether"}`, or null. Returns 1 true, 0 false, -1 undefined (apply your default), -2 unavailable (no data for this player, or invalid arguments) |
| `meta` | `{"prefix", "suffix", "meta", "primary_group"}`, or null without data |
| `revision` | The replica's revision; 0 for null |
| `apply` | `change_json` is a [change](permissions-protocol.md#change); `actor` is the acting player's UUID in hex, or null for the backend. Blocks until the proxy commits or the request timeout passes. Returns 0 with the applied JSON in `*reply_json`, -1 on failure with `{"error", "code"}` (`code` is a protocol error code, `unavailable` or `timeout`), -2 for invalid arguments |
| `subscribe` | Calls `cb(uuid, revision, user)` whenever a player's data in the replica changes, until `close` |
| `join`, `leave` | Login and logout, as in the Rust client: `join` blocks up to the join timeout and returns 1 with a snapshot, 0 when the offline policy applies, -2 for invalid arguments |
| `free` | Frees every string the library returns |

The library owns a small tokio runtime per handle. Every function accepts
null and malformed input, and no panic crosses the boundary. Callbacks run on
the library's thread: return quickly and do not call `join` or `apply` from
them. Warnings such as `permissions unavailable` go to stderr unless the host
process installed a Rust logger.

```c
#include <stdio.h>
#include "rift_perms.h"

static void changed(const uint8_t *uuid, uint64_t revision, void *user) {
    (void)uuid; (void)user;
    printf("a player changed at revision %llu\n", (unsigned long long)revision);
}

int main(void) {
    char *err = NULL;
    rift_perms *perms = rift_perms_connect(
        "{\"address\": \"proxy.internal:7443\", \"server_name\": \"proxy.internal\","
        " \"ca\": \"ca.pem\", \"token_env\": \"RIFT_LOBBY_TOKEN\", \"server\": \"lobby\"}", &err);
    if (perms == NULL) {
        fprintf(stderr, "rift: %s\n", err);
        rift_perms_free(err);
        return 1;
    }
    rift_perms_subscribe(perms, changed, NULL);

    uint8_t player[16] = {0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
                          0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef};
    rift_perms_join(perms, player);
    switch (rift_perms_check(perms, player, "lobby.fly", "{\"world\": \"hub\"}")) {
        case 1: puts("may fly"); break;
        case 0: puts("may not fly"); break;
        default: puts("not set: use the default"); break;
    }

    /* As the backend itself, which needs a principal with control = true. */
    char *reply = NULL;
    if (rift_perms_apply(perms, NULL,
            "{\"op\": \"set_node\", \"subject\": {\"user\": \"0123456789abcdef0123456789abcdef\"},"
            " \"node\": {\"kind\": \"meta\", \"key\": \"lobby-visits\", \"value\": \"1\"}}",
            &reply) == 0) {
        printf("applied: %s\n", reply);
    }
    rift_perms_free(reply);

    rift_perms_leave(perms, player);
    rift_perms_close(perms);
    return 0;
}
```

Link with `-lrift_perms` (and an rpath or the library next to the program).
