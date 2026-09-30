# Lua scripts, modules and plugins

Write `rift.lua` as an ordinary script. No outer `return { ... }` is needed:

```lua
local config = require("rift.config") -- Also available as rift.config.

config.listeners.public = "0.0.0.0:25565"
config.backends.lobby = "127.0.0.1:25566"
config.routes.public = "lobby"
config.limits.max_connections = 1024

require("config.services")
rift.plugin("greeting")
```

`rift` is a global, like Neovim's `vim`; `local rift = require("rift")` returns
that same API. `rift.config` contains the existing configuration fields, with
empty `listeners`, `backends`, `routes` and `limits` tables ready to edit. Other
sections are optional; assign them before editing their fields. All existing
schema, authentication, permission and reload checks still apply. Bare globals
such as `listeners = ...` are ordinary Lua variables, not configuration fields.

`rift.setup({ ... })` assigns top-level configuration fields. It replaces each
supplied field, without recursively merging nested tables. You can call it more
than once. Edit individual entries to extend an existing section. Do not replace
the `rift.config` table itself.

Existing files that return a configuration table remain supported. Returned
fields override fields assigned through `rift.config` or `rift.setup`; event
and command registrations are then applied to the resulting configuration.

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
working directory. `require("config.network")` looks for `lua/config/network.lua`
and then `lua/config/network/init.lua`. Modules can execute settings directly,
return functions, or return tables. Modules with no return value yield `true`.
Like Lua's `require`, successful imports are cached by name within each VM;
a module returning `false` is evaluated again on the next import. A module
receives its import name as `...`.

Use dotted names with letters, digits, `_` or `-` in each component, up to 128
bytes. Filesystem paths, `..`, absolute paths, native libraries and `package.path`
are not supported. Import errors identify the module and file; circular imports
fail with an explicit diagnostic. `rift`, `rift.config`, `rift.store`, `rift.http` and `rift.permissions` are reserved modules.

## Folder-based plugins

`rift.plugin("greeting")` adds `plugins/greeting/lua/` to module lookup, then
executes `plugins/greeting/init.lua`. Plugin names use letters, digits, `_` or `-`,
up to 128 bytes. The entry script receives the plugin name as `...` and can use
the same API as `rift.lua`. It needs no return value:

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

Plugins load explicitly in the order you call `rift.plugin`. Repeated calls
return the cached result and do not register the plugin twice. The result is
the entry script's returned value, or `true` when it returns nothing. A plugin
may therefore return a module with its own `setup` function:

```lua
rift.plugin("my-plugin").setup({ greeting = "Welcome!" })
```

Lookup checks the configuration's `lua/` first, then each loaded plugin's `lua/`
in import order; within each directory `name.lua` precedes `name/init.lua`.
Already imported modules remain cached. Give plugin modules their own namespace
(for example `greeting.format`) to avoid collisions. Copy a plugin folder into
`plugins/` to install it; Rift does not download or auto-enable plugins.

## Registering callbacks and commands

`rift.on(event, callback)` appends a handler. Supported events are `route`, `http`,
`message`, `login`, `initial_server`, `join`, `disconnect`, `before_transfer` and
`after_transfer`. Arguments and results match the existing `on_route`, `on_http`,
`on_message` and [authenticated extension](extensions.md) callbacks.

Handlers run in registration order. A directly configured callback runs first.
For all events except `message`, the first non-nil result ends the chain and is
validated by the host. Return nil to let later handlers run. This means observer
events such as `join` must still return nil. Message handlers all run, and their
return values are ignored. An error stops the chain and follows the existing
event's failure policy; handlers share one execution budget.

```lua
rift.on("route", function(connection)
    if connection.peer_ip == "192.0.2.10" then
        return { reject = true, reason = "Access denied" }
    end
end)

-- Requires online authentication, Velocity forwarding and a permission grant.
rift.config.extensions = {
    api_version = 1,
    permissions = { ["*"] = { ["greeting.hello"] = true } },
}
rift.command("hello", {
    permission = "greeting.hello",
    run = function(ctx) return { message = "Hello " .. ctx.name } end,
})
```

Authenticated event and command registration creates `extensions` and defaults
its `api_version` to 1 when absent. Explicit versions are still validated.
`rift.command(name, definition)` uses the existing command schema and permission
checks. Duplicate names, including collisions with `extensions.commands`, fail
validation. Settings and registrations belong in initialization, outside runtime
callbacks. `rift.on`, `rift.command` and `rift.setup` reject late registration.

`rift.publish(subject, payload, reply?)` and `rift.enabled` remain available in
callbacks. Imports and top-level initialization cannot publish messages. Captured
API references, including `local publish = rift.publish`, work in callbacks.

## Reloads and execution limits

Startup, validation and reload capture the entry script and all `.lua` files
under neighboring `lua/` and `plugins/` directories. Only explicitly imported
modules and plugins execute. Hidden entries such as `.git` are skipped; symlinks
in these trees are rejected. Keep large assets and unrelated files elsewhere.
The combined source limit is 256 KiB, with up to 256 module/plugin files, 1024
directory entries and 16 directory levels. Files must contain UTF-8 Lua source;
bytecode is not accepted.

Each callback builds a fresh VM from that immutable snapshot. Even a `require`
inside a callback uses the saved file contents; it never reads live files.
Modules, globals and upvalues do not persist between callbacks. A successful
reload captures edited modules and plugins even when `rift.lua` did not change.
Existing player sessions retain their original code. API v1 also pins permissions;
API v2 supports live grants, durable state, jobs and HTTP integrations as described
in the [extension API](extensions.md). A failed reload preserves the active
configuration.

All imports and handlers share the existing 8 MiB VM memory limit, 100,000
instruction budget and 50 ms execution deadline (v2 scheduled jobs have a five-second
wall deadline). Direct filesystem/process I/O, native
modules, arbitrary code loaders and the other restricted facilities remain
unavailable. This is a local Lua plugin API, not the full Neovim runtime.

`rift check path/to/rift.lua`, reloads, and web-editor validation/saves resolve
the same local module directories. The web editor edits the entry script;
edit module/plugin files on disk and reload to apply them. Rust embedders can
use `Config::load(path)` or `Config::from_lua_at(source, path)` for local imports;
`Config::from_lua(source, name)` has no filesystem access and only exposes the
built-in `rift` modules.
