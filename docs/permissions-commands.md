# Permission commands

`/perms` edits and inspects the [permission store](permissions.md) with a
LuckPerms-style grammar. This page lists every command, who may run it, and
how LuckPerms import and export work.

```sh
/perms group vip create 10 VIP
/perms group vip meta setprefix 100 "&6[VIP] "
/perms user Alice parent addtemp vip 30d
/perms trace Alice essentials.fly server=survival
```

## Where commands run

Every surface speaks the same grammar and produces the same audit rows.

| Surface | How |
| --- | --- |
| Proxy, in game | The proxy answers `/perms` and its alias `/rp` itself, for network sessions; an extension command of the same name takes precedence. The command tree shows them to players who hold any `rift.perms.` node |
| Backend, in game | A backend adapter forwards its own `/perms` on [`rift.perms.command`](permissions-protocol.md#commands) and shows the reply |
| Proxy host | `rift admin perms <command>`; see [Command line](#command-line) |

In game, lines are colored by the theme styles `heading`, `good`, `bad`,
`muted` and `plain` (gold, green, red, gray and none by default), which you
restyle under `text.theme` ([styled text](text.md#theme)).

## Arguments

| Argument | Form |
| --- | --- |
| `<player>` | A name (anyone who has joined once) or a UUID |
| `[context...]` | `key=value` arguments (`server=survival world=world_nether`); `global` and `server=global` mean no context |
| `<time>` | A duration (`90s`, `30m`, `12h`, `7d`, `2w`, `1mo`, `1y`, combined like `1d12h`) or Unix seconds |
| Text with spaces | In double quotes: `meta setprefix 100 "&6[VIP] "` |

## User commands

Prefix each with `/perms user <player>`.

| Command | Effect |
| --- | --- |
| `info` | Online server, primary group, groups and contexts in effect, prefix, suffix and every own node |
| `permission set <node> [true\|false] [context...] [--duration <time>]` | Set a node (default `true`); `--expiry <unix>` sets an absolute end |
| `permission settemp <node> <true\|false> <time> [context...]` | Set a temporary node |
| `permission unset\|unsettemp <node> [context...]` | Remove the node in exactly these contexts (`unsettemp` only a temporary one) |
| `permission check <node> [context...]` | Show the node and what it resolves to |
| `parent add\|remove\|set <group> [context...]` | Inherit, stop inheriting, or make the group the only parent in these contexts |
| `parent addtemp <group> <time> [context...]` | Temporary membership |
| `parent removetemp <group> [context...]` | Remove a temporary membership |
| `parent switchprimarygroup <group>` | Explicit primary group |
| `meta set <key> <value> [context...]`, `meta unset <key> [context...]` | Set or remove a meta value |
| `meta settemp <key> <value> <time> [context...]` | Set a temporary meta value |
| `meta setprefix\|setsuffix <priority> <text> [context...]` | Set a prefix or suffix; also `addprefix`, `addsuffix` |
| `meta settempprefix\|settempsuffix <priority> <text> <time> [context...]` | Temporary prefix or suffix |
| `meta removeprefix\|removesuffix <priority> [context...]` | Remove a prefix or suffix |
| `meta clear [context...]` | Remove every prefix, suffix and meta value, or those in exactly these contexts |
| `permission info`, `parent info`, `meta info` | List the nodes of that kind |
| `promote\|demote <track> [context...]` | Move along a [track](permissions.md#tracks) |
| `clear [context...]` | Remove every node, or those in exactly these contexts |

## Group commands

Prefix each with `/perms group <group>`.

| Command | Effect |
| --- | --- |
| `create [weight] [display name]` | Also `/perms creategroup <group>` |
| `delete` | Also `/perms deletegroup <group>` |
| `info`, `listmembers` | Show the group; list its members |
| `setweight <weight>` | Set the weight |
| `setdisplayname [name]` | No name clears the display name |
| `permission …`, `parent …`, `meta …` | As for users, except `parent switchprimarygroup` |
| `clear [context...]` | As for users; `group default clear` empties `default` |

## Track commands

Prefix each with `/perms track <track>`.

| Command | Effect |
| --- | --- |
| `create [group...]` | Also `/perms createtrack <track>` |
| `delete` | Also `/perms deletetrack <track>` |
| `info`, `clear` | Show the track; remove every group from it |
| `append <group>`, `remove <group>` | Add a group at the end; remove a group |
| `insert <group> <position>` | Positions start at 1 |

## Other commands

Prefix each with `/perms`.

| Command | Effect |
| --- | --- |
| `check <player> <node> [context...]` | The proxy's answer, in the player's current contexts unless given |
| `trace <player> <node> [context...]` | The answer, the winning entry (pattern, subject, rank, contexts, expiry) and why every other match lost |
| `listgroups`, `listtracks` | Groups by weight; tracks |
| `search <node>` | Who sets the node, and who holds a wildcard covering it; `%` makes it a `LIKE` pattern |
| `log [--since <revision>] [--actor <actor>] [--subject <player\|group:name>] [--limit <n>]` | The change log |
| `compiled <player>` | The compiled-subject JSON a backend holds |
| `registered` | Permissions backends registered, with owner, default and description |
| `backends` | Backends the bus has heard from: server, revision at `hello`, last seen, client version |
| `sync` | Ask every backend to resync, then list them with their revisions |
| `doctor` | Problems; see [Doctor](#doctor) |
| `import <file> [--replace]`, `export <file>` | [LuckPerms JSON](#luckperms-import-and-export) (`.gz` is compressed); export never overwrites |
| `bulk [users\|groups\|all] [filters] <actions> [--dry-run]` | Patch matching nodes in one revision; see [Bulk edits](#bulk-edits) |

## Bulk edits

```sh
/perms bulk users key essentials.% kind permission set-value false --dry-run
```

| Filter | Selects |
| --- | --- |
| `key <like>` | SQL `LIKE` on the key (`%` and `_`) |
| `kind <kind>`, `value <value>`, `source <source>` | That kind, value or source |
| `context <k=v>` | Those contexts; repeat for several pairs. `context global` selects global nodes |
| `subject user:<player>\|group:<name>` | One subject |

Actions are `delete`, `set-value <true|false>`, `set-context <k=v|global>`
(repeatable), `set-expiry <time>`, `permanent` and `set-source <source>`.

## Who may run a command

- **Players** need `rift.perms.<verb>`, mirroring the grammar:
  `rift.perms.user.permission.set`, `rift.perms.user.parent.addtemp`,
  `rift.perms.group.meta.setprefix`, `rift.perms.track.append`,
  `rift.perms.trace`, `rift.perms.bulk` and so on. `settemp`, `unsettemp`,
  `addtemp`, `removetemp` and `settempprefix` are verbs of their own.
  `rift.perms.*` grants everything.
- **No escalation.** A player cannot grant a permission or group they do not
  hold themselves, in the node's contexts, unless they hold
  `rift.perms.escalate`. Denying and removing are always allowed.
- **Files.** In game, `import` and `export` take a bare file name in the
  proxy's working directory.
- **Backend consoles** may run commands only when their principal has
  `control = true`.
- **`rift admin perms` and the dashboard** act as the console.

## Command line

`rift admin perms <command>` runs with console rights and needs the
[admin CLI](admin.md) permission `perms`.

```sh
rift admin perms user Alice parent add vip
rift admin perms trace Alice essentials.fly server=survival
rift admin perms log --since 400 --actor cli
rift admin perms export ./luckperms.json.gz
rift admin perms compiled Alice
rift admin perms doctor
```

The text is printed as it would appear in game; a failed command exits
non-zero. `import` and `export` paths are resolved where the CLI runs and read
or written by the proxy process.

## Doctor

`/perms doctor` lists:

| Finding | Meaning |
| --- | --- |
| Dangling parents | `inherit` nodes, or track entries, naming groups that do not exist. They grant nothing until the group is created again |
| Unreachable groups | Groups nobody inherits and no track lists |
| Expired nodes | Nodes the sweeper has not deleted yet. They already grant nothing |
| Stale backends | Backends whose last `hello` reported an older revision than the proxy's, with no `hello` since (run `/perms sync`), and principals that never said `hello` |
| Ungranted commands | Extension commands whose `permission` is neither registered by a backend nor granted anywhere. Commands with an empty permission need none and are skipped |

`rift check --strict` warns about the last kind as well, reading the store
read-only and `extensions.groups`.

## LuckPerms import and export

Rift reads the JSON that `/lp export` writes, gzipped or plain, and exports
the same format, so `/lp import` can read it back.

| Surface | Import | Export |
| --- | --- | --- |
| Command | `/perms import <file> [--replace]` | `/perms export <file>` |
| [HTTP API](permissions-http.md) | `POST /api/perms/import` with the file, gzipped or not; `?replace=true` for replace | `GET /api/perms/export` |

An import maps LuckPerms data like this:

- `group.x`, `prefix.N.text`, `suffix.N.text`, `meta.key.value`, `weight.N`
  and `displayname.x` become typed nodes; group weights and display names come
  from their global nodes.
- `{a,b}` and `(a|b)` shorthands expand into separate nodes.
- LuckPerms context sets mean "any listed value"; Rift writes one node per
  combination, and drops `server=global` and `world=global`.
- `server=<name>` naming one of the configuration's service groups becomes
  `group=<name>`, because Rift's instances of that group are `<name>-1`,
  `<name>-2`, …, where only `group` matches. Names that are not service groups
  stay `server`.
- Regex nodes (`R=…`), invalid names, nodes past a limit and expiries already
  in the past are skipped, and the report lists each with the reason.

Everything lands with source `import` in one revision.

- **Additive by default.** Nodes replace what is in their slots and nothing
  else is deleted.
- **With replace**, every earlier `import` node is deleted first, in the same
  revision. Nodes from other sources are never touched.

An export differs from the store in two ways:

- `group=<name>` is written back as `server=<name>`, so LuckPerms reads it the
  old way.
- A context key with several values keeps its first value, since LuckPerms
  would read the list as "any".
