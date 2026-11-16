# Permissions

Rift keeps one permission store for the whole network, with the model LuckPerms
users know: users, groups, weighted inheritance, negated nodes, wildcards,
temporary nodes, contexts, prefixes, suffixes, meta and tracks. The proxy owns
and resolves the data; [backends](#backends) hold replicas and answer
`hasPermission` from memory.

On the proxy the store answers extension commands, `ctx.has_permission`,
[`rift.permissions`](permissions-lua.md), the `RiftInGroup`
[BungeeCord request](bungeecord.md#rift-requests), the [dashboard](#dashboard),
the [HTTP API](permissions-http.md) and
[`/perms`](permissions-commands.md).

## Create a group and add a player

Run these on the proxy host. `rift admin perms` has console rights and needs
the [admin CLI](admin.md) permission `perms`.

```sh
rift admin perms group vip create 10 VIP
rift admin perms group vip permission set essentials.fly
rift admin perms user Alice parent add vip
rift admin perms check Alice essentials.fly
```

A player is a name (anyone who has joined once) or a UUID. To let Alice use
`/perms` in game, grant her `rift.perms.*`; see
[Permission commands](permissions-commands.md).

## The model

| Term | Meaning |
| --- | --- |
| User | Keyed by UUID (32 lowercase hex digits; the dashed form is accepted on input). Has nodes, an optional explicit primary group and the name they last joined with |
| Group | A name (1 to 64 bytes of `[a-z0-9_.-]`), a weight (higher wins, default 0), an optional display name and nodes |
| Node | A typed entry on a user or group; see [Node kinds](#node-kinds) |
| Context | `key=value` pairs on a node that must all be active for it to apply. No contexts means global |
| Temporary node | A node with an expiry (Unix seconds). It applies while the time is before the expiry, exactly, at every check |
| Source | Where a node came from: `store` (edited at runtime), `config` (mirrored from `extensions.groups`), `import` (LuckPerms import) or `plugin:<name>` |
| Track | An ordered list of groups (`default → vip → mod → admin`) |
| Registered permission | A node a backend declares, with a description and a default (`true`, `false`, `op`, `op:<level>`, `not_op`). It never changes how a node with an explicit value resolves |

The `default` group always exists, may be empty and cannot be deleted. Every
user inherits it implicitly and last; a negated `inherit` node is the only way
to opt a user out.

### Node kinds

| Kind | Payload | Meaning |
| --- | --- | --- |
| `permission` | pattern, `true`/`false` | Grant or deny `a.b`, `a.*` or `*` |
| `inherit` | group, `true`/`false` | Inherit a group; `false` blocks it on every path |
| `prefix`, `suffix` | priority, text | Chat decoration; the highest priority wins. Also shown in the [network tab list](server-list.md) |
| `meta` | key, value | Arbitrary key/value data such as `homes = "5"` |
| `weight` | integer | Groups only; the same as the group's weight |
| `display_name` | text | Groups only |

- Patterns are dot-separated segments of `[a-z0-9_:-]`, at most 256 bytes,
  with `*` only as a whole trailing segment. Input is lowercased as Bukkit
  does, and Pumpkin's `plugin:node` fits as written.
- One node exists per subject, kind, key and contexts; setting a node replaces
  the one in that slot.

### Context keys

Rift and the backend adapters fill `server`, `group`, `world`, `dimension` and
`gamemode`. Any other key is custom (`region=spawn`).

- `server` is the backend's name, which for an instance of a
  [service group](service-groups.md) is the instance (`lobby-2`).
- `group` is the backend's service group, absent on backends outside groups.
  A grant meant for every lobby uses `group=lobby`.

### Tracks

Promoting moves a user's `inherit` node to the next group on the track in the
contexts given; demoting moves it back. Leaving or arriving at `default`
touches no node, because that edge is implicit.

## How a check is decided

The [protocol](permissions-protocol.md#compiled-subject) specifies both stages
exactly.

1. **Compile** (proxy only). Walk inheritance from the user, keeping each
   edge's contexts, and flatten everything into ranked entries. The user's own
   nodes rank 0; groups rank 1, 2, 3, … by weight descending, then by depth (a
   direct parent before a grandparent), then by name, with `default` last.
2. **Check** (anywhere). Among the entries whose contexts are active, the most
   specific pattern wins: the exact node, then the longest `prefix.*`, then
   `*`.
3. **Break ties.** More context pairs win, then the lower rank, then `false`.

A check answers `true`, `false` or undefined. Commands and
`ctx.has_permission` treat undefined as `false`; `rift.permissions.check`
returns `nil` for it, so a script can tell "denied" from "never set".

### Contexts on the proxy

Proxy-side checks use the contexts Rift knows for an online player:

| Context | Where the proxy gets it |
| --- | --- |
| `server`, `group` | The backend the player is on, and its service group when it is an instance of one. Both follow the player on every switch |
| `dimension`, `gamemode` | The play packets Rift relays from that backend, on every supported version (1.8.9–26.3) |
| `world` | Never: a Bukkit world name (`world_nether`) does not cross the wire. Nodes that require `world` apply on the backends, whose adapters fill it |
| Keys in `permissions.contexts.external` | Reports from the player's backend. A report replaces the backend's earlier one and is accepted only while the player is on that backend, or while the proxy forwards their connection to it |
| Other custom keys | Lua [context providers](permissions-lua.md#context-providers) |

`dimension` and `gamemode`:

- Join Game and Respawn name both; the game-mode Game Event updates `gamemode`
  in place.
- `dimension` is the world's namespaced key: `minecraft:overworld`,
  `minecraft:the_nether` and `minecraft:the_end` for the vanilla worlds,
  exactly as the backend adapters report them. Before 1.16 the packets carry
  only those three, as dimension IDs.
- `gamemode` is `survival`, `creative`, `adventure` or `spectator`.
- A packet Rift cannot read is relayed unchanged and leaves the values as they
  were; a Join Game it cannot read clears them.

Every server switch clears reported and provided contexts and restarts the
proxy's own from the new `server`; the new backend's Join Game fills
`dimension` and `gamemode` again. Offline players have no contexts, so only
global nodes apply to them.

A backend computes the reserved contexts from its own player state, which is
more precise: it knows `world`, and a custom Paper world with a vanilla
environment reports that environment's dimension (`minecraft:the_nether`)
where the proxy sees the world's own key. Grants scoped to the three vanilla
dimensions and to game modes resolve the same on both sides.

The `/perms` entries in a player's command tree follow a context change with
the backend's next command tree.

## Storage and revisions

- **File.** Everything lives in the `instance_database` file (`rift.sqlite3`
  beside the configuration by default), in tables prefixed `perm_`. Each part
  records its schema version in `rift_schema`. Without a configuration file
  (a bare `rift <listen> <backend>`), the store is in memory.
- **Integrity.** Startup runs `PRAGMA integrity_check` and refuses to start on
  a damaged file.
- **Revisions.** Every write is one transaction that also appends one row to
  `perm_change`. The row's number is the new revision; it is stored, so it
  keeps increasing across restarts. A write that changes nothing creates no
  row.
- **Audit.** Each row lists its effects as before/after pairs and who made
  them: `uuid:<player>`, `operator:<name>`, `lua:<callback>` (for example
  `lua:command.rank` or `lua:join`), `backend:<name>`, `cli`, `import` or
  `system`.
- **Propagation.** After each commit the proxy recompiles the online players
  the change affects and tells subscribers, which is how backends learn about
  it.
- **Expiry.** Expired nodes stop applying the moment they expire. Every
  `permissions.sweep_interval_ms` the proxy deletes them in one revision, with
  actor `system`, so subscribers and `on_change` handlers see the removal.

## Configuration

```lua
rift.config.permissions = {
    -- Defaults shown. They keep a compiled subject under 64 KiB.
    limits = {
        groups = 1024,           -- groups in the store
        group_nodes = 256,       -- nodes per group
        user_nodes = 512,        -- nodes per user
        parents = 16,            -- inherit nodes per user or group
        contexts = 32,           -- context pairs per node
        tracks = 64,
        compiled_entries = 4096, -- entries of one compiled subject
    },
    -- Custom context keys backends may report for their players.
    contexts = { external = { "region" } },
    sweep_interval_ms = 60000,   -- 1000 to 86400000
}
```

Every field is optional and can change on reload. A write past a limit fails
without changing anything. A user whose inheritance compiles to more than
`compiled_entries` entries resolves everything as undefined, and the proxy
logs why.

### Groups as configuration

Extensions can keep groups in Lua:

```lua
rift.config.extensions = {
    groups = {
        default = { permissions = { ["network.queue"] = true, ["chat.color"] = false } },
        staff = {
            inherits = { "default" },
            weight = 10,
            prefix = "&9[Staff] ",                 -- priority = weight
            suffix = { priority = 5, text = " ✦" },
            meta = { homes = 5 },
            permissions = { ["network.*"] = true, ["chat.color"] = true },
        },
    },
}
```

| Field | Value |
| --- | --- |
| `permissions` | Patterns mapped to `true` or `false` |
| `inherits` | Parent groups. They need not be defined here; a missing parent grants nothing until it exists |
| `prefix`, `suffix` | Text, with the group's weight as priority, or `{ priority = n, text = "..." }` |
| `meta` | Strings or integers |

On every committed reload Rift makes the store's `config` nodes equal this
table, in one revision when anything differs and none when nothing does.

- Rift creates missing groups and never touches other nodes, so the dashboard
  and Lua can add to configured groups. The change log shows which side wrote
  what.
- Removing a group from the table removes its configured nodes; the group
  itself and its runtime nodes stay.
- The table must fit `permissions.limits`, which `rift check` verifies.

### Server access

`network.access` restricts who may join a backend or a
[service group](service-groups.md); a group's rule applies to every instance
(`lobby-1`, `lobby-2`). `network.server_permission` gates the `/server`
command itself.

```lua
rift.config.network = {
    hubs = { "lobby" },
    server_permission = "rift.command.server", -- omit to let everyone use /server
    access = {
        vip = { groups = { "vip", "staff" } },
        build = { permission = "rift.server.build", deny = { "Griefer" } },
        staff = { groups = { "staff" }, allow = { "Owner" } },
    },
}
```

Rift decides a rule in this order:

1. A name in `deny` is always refused.
2. A rule without `allow`, `groups` or `permission` is public.
3. Otherwise the player must match one: be named in `allow`, be in one of
   `groups` (directly, inherited or through the implicit `default`), or have
   `permission` resolve to `true`.

- **Contexts.** Groups and nodes are checked in the
  [contexts the proxy knows](#contexts-on-the-proxy) at that moment. A grant
  scoped to `server=lobby` counts while the player is on lobby; at login only
  global grants apply.
- **Scope.** Rules apply to the initial server, `/server`, `/hub`, fallbacks
  and recovery, and transfers from extensions, BungeeCord and the admin API.
  `/server` lists and suggests only the destinations a player may join.
- **Wildcards instead of server globs.** Guard each backend with its own node
  (`rift.server.build`, `rift.server.vip`) and grant `rift.server.*` to a
  group that may go everywhere.
- **`server_permission`** must resolve to `true` for a player to list or join
  servers with `/server`. Others are told they lack permission and get no
  suggestions. It does not affect `/hub`, fallbacks or transfers.
- **Names.** Group names and nodes are lowercased; nodes cannot end in a
  wildcard.

[Queues](queues.md) read two more things from the store:

| Key | Kind | Effect |
| --- | --- | --- |
| `queue_priority`, or `queue_priority.<destination>` | meta | Orders a player's ticket |
| `rift.queue.bypass`, or `rift.queue.bypass.<destination>` | permission | Lets the player enter a full destination |

## Visibility

A player whose meta key `rift.visible-to` is set is shown only to viewers who
hold the permission node in its value. A vanish plugin sets the key and Rift
does the rest:

```sh
/perms user Staff meta set rift.visible-to vanish.see          # hidden everywhere
/perms user Staff meta settemp rift.visible-to vanish.see 30m  # for 30 minutes
/perms user Staff meta set rift.visible-to vanish.see server=lobby-1
/perms group mod meta set rift.visible-to vanish.mod           # a vanish level
/perms group admin permission set vanish.see                   # who sees them
```

The rule is meta, not a permission node, so staff holding `*` are not all
hidden. Like every node it is known at login, scoped by contexts, expirable,
audited and replicated to backends. A value that is not a valid node still
hides the player: nobody holds it.

Every reader of "who is online" names who is looking:

| Viewer | Sees | Used by |
| --- | --- | --- |
| player | unrestricted players, themselves, and players whose node they hold (in the viewer's current contexts) | the [network tab list](server-list.md), the `{online}` and `{server_online}` placeholders, player-name completion of extension and plugin commands |
| public | unrestricted players only | the server-list player count, the status server's counts and its badge |
| backend | unrestricted players only | BungeeCord `PlayerCount`, `PlayerList` and the `RiftServers` counts |
| system | everyone | capacity, queues, scaling and idle shutdown, Prometheus metrics, `/api/status`, kick and transfer by name, `GetPlayerServer`, `rift.players.*` and the plugin API |

System readers must see hidden players, or idle shutdown could stop the server
a hidden administrator stands on. Plugins see everyone too, with the rule
beside each player (`visible_to`; see [the plugin API](plugins.md#host-api)).

Rift resolves the rule onto the player:

- at login, before any tab list or count includes them;
- when they join a world, before the join event is published;
- on every server switch, so per-server rules apply;
- whenever their permissions change.

Each change is published on `rift.events.players` as a `visibility` event.
Sessions rebuild their tab list when any online player's permissions change,
so a viewer who gains or loses the node sees the difference at once. An
expiring rule or `see` node takes effect when the sweeper deletes it, within
`sweep_interval_ms`.

> [!WARNING]
> A dimension or game-mode change does not resolve the rule again. Scope
> rules by `server` or `group`, not by `dimension` or `gamemode`.

Rift hides players from what it builds itself. A backend lists its own players
in their tab list and in its own player count; a Paper plugin reads the same
key through RiftPerms and hides the player locally (see
[RiftPerms](permissions-java.md#visibility-on-paper)). The example
[`vanish-lua`](../examples/plugins/vanish-lua/init.lua) plugin implements
`/vanish`, `/vanish here` and `/vanish <minutes>` in 40 lines.

## Backends

Backends keep replicas through the
[permission protocol](permissions-protocol.md) on the
[messaging bus](messaging.md). Give each one a principal with the `backend`
preset; its name must be `server.<backend>`:

```lua
rift.config.messaging = {
    listen = "0.0.0.0:7443",
    certificate = "messaging-cert.pem",
    private_key = "messaging-key.pem",
    principals = {
        { name = "server.survival-1", token_env = "RIFT_SURVIVAL_1_TOKEN", preset = "backend" },
        { name = "server.lobby", token_env = "RIFT_LOBBY_TOKEN", preset = "backend" },
    },
}
```

- The proxy publishes each player's compiled subject before it forwards their
  connection to a backend, and again after every change that affects them. The
  replica usually holds it before the player logs in.
- The backend must know the player by the proxy's UUID. Pumpkin does only with
  [Velocity modern forwarding](permissions-pumpkin.md#velocity-modern-forwarding),
  and otherwise fetches the snapshot at login.
- With a principal that may receive these snapshots, the proxy reads the login
  name before connecting even without network features.
- Every committed change is also appended to the retained stream
  `rift.perms.changes`, file-backed beside the instance database
  (`rift.perms-changes/`).

### `rift dev`

`rift dev` starts a proxy for plugin development in one command:

```sh
rift dev --server paper --backend 127.0.0.1:25566
```

| Part | Setup |
| --- | --- |
| Permission store | In memory; nothing is saved |
| Bus | `127.0.0.1:7443`, with a self-signed certificate written to `./rift-dev/ca.pem` |
| Backend | `dev` at `127.0.0.1:25566`; its principal `server.dev` has the `backend` preset and `control = true`. Run it in offline mode |
| Admin service | `127.0.0.1:9091` |
| [Dashboard](#dashboard) and [HTTP API](permissions-http.md) | `http://127.0.0.1:8080/` |

It prints the token (`RIFT_DEV_TOKEN`, generated unless set), a ready plugin
configuration, the `rift admin perms` command line and the dashboard URL.

The admin service and the dashboard accept the admin token: `RIFT_ADMIN_TOKEN`
if set, otherwise the `RIFT_DEV_TOKEN` value. Enter it under API access in the
dashboard, or send it as `Authorization: Bearer <token>`. Groups edited there
reach the connected backends like any other change, and
`GET /api/perms/backends` lists them.

Options: `--listen`, `--backend`, `--server`, `--bus`, `--admin`, `--web`
(a loopback address, or `off`), `--dir`.

## Dashboard

The [web dashboard](http.md) has four views under **Permissions**. They call
the [HTTP API](permissions-http.md) for everything, so an edit there is
validated, audited and pushed to backends exactly like `/perms` or a Lua
write, and the change log records it as `operator:<name>`.

| View | What it does |
| --- | --- |
| **Groups** | Every group as a tree by inheritance (each group under its heaviest parent). Create, delete and edit groups, their nodes and, below the tree, tracks: order a track's groups with the Up and Down buttons, lowest first |
| **Players** | Search by name, UUID prefix or group. Set a player's primary group, parent groups (with contexts and an expiry), track position and nodes |
| **Nodes** | The registered node catalogue by backend and plugin, with defaults and descriptions. Choose a node to list who sets it directly; wildcards are not expanded there, so use a player's check for the full answer |
| **Activity** | The change log, newest first, filterable by actor, subject and revision. Open a revision to compare every node before and after |

On a player's page, *What backends see* shows the compiled subject with each
entry's source group and rank, the resolved prefix, suffix and meta for any
contexts you enter, and checks one node with the winning entry and why the
others lost.

The node editor takes a kind (permission, parent group, prefix, suffix or
meta), its key and value, contexts as `key=value` pairs separated by commas,
and an expiry (a duration from now or a date).

- Editing a node replaces it, so changing its contexts moves it.
- Nodes from `extensions.groups` show the source `config` and come back on the
  next reload if you change them here; edit those in Lua instead.
- Weight and display-name nodes are edited through the group's fields.
- Operators with the `perms` permission see the views without the editing
  controls; `config` adds them.

## Upgrading from extensions API v2

The weighted model replaces API v2's five fixed layers (overrides, `*`
overrides, configured grants, groups, configured `*` grants). Rift is pre-0.1,
so there is no compatibility layer.

On the first start with the permission store, Rift copies, once and in one
revision with actor `system`:

- memberships from the extension store namespace `rift.groups`, as global
  `inherit` nodes;
- the grants of `extensions.permissions`: UUID entries as user nodes, `*`
  entries as nodes of `default`.

Rift marks the store so this never repeats, and deletes nothing from the
extension store, so downgrading remains possible. Runtime overrides were never
persisted and are not carried over.

> [!WARNING]
> `rift check` and the loader reject `extensions.permissions`. After the first
> start copied it, Rift refuses to start until you delete the field.

| API v2 | API v3 |
| --- | --- |
| `rift.permissions.has(uuid, node)` | `rift.permissions.check(uuid, node) == true` |
| `rift.permissions.set(uuid, node, value)` | `rift.permissions.user(uuid):set(node, value)`; `nil` is `:unset(node)` |
| `rift.permissions.set("*", node, value)` | `rift.permissions.group("default"):set(node, value)` |
| `rift.permissions.add_group(uuid, group)` | `rift.permissions.user(uuid):add_parent(group)` |
| `rift.permissions.remove_group(uuid, group)` | `rift.permissions.user(uuid):remove_parent(group)` |
| `rift.permissions.groups(uuid)` | the `inherit` entries of `rift.permissions.user(uuid):nodes()` |
| `extensions.permissions` | user nodes (`user(uuid):set` in a callback, or the dashboard) or `extensions.groups` |

Scripts written for API v1 and v2 keep working unchanged:

- The store answers their commands and `ctx.has_permission`.
- `api_version` no longer selects anything, so `rift.permissions` is available
  to them too.
- Group inheritance no longer has to avoid cycles or name defined groups.
- Where API v2 let any unrelated denial win, weights now decide; give the
  group that should win a higher weight.

## Related pages

| Page | Covers |
| --- | --- |
| [Permission commands](permissions-commands.md) | `/perms`, `rift admin perms`, doctor, LuckPerms import and export |
| [Permissions from Lua and plugins](permissions-lua.md) | `rift.permissions`, change events, context providers |
| [Permissions HTTP API](permissions-http.md) | The API behind the dashboard |
| [RiftPerms for Paper](riftperms.md) | The Paper plugin |
| [Permissions API for Java](permissions-java.md) | Java plugins and services |
| [Permissions for Pumpkin](permissions-pumpkin.md) | The Pumpkin adapter |
| [Permissions clients (Rust and C)](permissions-clients.md) | The Rust client and the C ABI |
| [Permissions protocol](permissions-protocol.md) | The wire specification |
