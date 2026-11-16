# Host services

Sandboxed code in `rift.lua` cannot open files or sockets. Rift gives it
bounded services instead: a durable key-value store (`rift.store`), scheduled
jobs, outbound HTTP to services you name (`rift.http`), the permission store
(`rift.permissions`) and the [online players API](players-api.md). This page
describes them as [`rift.lua`](extensions.md) uses them. Persistent plugins
reach the same store and permissions through the
[host API](plugins.md#host-api).

## Where the services work

- Only in authenticated callbacks and scheduled jobs. They are unavailable in
  the legacy route, HTTP and message callbacks, except that `rift.players` also
  works in `on_message` and `on_http` when `rift.config.plugins` is set.
- Configuration loading, `rift check` and candidate reload validation never
  execute jobs, HTTP requests or storage operations.
- Capture API references in initialization freely
  (`local store = require("rift.store")`); calling them there is an error.
  `require("rift.http")` and `require("rift.players")` work in the same way.
- All methods use a dot, not a colon.

## Persistent state

```lua
rift.config.plugins = {
    storage = { path = "extensions.state" },
    join = function(ctx)
        local visits = rift.store.increment("visits", ctx.uuid, 1)
        rift.store.set("last_server", ctx.uuid, ctx.server)
        rift.publish("visits", ctx.uuid .. " " .. tostring(visits))
    end,
}
```

The store is shared across sessions, jobs and reload generations.

| Function | Returns | Notes |
| --- | --- | --- |
| `rift.store.get(namespace, key)` | Stored string, or nil when absent | |
| `rift.store.set(namespace, key, value)` | Nothing | Stores a binary-safe string |
| `rift.store.delete(namespace, key)` | Boolean: whether an entry was removed | |
| `rift.store.increment(namespace, key, delta)` | The result | Atomically adds an integer; missing values start at zero |

- A read followed by a write is not atomic; use `increment` for concurrent
  counters.
- `increment` stores a decimal string. The existing value, delta and result
  must be integers in −(2^53−1) through 2^53−1, so LuaJIT numbers stay exact;
  invalid values or overflow fail without mutation.
- Namespaces organize data; they do not isolate mutually untrusted plugins.
- `rift.store` rejects namespaces beginning with `rift.` (host-owned data, such
  as the group memberships that an
  [upgrade](permissions.md#upgrading-from-extensions-api-v2) copies into the
  permission store) and with `plugin.` (persistent plugins' private data).

### The storage file

With `storage`, the store survives process restarts in a bundled-SQLite
database file. Without it, the same API uses memory and resets on restart.

- Relative paths are resolved against the configuration file's directory; the
  parent directory must already exist and be writable.
- Startup loads or creates the database and rejects corrupt files, SQLite
  databases that are not Rift extension stores, and entries that break the
  limits below.
- Each file belongs to one Rift process: Rift holds an exclusive lock on it
  while running, and a second process fails at startup. It is not a
  distributed database.
- Changing `storage` (path or quotas) requires a restart.
- Stores written by earlier Rift versions (versioned binary snapshots) are
  converted at startup. The original snapshot is kept as `<file>.v1-backup`; to
  go back to an earlier Rift version, restore it in place of the database.

> [!NOTE]
> While Rift runs, recent commits can be in a `-wal` file next to the database.
> To back it up, copy both, or stop Rift first.

### Limits and quotas

| Limit | Value |
| --- | --- |
| Namespace and key | UTF-8 strings of 1–128 bytes |
| Value | 64 KiB |
| Entries per owner | 4096 by default; `quota.entries` may be 1 to 1048576 |
| Bytes per owner | 1 MiB by default; `quota.bytes` may be 1 to 1073741824 (1 GiB) |
| Host-owned data | Fixed and separate: 65536 entries and 8 MiB |

Quotas apply per owner, counting each entry as its namespace, key and value
plus 8 bytes. The configuration script's `rift.store` data is one owner, and
each persistent plugin's `plugin.<name>.*` namespaces are another, so one
plugin filling its space cannot block another or the script. `storage.quota`
changes the defaults:

```lua
storage = {
    path = "extensions.state",
    quota = {
        entries = 8192,             -- every owner, default 4096
        bytes = 2 * 1024 * 1024,    -- every owner, default 1048576
        plugins = {
            -- per plugin; a missing field keeps the value above
            relaxoban = { entries = 100000, bytes = 32 * 1024 * 1024 },
        },
    },
}
```

- Exceeding a limit fails without changing committed state.
- Memory mirrors the whole store, so the sum of what owners actually use is
  kept in RAM.
- Lowering a quota below what an owner already stores does not fail startup.
  Its existing entries stay, and deletes and writes that do not add entries or
  bytes keep working, but writes that would grow it are refused until it is
  back under the limit.

### Durability

Each mutation commits only the entries it changes, in one SQLite transaction
that is synced to disk (WAL mode, `synchronous=FULL`) before the call returns.
A failed write changes nothing. Writes are serialized; reads never wait for a
write to sync. A committed mutation can outlive a timed-out callback.

## Scheduled work

```lua
rift.config.plugins.jobs = {
    heartbeat = {
        every_ms = 60000,
        run = function(ctx)
            rift.publish("jobs." .. ctx.job, "alive")
        end,
    },
}
```

Each job requires `every_ms` and a `run` function. Its context contains `job`
(and `api_version`) and no player identity. Return nil.

| Limit | Value |
| --- | --- |
| Jobs | 32 |
| Job name | 1–64 bytes of lowercase letters, digits, `_`, `-` or `.` |
| `every_ms` | An integer from 100 to 86400000 |
| Wall deadline | Five seconds, including worker scheduling and HTTP requests |
| Lua instructions, VM memory | 100,000 and 8 MiB, as for player callbacks |
| Job workers | Two of their own |

- The first run waits a full interval, and each next run waits a full interval
  after completion.
- Jobs never overlap by name, including across reloads.
- Missed intervals are skipped, never queued for catch-up. When both job
  workers are busy, the tick is skipped.
- Errors are logged and the job retries at its next interval.

A job waiting seconds on an HTTP call cannot make logins or commands fail. Use
jobs for external I/O and keep player policy checks local.

**Reload and shutdown.** Only the committed configuration schedules jobs. A
successful reload replaces the schedule and job source; a failed reload leaves
both intact. Shutdown stops new jobs before draining players. An already
running callback may finish within its remaining budget, keeping its permits
and excluding the same job name in a new generation.

Schedules are process-local and do not survive downtime or promise exactly-once
execution; use idempotent external operations.

## HTTP integrations

```lua
rift.config.plugins.integrations = {
    directory = {
        url = "https://directory.example.com/permissions/",
        timeout_ms = 1000,
        headers = { ["Accept"] = "text/plain" },
        -- Header values read from the environment, never written in rift.lua.
        headers_env = { ["Authorization"] = "DIRECTORY_API_TOKEN" },
    },
}
-- Inside a job:
local response = rift.http.request("directory", {
    method = "GET", path = "0123456789abcdef0123456789abcdef",
})
if response.status == 200 then
    -- The write commits before the call returns.
    rift.permissions.user("0123456789abcdef0123456789abcdef")
        :set("network.staff", response.body == "allow")
end
```

Up to 16 named integrations authorize only their configured HTTP(S) origin and
base path.

- **Base URL.** No embedded credentials, query or fragment; paths are treated
  as directories. It may be a private or loopback address: configuration
  authors choose the allowed endpoints.
- **Request.** A relative `path` (empty by default), `method` (GET by default),
  optional string `body` and optional `headers` table. Absolute URLs and paths,
  traversal and encoded separators are rejected. Request headers override
  configured headers.
- **Result.** `{ status = number, body = string, headers = table }`, with
  lowercase header names. HTTP error statuses are ordinary responses; transport
  errors, timeout, malformed options and oversized responses fail the callback.
- **Limits.** Bodies are limited to 64 KiB, headers to 32 entries and 8 KiB.
  `timeout_ms` defaults to 1000 and must be 1–5000; the enclosing callback's
  remaining deadline always takes precedence.
- Redirects are returned without following, environment HTTP proxies are
  disabled, and there are no automatic retries.

### Keep credentials in the environment

The values in `headers` are part of `rift.lua`, so they end up in deployment
history and in the web editor. `headers_env` maps a header name to an
environment variable name instead.

- Rift reads the variable when it sends each request, so the value never
  appears in the configuration, deployment history or diagnostics; errors name
  the variable only.
- A header may appear in `headers` or `headers_env`, not both, and both count
  toward the 32-header limit.
- Startup and reloads fail while a referenced variable is unset or is not a
  valid header value, and `rift check --strict` reports it before you start.
- Set the variable in the service environment (for example an
  `EnvironmentFile=` or a Compose secret), not on the command line.

## Permissions

Permission groups, memberships, live grants and their Lua API
(`rift.permissions`) are described in [permissions](permissions.md).

- The `extensions.groups` table is mirrored into the permission store on every
  committed reload; [groups as configuration](permissions.md#groups-as-configuration)
  lists its fields.
- Persistent plugins reach the same store through the contract's `permissions`
  interface ([permissions](permissions-lua.md#persistent-plugins)).
- Backend plugins can ask whether a player is in a group with the `RiftInGroup`
  request on the [BungeeCord channel](bungeecord.md#rift-requests).
