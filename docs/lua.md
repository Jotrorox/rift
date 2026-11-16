# Lua configuration

`rift.lua` is an ordinary Lua script that fills in `rift.config`. This page
covers how to split it into modules and plugin folders, how to register
callbacks and commands, and what the sandbox it runs in allows.

```lua
local config = require("rift.config") -- Also available as rift.config.

config.listeners.public = "0.0.0.0:25565"
config.backends.lobby = "127.0.0.1:25566"
config.routes.public = "lobby"
config.limits.max_connections = 1024

require("config.services")
rift.plugin("greeting")
```

## Write the configuration

`rift` is a global, like Neovim's `vim`; `local rift = require("rift")` returns
the same API. You can set fields in three ways and mix them:

| Way | Behavior |
| --- | --- |
| `rift.config.<field> = ...` | Edits the configuration in place. `listeners`, `backends`, `managed_servers`, `templates`, `template_sources`, `service_groups`, `nodes`, `routes` and `limits` start as empty tables. Assign any other section before editing its fields. Do not replace the `rift.config` table itself. |
| `rift.setup({ ... })` | Assigns top-level fields. It replaces each supplied field, without merging nested tables, so edit individual entries to extend a section. You can call it more than once. |
| `return { ... }` | Still supported, but no outer `return` is needed. Returned fields override fields assigned the other two ways; event and command registrations are then applied to the result. |

Bare globals such as `listeners = ...` are ordinary Lua variables, not
configuration fields. The schema, authentication, permission and reload checks
apply whichever way you choose.

A backend may run another Minecraft version than its players (1.12 to 26.3):
Rift asks it for its version and translates. A table such as
`{ address = "127.0.0.1:25567", version = "1.12.2" }` declares the version
instead, and `config.translation = false` turns translation off; see
[protocol translation](translation.md).

`rift.config.templates` and `rift.config.service_groups` describe servers that
Rift provisions and starts itself. They are separate from the Lua modules and
folder plugins on this page; see [templates](templates.md),
[service groups](service-groups.md) and the
[provisioning example](../examples/templates.lua).

## Folder layout and imports

[`examples/modular/`](../examples/modular/rift.lua) is a complete runnable example:

```text
my-network/
├── rift.lua
├── lua/
│   └── config/
│       ├── network.lua
│       └── services.lua
└── plugins/
    ├── greeting/
    │   ├── init.lua
    │   └── lua/
    │       └── greeting/
    │           └── init.lua
    └── echo/
        └── init.lua
```

Paths are relative to the configuration file's directory, not the process's
working directory. `require("config.network")` looks for
`lua/config/network.lua` and then `lua/config/network/init.lua`.

- A module can execute settings directly, return a function or return a table.
  Without a return value it yields `true`. It receives its import name as
  `...`.
- Successful imports are cached by name within each VM, as with Lua's
  `require`. A module that returns `false` is evaluated again on the next
  import.
- Names are dotted, with letters, digits, `_` or `-` in each component, up to
  128 bytes. Filesystem paths, `..`, absolute paths, native libraries and
  `package.path` are not supported.
- Import errors name the module and file. Circular imports fail with an
  explicit diagnostic.
- `rift`, `rift.config`, `rift.store`, `rift.http`, `rift.permissions` and
  `rift.players` are reserved modules.

## Folder-based plugins

`rift.plugin("greeting")` adds `plugins/greeting/lua/` to module lookup, then
executes `plugins/greeting/init.lua` inside this sandbox. The entry script
receives the plugin name as `...`, can use the same API as `rift.lua` and needs
no return value:

```lua
-- plugins/greeting/init.lua
local rift = require("rift")
local greeting = require("greeting")

rift.on("http", function(request)
    if request.path == "/ext/hello" then
        return { body = greeting.text() }
    end
end)
```

- **Names** use letters, digits, `_` or `-`, up to 128 bytes.
- **Order.** Plugins load in the order you call `rift.plugin`. A repeated call
  returns the cached result and does not register the plugin twice.
- **Result.** The call returns what the entry script returned, or `true`. So a
  plugin may return a module with its own `setup` function:

  ```lua
  rift.plugin("my-plugin").setup({ greeting = "Welcome!" })
  ```

- **Lookup** checks the configuration's `lua/` first, then each loaded
  plugin's `lua/` in import order; within each directory `name.lua` precedes
  `name/init.lua`. Give plugin modules their own namespace (for example
  `greeting.format`) to avoid collisions.
- **Install** a plugin by copying its folder into `plugins/`. Rift does not
  download or auto-enable plugins.

> [!NOTE]
> A folder whose `plugin.toml` names `engine = "lua"` is a
> [persistent Lua plugin](plugins-lua.md). `rift.plugin(name, config)` records
> it instead of running it here.

## Registering callbacks and commands

`rift.on(event, callback)` appends a handler for one of these events:

| Event | Same arguments and results as |
| --- | --- |
| `route`, `http`, `message` | `on_route`, `on_http`, `on_message` |
| `status` | [`on_status`](server-list.md#server-list) |
| `login`, `initial_server`, `join`, `disconnect`, `before_transfer`, `after_transfer`, `dialog`, `kick` | The [callbacks in `rift.lua`](extensions.md#callbacks-and-decisions) |

- Handlers run in registration order, after a directly configured callback.
- For every event except `message`, the first non-nil result ends the chain
  and Rift validates it. Return nil to let later handlers run; observer events
  such as `join` must always return nil.
- All `message` handlers run, and their return values are ignored.
- An error stops the chain and follows the event's failure policy. Handlers
  share one execution budget.

```lua
rift.on("route", function(connection)
    if connection.peer_ip == "192.0.2.10" then
        return { reject = true, reason = "Access denied" }
    end
end)

-- Requires online authentication, forwarding and a permission grant.
rift.config.extensions = {
    groups = { default = { permissions = { ["greeting.hello"] = true } } },
}
rift.command("hello", {
    permission = "greeting.hello",
    run = function(ctx) return { message = "Hello " .. ctx.name } end,
})
```

`rift.command(name, definition)` uses the command schema and permission checks
of [`extensions.commands`](extensions.md#permissions-and-commands), including
`permission = ""` for a command open to every player.

- Registering an authenticated event or a command creates `extensions` when it
  is absent.
- Duplicate names fail validation, including collisions with
  `extensions.commands`.
- `rift.on`, `rift.command` and `rift.setup` work only during initialization
  and reject calls from a runtime callback.
- `rift.publish(subject, payload, reply?)` and `rift.enabled` are for
  callbacks: imports and top-level initialization cannot publish messages.
  Captured references such as `local publish = rift.publish` work in
  callbacks.

## The Lua environment

Scripts run on LuaJIT (Lua 5.1 syntax) with the JIT compiler off. Only these
globals exist:

| Global | Contents |
| --- | --- |
| Base functions | `assert`, `error`, `ipairs`, `next`, `pairs`, `select`, `tonumber`, `tostring`, `type`, `unpack`, `_G`, `_VERSION` |
| `string` | `byte`, `char`, `format`, `len`, `lower`, `rep`, `reverse`, `sub`, `upper`; strings also have these as methods (`s:sub(1, 2)`) |
| `table` | `concat`, `insert`, `remove`, `maxn` |
| `math` | `abs`, `acos`, `asin`, `atan`, `atan2`, `ceil`, `cos`, `cosh`, `deg`, `exp`, `floor`, `fmod`, `frexp`, `huge`, `ldexp`, `log`, `log10`, `max`, `min`, `modf`, `pi`, `pow`, `rad`, `random`, `randomseed`, `sin`, `sinh`, `sqrt`, `tan`, `tanh` |
| `require` | Rift's [module loader](#folder-layout-and-imports) |
| `rift` | The API on this page and in [Hooks in `rift.lua`](extensions.md) |

Everything else in the standard library is removed on purpose:

| Removed | Names |
| --- | --- |
| Error handling, coroutines | `pcall`, `xpcall`, `coroutine` |
| Code loading | `load`, `loadstring`, `dofile`, `loadfile`, `package` |
| Metatables, raw access | `setmetatable`, `getmetatable`, `rawget`/`rawset`/`rawequal` |
| System and runtime | `collectgarbage`, `print`, `io`, `os`, `debug`, `jit`, `ffi` |
| Patterns | `string.find`, `string.match`, `string.gmatch`, `string.gsub`: Lua patterns can run for unbounded time outside the instruction budget |
| Sorting | `table.sort` |

`table.insert` is reimplemented in Lua so that it counts against the budget.
`string.format` follows Lua 5.1: `%d`, `%i`, `%u`, `%c`, `%x`, `%X`, `%o`,
`%e`, `%E`, `%f`, `%g`, `%G`, `%q`, `%s` and `%%`, with widths and precisions
up to 99.

Besides configuration (`rift.config`, `rift.setup`, `rift.on`, `rift.command`,
`rift.plugin`), `rift` provides:

| Name | What it does |
| --- | --- |
| `rift.try(f, ...)` | Calls `f(...)` like Lua's `pcall`: returns `true` and `f`'s results, or `false` and the error value |
| `rift.clock()` | Seconds, with sub-millisecond precision, from a fixed point in the process's life. Subtract two readings to measure elapsed time |
| `rift.time()` | The current Unix time in whole seconds |
| `rift.text` | [Styled text](text.md): `rift.text.mini(markup, args)` and `rift.text.legacy(text, marker)` return chat component tables; `to_legacy`, `plain`, `to_mini` and `escape` convert text |
| `rift.publish`, `rift.enabled`, `rift.servers` | Callbacks only |
| `rift.store`, `rift.http`, `rift.players` | Extension callbacks; `rift.players` also in `on_message` and `on_http` |
| `rift.permissions` | Extension callbacks; its `on_change` and `contexts.provide` registrations run during configuration ([permissions](permissions-lua.md#functions)) |

Use `rift.try` for errors you can handle, such as a failed
`rift.http.request`:

```lua
local ok, response = rift.try(rift.http.request, "directory", { path = uuid })
if not ok then return nil end -- fall back to the default
```

It does not catch an exhausted budget: when the instruction limit, the
deadline or the memory limit is reached, the callback fails as it would
without `rift.try`.

## Reloads and execution limits

Startup, validation and reload capture the entry script and all `.lua` files
under the neighboring `lua/` and `plugins/` directories into one snapshot.

- Only explicitly imported modules and plugins execute.
- Hidden entries such as `.git` are skipped; symlinks in these trees are
  rejected. Keep large assets and unrelated files elsewhere.
- Files must contain UTF-8 Lua source; bytecode is not accepted.

| Limit | Value |
| --- | --- |
| Combined source | 256 KiB |
| Module and plugin files | 256 |
| Directory entries | 1024 |
| Directory levels | 16 |
| VM memory, shared by all imports and handlers | 8 MiB |
| Instructions for evaluating the configuration | 100,000 |
| Instructions for the callback that follows | A fresh 100,000 |
| Deadline of a runtime callback | 50 ms |
| Deadline of a scheduled job | 5 s |
| Deadline of configuration evaluation at startup, checks, reloads and web-editor validation or saves | 5 s |

Every callback first evaluates the configuration again. A heavy configuration
cannot starve the callback of instructions, but its cost is paid in time on
every callback, against the 50 ms deadline. `rift check` prints how many
instructions the configuration uses and warns at half the budget or more.

> [!NOTE]
> On Linux aarch64 LuaJIT cannot take a memory limit, so only the instruction
> budget applies there. Rift logs one warning at startup.

**Nothing persists between callbacks.** Each callback builds a fresh VM from
the snapshot: a `require` inside a callback never reads live files, and
modules, globals and upvalues are gone when it ends. Rift keeps durable state
for you through its [host services](host-services.md).

**Reloads.** A successful reload captures edited modules and plugins even when
`rift.lua` did not change. Existing player sessions keep their original code.
A failed reload preserves the active configuration.

**Checks.** `rift check path/to/rift.lua`, reloads and web-editor validation
and saves resolve the same local module directories.
`rift check --strict` also checks environment secrets, the instance database,
extension store, port binds and reload compatibility; see
[strict checks](install.md#strict-checks).

**The web editor** edits the entry script only. Edit module and plugin files
on disk and reload to apply them.

**Rust embedders** can use `Config::load(path)` or
`Config::from_lua_at(source, path)` for local imports.
`Config::from_lua(source, name)` has no filesystem access and exposes only the
built-in `rift` modules.
