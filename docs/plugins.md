# Plugins

A plugin is code that receives Rift's events and calls Rift's host API. Every
plugin uses the same contract, [`wit/rift-plugin.wit`](../wit/rift-plugin.wit)
(`rift:plugin@0.3.0`): one event schema, one set of answers, one host API. What
differs is where the code runs:

| Mode | What it is | Trust | Configured with |
| --- | --- | --- | --- |
| **script** | `rift.lua`, its modules and script-mode plugin folders, in the sandboxed configuration VM | Sandboxed: instruction and memory budgets, no I/O | `rift.on`, `rift.command`, `rift.config.plugins`, and `rift.plugin` for a folder without a persistent manifest |
| **persistent** | A plugin folder on disk with a `plugin.toml`, running in its own long-lived Lua state or WebAssembly instance on its own OS thread | Trusted: runs with Rift's process privileges | `rift.plugin(name, config)` |
| **remote** | A service on the [message bus](messaging.md) that answers `login`, `initial_server`, `before_transfer` and `kick`, and calls the `players` interface as [`rift.players.*`](messaging-subjects.md#player-api) | Whatever the service is; Rift only reads its replies and checks its login's grants | `rift.config.plugins.decisions`, `messaging.principals` |

Persistent plugins have two engines: [Lua](plugins-lua.md) (LuaJIT, the same
`rift` API as `rift.lua`) and WebAssembly components written in
[Rust](plugins-rust.md), [C](plugins-c.md), [Go](plugins-go.md) or any language
that targets WASI 0.2. The [examples](../examples/plugins) implement one
plugin, `hello`, in all four languages with identical behavior.

## Find the right page

| You want to | Read |
| --- | --- |
| Write policy in `rift.lua`: callbacks, commands, queues | [Hooks in `rift.lua`](extensions.md) |
| Let a bus service decide logins | [Decisions answered over the message bus](remote-decisions.md) |
| Know the events and their answers | [Plugin events and commands](plugin-events.md) |
| Message, kick or transfer players | [Online players API](players-api.md) |
| Style text, ship messages in each player's language | [Styled text](text.md), [plugin messages](text.md#plugin-messages) |
| Store data, schedule work, call HTTP | [Host services](host-services.md) |
| Write a persistent plugin | [Lua](plugins-lua.md), [Rust](plugins-rust.md), [C](plugins-c.md), [Go](plugins-go.md) |
| Operate plugins: restarts, reloads, logs | [Running plugins](plugin-operations.md) |

## Enable a plugin

Copy the plugin folder into `plugins/` beside `rift.lua` and name it in the
configuration:

```lua
-- rift.lua
rift.plugin("hello-lua", { deny = "Mallory" })
rift.plugin("relaxoban", { database_env = "RELAXOBAN_DSN" })
rift.plugin("greeting") -- a script-mode folder
```

`rift.plugin(name, config?)` looks at `plugins/<name>/`:

- With a `plugin.toml` that is persistent (the default mode), the plugin is
  **recorded, not run**: its code never executes in the configuration VM, not
  even during `rift check`. `config` (an optional table) is encoded as JSON, at
  most 64 KiB, and passed to the plugin's `init`. The call returns `true`.
  Naming the same persistent plugin twice is an error.
- With a `plugin.toml` that says `mode = "script"`, or with only an `init.lua`,
  `init.lua` runs inside the configuration VM with the plugin name and `config`
  as `...` ([folder-based plugins](lua.md#folder-based-plugins)). Its result is
  cached and returned.

Plugin names are 1–128 letters, digits, `_` or `-`, and a manifest's `name`
must match its folder. Persistent plugins are kept in `rift.plugin()` call
order, which is their [dispatch order](plugin-events.md#dispatch-order).

## `plugin.toml`

Parsing is strict: unknown keys, keys that do not apply to the selected mode or
engine, and out-of-range values are errors, reported by `rift check`. The file
is at most 64 KiB.

```toml
name = "relaxoban"          # must match the folder plugins/relaxoban
version = "0.3.1"           # 1–64 letters, digits, '.', '_', '+' or '-'
api = "0.3"                 # the rift:plugin track this Rift implements
min_api = "0.3.0"           # optional: the lowest 0.3.x the plugin runs on
mode = "persistent"         # or "script"; default "persistent"
engine = "wasm"             # "lua" or "wasm"; required for persistent plugins
entry = "relaxoban.wasm"    # default "init.lua" for Lua; required for WebAssembly
hot_reload = false          # call reload(config) instead of restarting on a config change
cluster = true              # optional: works with one instance per cluster peer ("aware" also works)

[deadlines]                 # milliseconds
login = 2000                # any hook that is awaited (see the event table)
init = 30000                # default 30 000, at most 300 000
reload = 5000               # default 5 000, at most 60 000
shutdown = 10000            # grace period; default 10 000, at most 60 000
work = 30000                # one background job; default 30 000, at most 300 000

[failure]
on_timeout = "deny"         # "deny" or "continue"; default depends on the hook
on_failure = "deny"         # "deny" or "continue"; default "deny"
restart = "on-failure"      # "never", "on-failure" or "always"; default "on-failure"

[capabilities]
filesystem = ["data"]       # "data", "host" or absolute paths; default ["data"]
network = true              # default true
env = ["RELAXOBAN_DSN"]     # omit to inherit Rift's environment minus its own secrets; [] for none

[background]                # workers for rift.background / the background interface
workers = 2                 # 0..=16; default 1 when the table is present, else none
entry = "worker.lua"        # engine = "lua" only: the worker script; default "worker.lua"

[lua]                       # engine = "lua" only
ffi = false                 # LuaJIT's FFI; default false
native_modules = false      # require() of C modules; default false
memory_limit_mb = 256       # 0..=65536; default 256, 0 is unlimited
jit = true                  # false: interpret everything so deadlines interrupt every loop
```

| Key | Default | Notes |
| --- | --- | --- |
| `name`, `version`, `api` | required | `api` must be `"0.3"`, for every engine. A manifest that says `"0.2"` is refused with a pointer to [Moving from 0.2](#moving-from-02), one that says `"0.1"` to [Moving from 0.1](#moving-from-01) as well. |
| `min_api` | none | An exact version of the track, such as `"0.3.1"`, for a plugin that calls something a later 0.3 release added. An older Rift refuses to load the plugin instead of failing at the first call. Everything in 0.3.0, menus included, needs none. `meta.api` returns the version of the running host. |
| `mode` | `"persistent"` | A `"script"` manifest may contain only `name`, `version` and `api`; the folder's `init.lua` runs in the sandbox. |
| `engine` | required | An engine missing from this build is a warning in `rift check`, an error in `rift check --strict`, and refused at startup and reload. |
| `entry` | `init.lua` (Lua) | A relative path of at most 256 bytes inside the folder, without `..`, not inside `data/` and without hidden (dot) components. |
| `hot_reload` | `false` | See [reloads](plugin-operations.md#reloads). |
| `cluster` | not declared | `true` or `"aware"` declares that the plugin works in a [cluster](#plugins-in-a-cluster); `false` is the same as leaving it out. Allowed in `"script"` manifests too. |
| `[deadlines]` | per hook | Keys are `init`, `reload`, `shutdown`, `work` and the awaited hooks of the [event table](plugin-events.md#all-events). Awaited hooks allow 1–10 000 ms, `http` up to 60 000. Observers have no deadline and are rejected here. |
| `[failure]` | | `on_timeout` overrides the per-hook default for every hook ([deadlines and failures](plugin-events.md#deadlines-and-failures), [restarts](plugin-operations.md#failures-and-restarts)). |
| `[background]` | no workers | See [background work](plugin-events.md#background-work). |
| `[lua]` | `memory_limit_mb = 256` | See [the JIT and deadlines](plugins-lua.md#the-jit-and-deadlines). |
| `[wasm]` | `memory_limit_mb = 256` | `engine = "wasm"` only: per-instance limits; see [WebAssembly plugins](#webassembly-plugins). |

### Capabilities

`[capabilities]` defaults to `filesystem = ["data"]`, `network = true` and all
of `env` except
[Rift's own secrets](hardening.md#secrets-in-environment-variables).
WebAssembly enforces all three grants; Lua does not:

| Grant | Meaning | In a Lua plugin |
| --- | --- | --- |
| `filesystem`: `data` | `plugins/<name>/data/`, created on start, read-write | Advisory: Lua `io` sees what Rift sees |
| `filesystem`: `host` | The configuration directory, read-only | Advisory |
| `filesystem`: an absolute path | Read-write. For WebAssembly it may not contain or lie inside `local/plugincache` or `plugins/` | Advisory |
| `network` | Network access | Gates only `rift.http.request` |
| `env` | The visible environment variables | Gates only `os.getenv` |

`rift check` prints each plugin with its mode and capabilities, and marks the
grants Lua does not enforce, so the trust each plugin gets is visible before
anything starts.

## `messages.toml`

A plugin folder, persistent or script mode, may hold a `messages.toml` beside
its `plugin.toml` or `init.lua`: the plugin's own messages, in each player's
language, which the operator can reword by qualified key (`party:invited`).
Rift reads and checks it with the configuration, without running plugin code,
and `rift check` prints its message and language counts. A changed file
applies on the next reload without restarting the plugin. The `hello`
examples ship one. See [plugin messages](text.md#plugin-messages).

## Host API

Every engine exposes the same host API; the
[Lua guide](plugins-lua.md#host-api) and the SDK guides map it to each
language. The WIT spells names with hyphens (`set-language`), Lua with
underscores (`set_language`).

| Interface | Functions | Notes |
| --- | --- | --- |
| `players` | `list(server?)`, `get(target)`, `names(prefix?)`, `message`, `action-bar`, `title`, `tab-list`, `boss-bar`, `dialog`, `clear-dialog`, `menu`, `update-menu`, `close-menu`, `kick`, `transfer`, `broadcast`, `lookup`, `search`, `set-language` | See [Online players API](players-api.md). `target` is a name, a UUID or a connection id. Actions return whether the player was online and take [`text`](plugin-events.md#text). A `title-spec` has an optional title and subtitle (at least one) and times in ticks. `list` and `get` return a `player-info`, the [player table](players-api.md#player-tables). `lookup`, `search` and `set-language` are [the player directory](plugin-events.md#the-player-directory-and-languages). `menu`, `update-menu` and `close-menu` show [inventory menus](menus.md), whose events come back through the `menu` hook. |
| `store` | `get`, `set`, `delete`, `increment`, `list(namespace, prefix)` | The durable [store](host-services.md#persistent-state) the configuration script uses too. Namespaces are private: `plugin.<name>.<namespace>`, which the script's `rift.store` cannot read or write. Each plugin has its own quota, 4096 entries and 1 MiB unless [`extensions.storage.quota`](host-services.md#limits-and-quotas) sets another. |
| `permissions` | `has`, `set`, `add-group(uuid, group, expires-at?)`, `remove-group`, `groups`, `list-groups`, `meta`, `set-meta` | Rift's [permission store](permissions-lua.md#persistent-plugins): user nodes, group memberships (until `expires-at` Unix seconds when given, for a rank that runs out) and meta values with contexts and expiry, checked in the player's current contexts. |
| `bus` | `publish`, `request(subject, payload, timeout_ms)`, `subscribe(subject, queue_group?)`, `unsubscribe`, `stream-publish` | Messages arrive through the `message` hook. At most 256 subscriptions; requests wait at most 60 s. |
| `servers` | `create`, `remove`, `start`, `stop`, `download`, `control(operation, payload_json)` | The same `rift.control.*` operations as the bus, awaiting the reply (at most 60 s). |
| `timers` | `every(ms)`, `after(ms)`, `cancel(handle)` | Ticks arrive through the `timer` hook. At least 10 ms; at most 1024 timers. |
| `log` | `log(level, message)` | Rift's log, prefixed with the plugin name, and the plugin's [log tail](plugin-operations.md#logs). Control characters are replaced. |
| `background` | `defer()`, `resolve(ticket, decision)`, `run(payload)`, `is-worker()` | [Background work](plugin-events.md#background-work). |
| `meta` | `name`, `version`, `data-dir`, `config-dir`, `clock`, `time`, `api` | `data-dir` and `config-dir` are guest paths (`/data`, `/host` for WebAssembly). `api` is the contract version of the host, such as `0.3.0`. |
| `text` | `render(text, language?)`, `plain`, `to-legacy`, `to-mini` | What [`rift.text`](text.md#rifttext-in-lua) does in Lua. Each converts [`text`](plugin-events.md#text) with markup, legacy codes and [messages](text.md#plugin-messages) resolved, the plugin's own by unqualified key, in `language` (a tag such as `de` or `de_de`) or the default, and returns compact JSON, the words, `§` codes or markup. An invalid text or language is the error. It works in `init`, `reload` and background workers, so `init` can check a template from the configuration. |

Calls that wait (bus requests, server operations, store writes) block only the
plugin's own thread. Persistent plugins get no per-call budgets beyond their
deadlines.

## Plugins in a cluster

In a [cluster](clustering.md) of several proxies every peer loads every
plugin and runs its own instance. Session events (login, chat, commands,
joins) arrive on the peer that holds the session, so plugin code sees each of
them once. What differs from a single Rift:

- **In-memory state is per peer.** A counter, cache or list a plugin keeps in
  memory holds only what its own peer saw. Keep network-wide state in the
  [store](host-services.md#persistent-state), which the cluster replicates,
  or on the bus.
- **Jobs.** `rift.every` jobs (in the configuration script and Lua plugins)
  run on the leader only by default (`scope = "cluster"`); `scope = "peer"`
  runs them everywhere. `rift.timers.*` and the WebAssembly `timers`
  interface tick on every peer: the contract has no scoped timer yet, so a
  WebAssembly job that must run once takes a lease instead.
- **Work that must run once** takes a lease: `rift.cluster.lease(name,
  ttl_ms)` returns a fencing token on at most one peer at a time ([leases in
  a cluster](host-services.md#leases-in-a-cluster)). The leases are
  available to the configuration script and Lua plugins; WebAssembly plugins
  cannot reach them yet.

A plugin whose author checked these points declares it in `plugin.toml`:

```toml
cluster = true
```

Every plugin loads with or without the declaration, so existing plugins keep
working. In a cluster, persistent plugins without it are flagged: `rift check`
warns about each one, and the plugin status in `rift admin plugins list`,
`GET /api/plugins` (`cluster_warning`) and the dashboard's Plugins view says
"not declared cluster-aware". The flag is advice for the operator, not a
refusal. A single Rift never shows it.

## Trust model

Persistent plugins are trusted code, like a Velocity or BungeeCord plugin jar.
Install only plugins you would run as Rift's user.

> [!WARNING]
> A persistent Lua plugin can open any file Rift can, run programs through
> `io.popen` and use the network. With `lua.ffi = true` or
> `lua.native_modules = true` it runs native code in Rift's address space and
> can crash or corrupt the proxy.

- **WebAssembly isolates memory, not privilege.** A WebAssembly plugin cannot
  touch Rift's memory, and a trap ends only its own instance. No grant lets it
  change code Rift runs: `host` is read-only, absolute grants may not cover the
  compiled component cache or `plugins/`, and Rift loads a compiled component
  only when it carries an HMAC under a per-install secret
  (`local/plugincache/key`, mode 0600, created on first use).
- **A plugin can do everything its capabilities allow.** With full WASI
  (`filesystem = ["host"]`, `network = true`, the whole environment) it can
  read `rift.sqlite3` and the configuration with its secrets, fill the disk
  through `data`, read every secret in the environment (Rift's own token
  variables only when the manifest names them in `env`), or open ports.
- **Every plugin can use the whole host API**: kick players, change
  permissions, publish on the bus and start servers.
- **Manifests restrict capabilities.** `filesystem = ["data"]`,
  `network = false` and `env = []` (or a list of names) confine a WebAssembly
  plugin to its own folder. The defaults are permissive. Lua honors `env` and
  `network` for the Rift API only: it can write anywhere Rift can, the
  component cache included, and `io.popen` starts programs with Rift's full
  environment.
- **Rift never downloads or auto-enables plugins.** A plugin runs only when its
  folder is on disk and the configuration names it.

## Why configuration code stays sandboxed

`rift.lua` can be edited over HTTP by anyone holding a `config` token, through
the [web dashboard](http.md) and `PUT /api/config`, and it is evaluated on
every validation, save, reload and `rift check`. If configuration code could
open files, load native libraries or keep state in the proxy, a token meant for
editing routes would be a remote shell inside the proxy process.

So code that arrives through the configuration path never runs trusted. The
script, its modules and script-mode plugin folders run in the restricted VM of
[Lua configuration](lua.md#the-lua-environment), with a fresh state per
callback, an instruction budget, a memory limit and a 50 ms deadline.

Trusted code reaches Rift only when an operator copies a plugin folder into
`plugins/` on the proxy host and the configuration names it. The configuration
can enable, configure, reorder or remove an installed plugin, but cannot
install or change one: the plugin folders are not part of the configuration
source the web API edits.

> [!WARNING]
> The sandbox does not make `config` a harmless grant. A `config` token can
> also define [managed server](managed-servers.md) commands, which Rift runs as
> processes, so grant it only to operators you would trust with a shell on the
> host. Keep `plugins/` outside `template_storage` and every managed server
> directory, so that `files` access cannot write plugin code either.

## WebAssembly plugins

A WebAssembly plugin is a component built for `wasm32-wasip2` against world
`plugin`. Rift runs it on Wasmtime, one instance per plugin on the plugin's
thread (plus one per [background worker](plugin-events.md#background-work)).
It links Rift's interfaces plus WASI 0.2, and `[capabilities]` decides which
directories, network access and environment variables the instance sees.

The guides for [Rust](plugins-rust.md), [C](plugins-c.md) and
[Go](plugins-go.md) cover building one.
[The WebAssembly examples](../examples/plugins/README.md) describe the engine's
limits (`[wasm]`), deadline interrupts, captured output and compilation cache.

## Contract rules

The WIT keeps guests in C and Go pleasant:

- WASI 0.2 without `async`, `stream` or `future`.
- No `resource` types: host-managed things are `u64` handles.
- Plain records, variants and lists; configuration and free-form payloads are
  JSON strings.
- Every hook is an export with a default in each SDK.
- Guests build against world `plugin`, which includes the WASI CLI imports;
  world `core` is the same without WASI.

Additions keep compiled components loading. Wasmtime matches interface versions
on the `0.3` track, so a component built against `0.3.0` links against every
later `0.3` host. An addition may bring new host functions, types and
interfaces, but never changes an existing type or adds a case to an existing
enum or variant, since exports are checked exactly.

A host function a component calls needs a host that has it: such a plugin names
the release in [`min_api`](#plugintoml), for example `min_api = "0.3.1"` for
something 0.3.1 adds. Breaking changes move to the next minor version until
1.0. A new hook is one, because a component exports exactly what `handler`
lists. A Rift hosts one track: a 0.3 host loads only `api = "0.3"` plugins.

## Moving from 0.2

`rift:plugin@0.3.0` is 0.2.1 plus [inventory menus](menus.md): the
`menu-event` record, `players.menu`, `update-menu` and `close-menu`, and the
`menu` hook in `handler`. Nothing else changed, so the move needs no change in
a plugin's source. Rift hosts with this contract load only `api = "0.3"`
plugins, for every engine; a 0.2 manifest is refused with a pointer here, and
a component built against 0.2 does not link.

| Plugin | What changes |
| --- | --- |
| `rift.lua` | Nothing |
| Persistent Lua | `api = "0.3"` in `plugin.toml` |
| WebAssembly | `api = "0.3"` in `plugin.toml`, the 0.3.0 SDK and a rebuild. Every SDK gives the `menu` hook a default, as it does for the other hooks |

`min_api` starts again on the new track: a manifest that names a 0.2 version
is refused, and a plugin that shows menus needs none, because every 0.3 host
has them. The `text` interface, which needed `min_api = "0.2.1"`, is part of
0.3.0.

## Moving from 0.1

`rift:plugin@0.2.0` is 0.1.5 without its duplicates, plus background work, the
player directory, languages, command aliases and the console. A 0.1 plugin
takes this table and then [Moving from 0.2](#moving-from-02): today's Rift
refuses a 0.1 manifest with a pointer to both sections, and a component built
against 0.1 does not link.

| 0.1 | 0.2 |
| --- | --- |
| `api = "0.1"` | `api = "0.2"`; rebuild WebAssembly components against the 0.2 SDK |
| `message`/`message-json`, `action-bar`/`-json`, `kick`/`-json`, `broadcast`/`-json` | one function each, taking `text` (`plain` or `json`) |
| `title` and `title-json`, `title-spec` and `title-json-spec` | `title` with a `title-spec` of optional `text` title and subtitle |
| `action` (export `handler`) and `decision` (export `decisions`) | `decision`, with `deny(text)` and `message(text)` |
| exports `styled` (`rich-suggestion`, `rich-status-override`) | `suggestion.tooltip` and `status-override.motd`, `version`, `sample` are `text` |
| exports `dialogs`, `decisions`, `styled`, `chat`; worlds `plugin-chat`, `core-chat`, `optional*`, `base` | everything is in `handler`; worlds `plugin` and `core` |
| exporting `chat` registers for chat | list the `chat` hook; `kick` is a hook for components too |
| `players.visible-to(target)`, `players.locale(target)` | fields of `player-info`, with `ip`, `properties` and `language` |
| `permissions.add-group(uuid, group)` | `add-group(uuid, group, expires-at?)` |
| `command-spec { name, permission, suggest }` | plus `aliases` and `console` |
| `context` | plus `language` and `console` |

Persistent Lua plugins only change `api` in their manifest: the `rift` API
kept its functions and gained the new ones ([Lua](plugins-lua.md)).
`ctx.api_version` and `rift.api_version` are now always 3.

The configuration script's `extensions.api_version` (1, 2 or 3) no longer
selects anything: every script has the whole API. The field is still accepted
and ignored, so existing configurations load unchanged.
