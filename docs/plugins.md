# Plugins

Rift has one plugin system. A plugin is anything that receives Rift's events
and calls Rift's host API, and every plugin uses the same contract,
[`wit/rift-plugin.wit`](../wit/rift-plugin.wit) (`rift:plugin@0.1.3`): one
event schema, one set of answers, one host API. What differs is where the code
runs:

| Mode | What it is | Trust | Configured with |
| --- | --- | --- | --- |
| **script** | `rift.lua`, its modules and script-mode plugin folders, in the sandboxed configuration VM | Sandboxed: instruction and memory budgets, no I/O | `rift.on`, `rift.command`, `rift.config.plugins`, `rift.plugin` for a folder without a persistent manifest |
| **persistent** | A plugin folder installed on disk with a `plugin.toml`, running in its own long-lived Lua state or WebAssembly instance on its own OS thread | Trusted: runs with Rift's process privileges | `rift.plugin(name, config)` |
| **remote** | A service on the [message bus](messaging.md) that answers `login`, `initial_server` and `before_transfer` | Whatever the service is; Rift only reads its replies | `rift.config.plugins.decisions` |

Persistent plugins come in two engines: [Lua](plugins-lua.md) (LuaJIT, the
same `rift` API as `rift.lua`) and WebAssembly components written in
[Rust](plugins-rust.md), [C](plugins-c.md), [Go](plugins-go.md) or any language
that targets WASI 0.2. The [examples](../examples/plugins) implement one
plugin, `hello`, in all four languages with identical behaviour.

The per-event callbacks, commands, permissions, queues and the API v2 host
services that the configuration script uses are described in
[Plugins: events, commands and policy](extensions.md).

## Why configuration code stays sandboxed

Code that arrives through the configuration path never runs trusted.
`rift.lua` can be edited over HTTP by anyone holding a `config` token, through
the [web dashboard](http.md) and `PUT /api/config`, and it is evaluated on every
validation, save, reload and `rift check`. If configuration code could open
files, load native libraries or keep state in the proxy, a token meant for
editing routes would be a remote shell inside the proxy process. So the script,
its modules and script-mode plugin folders run in the restricted VM described
in [Lua configuration](lua.md#the-lua-environment), with a fresh state per
callback, an instruction budget, a memory limit and a 50 ms deadline.

Trusted code reaches Rift only one way: an operator copies a plugin folder
into `plugins/` on the proxy host, and the configuration names it with
`rift.plugin`. The configuration can enable, configure, reorder or remove an
installed plugin, but it cannot install or change one: the plugin folders are
not part of the configuration source the web API edits. Rift never downloads
or auto-enables plugins.

The sandbox keeps configuration evaluation from being a way in; it does not
make `config` a harmless grant. A `config` token can also define
[managed server](managed-servers.md) commands, which Rift runs as processes, so
grant it only to operators you would trust with a shell on the host. Keep
`plugins/` outside `template_storage` and every managed server directory, so
that `files` access cannot write plugin code either.

## Enabling a plugin

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
- With a `plugin.toml` that says `mode = "script"`, or with only an
  `init.lua`, the folder's `init.lua` runs inside the configuration VM with the
  plugin name and `config` as `...`, exactly as described in
  [folder-based plugins](lua.md#folder-based-plugins). Its result is cached
  and returned.

Plugin names are 1–128 letters, digits, `_` or `-`, and a manifest's `name`
must match its folder. Persistent plugins are kept in `rift.plugin()` call
order, which is their dispatch order.

## `plugin.toml`

Parsing is strict: unknown keys, keys that do not apply to the selected mode
or engine, and out-of-range values are errors, reported by `rift check`. The
file is at most 64 KiB.

```toml
name = "relaxoban"          # must match the folder plugins/relaxoban
version = "0.3.1"           # 1–64 letters, digits, '.', '_', '+' or '-'
api = "0.1"                 # the rift:plugin contract version this Rift implements
mode = "persistent"         # or "script"; default "persistent"
engine = "wasm"             # "lua" or "wasm"; required for persistent plugins
entry = "relaxoban.wasm"    # default "init.lua" for Lua; required for WebAssembly
hot_reload = false          # call reload(config) instead of restarting on a config change

[deadlines]                 # milliseconds
login = 2000                # any hook that is awaited (see the event table)
init = 30000                # default 30 000, at most 300 000
reload = 5000               # default 5 000, at most 60 000
shutdown = 10000            # grace period; default 10 000, at most 60 000

[failure]
on_timeout = "deny"         # "deny" or "continue"; default depends on the hook
on_failure = "deny"         # "deny" or "continue"; default "deny"
restart = "on-failure"      # "never", "on-failure" or "always"; default "on-failure"

[capabilities]
filesystem = ["data"]       # "data", "host" or absolute paths; default ["data"]
network = true              # default true
env = ["RELAXOBAN_DSN"]     # omit to inherit Rift's whole environment; [] for none

[lua]                       # engine = "lua" only
ffi = false                 # LuaJIT's FFI; default false
native_modules = false      # require() of C modules; default false
memory_limit_mb = 0         # 0..=65536, 0 is unlimited
jit = true                  # false: interpret everything so deadlines interrupt every loop
```

| Key | Default | Notes |
| --- | --- | --- |
| `name`, `version`, `api` | required | `api` must be `"0.1"`. |
| `mode` | `"persistent"` | `"script"` manifests may contain only `name`, `version` and `api`; the folder's `init.lua` runs in the sandbox. |
| `engine` | required | `"lua"` or `"wasm"`. An engine missing from this build is a warning in `rift check`, an error in `rift check --strict`, and refused at startup and reload. |
| `entry` | `init.lua` (Lua) | A relative path of at most 256 bytes inside the folder, without `..`, not inside `data/` and without hidden (dot) components. Required for WebAssembly. |
| `hot_reload` | `false` | See [lifecycle](#lifecycle-failures-restarts-and-reloads). |
| `[deadlines]` | per hook | Keys are `init`, `reload`, `shutdown` and the awaited hooks; observers have no deadline and are rejected here. Awaited hooks allow 1–10 000 ms, `http` up to 60 000. |
| `[failure]` | | `on_timeout` overrides the per-hook default for every hook. |
| `[capabilities]` | `filesystem = ["data"]`, `network = true`, all of `env` | `data` is `plugins/<name>/data/` (created on start, read-write); `host` is the configuration directory, read-only; an absolute path is read-write and, for WebAssembly, may not contain or lie inside `local/plugincache` or `plugins/`. WebAssembly enforces all three. Lua enforces `env` (`os.getenv`) and `network` (`rift.http.request`) but not `filesystem`: Lua `io` sees what Rift sees. |
| `[lua]` | | `engine = "lua"` only. See [the JIT and deadlines](plugins-lua.md#the-jit-and-deadlines). |
| `[wasm]` | | `engine = "wasm"` only: per-instance limits; see [WebAssembly plugins](#webassembly-plugins). |

`rift check` prints each plugin with its mode and capabilities, so the trust
each one gets is visible before anything starts. For Lua plugins it marks what
is not enforced: `filesystem` is advisory, `network` gates only
`rift.http.request` and `env` only `os.getenv`, because Lua `io` and `os` run
with Rift's privileges.

## Events

Every hook is an export in the contract and has a default in every SDK, and a
plugin lists the hooks it wants in the registration `init` returns. The host
skips hooks a plugin did not list, so unused hooks cost nothing. **An empty
`registration.hooks` list delivers every hook.** The Lua engine computes the
list from `rift.on`, `rift.command`, timers and subscriptions.

| Hook | Kind | Answer | Default deadline | Default `on_timeout` |
| --- | --- | --- | --- | --- |
| `route` | awaited | keep the policy, pick a backend or group, or reject | 50 ms | deny (close the connection) |
| `login` | awaited | continue or deny | 1000 ms | deny |
| `initial_server` | awaited | continue, deny or a server | 1000 ms | deny |
| `before_transfer` | awaited | continue or deny | 300 ms | deny |
| `command` | awaited, owner only | message, server, queue, leave-queue or continue | 1000 ms | deny (the command reports an error) |
| `suggest` | awaited, owner only | completions | 50 ms | continue (no completions) |
| `status` | awaited | fields to replace in one server-list ping | 50 ms | continue |
| `http` | awaited | a response for `/ext/*`, or none | 5000 ms | continue |
| `join`, `after_transfer`, `disconnect` | observer | none | none (queued; interrupted after 60 s) | – |
| `message` | observer | a message on a subscription the plugin made | none (queued; interrupted after 60 s) | – |
| `timer` | observer | a tick of a timer the plugin started | none (queued; interrupted after 60 s) | – |
| `event` | observer | a `rift.events.*` publication | none (queued; interrupted after 60 s) | – |
| `dialog` | observer, owner only | a click on a plugin action of a [dialog](dialogs.md) the plugin showed | none (queued; interrupted after 60 s) | – |
| `init`, `reload`, `shutdown` | lifecycle | registration / ok / none | 30 s / 5 s / 10 s | init fails the plugin; reload keeps the old configuration |

Player hooks (`login`, `initial_server`, `before_transfer`, `command`,
`suggest`, `join`, `after_transfer`, `disconnect`) need
`authentication.online_mode`, because they carry a verified identity. Without
it, persistent plugins still run `route`, `status`, `http`, timers, bus
messages and events, but a plugin whose registration lists a player hook,
registers a command or has an empty hook list fails to start (it is not
retried), so a ban list cannot silently allow everyone; `rift check` and `rift
check --strict` warn when persistent plugins are configured without it.
Turning `online_mode` on or off restarts every persistent plugin. Answers are
validated exactly like Lua results and bus replies: strings are 1–1024 bytes,
JSON text components (the `deny-json` and `message-json` answers of the
optional `decisions` export, which a WebAssembly component that has it answers
decision hooks with instead of `handler`) at most 32 KiB, servers must exist, and an answer a hook does not accept is a failure of the plugin.
The same holds for the optional `styled` export (since 0.1.3), which a
component that has it answers `suggest` and `status` with instead of
`handler`: its suggestion tooltips (`tooltip-json`; empty means none), status
`version-json` and `sample-json` lines are JSON text components, the `mini`
and `legacy` [authoring objects](text.md) included, or text shown as written;
clients see the version and sample lines as `§` codes. One invalid entry fails
the whole answer.

**Deny or continue.** When a plugin misses its deadline, `on_timeout` applies;
when it fails (a handler error, a trap, an invalid answer, or the plugin is
failed or restarting), `on_failure` applies. `deny` means the event fails the
way an error in `rift.lua` would: the login or transfer is refused, a route
closes the connection, a command reports an error. `continue` skips this plugin
and asks the next one. Rift never treats an error as permission to proceed
unless a manifest says so. Each plugin has a queue of 1024 undelivered calls;
when it is full, awaited calls count as timeouts at once and observer events
are dropped (a slow plugin stalls only itself).

## Dispatch order

For `route`, `login`, `initial_server`, `before_transfer`, `status` and
`http`, Rift asks in this order:

1. **script**: the configuration script's callback and `rift.on` handlers;
2. **persistent** plugins, in `rift.plugin()` order;
3. **remote**: the bus responder in `rift.config.plugins.decisions`, for the
   three decision hooks.

The first answer other than `continue` (or the default) wins and stops the
chain, so a deny anywhere is final. Observers go to every plugin that wants
them; the script's observers run separately, as before. Each command has
exactly one owner: the configuration script or one persistent plugin. A name
registered twice, by two plugins or by a plugin and the script, is a load
error. `message` and `timer` go only to the plugin that made the subscription
or timer; `event` goes to every plugin that registered it. `dialog` goes only
to whoever showed the dialog (the script or one plugin) and needs no entry in
`registration.hooks`: the WIT `hook` enum has no case for it.

## Lifecycle, failures, restarts and reloads

Each persistent plugin runs on its own OS thread, one call at a time; engines
need no locking and the guest is never called concurrently. At startup Rift
starts plugins in order once the control service answers and before any
listener accepts players, and waits for every `init`: **a plugin that fails to
start fails startup**.

**Failures.** A handler error (a Lua error, a returned error) fails that call;
the plugin keeps running. A trap, a panic, running out of memory, an error
outside a handler, or (for WebAssembly) guest code interrupted at its deadline
marks the instance **failed**. `failure.restart` then decides:

| `restart` | After a runtime failure | After `init` fails on a reload |
| --- | --- | --- |
| `never` | stays failed | stays failed |
| `on-failure` | restarts | stays failed until the next configuration change |
| `always` | restarts | keeps retrying |

Restarts back off from 1 s, doubling to 60 s; an instance that ran for 60 s
before failing starts over at 1 s. An invalid registration or a command that
collides with another owner is never retried. While a plugin is failed or
restarting, its awaited calls apply `on_failure`, but only for hooks its last
registration listed: a failed plugin that registered `http` and `timer` does
not deny logins.

**Claims.** A plugin's registration is its claim on hooks and commands. A
plugin that is new (at startup or added by a reload) and still in `init` has
claimed nothing, so the host does not ask it yet; startup waits for every
`init` anyway. A restarted or replaced plugin keeps its predecessor's
registration until its own `init` returns: calls to the hooks it claimed queue
behind that `init` and apply `on_timeout` if it is slow, and every other hook
is unaffected.

**Reloads.** A configuration reload diffs the plugin set by name:

| Change | What happens |
| --- | --- |
| Nothing (same files, manifest and configuration table) | Untouched; state and commands survive. |
| Only the configuration table, `hot_reload = true`, running | `reload(config)` is called within `deadlines.reload`; on error the plugin keeps its previous configuration. |
| Only the configuration table, otherwise | The instance is replaced. |
| The manifest or any file in the folder except `data/` and hidden (dot) files | The instance is replaced. The folder may not contain symbolic links, hidden ones included. |
| Removed from the configuration | Stopped. |
| New | Started. |

A replacement starts only after its predecessor's thread exited, so two
generations of one plugin do not run at once; this also holds for a plugin
added again while its removed generation is still stopping. A predecessor
that is still running 1 s after its shutdown grace period ended (stuck in code
its engine cannot interrupt) is abandoned with a log line, and the new
instance starts anyway. Plugins initialize concurrently but claim commands in
`rift.plugin()` order; a plugin waits for the one before it to claim, at most
until that one's predecessor wait and `init` deadline (plus 30 s for creating
its engine) have passed. A reload that gives the configuration script a
command registered by a plugin that stays configured, whether unchanged,
hot-reloaded or replaced, is refused and leaves the running set as it is.
Sessions that started under an older configuration see the current plugin
set: a command is looked up when the player sends it, so commands a plugin
registers after the session began (added by a reload, or still starting when
the player joined) reach the plugin instead of the backend. The player's
command tree shows them with the backend's next command list.

**Stopping.** `shutdown(reason)` runs within `deadlines.shutdown`; a plugin
that does not return in time is told to stop (engines interrupt it where they
can) and its thread is abandoned. It receives no further events either way.

**Operator restarts.** `rift admin plugins restart <name>`, the dashboard's
Restart button, `POST /api/plugins/<name>/restart` and `rift.control.plugins`
stop a plugin and start a fresh instance with the same files and its
configuration table as currently configured (also while a hot reload to it is
still running), in the same position. Calls in the meantime to hooks the plugin
registered queue behind the new `init` or time out under the manifest; other
hooks are unaffected.

## Host API

Every engine exposes the same host API; the [Lua guide](plugins-lua.md#host-api)
and the SDK guides map it to each language.

| Interface | Functions | Notes |
| --- | --- | --- |
| `players` | `list(server?)`, `get(target)`, `names(prefix?)`, `message`, `action-bar`, `title`, `tab-list`, `boss-bar`, `dialog`, `clear-dialog`, `kick`, `transfer`, `broadcast`, `message-json`, `action-bar-json`, `kick-json`, `broadcast-json`, `title-json` | `target` is a name, a UUID or a connection id. Actions return whether the player was online. Transfers pass `before_transfer`. `dialog` and `clear-dialog` (since 0.1.1) show and close [dialogs](dialogs.md). The `-json` functions (since 0.1.2) take a JSON text component for colours, hover text and click events ([rich text](extensions.md#rich-text)); Lua takes a chat component table in place of the string. `title-json` (since 0.1.3) takes a `title-json-spec`: title and optional subtitle as JSON text components (an empty title shows only the subtitle) and times in ticks. Components may contain the authoring objects `{"mini": "<red>Hi <player>", "args": {...}}` and `{"legacy": "&cHi", "marker": "&"}` ([text](text.md)); a bare string is never read as markup. Every SDK has builders that write them. |
| `store` | `get`, `set`, `delete`, `increment`, `list(namespace, prefix)` | The durable store shared with API v2. Namespaces are private: `plugin.<name>.<namespace>`, which the configuration script's `rift.store` cannot read or write. |
| `permissions` | `has`, `set`, `add-group`, `remove-group`, `groups`, `list-groups` | Rift's [permission store](permissions.md#persistent-plugins): user nodes and group memberships, checked in the player's current contexts. |
| `bus` | `publish`, `request(subject, payload, timeout_ms)`, `subscribe(subject, queue_group?)`, `unsubscribe`, `stream-publish` | Messages arrive through the `message` hook. At most 256 subscriptions; requests wait at most 60 s. |
| `servers` | `create`, `remove`, `start`, `stop`, `download`, `control(operation, payload_json)` | The same `rift.control.*` operations as the bus, awaiting the reply (at most 60 s). |
| `timers` | `every(ms)`, `after(ms)`, `cancel(handle)` | Ticks arrive through the `timer` hook. At least 10 ms; at most 1024 timers. |
| `log` | `log(level, message)` | Rift's log, prefixed with the plugin name, and the plugin's [log tail](#operator-surface). Control characters are replaced. |
| `meta` | `name`, `version`, `data-dir`, `config-dir`, `clock`, `time` | `data-dir` and `config-dir` are guest paths (`/data`, `/host` for WebAssembly). |

Calls that wait (bus requests, server operations, store writes) block only the
plugin's own thread. Persistent plugins get no per-call budgets beyond their
deadlines.

## Trust model

Persistent plugins are trusted code, like a Velocity or BungeeCord plugin jar.
Install only plugins you would run as Rift's user.

- **Persistent plugins run with Rift's process privileges.** A Lua plugin can
  open any file Rift can, run programs through `io.popen`, and use the network.
  A Lua plugin with `lua.ffi = true` or `lua.native_modules = true` runs native
  code in Rift's address space and can crash or corrupt the proxy.
- **WebAssembly isolates memory, not privilege.** A WebAssembly plugin cannot
  touch Rift's memory, and a trap ends only its own instance. This holds
  whatever it is granted, because no grant lets it change code Rift runs:
  `host` is read-only, absolute grants may not cover the compiled component
  cache or `plugins/`, and Rift loads a compiled component only when it
  carries an HMAC under a per-install secret (`local/plugincache/key`, mode
  0600, created on first use). But a plugin can do everything its
  capabilities allow: with full WASI (`filesystem = ["host"]`,
  `network = true`, the whole environment) it can read `rift.sqlite3` and the
  configuration with its secrets, fill the disk through `data`, read every
  secret in the environment, or open ports. Every plugin can also use the
  whole host API: kick players, change permissions, publish on the bus and
  start servers. Lua plugins, being trusted, can write anywhere Rift can,
  the cache included.
- **Manifests restrict capabilities.** `filesystem = ["data"]`,
  `network = false` and `env = []` (or a list of names) confine a WebAssembly
  plugin to its own folder; Lua honours `env` and `network` for the Rift API
  only. The defaults are permissive: `data`, network on and the whole
  environment.
- **`rift check` prints the grants** for every plugin, including the Lua
  options and which grants Lua does not enforce, before anything runs.
- **Rift never downloads or auto-enables plugins.** A plugin runs only when its
  folder is on disk and the configuration names it.
- **Configuration code stays sandboxed**, as [above](#why-configuration-code-stays-sandboxed).
  A `config` token can enable or disable installed plugins and change their
  configuration tables, but cannot install code.

## Operator surface

**`rift check`** lists every plugin with its mode and grants:

```text
plugin (configuration script): mode script, sandboxed; hooks login, join
plugin hello-lua 0.1.0: mode persistent, engine lua, trusted; restart on-failure; capabilities filesystem=data(advisory) network=no(rift.http only) env=all(os.getenv only) ffi=false native_modules=false memory_limit_mb=0; Lua io and os are not sandboxed
plugin (bus responders): mode remote; hooks login
```

**`rift admin plugins`** (the [local administration](operations.md#enable-operational-administration) service):

```sh
rift admin plugins list            # name, mode, engine, status, restarts, calls, last error
rift admin plugins status hello-lua
rift admin plugins logs hello-lua  # the last 200 log lines
rift admin plugins restart hello-lua
```

`list` and `status` need the `status` admin permission, `logs` needs `logs`
and `restart` needs `plugins`.

**HTTP API and dashboard.** The [dashboard](http.md)'s Plugins page shows
every plugin's state, restarts, calls and last error, a live log tail, and a
Restart button.

| Method and path | Web permission |
| --- | --- |
| `GET /api/plugins` | `read` |
| `GET /api/plugins/{name}` | `read` |
| `GET /api/plugins/{name}/logs` | `logs` |
| `POST /api/plugins/{name}/restart` with `{}` (returns `202`) | `plugins` |

All four require unrestricted operators: plugins act on the whole proxy, so
group-scoped operators get `403`, and `plugins` cannot be granted to a scoped
operator.

Each entry has `name`, `mode` (`script`, `persistent`, `remote`), `engine`,
`version`, `status` (`starting`, `running`, `failed`, `stopped`; `active` for
the script and the bus responders, listed as `(script)` and `(remote)`),
`restarts` (automatic and operator restarts since Rift started), `calls`
(awaited hooks plus queued observer events, timer ticks and bus messages),
`last_error`, `hooks`, `commands`, `capabilities` and `restartable`.

**Log tail.** Rift keeps the last 200 lines per persistent plugin in memory:
its `log` calls (and `print` in Lua), captured standard output and error of
WebAssembly guests, lifecycle changes, and calls that timed out or failed with
the policy that applied. The same lines go to Rift's standard error.

**`rift.control.plugins`** on the bus takes `{"action":"list"}`,
`{"action":"status","name":"…"}`, `{"action":"logs","name":"…"}` or
`{"action":"restart","name":"…"}` and answers like the other
[control subjects](messaging.md#rift-control-subjects). It is authorized like
them: grant `publish` on `rift.control.plugins` only to principals with
`control = true` that may restart plugins.

**`rift.events.plugins`** reports lifecycle changes as
`{"event":"started"|"failed"|"restarting"|"stopped","plugin":"…","version":"…","restarts":n,"error":"…"}`
(`error` only for `failed`), and `{"event":"restart_requested","plugin":"…","version":"…","actor":"…"}`
for an operator restart. Plugins receive these through their `event` hook too.

**Audit.** Plugin starts, stops, automatic restarts, failures and hot reloads
appear in the [operator audit log](http.md#authentication-and-access) as
`plugin started`, `plugin stopped`, `plugin restarting`, `plugin failed`,
`plugin reloaded` and `plugin reload_failed` by `system`, and operator
restarts as `plugin restart_requested` by the operator (`local-admin` for the
admin socket, `rift.control` for the bus, a name no web operator can have). Records carry the plugin name, never
error text. The HTTP and admin requests themselves are audited as usual.

**Metrics** on the status service's `/metrics`:

| Metric | Labels | Meaning |
| --- | --- | --- |
| `rift_plugin_calls_total` | `plugin`, `hook`, `outcome` | Calls by outcome: `ok`, `error` and `timeout` for awaited hooks; `queued` and `dropped` for observers, timer ticks and bus messages |
| `rift_plugin_call_seconds` | `plugin`, `hook` | Histogram of awaited hook latency as the caller saw it, including timeouts |
| `rift_plugin_restarts_total` | `plugin` | Automatic and operator restarts |
| `rift_plugin_up` | `plugin` | 1 while the plugin is running |

Labels are bounded: `plugin` takes only configured names (series of a removed
plugin disappear at the next reload), and `hook` and `outcome` are fixed sets.

## WebAssembly plugins

A WebAssembly plugin is a component built for `wasm32-wasip2` against world
`plugin`. Rift runs it on Wasmtime, one instance per plugin on the plugin's
thread, and links Rift's interfaces plus WASI 0.2, with `[capabilities]`
deciding which directories, network access and environment variables the
instance sees. The guides for [Rust](plugins-rust.md), [C](plugins-c.md) and
[Go](plugins-go.md) cover building one; the engine's limits (`[wasm]`),
deadline interrupts, captured output and compilation cache are described in
[the WebAssembly examples](../examples/plugins/README.md).

## Contract rules

The WIT keeps guests in C and Go pleasant: WASI 0.2 without `async`, `stream`
or `future`; no `resource` types (host-managed things are `u64` handles); plain
records, variants and lists; configuration and free-form payloads as JSON
strings; every hook an export with a default in each SDK. Guests build
against world `plugin`, which includes the WASI CLI imports; world `core` is the
same without WASI.

Additions keep compiled components loading. Wasmtime matches interface
versions on the `0.1` track, so a component built against `0.1.0` links
against this `0.1.3` host and the other way round. An addition may bring new
host functions, types and interfaces, but never changes an existing type or
adds a case to an existing enum or variant, since exports are checked exactly.
A new export lives in its own interface, which the host looks up when it
loads a component and treats as optional: world `base` holds what every
component has, worlds `optional`, `optional-decisions` and `optional-styled`
what later releases added (`dialogs` in 0.1.1, `decisions` in 0.1.2, `styled`
in 0.1.3). The rich text host functions came in 0.1.2 and `players.title-json`
in 0.1.3. A host function a component calls needs a host that has it; a
component that only exports something new still loads on older 0.1 hosts.
Breaking changes move to 1.0.
