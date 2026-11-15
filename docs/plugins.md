# Plugins

Rift has one plugin system. A plugin is anything that receives Rift's events
and calls Rift's host API, and every plugin uses the same contract,
[`wit/rift-plugin.wit`](../wit/rift-plugin.wit) (`rift:plugin@0.2.0`): one
event schema, one set of answers, one host API. What differs is where the code
runs:

| Mode | What it is | Trust | Configured with |
| --- | --- | --- | --- |
| **script** | `rift.lua`, its modules and script-mode plugin folders, in the sandboxed configuration VM | Sandboxed: instruction and memory budgets, no I/O | `rift.on`, `rift.command`, `rift.config.plugins`, `rift.plugin` for a folder without a persistent manifest |
| **persistent** | A plugin folder installed on disk with a `plugin.toml`, running in its own long-lived Lua state or WebAssembly instance on its own OS thread | Trusted: runs with Rift's process privileges | `rift.plugin(name, config)` |
| **remote** | A service on the [message bus](messaging.md) that answers `login`, `initial_server`, `before_transfer` and `kick`, and calls the `players` interface as [`rift.players.*`](messaging.md#player-api-subjects) | Whatever the service is; Rift only reads its replies and checks its login's grants | `rift.config.plugins.decisions`, `messaging.principals` |

Persistent plugins come in two engines: [Lua](plugins-lua.md) (LuaJIT, the
same `rift` API as `rift.lua`) and WebAssembly components written in
[Rust](plugins-rust.md), [C](plugins-c.md), [Go](plugins-go.md) or any language
that targets WASI 0.2. The [examples](../examples/plugins) implement one
plugin, `hello`, in all four languages with identical behaviour.

The per-event callbacks, commands, permissions, queues and the host services
that the configuration script uses are described in
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
api = "0.2"                 # the rift:plugin track this Rift implements
min_api = "0.2.0"           # optional: the lowest 0.2.x the plugin runs on
mode = "persistent"         # or "script"; default "persistent"
engine = "wasm"             # "lua" or "wasm"; required for persistent plugins
entry = "relaxoban.wasm"    # default "init.lua" for Lua; required for WebAssembly
hot_reload = false          # call reload(config) instead of restarting on a config change

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
memory_limit_mb = 0         # 0..=65536, 0 is unlimited
jit = true                  # false: interpret everything so deadlines interrupt every loop
```

| Key | Default | Notes |
| --- | --- | --- |
| `name`, `version`, `api` | required | `api` must be `"0.2"`. A manifest that says `"0.1"` is refused with a pointer to [Moving from 0.1](#moving-from-01). |
| `min_api` | none | An exact version of the track, such as `"0.2.1"`, for a plugin that calls something a later 0.2 release added. Rift refuses to load the plugin when it implements an older one, instead of failing at the first call. `meta.api` returns the version of the running host. |
| `mode` | `"persistent"` | `"script"` manifests may contain only `name`, `version` and `api`; the folder's `init.lua` runs in the sandbox. |
| `engine` | required | `"lua"` or `"wasm"`. An engine missing from this build is a warning in `rift check`, an error in `rift check --strict`, and refused at startup and reload. |
| `entry` | `init.lua` (Lua) | A relative path of at most 256 bytes inside the folder, without `..`, not inside `data/` and without hidden (dot) components. Required for WebAssembly. |
| `hot_reload` | `false` | See [lifecycle](#lifecycle-failures-restarts-and-reloads). |
| `[deadlines]` | per hook | Keys are `init`, `reload`, `shutdown`, `work` and the awaited hooks; observers have no deadline and are rejected here. Awaited hooks allow 1–10 000 ms, `http` up to 60 000. |
| `[background]` | no workers | See [background work](#background-work). |
| `[failure]` | | `on_timeout` overrides the per-hook default for every hook. |
| `[capabilities]` | `filesystem = ["data"]`, `network = true`, all of `env` except [Rift's own secrets](operations.md#secrets-and-the-processes-rift-starts) | `data` is `plugins/<name>/data/` (created on start, read-write); `host` is the configuration directory, read-only; an absolute path is read-write and, for WebAssembly, may not contain or lie inside `local/plugincache` or `plugins/`. WebAssembly enforces all three. Lua enforces `env` (`os.getenv`) and `network` (`rift.http.request`) but not `filesystem`: Lua `io` sees what Rift sees. |
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
`registration.hooks` list delivers every hook except `chat` and `kick`**, which
a plugin always lists itself. `dialog` and `job_done` need no entry: they go
to the plugin that showed the dialog or started the job. The Lua engine
computes the list from `rift.on`, `rift.command`, timers and subscriptions.

| Hook | Kind | Answer | Default deadline | Default `on_timeout` |
| --- | --- | --- | --- | --- |
| `route` | awaited | keep the policy, pick a backend or group, or reject | 50 ms | deny (close the connection) |
| `login` | awaited | continue or deny | 1000 ms | deny |
| `initial_server` | awaited | continue, deny or a server | 1000 ms | deny |
| `before_transfer` | awaited | continue or deny | 300 ms | deny |
| `command` | awaited, owner only | message, server, queue, leave-queue or continue | 1000 ms | deny (the command reports an error) |
| `suggest` | awaited, owner only | completions | 50 ms | continue (no completions) |
| `status` | awaited | fields to replace in one server-list ping | 50 ms | continue |
| `http` | awaited | a response for `/ext` and `/ext/*`, or none | 5000 ms | continue |
| `join`, `after_transfer`, `disconnect` | observer | none | none (queued; interrupted after 60 s) | – |
| `message` | observer | a message on a subscription the plugin made | none (queued; interrupted after 60 s) | – |
| `timer` | observer | a tick of a timer the plugin started | none (queued; interrupted after 60 s) | – |
| `event` | observer | a `rift.events.*` publication | none (queued; interrupted after 60 s) | – |
| `dialog` | observer, owner only | a click on a plugin action of a [dialog](dialogs.md) the plugin showed | none (queued; interrupted after 60 s) | – |
| `chat` | awaited, only plugins that list it | continue or deny, for a chat message or a backend command ([below](#chat)) | 50 ms | deny (the message is dropped) |
| `kick` | awaited, only plugins that list it | continue, deny (disconnect with this text) or a server to move the player to, after a backend kick ([fallback on kick](operations.md#maintenance-and-transfers)) | 300 ms | deny (`network.on_kick` applies) |
| `job_done` | observer, owner only | the result of a [background job](#background-work) the plugin started | none (queued; interrupted after 60 s) | – |
| `init`, `reload`, `shutdown` | lifecycle | registration / ok / none | 30 s / 5 s / 10 s | init fails the plugin; reload keeps the old configuration |
| `work_init`, `work` | background workers only | ok / the job's result | 30 s / 30 s (`deadlines.work`) | the job fails; `job_done` reports why |

Player hooks (`login`, `initial_server`, `before_transfer`, `command`,
`suggest`, `join`, `after_transfer`, `disconnect`, `chat`, `kick`) need
`authentication.online_mode`, because they carry a verified identity. Without
it, persistent plugins still run `route`, `status`, `http`, timers, bus
messages and events, but a plugin whose registration lists a player hook,
registers a command or has an empty hook list fails to start (it is not
retried), so a ban list cannot silently allow everyone; `rift check` and `rift
check --strict` warn when persistent plugins are configured without it.
Turning `online_mode` or `signed_chat` on or off restarts every persistent
plugin.

**Text.** Wherever a plugin gives Rift text for a player (a deny reason, a
command's message, a suggestion tooltip, a title, the server-list MOTD,
version and sample lines), the contract has one type, `text`: `plain(string)`,
1–1024 bytes shown exactly as written, or `json(string)`, a JSON text
component of at most 32 KiB for colours, hover text and click events, the
`mini` and `legacy` [authoring objects](text.md) included. Lua writes a string
or a component table. Clients see the status version and sample lines as `§`
codes.

Answers are validated exactly like Lua results and bus replies: text within
those limits, servers must exist, and an answer a hook does not accept is a
failure of the plugin. One invalid entry fails the whole answer.

**Who is calling `http`.** Every web credential with the `extensions`
permission reaches `/ext`, an operator holding little else as much as the full
token; Rift does not ask for more, the handler decides. The request's context (`context-json` in
WebAssembly) is the status object the configuration script's
[`on_http`](http.md#lua-websites-and-endpoints) receives, including its
`operator` member: `{name, permissions, groups}` as `GET /api/access` reports
the caller, with `permissions` `null` (absent in Lua) for full access and
`groups` `null` when the caller is not limited to service groups. Check it
before a handler changes anything.

**Deny or continue.** When a plugin misses its deadline, `on_timeout` applies;
when it fails (a handler error, a trap, an invalid answer, or the plugin is
failed or restarting), `on_failure` applies. `deny` means the event fails the
way an error in `rift.lua` would: the login or transfer is refused, a route
closes the connection, a command reports an error. `continue` skips this plugin
and asks the next one. Rift never treats an error as permission to proceed
unless a manifest says so. Each plugin has a queue of 1024 undelivered calls;
when it is full, awaited calls count as timeouts at once and observer events
are dropped (a slow plugin stalls only itself).

### Chat

The `chat` hook sees what a player types into the chat box before the backend
does: chat messages, and the commands that would reach the backend. It answers
`continue`, and the backend receives the packet exactly as the client sent it,
in its place before everything the client sent afterwards; or `deny` with a
[text](#events), and Rift drops the packet and
shows the player the text as a system message. Commands that Rift or a plugin
owns (`/server`, `/hub`, `/perms`, registered commands) never come to the
hook. This is the hook for mutes and chat filters.

| Mode | Registering | Input |
| --- | --- | --- |
| Configuration script | `chat = function(ctx) ... end` in `rift.config.plugins` ([callbacks](extensions.md#callbacks-and-decisions)) | `ctx.kind` (`"message"` or `"command"`), `ctx.text`, and for a command `ctx.command` and `ctx.args` |
| Persistent Lua | `rift.on("chat", fn)` | the same `ctx` |
| WebAssembly | listing `chat` in `registration.hooks` | `chat-attempt`: `ctx`, `kind`, `text` |

`text` is the message, or the command line without its leading slash;
`ctx.command` is the command's root name and `ctx.args` the rest, as for
`command`. The chain is the configuration script, then persistent plugins in
`rift.plugin()` order; the first deny wins and there is no bus responder. Any
answer other than continue and deny is a failure of the plugin.

**Asking for chat is explicit.** Rift asks every plugin that handles chat about
every message of every player, so an empty hook list does not include it: a
WebAssembly component lists the `chat` hook, a Lua plugin calls
`rift.on("chat", fn)`. While no plugin and no script handles chat, sessions do
not even parse chat packets.

**Chat must be unsigned.** A backend that verifies a player's chat counts
every signed message; when the proxy withholds one, the backend rejects all
that follow and disconnects the player. So a `chat` handler needs
[`authentication.signed_chat = false`](network-protocol.md#unsigned-chat),
which keeps the signing material from backends:

- A plugin with a handler fails to start while `signed_chat` is `true`, with
  `the chat hook needs authentication.signed_chat = false` (it is not
  retried). A component says that it handles chat only in the registration
  its `init` returns, so `rift check` cannot tell beforehand. A configuration
  whose script defines `chat` does not load.
- Paper backends also need `enforce-secure-profile=false` in
  `server.properties`, or they refuse every chat message of players without a
  signing key.
- Players lose two things, on the whole network: their chat is shown to
  others as not secure (the client's unsigned-message marker), and chat
  reporting stops working.
- `signed_chat` applies to connections made after it changed. A player who
  connected while it was `true` keeps a signed chat session with the backend
  until they reconnect: for that player Rift asks the hook only about packets
  that carry no signature (commands without signed arguments, and all chat of
  clients before 1.19) and forwards signed ones without asking. The hook
  covers every message of players who connected after the setting became
  `false`.

The deadline is 50 ms (`deadlines.chat`, 1–10 000 ms). A missed deadline or a
failure applies `on_timeout` and `on_failure`; with `deny`, the default, the
message is dropped and the player sees the `chat.failed`
[message](text.md). Like every player hook, `chat` needs
`authentication.online_mode`. While a server switch is under way the old
backend's chat is dropped without asking the hook, as its commands are.

### Background work

A plugin runs one call at a time. A lookup that takes 200 ms inside `login`
keeps every other hook of that plugin waiting, and `chat` has 50 ms. Stretching
the deadlines does not help; moving the slow part off the plugin's thread does:

1. The hook calls `background.defer()`, which returns a ticket. Rift now
   ignores what the hook returns and waits for
   `background.resolve(ticket, decision)` instead.
2. The hook hands the slow part to a worker with `background.run(payload)`,
   which returns a handle, and returns. The plugin is free for the next call.
3. A **background worker** runs `work(payload)`: the database query, the HTTP
   request. It may block.
4. The plugin receives `job_done(handle, outcome)` with the worker's result or
   its error, and calls `resolve` with the decision.

```toml
[background]
workers = 2
```

- **Workers are separate instances.** Each of the `workers` (1–16) is another
  instance of the plugin on its own thread, with its own memory: a second
  instance of the component, or for Lua a state running `background.entry`
  (default `worker.lua`). A worker never gets `init` or any hook. It gets
  `work_init(config)` once before its first job, with the plugin's
  configuration, and then jobs. Payloads and results are bytes, at most 4 MiB.
  A worker may call the host API (`store`, `permissions`, `bus.request`, `log`
  and so on), but receives no timer ticks or bus messages and cannot `defer`,
  `resolve` or `run`.
- **`defer` works in the decision hooks**: `login`, `initial_server`,
  `before_transfer`, `command`, `chat` and `kick`, once per call. The deferred
  answer accepts what the hook accepts.
- **The deadline keeps running.** An answer that arrives after the hook's
  deadline is a missed deadline: `on_timeout` applies, and `resolve` returns
  false. So a deferred `login` still needs a deadline longer than its lookup,
  but that deadline no longer delays `chat`.
- **Failures.** A handler error in `work`, a job over `deadlines.work`
  (default 30 s) or a worker that traps reports an error through `job_done`;
  a trapped worker starts over at its next job. At most 1024 jobs wait at a
  time; `run` fails beyond that, and without a `[background]` table.
- Workers stop with the plugin. A restart or reload drops waiting jobs, and
  whoever waits for a deferred answer gets `on_failure`.

Each language has a shorter way to write this: see [Lua](plugins-lua.md#background-work),
[Rust](plugins-rust.md#background-work), [C](plugins-c.md#background-work) and
[Go](plugins-go.md#background-work).

### Testing player hooks without Mojang

Player hooks need `online_mode`, and `online_mode` asks Mojang's session server
about every login, so an automated test has no account to log in with. For
tests only, Rift can be built with the Cargo feature `test-session-server`:

```sh
cargo build --locked --features test-session-server
RIFT_TEST_SESSION_URL=http://127.0.0.1:8080/session/minecraft/hasJoined target/debug/rift
```

Such a build asks the server in `RIFT_TEST_SESSION_URL` instead of Mojang. The
test answers `GET <url>?username=<name>&serverId=<hash>` with status 200 and
the profile to log in (`{"id": "<32 hex digits>", "name": "<name>",
"properties": []}`), or with 204 to refuse the login.
[`tests/session_server.rs`](../tests/session_server.rs) has a login bot and
such a server.

- The feature is off by default. Release and nightly binaries are built
  without it and never read the variable; no configuration option exists.
- The URL must be `http://` or `https://` on a loopback IP address
  (`127.0.0.1`, `[::1]`; not `localhost`). Anything else stops Rift at startup.
- Rift logs a warning at startup whenever the variable is in effect. Without
  the variable, a build with the feature asks Mojang as usual.
- The bot must still perform the encryption handshake, as a premium client
  does. A real Minecraft client with an offline account (the headless harness
  in [`tests/real_client.py`](../tests/real_client.py)) still cannot join: it
  fails its own request to Mojang before Rift asks anyone.

## Dispatch order

For `route`, `login`, `initial_server`, `before_transfer`, `status` and
`http`, Rift asks in this order:

1. **script**: the configuration script's callback and `rift.on` handlers;
2. **persistent** plugins, in `rift.plugin()` order;
3. **remote**: the bus responder in `rift.config.plugins.decisions`, for the
   three decision hooks.

The first answer other than `continue` (or the default) wins and stops the
chain, so a deny anywhere is final. Observers go to every plugin that wants
them; the script's observers run separately, as before. `message` and `timer`
go only to the plugin that made the subscription or timer; `event` goes to
every plugin that registered it. `dialog` goes only to whoever showed the
dialog (the script or one plugin) and `job_done` to the plugin that started
the job. [`chat`](#chat) goes to the script, then to every plugin that handles
it, and stops at the first deny; `kick` likewise, stopping at the first answer.

## Commands

Each command has exactly one owner: the configuration script or one persistent
plugin. A name registered twice, by two plugins or by a plugin and the script,
is a load error. A command's `permission` is the node a player needs to see and
run it; an empty permission opens the command to every player, in every engine
and in the configuration script.

**Aliases.** A persistent plugin's command may list up to 16 `aliases`, each a
name like the command's own (1–64 bytes of `a-z0-9_-`; `server` and `hub` are
reserved). An alias is claimed like a name, appears in the player's command
tree and completes the same way. The hooks always see the command's own name
in `ctx.command`, whichever name the player typed.

**The console.** A command registered with `console` also runs for operators:

```sh
rift admin plugins run ban Mallory griefing
```

The `command` hook then sees `ctx.console` set, `ctx.name` `CONSOLE`, an empty
`ctx.uuid` and connection id 0, and no permission is checked: the caller
needed the `plugins` admin permission. The command's `message` answer is
printed (`continue` prints nothing); any other answer needs a player and is an
error. Commands without `console` are refused, so a command that assumes a
player never runs without one.

## The player directory and languages

Rift remembers every player who joined, in `rift.sqlite3` beside the
permission store:

| Function | Result |
| --- | --- |
| `players.lookup(target)` | The record of a UUID, or of the name a player last joined with (case-insensitive): `uuid`, `name`, `first-seen` and `last-seen` (Unix seconds), `last-ip`, `last-server`, `online`, `language`. `none` for a player Rift never saw. |
| `players.search(prefix, limit)` | Records whose name starts with `prefix`, most recently seen first, at most `limit` (1–100). |
| `players.set-language(target, language?)` | Sets the language Rift's own messages use for a player, online or not; `none` goes back to the client's locale. Returns whether the player is known. |

A player has a record from their first join on, so `login` of a first-time
player finds none. `players = { record_ip = false }` in the configuration keeps
addresses out of the directory: `last-ip` is then empty, and an address stored
earlier is cleared at the player's next join. The default is `true`.

**Languages.** A client reports its locale only after login, so the `login`
hook never has one, and a network's language switcher needs somewhere to keep
a player's choice. `set-language` stores a tag of 2–16 bytes of `a-z0-9_`
(`de`, `de_de`; `de-DE` is normalized) with the player's record. From then on:

- `ctx.language` carries it in every hook from `login` on, in later sessions
  too. Without a stored choice it is the client's locale once that is known.
- `player-info.language` is the same value; `player-info.locale` stays what
  the client reported.
- Rift's own messages to that player (queue notices, refusals, `/server`
  output) come from [`text.languages`](text.md#languages): the exact tag, then
  its language (`de` for `de_at`), then `text.messages`.

## Lifecycle, failures, restarts and reloads

Each persistent plugin runs on its own OS thread, one call at a time; engines
need no locking and the guest is never called concurrently. Its
[background workers](#background-work) are further instances on threads of
their own. At startup Rift
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
| `players` | `list(server?)`, `get(target)`, `names(prefix?)`, `message`, `action-bar`, `title`, `tab-list`, `boss-bar`, `dialog`, `clear-dialog`, `kick`, `transfer`, `broadcast`, `lookup`, `search`, `set-language` | `target` is a name, a UUID or a connection id. Actions return whether the player was online and take [`text`](#events). Transfers pass `before_transfer`. `dialog` and `clear-dialog` show and close [dialogs](dialogs.md). A `title-spec` has an optional title and subtitle (at least one) and times in ticks. `list` and `get` return a `player-info`: `connection-id`, `uuid`, `name`, `server`, `protocol`, `ip` (the address the player connected from), `properties` (the signed profile properties of the login: `textures` holds skin and cape), `visible-to` (the node a viewer needs to see a hidden player, see [visibility](permissions.md#visibility); `list` and `get` include hidden players), `locale` and `language`. `locale` is the language the client reported, as the client spells it (`en_us`; old clients send `en_US`), or `none`: the client sends its settings after login, during configuration from 1.20.2 and after Join Game before, so a missing value means "not known yet". `lookup`, `search` and `set-language` are [the player directory](#the-player-directory-and-languages). |
| `store` | `get`, `set`, `delete`, `increment`, `list(namespace, prefix)` | The durable store the configuration script uses too. Namespaces are private: `plugin.<name>.<namespace>`, which the configuration script's `rift.store` cannot read or write. Each plugin has its own quota, 4096 entries and 1 MiB unless [`extensions.storage.quota`](extensions.md#persistent-state) sets another. |
| `permissions` | `has`, `set`, `add-group(uuid, group, expires-at?)`, `remove-group`, `groups`, `list-groups`, `meta`, `set-meta` | Rift's [permission store](permissions.md#persistent-plugins): user nodes, group memberships (until `expires-at` Unix seconds when given, for a rank that runs out) and meta values with contexts and expiry, checked in the player's current contexts. |
| `bus` | `publish`, `request(subject, payload, timeout_ms)`, `subscribe(subject, queue_group?)`, `unsubscribe`, `stream-publish` | Messages arrive through the `message` hook. At most 256 subscriptions; requests wait at most 60 s. |
| `servers` | `create`, `remove`, `start`, `stop`, `download`, `control(operation, payload_json)` | The same `rift.control.*` operations as the bus, awaiting the reply (at most 60 s). |
| `timers` | `every(ms)`, `after(ms)`, `cancel(handle)` | Ticks arrive through the `timer` hook. At least 10 ms; at most 1024 timers. |
| `log` | `log(level, message)` | Rift's log, prefixed with the plugin name, and the plugin's [log tail](#operator-surface). Control characters are replaced. |
| `background` | `defer()`, `resolve(ticket, decision)`, `run(payload)`, `is-worker()` | [Background work](#background-work). |
| `meta` | `name`, `version`, `data-dir`, `config-dir`, `clock`, `time`, `api` | `data-dir` and `config-dir` are guest paths (`/data`, `/host` for WebAssembly). `api` is the contract version of the host, such as `0.2.0`. |

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
  secret in the environment (Rift's own token variables only when the
  manifest names them in `env`), or open ports. Every plugin can also use the
  whole host API: kick players, change permissions, publish on the bus and
  start servers. Lua plugins, being trusted, can write anywhere Rift can,
  the cache included.
- **Manifests restrict capabilities.** `filesystem = ["data"]`,
  `network = false` and `env = []` (or a list of names) confine a WebAssembly
  plugin to its own folder; Lua honours `env` and `network` for the Rift API
  only. The defaults are permissive: `data`, network on and the whole
  environment apart from Rift's own secrets. Lua's `io.popen` starts programs
  with Rift's full environment.
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
rift admin plugins run ban Mallory # a plugin command registered with console
```

`list` and `status` need the `status` admin permission, `logs` needs `logs`,
and `restart` and [`run`](#commands) need `plugins`.

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
admin socket; on the bus `bus:<login>`, `plugin:<name>` for another plugin
and `rift.control` for the configuration script, names no web operator can
have). Records carry the plugin name, never
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
thread (plus one per [background worker](#background-work)), and links Rift's interfaces plus WASI 0.2, with `[capabilities]`
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
versions on the `0.2` track, so a component built against `0.2.0` links
against every later `0.2` host. An addition may bring new host functions,
types and interfaces, but never changes an existing type or adds a case to an
existing enum or variant, since exports are checked exactly. A host function a
component calls needs a host that has it: such a plugin names the release in
[`min_api`](#plugintoml). Breaking changes move to the next minor version
until 1.0.

## Moving from 0.1

`rift:plugin@0.2.0` is 0.1.5 without its duplicates, plus background work, the
player directory, languages, command aliases and the console. Rift 0.2 hosts
load only `api = "0.2"` plugins; a 0.1 manifest is refused with a pointer
here, and a component built against 0.1 does not link.

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
and ignored, so existing configurations load unchanged
([extensions](extensions.md)).
