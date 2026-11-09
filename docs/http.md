# Web administration, status and Lua HTTP extensions

Run `rift --config examples/admin.lua`, then open <http://127.0.0.1:8080>.
The admin website shows listeners, backends, routes and live counters, edits the
complete Lua source, validates changes, saves and applies them, and reloads edits
made on disk. Its managed servers panel reports lifecycle state and requests
server starts and stops, creates instances from service groups, and removes
instances with explicit file retention/deletion labels. It also provides live server
logs and console commands, field-based group/template editing, permission
groups, players, nodes and their change history, configuration deployment
history, rollback and operator audit records. The status website runs separately at <http://127.0.0.1:9090>.
The dashboard assets are embedded in the binary; no Node installation or
separate frontend server is needed at runtime.

This guide covers `web` and `status` HTTP services. The operational `admin`
endpoint used by `rift admin` is a separate loopback JSON-line protocol with an
environment-provided secret and explicit operation permissions; see the
[operator guide](operations.md#enable-operational-administration). Both can run
on distinct ports. `admin.permissions` do not govern HTTP requests: the web
credential grants the enabled HTTP API capabilities, including configuration
editing and managed server lifecycle operations. Named `web.operators` can receive
restricted permissions and group scopes. Changes to operational `admin` and
static `managed_servers` definitions require a restart. Group/template changes
can apply live when existing instances retain their process definitions and
storage policy; remove affected instances before changing those settings.

## Configuration

Add these optional fields to your existing Lua configuration:

```lua
web = {
    enabled = true,
    listen = "127.0.0.1:8080",
    ui = true,
    api = true,
    -- token = "a-long-random-secret-at-least-16-characters",
},
status = {
    enabled = true,
    listen = "127.0.0.1:9090",
    ui = true,
    metrics = true,
},
metrics = false, -- Or "127.0.0.1:9092" for a separate metrics-only server.
```

Each server is disabled when omitted or set to `false`. `web` and `status` also
accept `enabled = false`, which retains the other settings in the source.
An enabled empty table uses the defaults above. `web.ui` controls the admin
HTML/assets; `web.api` controls the built-in `/api` endpoints. Lua `/ext` routes
remain available with `api = false`. The bundled admin dashboard needs the API
to display or edit data; disabling the API is useful for custom Lua sites.
`status.ui` controls the status HTML/assets (or styles them; see
[Public status page](#public-status-page)), and `status.metrics` controls its
Prometheus endpoint. The status JSON endpoint stays available whenever its
server is enabled. `metrics = false` disables the standalone metrics listener;
internal traffic counters and authenticated `/api/metrics` remain available.

The `web`, `status` and `metrics` settings, their addresses and `web.token` can
change through hot reload.
An unchanged address with port `0` keeps its assigned port; actual bound
addresses appear in logs and status JSON. New service sockets bind before a
change is committed. A failed HTTP save retains the previous file, runtime
configuration and services. A rejected reload preserves runtime and services
without undoing a file already edited externally. Moving a service onto a port
still occupied by another Rift service is rejected; use a spare port or separate
reloads. Gameplay listener names/addresses and the separate operational `admin`
bind, token-variable and permissions require a restart.

Turning off or moving the admin server may make the current browser connection
unavailable. Re-enable it by editing the Lua file and sending SIGHUP (Unix) or
Ctrl-Break (Windows), or restart Rift. The save response is allowed to finish
when disabling its own server. Accepted proxy sessions retain their existing
routes and are never interrupted by an HTTP configuration change.

## Public status page

The status server can be the page you link players to. Set
`exposure = "public"` and it reveals only names, health and player counts:

```lua
status = {
    listen = "0.0.0.0:9090",
    exposure = "public", -- Default "operator" keeps addresses and counters.
    backends = {
        survival1 = "Survival", -- Display name on the page and in JSON.
        arena = "Arena",        -- A service group: instances become "Arena 1", "Arena 2", …
        limbo = false,          -- Hidden from the status server.
    },
    cors = { "https://example.com" }, -- Or "*"; lets that site fetch /status.
    ui = {
        title = "Example Network",
        notice = "Maintenance Sunday 02:00 UTC",
        accent = "#ff6a00",
        refresh_ms = 10000,
        panels = { summary = true, backends = true, listeners = false, metrics = false },
        links = {
            { label = "Discord", href = "https://discord.gg/example" },
            { label = "Rules", href = "/ext/rules" },
        },
    },
},
```

| Field | Default | Meaning |
| --- | --- | --- |
| `exposure` | `"operator"` | `"public"` omits addresses, listeners, counters, the Rift version, the configuration revision and service settings from `/status`, and turns off `/metrics` unless `metrics = true` is set explicitly |
| `backends` | `{}` | Per backend or service group: a display name (1–64 characters) or `false` to hide it. Runtime instances are configured through their group. Hidden backends still appear on the authenticated `/api/status` and in the Lua `on_http` context |
| `cors` | none | Origins (`scheme://host[:port]`, no path) allowed to read `/status` from a browser, or `"*"` alone. Credentials are never allowed |
| `ui` | `true` | `false` disables the page; a table enables it and styles it |
| `ui.title` | `"Rift"` | Brand name in the header, the document title and the badge label (1–80 characters) |
| `ui.notice` | none | A banner under the verdict, such as planned maintenance (1–500 characters) |
| `ui.accent` | theme purple | `#rgb` or `#rrggbb`, applied to the theme's accent colour |
| `ui.refresh_ms` | `5000` | Refresh interval, `1000`–`300000` |
| `ui.panels` | all `true` | Show or hide `summary`, `backends`, `listeners` and `metrics`. Public exposure always hides listeners and metrics |
| `ui.links` | none | Up to 8 footer links; `href` is an `http(s)` URL or a path starting with `/` |
| `ui.enabled` | `true` | Keep the table but disable the page |

All of these hot-reload, so `ui.notice` doubles as an incident banner: edit it and
reload. The title and notice are rendered into the HTML for link previews and
readers without JavaScript; the page then applies everything else from the
`page` object in `/status`. Theming goes through the stylesheet's CSS
variables, so the strict Content Security Policy is unchanged.

Public `/status` has this shape (operator exposure adds the fields it always had,
plus `address` per backend):

```json
{
  "exposure": "public",
  "uptime_seconds": 86400,
  "players": 42,
  "health_checks_enabled": true,
  "backends": [
    { "name": "survival1", "display": "Survival", "version": null, "healthy": true, "players": 30 }
  ],
  "page": { "title": "Example Network", "notice": null, "accent": "#ff6a00", "refresh_ms": 10000,
            "panels": { "summary": true, "backends": true, "listeners": false, "metrics": false },
            "links": [ { "label": "Discord", "href": "https://discord.gg/example" } ] },
  "services": { "status": { "metrics": false } }
}
```

`players` counts everyone online, including players on hidden backends; each
backend's `players` counts players currently attached to it. To render the data
on your own website, list its origin in `cors` and fetch `/status` from there.

`GET /badge.svg` returns a small SVG for READMEs and forum signatures: the
`ui.title` (or "players") and `N online`, green when every backend is healthy,
amber when some are unhealthy, blue when health checks are off, and `offline` in
red when every visible backend is unhealthy. It is served whenever the status
server runs, including with `ui = false`.

## Authentication and access

A non-loopback bind requires `web.token` or at least one named operator token.
Tokens contain 16–4096 printable, non-whitespace ASCII characters. A loopback
bind may omit both: Rift then generates a random token when it starts and prints
it once to standard error:

```text
rift: web token for this run: 3f9c…(64 hex digits)… (enter it under API access; set web.token to choose a permanent one)
```

It is valid until Rift exits, across reloads, and grants full access. Loopback
alone is not treated as a credential, because any local process could use it,
including plugins on managed servers, which could otherwise define a service
group with an arbitrary command and run code as Rift's user. Every `/api` and
`/ext` request requires:

```http
Authorization: Bearer your-token
```

The HTML shell and static assets contain no private configuration and load
without authentication. Enter the token in the website's connection form; it is
held in memory and forgotten on page reload. API responses never include a
separate token field, but authenticated `/api/config` returns the original Lua
source, including any credentials it contains. The source is administrative data.

The generated token is accepted only with loopback IP or `localhost` Host headers. Requests
with mismatched Origin headers or cross-site Fetch Metadata are rejected, and
mutations require JSON. There is no CORS allowance or cookie-based login. Use
HTTPS at a trusted reverse proxy or an SSH tunnel for remote administration;
the built-in listener speaks HTTP. When proxying, preserve the browser's Host
header and use a token.

Named operators use independent tokens and permission lists:

```lua
rift.config.web = {
    listen = "127.0.0.1:8080",
    token = "replace-with-a-random-administrator-secret",
    records_directory = "operator-records", -- Relative to the config directory.
    operators = {
        lobby_staff = {
            token = "replace-with-a-different-random-staff-secret",
            permissions = { "read", "logs", "console", "servers", "audit" },
            groups = { "lobby" },
        },
        observer = {
            token = "replace-with-an-independent-observer-secret",
            permissions = { "read", "logs" },
        },
    },
}
```

| Permission | Access |
| --- | --- |
| `read` | Status, metrics and scoped group/server listings; include it for dashboard use |
| `logs` | Read managed server logs within the operator's scope |
| `console` | Send commands with the managed server's console privileges |
| `servers` | Start/stop servers and create/remove instances within scope |
| `config` | Read, validate, edit and reload complete Lua source and structured definitions |
| `deploy` | View configuration deployment metadata and restore retained deployments |
| `audit` | View operator records; group-scoped operators see only their own records |
| `extensions` | Invoke configured Lua `/ext` handlers |
| `files` | Browse, download, upload, edit and delete files in template folders (unrestricted scope only) and managed server directories within scope |
| `perms` | Read the [permission store](permissions.md) through `/api/perms` and the dashboard's permission views: groups, players, compiled subjects, traces, registered nodes, the change log and exports. Writing needs `config` |
| `plugins` | Restart persistent [plugins](plugins.md#operator-surface) (unrestricted scope only); reading plugin state needs `read`, plugin logs `logs` |

Omitting `groups` permits all groups and static managed servers. A list restricts
server actions and logs to instances of those groups; an empty list permits no
server targets. Scoped listings omit other groups/servers, backend routing and
configuration paths. Global counters and listener information remain visible.
`config`, `deploy`, `extensions`, `plugins` and `perms` require unrestricted
scope because Lua, configuration and plugin changes can affect the whole proxy,
permission data covers every server, and the plugin endpoints refuse
group-scoped operators entirely. `config` exposes any secrets
in the Lua source and can change credentials. The legacy `web.token` and
the generated token retain full privileges. Once `web.token` or any named
operator is configured, no token is generated. Tokens must be
unique, names must be distinct from reserved system/admin audit identities,
and permission/credential changes apply on the next request after reload.
`GET /api/access` reports the current identity and grants without credentials.

The separate status and metrics servers are read-only and unauthenticated. With
the default `status.exposure = "operator"`, status JSON includes backend/listener
addresses, health eligibility, counters and service settings, but no Lua source,
token or configuration filesystem path. Bind these servers to the monitoring
interface appropriate for that information, or use `exposure = "public"` (see
[Public status page](#public-status-page)) before exposing the status server to
players.
A healthy backend may still be administratively draining and reject new
attachments. With health checks disabled, an up value does not imply a successful
probe. `health_checks_enabled` reports the policy; the operational CLI and
`rift_backend_draining`/`rift_maintenance_mode` metrics expose effective maintenance
state, including runtime overrides.

## API

All bodies and errors are JSON except Lua responses and Prometheus metrics.
Errors use `{"error":"description"}`. `GET /api` provides endpoint discovery.

| Method and path | Purpose |
| --- | --- |
| `GET /api/access` | Current operator name, permissions (`null` for full access) and group scopes |
| `GET /api/status` | Runtime version, uptime, revision, actual listeners, backends, health, managed server state, service groups, routes, fallbacks, limits, counters and enabled services |
| `GET /api/servers` | `{servers: [...]}` with managed server lifecycle state, usage and `node` (`local` for this host) |
| `POST /api/servers/{name}/start` | Request a managed server start with `{}`; returns `202` when accepted |
| `POST /api/servers/{name}/stop` | Request an unused managed server stop and pause automatic wake with `{}`; returns `202` when accepted |
| `GET /api/servers/{name}/logs?cursor={byte_offset}` | Up to 64 KiB of log text with the next byte cursor, `truncated` and `has_more`; omit cursor to tail the log |
| `POST /api/servers/{name}/console` | Write one `{command: "list"}` to a running managed server; commands are limited to 4096 bytes without control characters |
| `GET /api/definitions` | Structured `templates`, `service_groups`, `permission_groups`, `tab_list`, `server_list` and the active `revision`; includes private paths and executable arguments. Absent settings are `null` |
| `PUT /api/definitions/{section}/{name}` | Apply `{revision, definition}`; section is `templates`, `service_groups` or `permission_groups`, and `null` removes a definition |
| `PUT /api/definitions/{section}` | Apply `{revision, definition}` to `tab_list` or `server_list`; `null` turns the setting off |
| `/api/perms/...` | The permission store; see [Permissions API](#permissions-api) |
| `GET /api/permissions` | Deprecated alias, kept for one release: `{enabled, members, truncated}`: up to 1000 users with global group memberships in the permission store as `{uuid, name, groups}`, with `name` the online or last-seen name |
| `POST /api/permissions/members/{player}/{group}` | Add a player (UUID, or a known player name) to an existing permission group with `{}`; returns `{changed, revision}` |
| `DELETE /api/permissions/members/{player}/{group}` | Remove a player from a permission group with `{}`; returns `{changed, revision}` |
| `GET /api/deployments` | Retained deployment metadata, active revision, retention limit and durability flag; never includes source |
| `POST /api/deployments/{id}/rollback` | Validate and restore a retained configuration with `{revision}` |
| `GET /api/audit` | Latest operator records and durability flag |
| `GET /api/groups` | `{groups: [...]}` with group names, inclusive port ranges, instance names, template and storage policy, and `nodes`/`placement` for groups on [nodes](nodes.md) |
| `GET /api/plugins` | `{plugins: [...]}`: every [plugin](plugins.md#operator-surface) in dispatch order with `name`, `mode`, `engine`, `version`, `status`, `restarts`, `calls`, `last_error`, `hooks`, `commands`, `capabilities` and `restartable`; unrestricted operators only |
| `GET /api/plugins/{name}` | One plugin's entry, or `404` |
| `GET /api/plugins/{name}/logs` | `{plugin, lines}`: the last 200 log lines of a persistent plugin as `{timestamp_unix_ms, level, message}`; `logs` permission |
| `POST /api/plugins/{name}/restart` | Stop a persistent plugin and start a fresh instance with `{}`; `plugins` permission, returns `202` when accepted. Poll `GET /api/plugins/{name}` for `status` and `restarts` |
| `GET /api/nodes` | `{nodes: [...]}` with each [node](nodes.md)'s `state` (`online`, `unreachable`, `never_connected`), address, labels, `instances`, `max_instances`, `memory_mb`, `allocated_memory_mb`, agent `version` and `last_seen_ms` |
| `POST /api/groups/{name}/instances` | Submit provisioning with `{}`; returns `202` with `{operation_id, status: "pending", poll}` (autostart follows group policy) |
| `DELETE /api/instances/{name}` | Submit removal of an unused instance with `{}`; returns `202` with `{operation_id, status: "pending", poll}` |
| `GET /api/operations/{id}` | Retrieve instance or download operation state and its completed result; requires current `servers` permission and the operation's group scope |
| `GET /api/templates` | `{templates: [...]}` with names, the groups using them, counts of config layers and plugins, and `software` with its download cache state (`cached`, `resolved_version`, `build`, `size`, `sha256`); never includes asset paths |
| `POST /api/templates/{name}/download` | Download (`{}`) or refresh (`{"refresh":true}`) a template's server software ahead of instance creation; `servers` permission, returns `202` with an operation to poll |
| `GET /api/software/{type}/versions` | Release names for `paper`, `folia`, `purpur`, `vanilla` or `fabric`, newest first, fetched upstream and cached for 10 minutes; `servers` permission |
| `GET /api/files/templates/{path}` | List a folder in `template_storage` as `{path, entries, truncated}`, or download a file (at most 64 MiB); `files` permission and unrestricted scope. `?manifest` returns `{files: [{path, size, sha256}], directories}` for the whole tree, which node agents use to fetch only changed files |
| `PUT /api/files/templates/{path}` | Write the raw request body to a file (at most 64 MiB), creating parent folders; add `?directory` to create a folder instead |
| `DELETE /api/files/templates/{path}` | Delete a file, or a folder with its contents |
| `GET`/`PUT`/`DELETE /api/files/servers/{name}/{path}` | The same operations inside a managed server's directory, within the operator's group scope. For servers on a node they go through its agent and files are limited to 512 KiB |
| `GET /api/metrics` | Counter values as JSON |
| `GET /api/config` | `{source, revision, writable}` for the active configuration |
| `POST /api/config/validate` | Validate `{source}` including live listener restrictions; does not save or bind sockets |
| `PUT /api/config` | Validate, atomically save and apply `{source, revision}` |
| `POST /api/reload` | Reload the selected file, with an empty JSON object `{}` |
| `GET /status` | Status JSON on the separate status server, as `status.exposure` allows; `status.cors` origins may read it cross-origin |
| `GET /badge.svg` | Player count and health badge on the status server |
| `GET /metrics` | Prometheus text on the status server when enabled, or the standalone metrics server |

Managed lifecycle requests return promptly; acceptance does not mean startup or
shutdown has completed. Poll `GET /api/servers` for `state`, `pid`, `players`,
`reservations`, `automatic_start` and `last_error`. `automatic_enabled` reports
whether an administrator has paused automatic starts. `restart_attempts` counts
automatic restarts since the last explicit Start, and `restart_exhausted`
identifies failed services requiring operator intervention. The same array is available
as `managed_servers` on authenticated `/api/status`. Listings include `address`,
`port`, `group` (`null` for static servers), `template` (`null` without one),
and `storage` (`persistent` or `disposable`; static managed servers are
`persistent`). Authenticated `/api/status` also includes `service_groups` with
the same objects as `/api/groups`, including the optional `scaling` policy.
Commands, arguments and
working directories are omitted. The public `/status` and Lua HTTP context omit
managed lifecycle details entirely. Authenticated `/api/config` still contains
the complete trusted configuration source.

A manual stop is rejected while players or backend attachments use the server.
The `reservations` count includes both pending and established attachments;
it overlaps the tracked player count rather than adding more players to it.
Once accepted, it pauses automatic wake until an explicit start or proxy restart;
idle shutdown preserves automatic wake. Unknown backend names return `404`,
existing unmanaged backends return `400`, conflicting lifecycle requests return
`409`, and an unavailable/full supervisor queue returns `503`. Start failures
after acceptance appear in `last_error`. See [managed servers](managed-servers.md)
for configuration and shutdown behavior. Managed definitions and their backend
addresses require a proxy restart; the configuration API cannot change them live.
Service-group operations allocate loopback ports and register/remove instance
backends without a proxy restart. Creation and removal return `202` promptly,
with an operation ID and polling path (also in the `Location` header).
Poll that path once per second using the same authorization header. A `200`
poll response has `operation_id`, `operation`, `target` and `status`:
`pending`, `succeeded` or `failed`. Successful operations include `result` with
the instance details or removal outcome and `http_status` (`201` for creation,
`200` for removal). Failed operations include `error` and `http_status`:
port exhaustion or an occupied instance is `409`; unknown groups/instances
are `404`. A failed operation is distinct from a failed polling request.
Authentication, authorization, malformed bodies and queue/tracking capacity
errors still reject the submission immediately.

Tracking permits at most 32 pending operations and 256 total records;
capacity exhaustion returns `503` without submitting work. Completed results
are retained for 10 minutes, then polls return `404`. Records are in memory
and do not survive proxy restarts. Pending operations retain their slots
until completion, regardless of HTTP disconnections. Each poll checks current
permissions and the group saved at submission, including after removal.
The dashboard polls through long operations, displays operation failures and
cleanup warnings, and identifies unresolved operations if polling fails.

Instance registration
survives configuration reloads and ends when the proxy restarts. Instance
removal runs asynchronously through its shutdown deadline. Persistent instances retain their world
and other files; disposable instances delete their generated directory after
the child exits. Stop/start preserves files for both storage policies. Successful
file deletion returns `files_removed: true`; persistent removal returns `false`.
If deletion fails after the child is reaped, the backend is still removed and
the operation result includes `files_removed: false` and `cleanup_error`; inspect the
remaining directory. Failed provisioning does not register an instance.
Template source assets are read-only inputs, and no EULA acceptance is generated.
See [templates and storage](managed-servers.md#local-asset-templates) for asset
layout and persistent directory reuse across proxy restarts.

Configuration saves preserve the source exactly, including comments, functions,
computed values and formatting. The source is the single editable configuration;
there is no generated JSON override file. Send the opaque revision from the last
`GET /api/config` with every save. A stale revision or a detected external file
change returns `409`; fetch/reload and review before retrying. Changes on disk
are applied only by an explicit reload or signal, not by a file watcher. The
website polls active state and preserves unsaved editor changes when another
client applies a revision.

The structured editor writes a single generated Lua wrapper around the original
script, preserving its comments, hooks and computed values. Field overrides are
ordinary Lua source and remain visible in `/api/config`. Definitions use the
same fields as Lua configuration. Structured reads return resolved absolute
paths; optional fields are omitted. Updates replace one complete definition,
validate all references and live-instance restrictions, then use the same save
transaction as source edits. Templates affect future provisioning; existing
instance files are preserved. Scaling policies can change while instances run.

The same editor covers the network settings on the Server list and Tab list
pages. `tab_list` and `server_list` definitions replace the whole
setting with the fields of their Lua tables; text fields take a string or a chat
component. A server list edit keeps the configured `favicon` and `hosts`, which
can only be set in Lua, so a definition containing either is rejected.
`GET /api/definitions` returns the favicon as its `data:` URL. Permission group
definitions take the fields of `extensions.groups` (`inherits`, `permissions`,
`weight`, `prefix`, `suffix`, `meta`) and need extensions with `api_version = 2`
or `3`; edits return `409` otherwise. They change configuration; the dashboard's
permission views edit the store directly through `/api/perms`.

The `/api/permissions` membership routes predate `/api/perms` and stay as
aliases for one release. Memberships are global `inherit` nodes in the
[permission store](permissions.md); changing one creates a permission revision
with actor `operator:<name>` and an operator audit record `group_membership`
with a `uuid/group` target. Everyone belongs to `default` implicitly, so it
can't be assigned. Reading needs `perms` or `config`, changing needs `config`.
Use `PUT /api/perms/users/{uuid}/nodes` and `DELETE
/api/perms/users/{uuid}/nodes/inherit:{group}` instead.

## Permissions API

`/api/perms` reads and edits the [permission store](permissions.md); the
dashboard's Groups, Players, Nodes and Activity views use nothing else. Every
write goes through the same single write path as `/perms`, Lua and the bus, so
it is validated the same way, applies to the next check, is pushed to backends,
and creates one revision in the change log with actor `operator:<name>` (the
web operator, `admin` for `web.token`, `local` for the generated token). Each
write also leaves an operator audit record (`GET /api/audit`) such as
`PUT perms_user_nodes` with the user and node as target. A write that changes
nothing creates no revision and returns `"ops": 0`.

Reading needs the `perms` or `config` permission; writing needs `config`.
`POST /api/perms/trace` only reads, so `perms` is enough for it. Shapes such as
typed nodes, compiled subjects, changes and change events are those of the
[permissions protocol](permissions-protocol.md).

Writes take a JSON object (`Content-Type: application/json`), `{}` when there
is nothing to send. Every write accepts an optional `reason` (at most 256
bytes) that the change log keeps. Successful writes return the
[applied](permissions-protocol.md#applied) result, sometimes with extra
members:

```json
{"revision": 413, "ops": 1, "changed_users": ["0123456789abcdef0123456789abcdef"], "changed_groups": []}
```

Errors carry the protocol's error code next to the message:

```json
{"error": "unknown group \"ghost\"", "code": "not_found"}
```

| Code | Status |
| --- | --- |
| `invalid_request` | `400` (also `413` for a body that is too large, `415` for a wrong content type) |
| `forbidden` | `403` |
| `not_found` | `404` |
| `conflict` | `409`, for example creating a group that exists or promoting past a track's last group |
| `limit` | `422`, a `permissions.limits` value would be exceeded |
| `unavailable` | `503` |
| `internal` | `500` |

Authentication and permission failures from the web service itself
(`401`, and `403` for a missing operator permission) carry no `code`.

Users in paths and bodies are a UUID, plain or dashed, or the name of a player
the proxy has seen (online first, then the last-seen name). Every UUID is a
valid user: one the store has never seen simply has no nodes.

**Node ids.** A node in a path is named by its slot, `<kind>:<key>`, followed
by `@<contexts>` when it has contexts, with the canonical context string
(pairs sorted, `;` between them): `permission:essentials.fly`,
`inherit:vip@server=survival`, `prefix:100`, `meta:homes@region=spawn;server=survival`.
The key is the protocol's storage key (the pattern, group, priority or meta
key). A subject holds one node per slot, so the id stays the same across
edits and restarts, and a client can build it from the typed node without
asking. Percent-encode it as one path segment
(`permission%3Aessentials.fly%40server%3Dsurvival`). Listings return each
node's id.

| Method and path | Purpose |
| --- | --- |
| `GET /api/perms/revision` | `{revision}`: the last committed revision |
| `GET /api/perms/backends` | `{revision, backends: [{principal, server, revision, hello_at, last_seen, client_version, requests, stale}]}`: every backend principal that has sent a `rift.perms.*` request since the proxy started. `revision` is what its last `hello` reported; `stale` when that is ahead of the proxy (its store went back), or behind it and it has not said `hello` since; times are Unix seconds |
| `GET /api/perms/registered` | `{registered: [{owner, node, description, default, plugin}]}`: nodes backend plugins registered, sorted by node |
| `GET /api/perms/search?node=&like=&kind=&subjects=&source=&limit=` | `{holders: [{subject, name, id, node}], truncated}`: who sets a node directly. `node` is an exact storage key, `like` a SQL `LIKE` pattern as in bulk filters; `subjects` is `any`, `users` or `groups` |
| `GET /api/perms/users?q=&group=&limit=` | `{users: [{uuid, name, online, groups}], truncated}`: players in the store (everyone who joined or has nodes), sorted by name. `q` matches part of the name or the start of the UUID; `group` keeps those with a positive `inherit` node for it in any context. `groups` lists global parents |
| `GET /api/perms/users/{user}` | `{uuid, name, online, primary_group, effective_primary_group, tracks, contexts, nodes: [{id, node}], revision}`; `primary_group` is the explicit one or `null`, `contexts` the player's current proxy contexts |
| `PATCH /api/perms/users/{user}` | `{primary_group}`: set the explicit primary group, `null` to clear it |
| `GET /api/perms/users/{user}/nodes` | `{subject, nodes: [{id, node}], revision}` |
| `PUT /api/perms/users/{user}/nodes` | Set the typed node in the body, replacing the node in its slot; the result adds its `id` |
| `PUT /api/perms/users/{user}/nodes/{id}` | Replace that node with the body, which may move it to another slot (other contexts, say); `404` when no node has that id |
| `DELETE /api/perms/users/{user}/nodes/{id}` | Remove the node; `"ops": 0` when it was not there |
| `DELETE /api/perms/users/{user}/nodes` | Remove every node, or with `{contexts}` only those with exactly these contexts |
| `GET /api/perms/users/{user}/compiled?server=&world=&dimension=&gamemode=&...` | `{subject, contexts, contexts_from, meta, groups}`: the compiled subject exactly as backends receive it, and the meta view and group memberships in the contexts given. Every query parameter is a context pair (repeat a custom key for several values); with none, the player's current contexts are used and `contexts_from` is `proxy` |
| `GET /api/perms/groups` | `{revision, groups: [{name, weight, display_name, parents, nodes, tracks}]}`; `parents` are the group's `inherit` nodes, `nodes` a count |
| `POST /api/perms/groups` | Create `{name, weight?, display_name?}`; `201` with the result and `name` |
| `GET /api/perms/groups/{name}` | `{name, weight, display_name, tracks, nodes: [{id, node}], revision}` |
| `PATCH /api/perms/groups/{name}` | `{weight?, display_name?}`, at least one; `display_name: null` clears it |
| `DELETE /api/perms/groups/{name}` | Delete a group and its nodes. `inherit` nodes naming it stay and grant nothing. `default` cannot be deleted |
| `GET`, `PUT`, `DELETE /api/perms/groups/{name}/nodes[/{id}]` | The same node routes as for users; `weight` and `display_name` nodes are allowed here |
| `GET /api/perms/tracks` | `{revision, tracks: [{name, groups}]}` |
| `GET /api/perms/tracks/{name}` | `{name, groups}` |
| `PUT /api/perms/tracks/{name}` | Create or replace with `{groups}`, lowest first, distinct |
| `DELETE /api/perms/tracks/{name}` | Delete a track; players keep their groups |
| `POST /api/perms/tracks/{name}/promote` | `{user, contexts?}`: move the player to the next group in exactly these contexts; the result adds `user` |
| `POST /api/perms/tracks/{name}/demote` | `{user, contexts?}`: move the player to the previous group |
| `GET /api/perms/audit?since=&actor=&subject=&limit=` | `{revision, events}`: [change events](permissions-protocol.md#change-event) in revision order. With `since`, the first `limit` after that revision; otherwise the latest `limit` (default 100, at most 1000). `actor` is an actor string (`operator:alice`, `cli`); `subject` is `user:<uuid>`, `group:<name>`, a bare UUID or a group name |
| `POST /api/perms/trace` | `{user, node, contexts?}` → `{node, result, winner, losers, ranks, contexts, contexts_from, uuid, revision}`: the answer for the node, the winning entry and why every other matching entry lost. `ranks` maps each rank to `user:<uuid>` or `group:<name>`. Without `contexts`, the player's current ones are used |
| `POST /api/perms/apply` | `{change, reason?}`: apply one raw [change](permissions-protocol.md#change), any `op` including `batch` |
| `POST /api/perms/bulk` | `{filter, patch, dry_run?, reason?}`: a bulk edit in one revision. With `dry_run: true` nothing is written and the reply is `{dry_run: true, count, ops}`; otherwise the result with `dry_run: false` |
| `POST /api/perms/import?replace=` | Import a LuckPerms `/lp export` file sent as the raw body, JSON (`application/json`) or gzipped (`application/gzip`), at most 32 MiB. Returns `{revision, ops, groups, users, tracks, skipped}`. Additive; `replace=true` first deletes earlier imported nodes |
| `GET /api/perms/export?gzip=` | The whole store in the LuckPerms export format; with `gzip` a gzipped download named `luckperms-export.json.gz` |

For example, give a player a temporary permission on one server, check it, and
move them up a track:

```sh
curl -s -X PUT http://127.0.0.1:8080/api/perms/users/Alice/nodes \
  -H "Authorization: Bearer $RIFT_WEB_TOKEN" -H 'Content-Type: application/json' \
  -d '{"kind": "permission", "key": "essentials.fly", "value": true,
       "contexts": {"server": "survival"}, "expires_at": 1762800000, "reason": "event"}'
# {"revision":414,"ops":1,"changed_users":["0123…"],"changed_groups":[],"id":"permission:essentials.fly@server=survival"}

curl -s -X POST http://127.0.0.1:8080/api/perms/trace \
  -H "Authorization: Bearer $RIFT_WEB_TOKEN" -H 'Content-Type: application/json' \
  -d '{"user": "Alice", "node": "essentials.fly", "contexts": {"server": "survival"}}'
# {"node":"essentials.fly","result":true,"winner":{"entry":{"k":"perm","key":"essentials.fly","v":true,"ctx":{"server":"survival"},"rank":0,"exp":1762800000},"specificity":"exact","outcome":"won"},"losers":[],"ranks":[{"rank":0,"subject":"user:0123…"},{"rank":1,"subject":"group:vip"}],…}

curl -s -X POST http://127.0.0.1:8080/api/perms/tracks/ranks/promote \
  -H "Authorization: Bearer $RIFT_WEB_TOKEN" -H 'Content-Type: application/json' \
  -d '{"user": "Alice"}'

# Remove the node again by its id.
curl -s -X DELETE 'http://127.0.0.1:8080/api/perms/users/Alice/nodes/permission%3Aessentials.fly%40server%3Dsurvival' \
  -H "Authorization: Bearer $RIFT_WEB_TOKEN" -H 'Content-Type: application/json' -d '{}'
```

Create a group with a prefix, preview a bulk edit, and move a LuckPerms
installation over:

```sh
api() { curl -s -H "Authorization: Bearer $RIFT_WEB_TOKEN" -H 'Content-Type: application/json' "$@"; }
api -X POST http://127.0.0.1:8080/api/perms/groups -d '{"name": "vip", "weight": 10, "display_name": "VIP"}'
api -X PUT http://127.0.0.1:8080/api/perms/groups/vip/nodes -d '{"kind": "prefix", "priority": 100, "text": "&6[VIP] "}'
api -X POST http://127.0.0.1:8080/api/perms/bulk -d '{"filter": {"kind": "permission", "key_like": "essentials.%"},
  "patch": {"value": false}, "dry_run": true}'
# {"dry_run":true,"count":3,"ops":[{"op":"node","subject":{"group":"vip"},"before":{…,"value":true},"after":{…,"value":false}},…]}
curl -s -X POST 'http://127.0.0.1:8080/api/perms/import' -H "Authorization: Bearer $RIFT_WEB_TOKEN" \
  -H 'Content-Type: application/gzip' --data-binary @luckperms-export.json.gz
# {"revision":420,"ops":312,"groups":6,"users":41,"tracks":1,"skipped":[{"subject":"group:mod","key":"r=^worldedit","reason":"…"}]}
curl -s 'http://127.0.0.1:8080/api/perms/export?gzip' -H "Authorization: Bearer $RIFT_WEB_TOKEN" -o rift-export.json.gz
```

The import and export routes are the same as `/perms import` and `/perms
export` and read and write the format described under
[LuckPerms import and export](permissions.md#luckperms-import-and-export).

The dashboard follows selected server logs every second, retaining a bounded
128 KiB view. Logs contain server output and may include private information;
grant `logs` accordingly. Console requests write exactly one line to the owned
process's stdin, with a bounded write deadline. A successful response confirms
the write; observe server logs for the command's result. Console commands have
the server's full console powers, including commands affecting players or files.

Deployment history retains the latest 64 successful source configurations,
including startup, HTTP saves, CLI/signal reloads and rollbacks. Rollback checks
the supplied active revision and external disk edits, validates the candidate,
reserves sockets, then atomically saves/applies it. Rejection preserves the
working file/runtime. Restore uses the retained entry Lua source and current
local module/plugin files; it does not restore asset contents, executable files,
runtime instances or worlds. Restoring creates a new history entry.

Records default to a private `<config filename>.operators` directory beside the
configuration. Set `web.records_directory` to writable storage when the config
mount is read-only; changing that directory requires a restart. Unix directories
use mode `0700`, files `0600`. Retained deployment sources may contain credentials;
protect and back up this directory as administrative data. With no file-backed
configuration, or an unwritable default directory, records stay in memory and
the APIs report `durable: false`. An explicitly configured unwritable records
directory fails startup. A post-commit history write failure is reported in Rift's
stderr; the already-applied configuration remains active.

HTTP mutations record the named actor, operation, target, timestamp and admission
result. Scope denials, local CLI commands and signal reload results are also
recorded. Requests without a valid token are summarised rather than recorded one
by one: the first in each minute writes an `authentication` record by
`anonymous` whose `count` is the number of such requests since the previous
one, and `GET /api/audit` reports the requests not yet written as
`unrecorded_denials`. The latest 64 of these summaries are kept apart from
operator records, so a flood of bad requests cannot push operator actions out. Bodies, console text, authorization headers and query strings
are excluded. Lifecycle `accepted` means queued; follow server state for completion.
An intent record precedes HTTP/CLI mutation execution; a timeout can leave an
intent without a final result. HTTP/CLI operations are refused if that intent
cannot be written. The API retains the latest 1024 audit records; the journal
compacts at 8 MiB. These are local records, not a tamper-proof external audit log.

Validation errors and failed socket reservations return `400`. Missing or
invalid tokens return `401`, denied browser origins return `403`, unknown or
disabled endpoints return `404`, oversized bodies return `413`, and incorrect
mutation content types return `415`. Busy HTTP/configuration/Lua capacity returns
`503`. Lua execution errors return `500`, and an async Lua deadline returns `504`.

For example, using Python's standard library (set `RIFT_WEB_TOKEN` to the
configured `web.token` value if needed; the server does not read this environment
variable automatically):

```python
import json, os, urllib.request

base = "http://127.0.0.1:8080"
headers = {"Content-Type": "application/json"}
if os.environ.get("RIFT_WEB_TOKEN"):
    headers["Authorization"] = "Bearer " + os.environ["RIFT_WEB_TOKEN"]

def api(method, path, value=None):
    body = None if value is None else json.dumps(value).encode()
    request = urllib.request.Request(base + path, body, headers, method=method)
    with urllib.request.urlopen(request) as response:
        return json.load(response)

current = api("GET", "/api/config")
# In a real script, make an intentional change to current["source"].
updated = current["source"] + "\n-- Reviewed through the HTTP API\n"
api("POST", "/api/config/validate", {"source": updated})
print(api("PUT", "/api/config", {
    "source": updated, "revision": current["revision"]
}))
```

A save needs write access to the configuration file and its parent directory.
The supplied systemd and Compose examples default to read-only configuration;
see the [operator guide](operations.md#http-administration) before enabling
browser saves in those environments. A save writes a new sibling file, preserves the original access permissions,
flushes it and atomically replaces the selected file. Configuration evaluation
and disk work run on blocking workers, outside gameplay I/O. The selected path
is canonicalized at startup, so a symlink selects its target. Source size is
limited to 256 KiB; the JSON envelope allows escaped source bytes. Saves and
signal, HTTP API and `rift admin reload` requests share one serialized
transaction path with a bounded queue.
Each HTTP server admits at most 64 open connections, with a 10-second socket
lifetime covering headers, request bodies and response writes. Retiring servers
allow up to three seconds for existing responses to finish. Gameplay admission
and Lua routing capacity remain independent of these HTTP limits.

External file conflict checks are best-effort: an editor writing during the
final check-and-rename interval can race a save. Use the admin API for competing
automated writers, or finish an external edit and reload it before editing in
the website. File replacement is atomic, but does not promise power-loss durability
of the directory entry.

## Template and server files

The Files page and `/api/files` serve two kinds of folders, like CloudNet's
template storage: `template_storage` (default `local/templates` beside the
configuration) and each managed server's working directory. Point template
`configs` layers at folders under `template_storage` to edit them from the
dashboard; see [managed servers](managed-servers.md#automatic-server-software-and-cloudnet-style-folders).
Paths are relative to the selected folder. `.`/`..` segments, backslashes,
colons, control characters and symlinks anywhere below the root are rejected,
and Rift-owned `.rift-*` files (such as the instance ownership marker) cannot
be written or deleted. Uploads are written to a temporary file and renamed into
place. Downloads are sent as `application/octet-stream` attachments. Folder
listings return at most 2000 entries. Writes to a running server's directory
take effect when the server reads them, usually on reload or restart. File
mutations are recorded in the audit log as `PUT files` and `DELETE files`.

Writing a server's plugins or configuration runs code with that server's
operating-system identity; grant `files` only to operators you would also
trust with `console`.

[Node agents](nodes.md) authenticate to `GET /api/files/templates` with their
node token (`rift.config.nodes.<name>.token_env`) to mirror template folders.
That token can only read template files; every other request, and every write,
is refused with `403`. Node reads are not audited, like other reads.

## Lua websites and endpoints

The optional `on_http` callback handles every HTTP method at `/ext` and `/ext/*`.
It has the same web bearer authentication and browser-origin protections as the
admin API. Use it for network dashboards, maintenance information, deployment
metadata, diagnostic pages and application-specific API responses. Paths outside
this namespace remain owned by Rift.

```lua
on_http = function(request)
    if request.method == "GET" and request.path == "/ext/connections" then
        return {
            status = 200,
            content_type = "text/plain; charset=utf-8",
            body = tostring(request.context.metrics.active),
        }
    end
    if request.method == "GET" and request.path == "/ext/help" then
        return {
            content_type = "text/html; charset=utf-8",
            body = "<!doctype html><title>Support</title><h1>Contact the network operator</h1>",
        }
    end
    return nil -- 404 for paths this script does not handle.
end,
```

The request table contains `method`, `path`, `query` (raw query string without
`?`), UTF-8 `body`, lowercase `headers`, and `context` (the status object with
operator detail, as a Lua table; `status.exposure` and hidden `status.backends`
do not apply to it, and each backend has `display` and `players`). Authorization, cookie and proxy authorization headers are
removed. JSON arrays become one-based Lua arrays. The context is an isolated
snapshot; modifying it cannot change the live runtime. With API v2 in
[`rift.config.plugins`](extensions.md#online-players), the handler can also
list, message, kick and transfer players through `rift.players`.

Return `nil`, or `{body = "...", status = 200, content_type = "text/plain"}`.
Only `body` is required. Status must be an integer from 200 to 599; 204, 205 and
304 require an empty body. The MIME value is validated, arbitrary response
headers are unavailable, and both request and response bodies are bounded to
256 KiB. HTML from a custom handler inherits the service's Content Security
Policy: external resources and inline scripts/styles are blocked. Escape any
request-derived text before putting it into HTML.

HTTP hooks reuse Rift's restricted Lua VM: fresh state per invocation, 8 MiB
memory limit, 100,000-instruction budget and 50 ms deadline. Four HTTP script
workers are shared across reloads, independently of routing hook capacity.
Scripts can import local modules and folder plugins from their saved snapshot
(see [Lua API](lua.md)); they have no filesystem, sockets, dynamic/native module
loading, OS or debug access. They
cannot call arbitrary URLs or mutate the running configuration; configuration
writes use the revision-checked admin API. Hook/source changes become visible
on the next successful reload. The bundled admin extension console can exercise
methods, paths and request bodies without leaving the dashboard.

For Rust integrations, `rift::http_script::{HttpScript, HttpRequest,
HttpResponse, HttpError}` provides an owned, thread-safe interface. Obtain the
script from `Config::from_lua(...).on_http`; async hosts call
`script.execute(request).await`. The synchronous `evaluate` entry point accepts
an explicit deadline and is intended for callers that own their worker policy.

## Development checks

Run `cargo test --locked --all-targets` and
`cargo clippy --locked --all-targets -- -D warnings` for Rust coverage, including
real-socket HTTP and proxy integration tests. `node src/web_assets/tests.js`
runs the frontend behavior harness using only Node's standard library; Node is
needed only for this optional local test command, never for building or running
Rift. CI runs both suites. The frontend harness exercises API interactions and
editor state transitions; it is not a browser rendering test.
