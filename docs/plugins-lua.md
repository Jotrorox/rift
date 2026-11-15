# Persistent Lua plugins

A plugin folder with a `plugin.toml` naming `engine = "lua"` is a
[persistent plugin](plugins.md): `rift.plugin("hello-lua", { deny = "Mallory" })`
records it instead of running it in the configuration sandbox, and Rift runs
its entry script in its own long-lived LuaJIT state on the plugin's own thread.
Nothing needs building; the folder is the plugin.

Use a persistent Lua plugin when a handler needs state that survives between
calls, a library the sandbox does not offer (`io`, `os`, `coroutine`, `pcall`,
Lua patterns), more than the sandbox's 50 ms and 100,000 instructions, or
blocking calls such as `rift.request`. Keep simple policy in `rift.lua`: it is
sandboxed, which a persistent plugin is not (see the
[trust model](plugins.md#trust-model)).

## A minimal plugin

```text
my-network/
├── rift.lua
└── plugins/
    └── hello-lua/
        ├── plugin.toml
        └── init.lua
```

```toml
# plugins/hello-lua/plugin.toml
name = "hello-lua"
version = "0.1.0"
api = "0.2"
engine = "lua"

[capabilities]
network = false
env = []
```

```lua
-- plugins/hello-lua/init.lua
local name, config = ...

rift.on("login", function(ctx)
    if ctx.name == config.deny then
        return { deny = "You are not welcome here." }
    end
end)

rift.command("hello", {
    permission = "hello.use",
    suggest = "players",
    run = function(ctx)
        return { message = "Hello, " .. ctx.name .. "!" }
    end,
})

rift.every("tick", 1000, function()
    rift.log("debug", "tick")
end)
```

```lua
-- rift.lua
rift.plugin("hello-lua", { deny = "Mallory" })
```

`rift check` lists the plugin with its capabilities, marking the ones Lua does
not enforce (`filesystem` is advisory, `network` gates only `rift.http`, `env`
only `os.getenv`); start or reload
Rift to run it. [`examples/plugins/hello-lua`](../examples/plugins/hello-lua)
is the complete example, behaving exactly like the Rust, C and Go examples.

## Same API, different environment

The `rift` API and the `ctx` tables are the ones described in
[Lua configuration](lua.md) and [Plugins: events, commands and policy](extensions.md),
and results are validated by the same rules, so a handler body moves between
`rift.lua` and a plugin folder unchanged. The entry script (`entry`, default
`init.lua`) receives the plugin name and its configuration table as `...` and
registers with `rift.on`, `rift.command`, `rift.every` and `rift.subscribe`.
What differs is the environment:

- The whole safe standard library is available, including `io`, `os`,
  `package`, `coroutine`, `pcall`, `xpcall`, `load` and the Lua pattern
  functions. `ffi` requires `lua.ffi = true`. `os.exit` raises an error, and
  with `capabilities.env` set `os.getenv` sees only the listed variables.
  Filesystem grants are not enforced for Lua `io`: persistent plugins are
  trusted.
- `require` finds `name.lua` and `name/init.lua` in the plugin folder and in
  its `lua/` subfolder; `require("rift")` and `require("rift.store")` (and the
  other `rift.*` tables) return the API. Native modules load only with
  `lua.native_modules = true`, from the folder, its `lua/` subfolder and
  LuaJIT's default `cpath`.
- Globals, modules and upvalues persist between calls. The JIT compiler is on
  unless `lua.jit = false` (see [below](#the-jit-and-deadlines)). There is no
  instruction budget and no per-call limit on host calls; memory is limited
  only by `lua.memory_limit_mb` (256 MiB unless the manifest says otherwise).
  On Linux aarch64 LuaJIT cannot take a memory limit: Rift logs one warning
  and Lua plugins, like configuration scripts, run without one.
- Each call runs under its `[deadlines]` entry (observers, timers and bus
  messages under 60 seconds). When a call overruns, the host applies
  `on_timeout`, and the plugin is interrupted 100 ms later: the next Rift
  function it calls, or the instruction hook in code that is not
  JIT-compiled, raises an error that `pcall`, `xpcall` and `rift.try` do not
  catch.
- A Lua error is a handler error (`on_failure` applies and the plugin keeps
  running). Running out of memory, or being interrupted because Rift is
  stopping the plugin, fails the plugin, which restarts per `failure.restart`.

## Registration

- `rift.on(event, fn)` accepts the events of `rift.lua` (`route`, `http`,
  `status`, `login`, `initial_server`, `before_transfer`, `join`,
  `after_transfer`, `disconnect`, `dialog`) plus `event` (Rift events from
  `rift.events.*`, as `{ subject, payload, payload_json }`), `timer` (timers
  started without a function), `message` (subscriptions without a function),
  `reload` (called with the new configuration table when `hot_reload = true`;
  without it a hot reload changes nothing until the next restart) and
  `shutdown` (called with the reason). Handlers run in registration order: for
  decision events the first non-nil result wins; observers all run and must
  return nil.
- `rift.on("chat", fn)` registers the plugin for
  [chat](plugins.md#chat): the handler receives every chat message and backend
  command as `ctx` with `ctx.kind` (`"message"` or `"command"`) and `ctx.text`
  (for a command also `ctx.command` and `ctx.args`), and returns nil or
  `{ deny = "reason" }`, which drops it. With signed chat, the first deny
  costs that player their chat signing.
- `rift.on("kick", fn)` registers the plugin for backend kicks in play: `ctx.server`
  is the backend that kicked the player and `ctx.reason` its text without
  formatting. Return `{ server = "lobby" }` to move the player there,
  `{ deny = "reason" }` to disconnect them with that text, or nil to leave it
  to [`network.on_kick`](operations.md#maintenance-and-transfers).
- `rift.command(name, { permission, run, suggest, aliases, console })` also
  accepts a list of strings as `suggest`. `permission` is required;
  `permission = ""` opens the command to every player. `aliases` is a list of
  up to 16 other names for the command; `ctx.command` is always the command's
  own name. `console = true` lets operators run it with
  `rift admin plugins run <command> [arguments]`: `ctx.console` is then true,
  `ctx.name` is `CONSOLE` and `ctx.uuid` is empty, and only a `message` result
  is accepted ([commands](plugins.md#commands)). A command name or alias
  belongs to exactly one owner across the configuration script and all
  plugins.
- `rift.on` and `rift.command` work only while the entry script runs.
  `rift.every(name, every_ms, fn)`, `rift.subscribe(subject, [queue,] [fn])`
  (returns a handle for `rift.unsubscribe`) and `rift.timers.every(ms, [fn])`,
  `rift.timers.after(ms, [fn])` and `rift.timers.cancel(handle)` also work in
  handlers, as long as the entry script created at least one timer (or
  subscription) or registered `rift.on("timer")` (or `"message"`): hooks are
  fixed once the entry script finishes. Timer functions receive the handle;
  `rift.every` functions receive `{ api_version = 3, job = name }`; messages
  are `{ subject, payload, reply, subscription, handle }`.

## Host API

The [host API](plugins.md#host-api) in Lua, and how it differs from script
mode:

| Contract | In a persistent Lua plugin |
| --- | --- |
| `players.*` | `rift.players.list`, `get`, `names`, `message`, `action_bar`, `title`, `tab_list`, `boss_bar`, `dialog`, `clear_dialog`, `kick`, `transfer`, `broadcast`, as in `rift.lua`. The player tables that `list` and `get` return have `uuid`, `name`, `server`, `connection_id`, `protocol`, `ip`, `properties` (a list of `{ name, value, signature }`; `textures` is the skin), `visible_to`, `locale` and `language`. `locale` is the client's language (`en_us`), or nil while Rift does not know it, which is always the case in `login` and, for clients before 1.20.2, in `join` ([timing](plugins.md#host-api)); `language` is what a plugin set for the player, else the locale. `rift.players.lookup(target)` returns what Rift remembers about a player, online or not (`{ uuid, name, first_seen, last_seen, last_ip, last_server, online, language }`, or nil), `rift.players.search(prefix, limit?)` a list of such tables (20 unless `limit` says otherwise, at most 100), and `rift.players.set_language(target, language_or_nil)` sets the language of Rift's own messages for a player ([directory and languages](plugins.md#the-player-directory-and-languages)); `ctx.language` carries it in every hook. `message`, `action_bar`, `kick` and `broadcast`, like `deny` and `message` results, take a chat component table in place of the string ([rich text](extensions.md#rich-text)). Clicks on the plugin's [dialogs](dialogs.md) reach its `rift.on("dialog", fn)` handlers. |
| `store.*` | `rift.store.*`. Namespaces are private (`plugin.<name>.<namespace>`). `delete` returns whether the entry existed, `increment`'s delta defaults to 1, and `rift.store.list(namespace, prefix)` returns `{ key, value }` entries sorted by key. |
| `permissions.*` | `rift.permissions.*`. `groups` returns effective groups, including inherited ones and `default`; `rift.permissions.list_groups()` returns every group of the [permission store](permissions.md#persistent-plugins). Lua also has `rift.permissions.check(uuid, node, contexts?)` (true, false or nil), `rift.permissions.meta(uuid, contexts?)` and `rift.permissions.set_meta(uuid, key, value_or_nil, { contexts?, expires_in?, expires_at? })`. `rift.permissions.add_group(uuid, group, { expires_in?, expires_at? })` makes the membership end after that many seconds or at that Unix time; adding again replaces the expiry. Player tables carry `visible_to`, the node a viewer needs to see a [hidden](permissions.md#visibility) player, or nil. |
| `bus.publish`, `bus.request` | `rift.publish(subject, payload, reply?)` returns `{ delivered = n }`; `rift.request(subject, payload, timeout_ms)` waits for a reply. |
| `bus.subscribe`, `bus.unsubscribe` | `rift.subscribe(subject, [queue,] [fn])` and `rift.unsubscribe(handle)`. |
| `servers.*` | `rift.servers.*` waits for the control service and returns its decoded reply; `rift.servers.control(operation, payload)` reaches any `rift.control.*` operation. |
| `timers.*` | `rift.timers.every`, `after`, `cancel`, and `rift.every` for named jobs. |
| `log.log` | `rift.log(level, message)` (`trace`, `debug`, `info`, `warn`, `error`); `print` logs at `info`. Lines reach the plugin's log tail in `rift admin plugins logs` and the dashboard, and Rift's log when [`RIFT_LOG_LEVEL`](operations.md#log-levels) includes their level: `debug` and `trace` show only at `debug`, which `rift dev` turns on. |
| `background.*` | `rift.background.defer()`, `resolve(ticket, decision)`, `run(payload, [fn])` and `is_worker()`: see [background work](#background-work). |
| `meta.*` | `rift.plugin_name` and `rift.meta` (`{ name, version, data_dir, config_dir, api }`, `api` being the contract version of the host, such as `"0.2.0"`); `rift.clock()` and `rift.time()`. |
| (Lua only) | `rift.http.request(url, options)` takes any `http`/`https` URL instead of an integration name; options are `method`, `body`, `headers` and `timeout_ms` (default 10 s, at most 60 s). Requires `capabilities.network` (default true). |
| (script only) | `rift.setup`, `rift.plugin` and `rift.config` configure Rift and exist only in `rift.lua`. |

The `status` input lacks `peer_addr`, which the plugin contract does not carry.
Errors raised by Rift functions reach `pcall` and `rift.try` as message
strings.

## Background work

One Lua state runs one call at a time, so a slow lookup in `login` delays the
plugin's `chat` handler past its 50 ms. With `[background]` in the manifest,
the lookup runs in a worker and the hook answers when it is done
([how it works](plugins.md#background-work)):

```toml
# plugin.toml
[deadlines]
login = 3000          # the deferred answer still has to come in time

[background]
workers = 2           # each is its own Lua state running worker.lua
```

```lua
-- init.lua
rift.on("login", function(ctx)
    local ticket = rift.background.defer()  -- this hook's result is now ignored
    rift.background.run(ctx.uuid, function(result, err)
        if err then
            rift.log("warn", "ban lookup failed: " .. err)
            rift.background.resolve(ticket, nil)             -- continue
        elseif result ~= "" then
            rift.background.resolve(ticket, { deny = result })
        else
            rift.background.resolve(ticket, nil)
        end
    end)
end)
```

```lua
-- worker.lua: receives the plugin name and configuration like init.lua
local name, config = ...

rift.on("work", function(uuid)
    -- This may block: it runs in a worker, not in the plugin.
    local reply = rift.http.request(config.bans_url .. "/" .. uuid)
    return reply.status == 200 and reply.body or ""
end)
```

- `rift.background.defer()` works once per call in `login`, `initial_server`,
  `before_transfer`, `chat`, `kick` and a command's `run`, and returns a
  ticket. `rift.background.resolve(ticket, decision)` takes what the hook
  would have returned (nil continues) and returns false when nobody waits any
  more, because the deadline passed.
- `rift.background.run(payload, fn)` queues a string for a worker and returns
  a handle. `fn(result, err)` runs in the plugin when the job is done, with
  one of the two nil. Without `fn`, `rift.on("job_done", fn)` handlers receive
  `(handle, result, err)`.
- The worker script handles `rift.on("work", fn)`: `fn(payload)` returns a
  string (nil is the empty string); a Lua error fails the job, and its
  message is the `err` of the callback. A job has 30 s unless
  `deadlines.work` says otherwise.
- A worker shares nothing with the plugin: no globals, no upvalues, only the
  strings that go in and out. It has the host API (`rift.store`,
  `rift.permissions`, `rift.http`, `rift.request`, `rift.log`), but no events,
  timers, subscriptions or commands, and `rift.background.is_worker()` is
  true there.

## The JIT and deadlines

LuaJIT compiles hot loops into machine-code traces, and compiled traces never
call the instruction hook Rift uses to interrupt a call that overran its
deadline. A loop that calls any Rift function (`rift.time()`, `rift.log`, …)
is still interrupted, because every Rift function checks the deadline. A
pure-Lua loop that never calls one cannot be interrupted while it is compiled:

```lua
rift.on("login", function(ctx)
    while true do end -- a bug: with the JIT on, this thread never returns
end)
```

Rift still answers the login: the call times out and `on_timeout` applies. But
the plugin's thread stays busy, every later call to the plugin times out, and
Rift abandons the thread when it stops the plugin.

Set `jit = false` to trade speed for that guarantee:

```toml
[lua]
jit = false
```

Rift then calls `jit.off()` before loading the entry script, flushes any
compiled code and makes `jit.on` raise an error, so everything is interpreted
and the instruction hook fires in every loop: the runaway call above is
interrupted shortly after its deadline, fails as a handler error, and the
plugin keeps serving. Interpreted LuaJIT is still fast, but numeric loops and
string-heavy code can run several times slower than compiled traces.

| `jit` | Speed | A pure-Lua infinite loop |
| --- | --- | --- |
| `true` (default) | compiled traces | times out; the plugin stays stuck until Rift stops it and abandons the thread |
| `false` | interpreted | interrupted after its deadline; the plugin keeps running |

Leave the JIT on for plugins you have tested and that do heavy computation;
turn it off for plugins whose code you do not fully control, or while
developing.

## Gotchas

- **The plugin is trusted.** `io.open`, `io.popen` and `os.remove` work on
  anything Rift's user can reach, whatever `capabilities.filesystem` says.
  `lua.ffi` and `lua.native_modules` run native code inside the proxy; a bug
  there crashes Rift.
- **Blocking calls block only the plugin.** `rift.request`, `rift.servers.*`,
  `rift.http.request` and store writes wait on the plugin's own thread; other
  calls to the same plugin queue behind them and may time out. Move slow work
  into a [background worker](#background-work).
- **One state, one thread.** Calls never overlap, so plain Lua tables are
  safe; coroutines work but never run in parallel.
- **Hooks are fixed after `init`.** Register every `rift.on` event and at least
  one timer or subscription in the entry script if handlers create more later.
- **Data lives in `data/`.** `rift.meta.data_dir` is `plugins/<name>/data/`; it
  is created before the plugin starts and excluded from the reload
  fingerprint, so writing there never restarts the plugin.
