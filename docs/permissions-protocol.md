# Rift permissions protocol, version 1

Backends keep a replica of the proxy's permission data and answer
`hasPermission` from memory. This document is what a client (the Rust crate
`rift-permissions-client`, the Java `rift-permissions-client`, the C ABI, or
anything else) implements: the bus subjects, every JSON shape, the check
algorithm a replica runs, and the revision rules that keep replicas current.

The Rust types in `crates/rift-permissions` (`wire`, `change`, `compiled`,
`check`) are the reference implementation. [Permissions clients](permissions-clients.md)
covers the ready-made Rust client, the in-memory testing crate and the C ABI. Every shape below has a test case
in [`tests/fixtures/permissions`](../tests/fixtures/permissions/README.md),
which every implementation runs.

## Transport

The protocol runs on Rift's [messaging bus](messaging.md) with the
[QUIC wire protocol](messaging-protocol.md); there is no separate listener.
A backend authenticates once as its own principal, conventionally
`server.<name>`, then uses REQUEST for request/reply subjects, PUBLISH or
PUBLISH_ACK for one-way messages, and SUBSCRIBE for pushes.

Every payload is a UTF-8 JSON document, at most 1,048,576 bytes like any bus
payload. Readers must ignore unknown object members, so later versions can add
fields. Integers are JSON numbers without fractions; revisions and Unix times
fit in an unsigned 64-bit integer.

An ACL denial or a malformed frame is reported by the bus itself (an ERROR
frame, code 1 for permission), not by a JSON reply.

### Principal permissions

Give a backend principal the `backend` preset, which grants exactly what the
protocol needs:

```lua
{ name = "server.survival-1", token_env = "RIFT_SURVIVAL_1_TOKEN", preset = "backend" }
```

Written out, that is:

```lua
{
    name = "server.survival-1",
    token_env = "RIFT_SURVIVAL_1_TOKEN",
    publish = {
        "rift.perms.hello", "rift.perms.subject.*", "rift.perms.groups.*",
        "rift.perms.apply", "rift.perms.register", "rift.perms.command",
        "rift.perms.context",
    },
    subscribe = {
        "rift.perms.update", "rift.perms.update.>", "rift.events.permissions",
        "rift.perms.changes",
    },
}
```

REQUEST creates its own private inbox, so no `_INBOX` grant is needed. Nothing
a backend sends writes the store directly: `apply`, `command` and `register`
go through the proxy's single write path with the backend (or the player it
names) recorded as the actor.

The proxy knows which principal sent each message (the QUIC listener records
it); payloads never name the sender. A principal named `server.<name>` speaks
for the Rift backend `<name>`: its `hello` must give `"server": "<name>"`, and
it may act for, and report contexts of, players on that backend only. A
principal with `control = true` may also write as itself and run console
commands. Only the proxy publishes `rift.perms.update`, `rift.perms.update.>`
and `rift.perms.changes`, and only control principals may subscribe to the
request subjects, whatever the grants say.

## Subjects

| Subject | Direction | Operation | Payload | Reply `data` |
| --- | --- | --- | --- | --- |
| `rift.perms.hello` | backend → proxy | REQUEST | [hello request](#hello) | [hello reply](#hello) |
| `rift.perms.subject.get` | backend → proxy | REQUEST | `{"uuid", "server"?}` | [compiled subject](#compiled-subject) |
| `rift.perms.subject.batch` | backend → proxy | REQUEST | `{"uuids": [...]}` | `{"revision", "subjects": [...]}` |
| `rift.perms.groups.get` | backend → proxy | REQUEST | `{"names"?, "permissions"?}` | `{"revision", "groups": [...]}` |
| `rift.perms.apply` | backend → proxy | REQUEST | `{"change", "actor_uuid"?, "reason"?}` | [applied](#applied) |
| `rift.perms.command` | backend → proxy | REQUEST | `{"actor_uuid"?, "args": [...]}` | `{"revision", "component"}` |
| `rift.perms.register` | backend → proxy | PUBLISH_ACK | `[{"node", "description"?, "default", "plugin"?}]` | none |
| `rift.perms.context` | backend → proxy | PUBLISH | `{"uuid", "contexts"}` | none |
| `rift.perms.update` | proxy → all | PUBLISH | [change event](#change-event) | none |
| `rift.perms.update.<uuid>` | proxy → all | PUBLISH | [compiled subject](#compiled-subject) | none |
| `rift.perms.update.resync` | proxy → all | PUBLISH | [resync request](#resync) | none |
| `rift.events.permissions` | proxy → all | PUBLISH | [change event](#change-event) | none |
| `rift.perms.changes` | proxy | retained stream | [change event](#change-event) | none |

`<uuid>` in a subject is 32 lowercase hex digits, so `rift.perms.update.resync`
never collides with a player; clients that do not know it ignore it.

## Replies

Every REQUEST is answered with one envelope, as `rift.control.*` subjects are:

```json
{"ok": true, "data": {...}}
{"ok": false, "error": "unknown group \"vip\"", "code": "not_found"}
```

`error` is a human-readable message. `code` is one of:

| Code | Meaning |
| --- | --- |
| `invalid_request` | Malformed JSON, a missing field, or a value that fails validation |
| `forbidden` | The backend or the named player may not do this |
| `not_found` | A named group, track or player does not exist |
| `limit` | A store limit would be exceeded |
| `conflict` | The change cannot apply to the current state, such as promoting a user who is already on a track's last group |
| `unavailable` | The proxy cannot serve the request right now; retry later |
| `internal` | Anything else |

A reader treats a missing or unknown `code` as `internal`. A request that
times out may still have been applied: after a timed-out `apply` or
`command`, compare revisions before retrying.

## Common types

### UUID

32 lowercase hex digits without dashes:
`"0123456789abcdef0123456789abcdef"`. Readers also accept uppercase digits and
the dashed form (`01234567-89ab-cdef-0123-456789abcdef`); writers always emit
the plain lowercase form.

### Permission node

A node is a pattern made of dot-separated segments. Each segment is one or
more of `a-z`, `0-9`, `_`, `:` and `-`; the whole node is at most 256 bytes.
The last segment may be `*` (`essentials.*`), and `*` alone matches
everything. `*` anywhere else is invalid, so `minecraft:*` is invalid while
`minecraft:command.*` is fine. Pumpkin's `plugin:node` names are used as
written.

Input is normalised by lowercasing ASCII letters (`Essentials.Fly` becomes
`essentials.fly`); other characters are not changed and make the node invalid.

Group and track names are 1 to 64 bytes of `a-z`, `0-9`, `_`, `.` and `-`,
also lowercased on input. Meta keys are 1 to 64 bytes of `a-z`, `0-9`, `_`,
`.`, `:` and `-`, lowercased. Prefixes, suffixes and display names are at
most 256 bytes and meta values at most 1024 bytes, without control characters.

### Context set

A set of `key=value` pairs. Keys are 1 to 64 bytes of `a-z0-9_.:-`; values
are 1 to 256 bytes without control characters, `;` or `=`. ASCII letters in
both are lowercased on input. JSON is an object; a key with several values
has an array:

```json
{"server": "survival", "region": ["market", "spawn"]}
```

Rift fills the reserved keys `server` (the backend name), `world`,
`dimension` (for example `minecraft:the_nether`) and `gamemode`
(`survival`, `creative`, `adventure`, `spectator`). Each has at most one value
in an active set. Other keys are custom. A backend fills all four from its own
player state; the proxy fills `server`, `dimension` and `gamemode` from the
session and the play packets it relays, never `world` (see
[contexts on the proxy](permissions.md#contexts-on-the-proxy)).

A node's contexts are satisfied when every one of its pairs is in the active
set (subset test); the empty set is global and always satisfied. A node that
names two values of a custom key needs both to be active. A node that names
two values of a reserved key could never apply and is rejected.

The canonical string form, used in storage and logs, is the pairs sorted by
key and then value, joined as `k=v;k2=v2`; the empty set is `""`.

### Typed node

A node on a user or group. The `kind` tag selects the payload fields:

| `kind` | Fields | Storage `key` |
| --- | --- | --- |
| `permission` | `key` (pattern), `value` (bool) | the pattern |
| `inherit` | `group`, `value` (bool; `false` blocks the group) | the group |
| `prefix` | `priority` (i32), `text` | the priority in decimal |
| `suffix` | `priority` (i32), `text` | the priority in decimal |
| `meta` | `key`, `value` (string) | the meta key |
| `weight` | `weight` (i32); groups only | `weight` |
| `display_name` | `name`; groups only | `display_name` |

Every node also has `contexts` (a context set, omitted when global),
`expires_at` (Unix seconds, omitted when permanent) and `source` (`store`,
`config`, `import` or `plugin:<name>`, omitted when `store`). Readers apply
those defaults when a field is missing.

```json
{"kind": "permission", "key": "essentials.fly", "value": true, "contexts": {"server": "survival"}, "expires_at": 1762800000}
{"kind": "inherit", "group": "vip", "value": true}
{"kind": "prefix", "priority": 100, "text": "&6[VIP] "}
{"kind": "meta", "key": "homes", "value": "5", "source": "plugin:essentials"}
```

A subject holds at most one node per slot, (kind, storage key, contexts);
setting a node replaces the one in its slot. So a user has at most one prefix
per priority and one value per meta key in each context. Kind codes for
storage are `permission` 0, `inherit` 1, `prefix` 2, `suffix` 3, `meta` 4,
`weight` 5, `display_name` 6.

A node expiring at `t` applies while `now < t`.

### Tristate

A check result: `true`, `false`, or `null` for undefined (nothing matched).
The caller decides what undefined means: Bukkit uses the registered
permission's default, Pumpkin its `PermissionDefault`, Lua `false`.

## Compiled subject

The proxy flattens a player's inheritance into one ranked list, the compiled
subject, and sends it to backends. It is the payload of
`rift.perms.update.<uuid>` and the `data` of `rift.perms.subject.get`:

```json
{"revision": 412, "uuid": "0123456789abcdef0123456789abcdef", "primary_group": "vip",
 "entries": [
   {"k": "perm", "key": "essentials.fly", "v": true, "ctx": {"server": "survival"}, "rank": 0, "exp": 1762800000},
   {"k": "group", "key": "vip", "v": true, "ctx": {}, "rank": 1},
   {"k": "perm", "key": "network.*", "v": true, "ctx": {}, "rank": 2}
 ],
 "prefix": [{"p": 100, "t": "&6[VIP] ", "ctx": {}, "rank": 1}],
 "suffix": [],
 "meta": [{"key": "homes", "v": "5", "ctx": {}, "rank": 1}]}
```

| Field | Meaning |
| --- | --- |
| `revision` | The store revision the subject was built from |
| `uuid` | The player |
| `primary_group` | The player's primary group |
| `entries[]` | `k`: `perm` (a permission pattern in `key`) or `group` (membership of the group named by `key`, `v` always `true`); `v`: the value; `ctx`: contexts (default `{}`); `rank`: 0 for the player's own nodes, then 1, 2, … per group; `exp`: expiry in Unix seconds, absent when permanent |
| `prefix[]`, `suffix[]` | `p`: priority, `t`: text, plus `ctx`, `rank`, `exp` |
| `meta[]` | `key`, `v`: value, plus `ctx`, `rank`, `exp` |

`prefix`, `suffix` and `meta` default to `[]`. An unknown `k` is a protocol
error: the protocol version changes before new entry kinds appear.

### How the proxy builds it

Clients never compile; this is for reading compiled subjects. The player's own
nodes get rank 0. Groups are collected from `inherit` nodes and the implicit
edge every player has to `default`; an edge's contexts are merged into every
node of the parent, a negated `inherit` removes that group on every path
confined to its contexts, and a path's expiry is the earliest expiry on it.
Groups are ranked by weight (descending), then distance from the player
(direct parents first), then name; `default` is always last. Each way a group
is reached adds a `group` entry followed by the group's nodes with that rank.

The primary group is the player's explicit primary group, otherwise the
heaviest group among their own global, positive `inherit` nodes (ties by
name), otherwise `default`.

A compiled subject has at most `limits.compiled_entries` (4096) entries in all
four lists together.

### Checking a node

A replica answers `check(subject, node, active, now)`:

1. Lowercase ASCII letters in `node`. If it is not a valid permission node,
   the answer is `null`.
2. Consider the entries that match `node`:
   - a `perm` entry whose `key` equals `node` matches exactly;
   - a `perm` entry whose `key` ends in `.*` matches when `node` starts with
     the key without its `*` (including the dot) and is longer than that; the
     specificity is the length of that prefix in bytes;
   - a `perm` entry with `key` `*` matches any node with the lowest
     specificity;
   - a `group` entry matches `group.<key>` exactly.

   So `a.*` matches `a.b` and `a.b.c` but not `a`, and exact beats every
   prefix, which beats `*`.
3. Drop entries that have expired (`exp <= now`) or whose `ctx` is not a
   subset of `active`.
4. The best remaining entry wins, comparing in order: higher specificity;
   more pairs in `ctx`; lower `rank`; `false` over `true`.
5. The winner's `v` is the answer; with no entry left the answer is `null`.

### Meta view

`meta(subject, active, now)` filters `prefix`, `suffix` and `meta` items the
same way (expired or unsatisfied contexts are ignored), then:

- prefix and suffix: the highest `p` wins; ties go to the lower `rank`, then
  more context pairs, then the earlier item;
- meta: per `key`, the lowest `rank` wins; ties go to more context pairs, then
  the earlier item.

```json
{"prefix": "&c[Admin] ", "suffix": null, "meta": {"homes": "5"}, "primary_group": "admin"}
```

Group membership for an active set is every `group` entry that is not expired
and whose `ctx` is satisfied; the player is in a group when such an entry
names it.

## Hello

Sent once after connecting and after every reconnect.

```json
{"server": "survival-1", "revision": 410, "client_version": "rift-permissions-client/0.1.0"}
```

`server` is the backend's name, the `server` context it fills; `revision` is
the last revision its replica saw (0 when empty). Reply data:

```json
{"revision": 412, "protocol": 1, "now": 1762790000,
 "limits": {"groups": 1024, "group_nodes": 256, "user_nodes": 512, "parents": 16, "contexts": 32,
            "tracks": 64, "compiled_entries": 4096, "batch": 256},
 "external_contexts": ["region"]}
```

A `server.<name>` principal whose `server` differs from `<name>` is refused
with `forbidden`. The proxy remembers each backend's last `hello` (server,
revision, client version, time) for `/perms backends`, `/perms doctor` and the
dashboard.

`protocol` is the protocol version; a client that does not support it stops
and applies its offline policy. `now` is the proxy's clock in Unix seconds, so
a backend can warn about clock skew, which shifts every expiry. `limits` are
the store limits. `external_contexts` are the custom keys a backend may
report on `rift.perms.context`.

If the reply's revision differs from the backend's (and its replica is not
empty), every stored subject is stale: refetch them with `subject.batch`.

## Fetching subjects

`rift.perms.subject.get` returns one compiled subject:

```json
{"uuid": "0123456789abcdef0123456789abcdef", "server": "survival-1"}
```

Every UUID compiles, a player the proxy has never seen to just `default`, so a
successful reply always carries a subject. Its `revision` is the revision the
subject was built from, which can be older than the proxy's current revision
when nothing about that player changed since.

`rift.perms.subject.batch` returns up to `limits.batch` (256) subjects, one per
requested UUID, in request order:

```json
{"uuids": ["0123456789abcdef0123456789abcdef", "00000000000000000000000000000001"]}
```

```json
{"revision": 412, "subjects": [{"revision": 400, "uuid": "0123456789abcdef0123456789abcdef", ...}]}
```

The reply's `revision` is the proxy's revision when the batch was built.

## Groups

`rift.perms.groups.get` lists groups for Vault's group API, `/perms group
list` and tab completion. `{}` returns every group without permission nodes;
`names` (at most 64) restricts the list, and `permissions: true`, allowed only
together with `names`, adds permission nodes:

```json
{"names": ["vip"], "permissions": true}
```

```json
{"revision": 412, "groups": [
  {"name": "vip", "weight": 10, "display_name": "VIP", "nodes": [
    {"kind": "inherit", "group": "default", "value": true},
    {"kind": "prefix", "priority": 100, "text": "&6[VIP] "},
    {"kind": "meta", "key": "homes", "value": "5"}]}]}
```

Each group carries `name`, `weight`, `display_name` (omitted when unset) and
its `inherit`, `prefix`, `suffix` and `meta` nodes (plus `permission` nodes
when requested). `weight` and `display_name` nodes are reported through the
columns only. Refetch the table when a change event lists `groups`.

## Changes

### Change

One write, tagged by `op`. All names are normalised by the proxy.

| `op` | Fields | Effect |
| --- | --- | --- |
| `set_node` | `subject`, `node` | Insert the node, replacing the one in its slot. `weight` and `display_name` nodes only on groups |
| `unset_node` | `subject`, `kind`, `key`, `contexts`? | Remove the node in that slot (`key` is the storage key) |
| `clear_subject` | `subject`, `contexts`? | Remove every node, or those with exactly these contexts |
| `create_group` | `name`, `weight`? (0), `display_name`? | Create a group |
| `delete_group` | `name` | Delete a group and its nodes; `inherit` nodes naming it stay and grant nothing. `default` cannot be deleted |
| `set_weight` | `group`, `weight` | Set the weight (and the global `weight` node) |
| `set_display_name` | `group`, `display_name` (string or `null`) | Set or clear the display name |
| `set_primary_group` | `user`, `group` (string or `null`) | Set or clear the explicit primary group |
| `set_track` | `name`, `groups` | Create or replace a track; groups must be distinct |
| `delete_track` | `name` | Delete a track |
| `promote` | `user`, `track`, `contexts`? | Move the user to the next group on the track in exactly these contexts; a user on none goes to the first group |
| `demote` | `user`, `track`, `contexts`? | Move the user to the previous group, or off the track from the first group |
| `bulk` | `filter`, `patch` | Patch every matching node |
| `batch` | `changes` | Several changes (not `batch`) in one transaction and one revision |

`subject` is `{"user": "<uuid>"}` or `{"group": "<name>"}`. For tracks,
`default` is implicit: a user with no track group counts as being on `default`
when the track lists it, and promoting from or demoting to `default` adds or
removes no `default` node.

```json
{"op": "set_node", "subject": {"user": "0123456789abcdef0123456789abcdef"},
 "node": {"kind": "permission", "key": "essentials.fly", "value": true, "expires_at": 1762800000}}
{"op": "unset_node", "subject": {"group": "vip"}, "kind": "prefix", "key": "100"}
{"op": "promote", "user": "0123456789abcdef0123456789abcdef", "track": "ranks"}
```

A bulk `filter` selects nodes; every field it sets must match, and `{}` selects
every node:

| Field | Matches |
| --- | --- |
| `subjects` | `any` (default), `users` or `groups` |
| `subject` | exactly this subject |
| `kind` | the node kind |
| `key_like` | the storage key, with SQL `LIKE` semantics: `%` any run, `_` one character, `\` escapes the next character, ASCII letters case-insensitive |
| `value` | the storage value exactly: `"true"`/`"false"`, the text, or the weight in decimal |
| `contexts` | exactly these contexts |
| `source` | the node source |

A `patch` sets at least one of: `delete: true` (remove the node, overriding the
rest), `value` (bool, for `permission` and `inherit` nodes only), `contexts`
(move the node, replacing one already in the target slot), `expires_at`,
`permanent: true` (remove the expiry), `source`.

```json
{"op": "bulk", "filter": {"kind": "permission", "key_like": "essentials.%"}, "patch": {"value": false}}
```

### Applying from a backend

`rift.perms.apply` asks the proxy to apply a change:

```json
{"change": {"op": "set_node", "subject": {"user": "0123456789abcdef0123456789abcdef"},
            "node": {"kind": "inherit", "group": "vip", "value": true}},
 "actor_uuid": "0123456789abcdef0123456789abcdef", "reason": "vault playerAddGroup"}
```

With `actor_uuid` the change is made on behalf of that player: the player must
be online on the sending backend (or have their connection forwarded there
right now) and hold the `rift.perms.*` node for the operation, and the audit
log records `uuid:<hex>`. Without it the change is made by the backend itself
(`backend:<principal>`), which requires a principal with `control = true`.
`reason` (at most 256 bytes) is stored in the audit log.

The node mirrors the [`/perms` verb tree](permissions.md#commands):

| Change | Node |
| --- | --- |
| `set_node` permission / inherit / meta (`*temp` with `expires_at`) | `rift.perms.<user\|group>.permission.set`, `.parent.add`, `.meta.set` (`.settemp`, `.addtemp`, `.settemp`) |
| `set_node` prefix / suffix | `rift.perms.<user\|group>.meta.setprefix`, `.meta.setsuffix` |
| `set_node` weight / display name | `rift.perms.group.setweight`, `rift.perms.group.setdisplayname` |
| `unset_node` | `rift.perms.<user\|group>.permission.unset`, `.parent.remove`, `.meta.unset`, `.meta.removeprefix`, `.meta.removesuffix` |
| `clear_subject` | `rift.perms.<user\|group>.clear` |
| `create_group`, `delete_group`, `set_weight`, `set_display_name` | `rift.perms.group.create`, `.delete`, `.setweight`, `.setdisplayname` |
| `set_primary_group` | `rift.perms.user.parent.switchprimarygroup` |
| `set_track`, `delete_track` | `rift.perms.track.edit`, `rift.perms.track.delete` |
| `promote`, `demote` | `rift.perms.user.promote`, `rift.perms.user.demote` |
| `bulk` | `rift.perms.bulk` |
| `batch` | every node its changes need |

`rift.perms.*` covers them all. A player also cannot grant what they do not
hold: when the change would add or alter a positive permission or group
membership, the player must have that permission (or be in that group) in the
node's contexts, completed with their own, unless they hold
`rift.perms.escalate`. Refusals answer `forbidden`.

### Applied

The reply `data` of `apply`:

```json
{"revision": 413, "ops": 1, "changed_users": ["0123456789abcdef0123456789abcdef"], "changed_groups": []}
```

`ops` counts the recorded effects; `0` means nothing changed, no revision was
created, and `revision` is the current one. `changed_users` lists the players
whose compiled subjects changed; `changed_groups` the groups whose own data
changed.

### Change event

After every committed write the proxy publishes the change log row on
`rift.perms.update`, `rift.events.permissions` and the retained stream
`rift.perms.changes`:

```json
{"revision": 413, "at": 1762800000, "actor": "uuid:0123456789abcdef0123456789abcdef", "reason": "event reward",
 "ops": [
   {"op": "node", "subject": {"user": "0123456789abcdef0123456789abcdef"}, "before": null,
    "after": {"kind": "inherit", "group": "vip", "value": true, "expires_at": 1765392000}},
   {"op": "group", "name": "vip", "before": {"weight": 0}, "after": {"weight": 50, "display_name": "VIP"}},
   {"op": "track", "name": "ranks", "before": null, "after": ["default", "vip"]},
   {"op": "primary_group", "user": "0123456789abcdef0123456789abcdef", "before": null, "after": "vip"}],
 "changed": ["0123456789abcdef0123456789abcdef"], "groups": ["vip"]}
```

| Field | Meaning |
| --- | --- |
| `revision` | The new revision |
| `at` | Commit time, Unix seconds |
| `actor` | `uuid:<hex>`, `operator:<name>`, `lua:<plugin>`, `backend:<name>`, `cli`, `import` or `system` (expiry sweeps, configuration mirroring, migrations) |
| `reason` | Optional free text |
| `ops` | The effects as before/after pairs, `null` meaning absent: `node` (a node on `subject`), `group` (`weight` and `display_name` columns of group `name`), `track` (the group list of track `name`), `primary_group` (of `user`) |
| `ops_truncated` | `true` when `ops` was cut short to bound the message size (large bulk edits); the full list is in the audit log. Omitted when `false` |
| `changed` | Players whose compiled subjects changed (default `[]`) |
| `groups` | Groups whose own data changed (default `[]`) |

The expiry sweeper's removals are ordinary change events with actor `system`.

## Commands

`rift.perms.command` forwards a `/perms` command typed on a backend, split
into arguments without the command name. `actor_uuid` is the player who typed
it, absent for the backend console (allowed only for principals with
`control = true`):

```json
{"actor_uuid": "0123456789abcdef0123456789abcdef", "args": ["user", "Alice", "permission", "set", "fly", "true"]}
```

```json
{"revision": 413, "component": {"text": "Set fly to true for Alice", "color": "green"}}
```

`component` is Adventure component JSON to show the sender; `revision` is the
proxy's revision after the command. A command that fails as a command
(unknown player, bad syntax, missing permission) still replies `ok: true` with
a component explaining it. `ok: false` is reserved for protocol failures: a
console command (no `actor_uuid`) from a principal without `control = true`,
or an `actor_uuid` who is not on the sending backend, is refused with
`forbidden`.

The proxy authorizes a player's command exactly as its own in-game `/perms`:
each verb needs `rift.perms.<verb>` and grants need the granted node or
`rift.perms.escalate` (see [Commands](permissions.md#commands)). Backends that
split arguments on spaces may send quoted text in pieces
(`"\"&6[VIP]"`, `"\""`); the proxy rejoins arguments that contain quotes.

## Registered permissions

`rift.perms.register` (PUBLISH_ACK) declares the permissions plugins on the
backend know, with descriptions and defaults, for tab completion and the
dashboard. The payload is an array of at most 4096 entries:

```json
[{"node": "essentials.fly", "description": "Fly", "default": "op", "plugin": "Essentials"},
 {"node": "myplugin:fly", "default": "op:2"}]
```

`default` is `true`, `false`, `op`, `not_op` (Bukkit) or `op:<0-4>` (Pumpkin op
levels). The proxy stores them under the sending principal (with `plugin` when given),
replacing that principal's earlier registration; a REQUEST instead of
PUBLISH_ACK gets `{"registered": <count>}`. Registered nodes are stored with
source `plugin:<principal>` and never
change the answer for a node that has an explicit value anywhere; they only
supply the default the backend applies to `null`.

## Contexts reported by backends

`rift.perms.context` (PUBLISH) reports custom contexts for a player on the
sending backend, replacing that player's previous report:

```json
{"uuid": "0123456789abcdef0123456789abcdef", "contexts": {"region": "spawn"}}
```

Keys must be listed in `external_contexts`; others are dropped (and logged).
Reports are accepted only from a `server.<name>` principal for players on that
backend, or whose connection the proxy is forwarding to it at that moment (so
a report made at login is not lost); they are cleared when the player switches
server, and are used by proxy-side checks as well.

## Revisions and ordering

The proxy's revision is a `u64` that grows by one with every committed write
and survives restarts. Live delivery on the bus is at most once and unordered
across publishers, so replicas follow these rules:

1. **Snapshots.** Store a compiled subject only if its `revision` is greater
   than the stored subject's for that UUID. Duplicates and late arrivals are
   ignored, whichever path (`update.<uuid>`, `subject.get`, `subject.batch`)
   they came from.
2. **Replica revision.** The replica's revision comes from the `hello` reply
   and from change events, never from snapshots.
3. **Change events.** For an event with revision `r` and replica revision
   `local`:
   - `r <= local`: a duplicate or reordered event; ignore it.
   - `r == local + 1`: set `local = r`. For each UUID in `changed` whose stored
     subject has a revision below `r`, refetch it with `subject.get` unless its
     pushed snapshot arrives first. If `groups` is non-empty, refetch the group
     table.
   - `r > local + 1`: an event was lost. Set `local = r` and refetch every
     stored subject with `subject.batch` and the group table.
4. **Pushes.** For every player online somewhere, the proxy publishes
   `rift.perms.update.<uuid>` after each change that affects them, and before
   forwarding a joining or switching player's connection to the backend, so
   the snapshot is usually present before the player logs in. For the first
   join the UUID is the Mojang one in online mode and the offline UUID
   (`OfflinePlayer:<name>`) otherwise.
5. **Missing snapshot at login.** Request `subject.get` with a bounded timeout
   (default 2 s); on timeout apply the offline policy to that player and retry
   every 5 s.
6. **Reconnect.** Send `hello` again; a different revision means rule 3's gap
   handling.
7. **Eviction.** Drop a player's snapshot 10 minutes after they leave the
   backend.

Expiry is evaluated at check time against the local clock, so an expired entry
stops applying exactly at its expiry even before the proxy's sweeper removes
the node and publishes the change.

The retained stream `rift.perms.changes` holds the same events with stream
sequence numbers. A client that wants the full history (or a future follower
proxy) reads it with a durable consumer; replicas normally need only the live
subjects.

## Resync

`/perms sync` publishes `rift.perms.update.resync`:

```json
{"revision": 413}
```

A replica treats it like a reconnect: it sends `hello` again (so the proxy
sees its revision) and refetches every stored subject and the group table.
The Rust client does this; clients that predate it ignore the subject, which
is harmless.

## Versioning

This is protocol version 1, announced in the `hello` reply's `protocol`.
Adding object members is compatible and does not change the version; readers
ignore members they do not know. Removing or reinterpreting a member, or
adding an entry kind (`k`) to compiled subjects, increments it.
