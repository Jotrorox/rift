# RiftPerms for Paper

RiftPerms is the plugin that brings Rift's [permissions](permissions.md) to a
Paper, Folia or Purpur backend. It keeps a replica of the proxy's permission
store and takes LuckPerms' place: `Player#hasPermission`, Vault and
PlaceholderAPI keep working. Plugin developers want the
[permissions API for Java](permissions-java.md) instead.

## Install RiftPerms

1. Give the backend a principal on the proxy's messaging bus named
   `server.<backend>`, with the `backend` preset (the grants listed under
   [principal permissions](permissions-protocol.md#principal-permissions)):

   ```lua
   messaging = {
       listen = "0.0.0.0:4222",
       certificate = "messaging-cert.pem",
       private_key = "messaging-key.pem",
       principals = {
           { name = "server.survival-1", token_env = "RIFT_SURVIVAL_1_TOKEN", preset = "backend" },
       },
   }
   ```

   Add `control = true` only if this server's console should run `/perms` and
   write as itself.

2. Drop `RiftPerms-<version>.jar` (attached to each GitHub release) into
   `plugins/` and remove LuckPerms. Copy the bus certificate (or its CA) to
   `plugins/RiftPerms/messaging-cert.pem`, and start the server with the
   token in the environment (`RIFT_PERMS_TOKEN` by default).

3. Edit `plugins/RiftPerms/config.yml`:

   ```yaml
   server: survival-1
   bus:
     address: 127.0.0.1:4222
     server_name: localhost
     certificate: messaging-cert.pem
     token_env: RIFT_PERMS_TOKEN
   ```

> [!WARNING]
> A principal speaks for one backend only. `server` must be the name in
> `server.<backend>`, or the proxy refuses the handshake and RiftPerms logs
> `The proxy refused hello`.

## Settings

Every setting of `config.yml`. File paths are relative to
`plugins/RiftPerms/`. An invalid setting keeps RiftPerms disabled.

| Setting | Default | Meaning |
| --- | --- | --- |
| `server` | required | This backend's name on the proxy: the `server` context |
| `group` | none | Its service group, for an instance of one: the `group` context |
| `bus.address` | required | `messaging.listen` on the proxy, as `host:port` |
| `bus.server_name` | host of `bus.address` | The DNS name in the bus certificate |
| `bus.certificate` | required | The bus certificate or its CA |
| `bus.token_env` | `RIFT_BUS_TOKEN` | Environment variable holding the bus token; the bundled file sets `RIFT_PERMS_TOKEN` |
| `join_timeout_ms` | `2000` | How long a joining player waits for their snapshot |
| `request_timeout_ms` | `5000` | How long writes, `/perms` and Vault writes wait |
| `offline_policy` | `deny` | `deny`, `last_snapshot` or `fallback_file`; see [offline policy](#offline-policy) |
| `snapshot_file` | `replica.json` | The `last_snapshot` file |
| `fallback_file` | `fallback.json` | The `fallback_file` file |
| `vault` | `true` | Register the [Vault](#vault) providers |
| `placeholders` | `true` | Register the [PlaceholderAPI](#placeholderapi) expansion |
| `chat.enabled` | `false` | Format chat with prefixes and suffixes |
| `chat.format` | `{prefix}{name}{suffix}&7: &r{message}` | The chat line |
| `tab.enabled` | `false` | Decorate and sort the tab list |
| `tab.format` | `{prefix}{name}{suffix}` | A name in the tab list; `""` leaves the names alone |
| `tab.sort` | `true` | Sort the tab list by primary group weight |

On a server Rift manages, the required settings can come from the
[environment](#one-configuration-for-every-instance).

## How RiftPerms applies permissions

- It waits for a joining player's snapshot during `AsyncPlayerPreLoginEvent`.
  The proxy usually pushes it before the player arrives.
- It injects its permissible at `PlayerLoginEvent`. Permission attachments
  still work and are consulted first, as in vanilla Bukkit.
- When a player's snapshot changes, it recalculates their permissions on
  their own entity scheduler, so it is safe on Folia.
- While the proxy is unreachable, the replica keeps answering at its last
  revision.

RiftPerms follows the [protocol](permissions-protocol.md) like the
[Rust client](permissions-clients.md#client-lifecycle): `hello` on every
connection (a different revision refetches every stored snapshot), then
change events and pushed snapshots. `/perms sync` makes it repeat the
handshake and refetch everything.

## Offline policy

The policy decides what a player gets when no snapshot came from the proxy,
because it is unreachable or did not answer within `join_timeout_ms`.
RiftPerms retries the fetch every 5 seconds. Players whose snapshot is already
in the replica keep being served from it.

| Policy | What the player gets |
| --- | --- |
| `deny` | Nothing is set, so plugin defaults and operator status apply |
| `last_snapshot` | What they had before the restart: RiftPerms writes the replica to `snapshot_file` when it is disabled and loads it on enable |
| `fallback_file` | What a file you write for emergencies gives them; a player it does not list gets `default` |

The fallback file lists groups and users in the
[typed node](permissions-protocol.md#typed-node) format. Inheritance,
weights, contexts, prefixes and meta all work:

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

`groups[]` take `name`, `weight` (default 0), `display_name` and `nodes`
(default `[]`); `users[]` take `uuid`, `primary_group` and `nodes`.

RiftPerms validates the file on enable, and a snapshot from the proxy always
replaces a fallback one.

Both files have the Rust client's formats, so one file serves Paper and
Pumpkin backends alike. See
[offline policies](permissions-clients.md#offline-policies) for the snapshot
file's format.

## Vault

With Vault installed, RiftPerms registers `Permission` and `Chat` providers.

- Reads come from the replica. An offline player's snapshot is fetched once.
- Writes become changes on the proxy and block for its reply, because Vault
  is synchronous.
- A `world` argument scopes a write to `server=<this server>` and
  `world=<world>`.
- `getPlayerGroups` lists every group the player is in, inherited ones
  included. `groupHas` looks at the group's own nodes only.
- A prefix or suffix set through Vault gets a priority ten above the highest
  one the subject has, so it wins.

## PlaceholderAPI

| Placeholder | Value |
| --- | --- |
| `%rift_prefix%`, `%rift_suffix%` | The winning prefix or suffix, or empty |
| `%rift_primary_group%` | The primary group |
| `%rift_meta_<key>%` | A meta value, or empty |
| `%rift_has_permission_<node>%` | `true` or `false` |
| `%rift_in_group_<name>%` | `true` or `false`, inherited groups included |

## Chat and tab list

RiftPerms can show prefixes and suffixes itself, so a server needs no chat or
tab plugin for rank tags. Both features are off by default and when their
section is missing, which leaves an existing install's plugins alone.

```yaml
chat:
  enabled: true
  format: "{prefix}{name}{suffix}&7: &r{message}"
tab:
  enabled: true
  format: "{prefix}{name}{suffix}"   # "" leaves the names alone
  sort: true                         # by primary group weight
```

| Placeholder | Value |
| --- | --- |
| `{prefix}`, `{suffix}` | The winning prefix or suffix in the player's current contexts, or nothing |
| `{name}` | The player's name |
| `{display_name}` | The display name, as other plugins set it |
| `{group}` | The primary group |
| `{message}` | The chat message; `chat.format` only, and required there |

Formats are legacy text like the prefixes themselves: `&` color and style
codes and `&#rrggbb`.

- A prefix is inserted before the line is parsed, so `&6[VIP] ` colors the
  name after it. Put `&r` or a color in front of what should not inherit.
- Names, display names and messages are never parsed. `&c` or `{prefix}`
  typed into chat stays text, and a player cannot add click or hover events.
- Players with `rift.chat.color` may use the legacy codes in their messages:
  colors and styles, nothing else. Nobody has it by default, operators
  included.

The chat format is a renderer set on Paper's `AsyncChatEvent` at priority
`LOW`. A chat plugin that sets its own renderer afterwards wins.

The tab list is sorted by the weight of each player's primary group, heaviest
first. Among equals the client's usual order applies: spectators last, then
by name.

- RiftPerms sets the player list order the 1.21.2 protocol introduced, not
  scoreboard teams. Name tags, collision rules and other plugins' teams are
  not involved, and it works on Folia.
- Paper 1.21 and 1.21.1 lack that order. Names are still decorated, RiftPerms
  logs one warning, and the list keeps the client's alphabetical order.
- Entries are redone on join, on a world or game mode change (contexts may
  select another prefix), when the player's snapshot changes and when group
  weights change. They reset when RiftPerms is disabled.

## Use `/perms` on the backend

`/perms` (alias `/rp`) accepts the LuckPerms grammar for common operations:

```text
/perms user Alice permission set essentials.fly true server=survival
/perms group vip setweight 50
/perms trace Alice essentials.fly
```

- The backend only forwards the arguments. The proxy parses, authorizes
  (`rift.perms.<verb>` nodes), applies, audits and answers with a message.
- A player cannot grant what they do not hold without `rift.perms.escalate`.
- The console may use `/perms` only when the server's principal has
  `control = true`.
- Tab completion offers online players, the group table and registered
  permissions.

On enable, RiftPerms sends every plugin's `plugin.yml` permissions to the
proxy for the dashboard and tab completion. For the commands themselves, see
[permission commands](permissions-commands.md).

## One configuration for every instance

When the server is an instance of a group (or a managed server) whose
definition has a `messaging` block, Rift gives it its own bus login and sets
`RIFT_SERVER_NAME`, `RIFT_SERVER_GROUP` and the `RIFT_BUS_*` variables (see
[bus logins for instances](bus-logins.md)).
RiftPerms falls back to them for every setting the file leaves out, so the
group's template can ship this `config.yml` for all instances:

```yaml
offline_policy: last_snapshot
```

| Setting left out | Value comes from |
| --- | --- |
| `server` | `RIFT_SERVER_NAME` |
| `group` | `RIFT_SERVER_GROUP` |
| `bus.address` | `RIFT_BUS_ADDRESS` |
| `bus.server_name` | `RIFT_BUS_SERVER_NAME` |
| `bus.certificate` | `RIFT_BUS_CERT` |
| `bus.token_env` | The token is read from `RIFT_BUS_TOKEN` |

Settings in the file win.

## What a LuckPerms user loses

| LuckPerms | With Rift |
| --- | --- |
| `/lp editor` | The Rift [dashboard](permissions.md#dashboard) edits groups, users and tracks |
| `/lp verbose` | `/perms trace <player> <node>` explains the winning entry and why every other match lost |
| The SQL tables, which some analytics plugins read directly | Gone |
| Plugins with a hard `depend: [LuckPerms]` or imports of `net.luckperms.api` | No LuckPerms API shim: they need a port to [`rift-permissions-api`](permissions-java.md), which is mostly renames |
| Regex nodes (`r=`) | The importer refuses them; `{a,b}` alternations are expanded into separate nodes on import |
