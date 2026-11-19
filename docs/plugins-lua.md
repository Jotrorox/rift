# Lua plugins

A persistent Lua plugin is a plugin folder whose `plugin.toml` names
`engine = "lua"`. Rift runs its entry script in its own long-lived LuaJIT state
on the plugin's own thread, with the same `rift` API as `rift.lua`. Nothing
needs building; the folder is the [plugin](plugins.md).

Use one when a handler needs:

- state that survives between calls;
- a library the sandbox does not offer (`io`, `os`, `coroutine`, `pcall`, Lua
  patterns);
- more than the sandbox's 50 ms and 100,000 instructions;
- blocking calls such as `rift.request`.

Keep simple policy in `rift.lua`: it is sandboxed, which a persistent plugin is
not (see the [trust model](plugins.md#trust-model)).

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

`rift check` lists the plugin with its capabilities and marks the ones Lua
does not enforce: `filesystem` is advisory, `network` gates only `rift.http`,
`env` only `os.getenv`. Start or reload Rift to run it.
[`examples/plugins/hello-lua`](../examples/plugins/hello-lua) is the complete
example, behaving exactly like the Rust, C and Go examples.

## Differences from `rift.lua`

The `rift` API, the `ctx` tables and the rules for results are those of
[Lua configuration](lua.md) and [Hooks in `rift.lua`](extensions.md), so a
handler body moves between `rift.lua` and a plugin folder unchanged. What
differs is the environment:

| | `rift.lua` | Persistent Lua plugin |
| --- | --- | --- |
| Entry script | `rift.lua` | `entry`, default `init.lua`. It receives the plugin name and its configuration table as `...` |
| State | A fresh VM per callback | Globals, modules and upvalues persist between calls |
| Standard library | [Restricted](lua.md#the-lua-environment) | The whole safe standard library, including `io`, `os`, `package`, `coroutine`, `pcall`, `xpcall`, `load` and the Lua pattern functions. `ffi` requires `lua.ffi = true`. `os.exit` raises an error |
| Grants | No I/O | With `capabilities.env` set, `os.getenv` sees only the listed variables. Filesystem grants are not enforced for Lua `io` |
| `require` | Modules under `lua/` | `name.lua` and `name/init.lua` in the plugin folder and in its `lua/` subfolder. `require("rift")` and `require("rift.store")` (and the other `rift.*` tables) return the API |
| Native modules | Never | Only with `lua.native_modules = true`, from the folder, its `lua/` subfolder and LuaJIT's default `cpath` |
| JIT compiler | Off | On unless `lua.jit = false` ([the JIT and deadlines](#the-jit-and-deadlines)) |
| Budget | 100,000 instructions | No instruction budget and no per-call limit on host calls |
| Memory | 8 MiB | `lua.memory_limit_mb`, 256 MiB unless the manifest says otherwise |
| Deadline | 50 ms | The call's `[deadlines]` entry; observers, timers and bus messages get 60 seconds |

- **Overruns.** When a call overruns, the host applies `on_timeout` and
  interrupts the plugin 100 ms later. The next Rift function it calls, or the
  instruction hook in code that is not JIT-compiled, raises an error that
  `pcall`, `xpcall` and `rift.try` do not catch.
- **Errors.** A Lua error is a handler error: `on_failure` applies and the
  plugin keeps running. Running out of memory, or being interrupted because
  Rift is stopping the plugin, fails the plugin, which restarts per
  `failure.restart`.
- **Linux aarch64.** LuaJIT cannot take a memory limit there. Rift logs one
  warning, and Lua plugins, like configuration scripts, run without one.

## Registration

`rift.on(event, fn)` and `rift.command` work only while the entry script runs.
Handlers run in registration order. For decision events the first non-nil
result wins; observers all run and must return nil.

| Event | Handler receives |
| --- | --- |
| `route`, `http`, `status`, `login`, `initial_server`, `before_transfer`, `join`, `after_transfer`, `disconnect`, `dialog` | The same input as in `rift.lua`, with the same results |
| `chat` | Every [chat](plugin-events.md#chat) message and backend command: `ctx.kind` (`"message"` or `"command"`) and `ctx.text`; for a command also `ctx.command` and `ctx.args`. Return nil, or `{ deny = "reason" }` to drop it. With signed chat, the first deny costs that player their chat signing |
| `kick` | A backend kick in play: `ctx.server` is the backend that kicked the player, `ctx.reason` its text without formatting. Return `{ server = "lobby" }` to move the player there, `{ deny = "reason" }` to disconnect them with that text, or nil to leave it to [`network.on_kick`](operations.md#run-a-maintenance-window) |
| `event` | Rift events from `rift.events.*`, as `{ subject, payload, payload_json }` |
| `timer` | Ticks of timers started without a function |
| `message` | Messages of subscriptions made without a function |
| `reload` | The new configuration table, when `hot_reload = true`. Without a handler, a hot reload changes nothing until the next restart |
| `shutdown` | The reason |

**Commands.**
`rift.command(name, { permission, run, suggest, aliases, console })` takes the
definition of [`rift.lua`](extensions.md#permissions-and-commands), with these
differences:

| Field | In a persistent plugin |
| --- | --- |
| `permission` | Required. `permission = ""` opens the command to every player |
| `suggest` | Also accepts a list of strings |
| `aliases` | Up to 16 other names for the command. `ctx.command` is always the command's own name |
| `console` | `console = true` lets operators run it with `rift admin plugins run <command> [arguments]`. `ctx.console` is then true, `ctx.name` is `CONSOLE` and `ctx.uuid` is empty, and only a `message` result is accepted |

A command name or alias has exactly one owner across the script and all
plugins ([commands](plugin-events.md#commands)).

**Timers and subscriptions.**

| Function | Its `fn` receives |
| --- | --- |
| `rift.every(name, every_ms, fn)` | `{ api_version = 3, job = name }` |
| `rift.timers.every(ms, [fn])`, `rift.timers.after(ms, [fn])` | The timer's handle, which `rift.timers.cancel(handle)` takes |
| `rift.subscribe(subject, [queue or { queue = ..., scope = ... },] [fn])` | `{ subject, payload, reply, subscription, handle }`. The call returns a handle for `rift.unsubscribe`. `scope` is `"cluster"` (the default: messages published on any peer of a [cluster](clustering.md#subject-scopes)) or `"local"` (only this peer's); a single Rift treats both alike |

These also work in handlers. But hooks are fixed once the entry script
finishes, so it must create at least one timer (or subscription) or register
`rift.on("timer")` (or `"message"`) if handlers create more later.

## Host API

The [host API](plugins.md#host-api) in Lua, and how it differs from script
mode:

| Contract | In a persistent Lua plugin |
| --- | --- |
| `players.*` | `rift.players.*` as in `rift.lua`: the same [functions](players-api.md#functions), [player tables](players-api.md#player-tables) and [rich text](players-api.md#rich-text), including `lookup`, `search` and `set_language` for the [player directory](plugin-events.md#the-player-directory-and-languages). Clicks on the plugin's [dialogs](dialogs.md) reach its `rift.on("dialog", fn)` handlers. |
| `store.*` | `rift.store.*`. Namespaces are private (`plugin.<name>.<namespace>`). `delete` returns whether the entry existed, `increment`'s delta defaults to 1, and `rift.store.list(namespace, prefix)` returns `{ key, value }` entries sorted by key. |
| `permissions.*` | `rift.permissions.*`. `groups` returns effective groups, including inherited ones and `default`; `rift.permissions.list_groups()` returns every group of the [permission store](permissions-lua.md#persistent-plugins). Lua also has `rift.permissions.check(uuid, node, contexts?)` (true, false or nil), `rift.permissions.meta(uuid, contexts?)` and `rift.permissions.set_meta(uuid, key, value_or_nil, { contexts?, expires_in?, expires_at? })`. `rift.permissions.add_group(uuid, group, { expires_in?, expires_at? })` makes the membership end after that many seconds or at that Unix time; adding again replaces the expiry. |
| `bus.publish`, `bus.request`, `bus.subscribe`, `bus.unsubscribe` | `rift.publish(subject, payload, reply?)` returns `{ delivered = n }`; `rift.request(subject, payload, timeout_ms)` waits for a reply; `rift.subscribe` and `rift.unsubscribe(handle)`. |
| `servers.*` | `rift.servers.*` waits for the control service and returns its decoded reply; `rift.servers.control(operation, payload)` reaches any `rift.control.*` operation. |
| `timers.*` | `rift.timers.every`, `after`, `cancel`, and `rift.every` for named jobs. |
| `log.log` | `rift.log(level, message)` (`trace`, `debug`, `info`, `warn`, `error`); `print` logs at `info`. [Logs](plugin-operations.md#logs) says where the lines go. |
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
([how it works](plugin-events.md#background-work)):

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

| Function | Behavior |
| --- | --- |
| `rift.background.defer()` | Returns a ticket. Works once per call in `login`, `initial_server`, `before_transfer`, `chat`, `kick` and a command's `run` |
| `rift.background.resolve(ticket, decision)` | Takes what the hook would have returned (nil continues). Returns false when nobody waits any more, because the deadline passed |
| `rift.background.run(payload, fn)` | Queues a string for a worker and returns a handle. `fn(result, err)` runs in the plugin when the job is done, with one of the two nil. Without `fn`, `rift.on("job_done", fn)` handlers receive `(handle, result, err)` |
| `rift.on("work", fn)`, in the worker script | `fn(payload)` returns a string (nil is the empty string). A Lua error fails the job, and its message is the `err` of the callback. A job has 30 s unless `deadlines.work` says otherwise |

A worker shares nothing with the plugin: no globals, no upvalues, only the
strings that go in and out. It has the host API (`rift.store`,
`rift.permissions`, `rift.http`, `rift.request`, `rift.log`), but no events,
timers, subscriptions or commands. `rift.background.is_worker()` is true
there.

## The JIT and deadlines

Rift interrupts a call that overran its deadline through an instruction
hook, and every Rift function (`rift.time()`, `rift.log`, …) checks the
deadline too. But LuaJIT compiles hot loops into machine-code traces that
never call the hook. So a pure-Lua loop that calls no Rift function cannot be
interrupted while it is compiled:

```lua
rift.on("login", function(ctx)
    while true do end -- a bug: with the JIT on, this thread never returns
end)
```

| `jit` | Speed | A pure-Lua infinite loop |
| --- | --- | --- |
| `true` (default) | compiled traces | times out; the plugin stays stuck until Rift stops it and abandons the thread |
| `false` | interpreted | interrupted after its deadline; the plugin keeps running |

With the JIT on, Rift still answers the login (`on_timeout` applies), but the
plugin's thread stays busy and every later call to the plugin times out.

```toml
[lua]
jit = false
```

With `jit = false`, Rift calls `jit.off()` before loading the entry script,
flushes any compiled code and makes `jit.on` raise an error. The runaway call
then fails as a handler error shortly after its deadline. Numeric loops and
string-heavy code can run several times slower than compiled traces.

Leave the JIT on for tested plugins that do heavy computation. Turn it off
for code you do not fully control, or while developing.

## Gotchas

- **The plugin is trusted.** `io.open`, `io.popen` and `os.remove` work on
  anything Rift's user can reach, whatever `capabilities.filesystem` says.
  `lua.ffi` and `lua.native_modules` run native code inside the proxy; a bug
  there crashes Rift.
- **Blocking calls block the plugin.** `rift.request`, `rift.servers.*`,
  `rift.http.request` and store writes wait on the plugin's own thread; other
  calls to the same plugin queue behind them and may time out. Move slow work
  into a [background worker](#background-work).
- **One state, one thread.** Calls never overlap, so plain Lua tables are
  safe; coroutines work but never run in parallel.
- **Data lives in `data/`.** `rift.meta.data_dir` is `plugins/<name>/data/`; it
  is created before the plugin starts and excluded from the reload
  fingerprint, so writing there never restarts the plugin.
