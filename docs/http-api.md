# HTTP API

The `web` service serves a JSON API under `/api`. The dashboard uses nothing
else, so you can script everything it does. This page lists every route by
area; the permission store has its own page, the
[Permissions HTTP API](permissions-http.md).

```sh
curl -s http://127.0.0.1:8080/api/status -H "Authorization: Bearer $RIFT_WEB_TOKEN"
```

## Make a request

- Send a [bearer token](http.md#authentication-and-access) with every request.
- Bodies and errors are JSON, except file contents, Lua responses and
  Prometheus metrics. Errors use `{"error":"description"}`.
- Mutations require a JSON body; send `{}` when a route takes no fields.
- `GET /api` provides endpoint discovery.

**Permission** is the operator permission a route needs; full-access tokens
pass every check. "Scoped" routes also require the target to be in the
operator's [group scope](http.md#group-scopes); "unrestricted" routes refuse
group-scoped operators.

| Status | Returned for |
| --- | --- |
| `400` | Validation errors and failed socket reservations |
| `401` | Missing or invalid tokens |
| `403` | Denied browser origins, permissions or scopes |
| `404` | Unknown or disabled endpoints |
| `413` | Oversized bodies |
| `415` | Incorrect mutation content types |
| `500` | Lua execution errors |
| `503` | Busy HTTP, configuration or Lua capacity |
| `504` | An async Lua deadline |

The [connection limits](http.md#limits) apply to every request.

## Status and metrics

| Method and path | Permission | What it does |
| --- | --- | --- |
| `GET /api` | none | Endpoint discovery |
| `GET /api/access` | none | Current operator name, permissions (`null` for full access) and group scopes |
| `GET /api/status` | `read` | Runtime version, uptime, revision, actual listeners, backends, health, managed server state, service groups, routes, fallbacks, limits, counters and enabled services |
| `GET /api/metrics` | `read` | Counter values as JSON |
| `GET /status` | none | Status JSON on the separate [status server](status-page.md), as `status.exposure` allows; `status.cors` origins may read it cross-origin |
| `GET /badge.svg` | none | Player count and health badge on the status server |
| `GET /metrics` | none | Prometheus text on the status server when enabled, or the standalone metrics server |

`/api/status` has the array of `/api/servers` as `managed_servers`, and
`service_groups` with the same objects as `/api/groups`, including the
optional `scaling` policy. It lists backends hidden from the status site and
counts every player.

## Managed servers

| Method and path | Permission | What it does |
| --- | --- | --- |
| `GET /api/servers` | `read` | `{servers: [...]}` with managed server lifecycle state, usage and `node` (`local` for this host) |
| `POST /api/servers/{name}/start` | `servers`, scoped | Request a managed server start with `{}`; returns `202` when accepted |
| `POST /api/servers/{name}/stop` | `servers`, scoped | Request an unused managed server stop and pause automatic wake with `{}`; returns `202` when accepted |
| `GET /api/servers/{name}/logs?cursor={byte_offset}` | `logs`, scoped | Up to 64 KiB of log text with the next byte cursor, `truncated` and `has_more`; omit cursor to tail the log |
| `POST /api/servers/{name}/console` | `console`, scoped | Write one `{command: "list"}` to a running managed server; commands are limited to 4096 bytes without control characters |

Lifecycle requests return promptly; acceptance does not mean startup or
shutdown has completed. Poll `GET /api/servers` for these fields:

| Field | Meaning |
| --- | --- |
| `state`, `pid`, `players`, `automatic_start` | Current lifecycle state |
| `reservations` | Pending and established attachments; it overlaps the tracked player count rather than adding to it |
| `automatic_enabled` | Whether an administrator has paused automatic starts |
| `restart_attempts` | Automatic restarts since the last explicit start |
| `restart_exhausted` | Marks a failed service that requires operator intervention |
| `last_error` | Includes start failures after acceptance |
| `address`, `port`, `group`, `template` | `group` is `null` for static servers, `template` `null` without one |
| `storage` | `persistent`, `disposable` or `ephemeral`; static managed servers are `persistent` |
| `group_suspended`, `group_mode`, `draining` | State of a [whole group](#start-or-stop-a-whole-group) |

Commands, arguments and working directories are omitted.

A manual stop is rejected while players or backend attachments use the
server. Once accepted, it pauses automatic wake until an explicit start or
proxy restart; idle shutdown preserves automatic wake.

| Status | Returned for |
| --- | --- |
| `404` | Unknown backend names |
| `400` | Existing unmanaged backends |
| `409` | Conflicting lifecycle requests |
| `503` | An unavailable or full supervisor queue |

The configuration API cannot change managed definitions and their backend
addresses live; they require a proxy restart. See
[managed servers](managed-servers.md) for configuration and shutdown behavior.

Logs may include private information; grant `logs` accordingly. A console
request writes exactly one line to the owned process's stdin, with a bounded
write deadline. A successful response confirms the write; observe the server
log for the result.

> [!WARNING]
> Console commands have the server's full console powers, including commands
> affecting players or files.

## Players

| Method and path | Permission | What it does |
| --- | --- | --- |
| `GET /api/players` | `read` | `{players: [...]}`: online players as `rift.players.list` reports them (`connection_id`, `uuid`, `name`, `server`, `protocol`, `visible_to`, `locale`), hidden players included; group-scoped operators see the players on their servers |
| `POST /api/players/{player}/message` | `console`, scoped | Send `{text}` to one player (a name, UUID or connection id). Returns `{online: true}`, or `404` when the player is not online |
| `POST /api/players/{player}/kick` | `console`, scoped | Disconnect one player with `{reason}`. Same answers and scope as `message` |

`text` and `reason` take the same values. A JSON string is plain text of
1..=1024 bytes. An object or list is a JSON text component of at most 32 KiB,
with colors, hover text and click events ([rich text](players-api.md#rich-text));
line breaks in a kick component make a multi-line screen. For a group-scoped
operator, the player must be on one of their servers.

## Service groups and instances

| Method and path | Permission | What it does |
| --- | --- | --- |
| `GET /api/groups` | `read` | `{groups: [...]}`; see the fields below |
| `POST /api/groups/{name}/instances` | `servers`, scoped | Submit provisioning with `{}`; returns `202` with `{operation_id, status: "pending", poll}` (autostart follows group policy) |
| `DELETE /api/instances/{name}` | `servers`, scoped | Submit removal of an unused instance with `{}`; returns `202` with `{operation_id, status: "pending", poll}` |
| `POST /api/groups/{name}/start` | `servers`, scoped | Start a whole [service group](service-groups.md#start-stop-and-dev-mode-for-a-whole-group) with `{}`: resume it and start every instance that is not running; returns `202` with `{group, suspended, mode, instances}` |
| `POST /api/groups/{name}/stop` | `servers`, scoped | Stop a whole group with `{}`: keep new players out, pause scaling and stop every empty instance now and the others once their players leave; returns `202` with the same shape |
| `PUT /api/groups/{name}/mode` | `servers`, scoped | Set `{"mode": "dev"}` or `{"mode": "active"}`; returns `200` with `{group, suspended, mode, dev_permission}` |
| `GET /api/operations/{id}` | `servers`, scoped | Retrieve instance or download operation state and its completed result; the scope is the operation's group |
| `GET /api/nodes` | `read` | `{nodes: [...]}` with each [node](nodes.md)'s `state` (`online`, `unreachable`, `never_connected`), `schedule` (`active`, `cordoned`, `draining`), `drained`, address, labels, `instances`, `max_instances`, `memory_mb`, `allocated_memory_mb`, `min_free_disk_mb`, agent `version`, the start settings it applies (`features`), the controller's it lacks (`missing_features`), `version_mismatch`, `last_seen_ms` and the host `resources` its agent measured (`null` unless online; see [host resources](nodes.md#host-resources)) |
| `POST /api/nodes/{name}/cordon`, `/drain`, `/uncordon` | `servers`, unrestricted operators only | [Take a node out of service](nodes.md#take-a-node-out-of-service) or return it, with `{}`; returns `200` with `{node, schedule, drained, instances}`, where each instance's `result` is `running`, `kept`, `retiring` or `draining` |

A `/api/groups` entry has the group name, inclusive port range, instance
names, template and storage policy, plus:

- `nodes` and `placement` for groups on [nodes](nodes.md)
- `balance`: `least_players` or `fill`
- `suspended`, the operator state, and `mode` (`active` or `dev`) with its
  `dev_permission`
- `translation`: `clients` as in the definition, a readable `summary`,
  `inherited` when it follows the proxy, and the proxy-wide `enabled`
- `summary`: counts of instance states (`running`, `starting`, `stopping`,
  `stopped`, `failed`, `unknown`), `players`, and `draining` (occupied
  instances of a stopped group still waiting to stop)

Service-group operations allocate loopback ports and register or remove
instance backends without a proxy restart. Instance registration survives
configuration reloads and ends when the proxy restarts.

### Poll an operation

Creation and removal return `202` promptly, with an operation ID and polling
path (also in the `Location` header). Poll that path once per second using
the same authorization header. A `200` poll response has `operation_id`,
`operation`, `target` and `status`:

| `status` | Also includes |
| --- | --- |
| `pending` | Nothing |
| `succeeded` | `result` with the instance details or removal outcome, and `http_status` (`201` for creation, `200` for removal) |
| `failed` | `error` and `http_status`: `409` for port exhaustion or an occupied instance, `404` for unknown groups or instances |

A failed operation is distinct from a failed polling request.
Authentication, authorization, malformed bodies and queue or tracking capacity
errors still reject the submission immediately.

| Limit | Value |
| --- | --- |
| Pending operations | 32 |
| Total records | 256 |
| Retention of completed results | 10 minutes, then polls return `404` |

Capacity exhaustion returns `503` without submitting work. Records are in
memory and do not survive proxy restarts. A pending operation retains its slot
until completion, regardless of HTTP disconnections. Each poll checks current
permissions and the group saved at submission, including after removal.

### Remove an instance

Removal runs asynchronously through the instance's shutdown deadline.

- Persistent instances retain their world and other files; the result has
  `files_removed: false`.
- Disposable and ephemeral instances delete their generated directory after
  the child exits; the result has `files_removed: true`.
- If deletion fails after the child is reaped, the backend is still removed
  and the result includes `files_removed: false` and `cleanup_error`; inspect
  the remaining directory.

Failed provisioning does not register an instance. Template source assets are
read-only inputs, and no EULA acceptance is generated. See
[templates and storage](templates.md#local-asset-templates) for asset
layout, what each storage policy keeps on stop and start, and persistent
directory reuse across proxy restarts.

### Start or stop a whole group

Whole-group requests are audited as `start_group`, `stop_group` and
`group_mode`. They record the group's state and return immediately. Each entry
of `instances` is `{name, result}` with an optional `message`, where `result`
is `starting`, `running` (already up), `stopping`, `stopped` (already
stopped), `draining` (has players; stops once they leave) or `failed`.

Process changes complete asynchronously; poll `/api/servers`, whose entries
carry `group_suspended`, `group_mode` and `draining`. Unknown groups are
`404`, an unknown mode `400`, and creating an instance in a stopped group
fails with `409`.

## Templates, software and files

| Method and path | Permission | What it does |
| --- | --- | --- |
| `GET /api/templates` | `read` | `{templates: [...]}` with names, the groups using them, counts of config layers and plugins, and `software` with its download cache state (`cached`, `resolved_version`, `build`, `size`, `sha256`); never includes asset paths. Unrestricted operators also get `sources`: each git template source with its `git` address, `ref` or `rev`, `commit`, `synced_at` and `synced` |
| `POST /api/templates/{name}/download` | `servers`, scoped | Download (`{}`) or refresh (`{"refresh":true}`) a template's server software ahead of instance creation; returns `202` with an operation to poll |
| `POST /api/template-sources/{name}/sync` | `files`, unrestricted | Fetch a [git template source](templates.md#template-folders-from-git) with `{}` and replace its folder in `template_storage`; returns `202` with an operation to poll (polling needs `servers`) |
| `GET /api/software/{type}/versions` | `servers` | Release names for `paper`, `folia`, `purpur`, `vanilla` or `fabric`, newest first, fetched upstream and cached for 10 minutes |
| `GET /api/files/templates/{path}` | `files`, unrestricted | List a folder in `template_storage` as `{path, entries, truncated}`, or download a file (at most 64 MiB). `?manifest` returns `{files: [{path, size, sha256}], directories}` for the whole tree, which node agents use to fetch only changed files |
| `PUT /api/files/templates/{path}` | `files`, unrestricted | Write the raw request body to a file (at most 64 MiB), creating parent folders; add `?directory` to create a folder instead |
| `DELETE /api/files/templates/{path}` | `files`, unrestricted | Delete a file, or a folder with its contents |
| `GET`/`PUT`/`DELETE /api/files/servers/{name}/{path}` | `files`, scoped | The same operations inside a managed server's directory. For servers on a node they go through its agent and files are limited to 512 KiB |

See [Template and server files](http.md#template-and-server-files) for path
rules, protected files and node tokens.

## Plugins

Plugin routes are for unrestricted operators only.

| Method and path | Permission | What it does |
| --- | --- | --- |
| `GET /api/plugins` | `read` | `{plugins: [...]}`: every [plugin](plugin-operations.md) in dispatch order with `name`, `mode`, `engine`, `version`, `status`, `restarts`, `calls`, `last_error`, `hooks`, `commands`, `capabilities` and `restartable` |
| `GET /api/plugins/{name}` | `read` | One plugin's entry, or `404` |
| `GET /api/plugins/{name}/logs` | `logs` | `{plugin, lines}`: the last 200 log lines of a persistent plugin as `{timestamp_unix_ms, level, message}` |
| `POST /api/plugins/{name}/restart` | `plugins` | Stop a persistent plugin and start a fresh instance with `{}`; returns `202` when accepted. Poll `GET /api/plugins/{name}` for `status` and `restarts` |

## Configuration

| Method and path | Permission | What it does |
| --- | --- | --- |
| `GET /api/config` | `config` | `{source, revision, writable}` for the active configuration |
| `POST /api/config/validate` | `config` | Validate `{source}` including live listener restrictions; does not save or bind sockets |
| `PUT /api/config` | `config` | Validate, atomically save and apply `{source, revision}` |
| `POST /api/reload` | `config` | Reload the selected file, with an empty JSON object `{}` |

Send the opaque revision from the last `GET /api/config` with every save. A
stale revision or a detected external file change returns `409`; fetch or
reload, and review before retrying.

Source size is limited to 256 KiB; the JSON envelope allows escaped source
bytes. `/api/config` contains the complete trusted configuration source. See
[Let the browser save configuration](http.md#let-the-browser-save-configuration)
for what a save does and the file access it needs.

This example uses Python's standard library. Set `RIFT_WEB_TOKEN` to the
configured `web.token` value if needed; the server does not read this
environment variable automatically.

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

## Structured definitions

| Method and path | Permission | What it does |
| --- | --- | --- |
| `GET /api/definitions` | `config` | Structured `templates`, `service_groups`, `permission_groups`, `tab_list`, `server_list` and the active `revision`; includes private paths and executable arguments. Absent settings are `null` |
| `PUT /api/definitions/{section}/{name}` | `config` | Apply `{revision, definition}`; section is `templates`, `service_groups` or `permission_groups`, and `null` removes a definition |
| `PUT /api/definitions/{section}` | `config` | Apply `{revision, definition}` to `tab_list` or `server_list`; `null` turns the setting off |

Definitions use the same fields as Lua configuration. An update replaces one
complete definition, validates all references and live-instance restrictions,
then uses the same save transaction as source edits.

- The structured editor writes a single generated Lua wrapper around the
  original script, preserving its comments, hooks and computed values. Field
  overrides are ordinary Lua source and remain visible in `/api/config`.
- Structured reads return resolved absolute paths; optional fields are
  omitted.
- Templates affect future provisioning; existing instance files are
  preserved. Scaling policies can change while instances run.

Per section:

- **Service groups.** `translation` is `"all"`, `"none"`, `"newer"`,
  `"older"` or a list of
  [version patterns](translation.md#which-versions-a-backend-takes), and
  absent while the group follows the proxy. `translation` at the top level has
  the proxy-wide `enabled`, `clients` and `summary`, every known release as
  `releases` (`[name, protocol]`) and the translated protocol `range`.
- **`tab_list` and `server_list`.** A definition replaces the whole setting
  with the fields of its Lua table; text fields take a string or a chat
  component. File paths can only be set in Lua.
- **Server list `hosts`.** An edit keeps the configured `hosts`, which can
  only be set in Lua, so a definition containing `hosts` is rejected.
- **Server list `favicon`.** An edit keeps the configured `favicon` unless the
  definition sets one: a `data:image/png;base64,...` URL of a 64×64 PNG up to
  16 KiB replaces it, and `false` removes it. Later edits that omit `favicon`
  keep that choice. `GET /api/definitions` returns the favicon as its `data:`
  URL.
- **Permission groups.** Definitions take the fields of `extensions.groups`
  (`inherits`, `permissions`, `weight`, `prefix`, `suffix`, `meta`) and need
  `rift.config.plugins` to be set; edits return `409` otherwise. They change
  configuration, while the dashboard's permission views edit the store
  directly through `/api/perms`.

## Deployments and audit

| Method and path | Permission | What it does |
| --- | --- | --- |
| `GET /api/deployments` | `deploy` | Retained deployment metadata, active revision, retention limit and durability flag; never includes source |
| `POST /api/deployments/{id}/rollback` | `deploy` and `config` | Validate and restore a retained configuration with `{revision}` |
| `GET /api/audit` | `audit` | Latest operator records and durability flag |

See [Keep history and audit records](http.md#keep-history-and-audit-records)
for retention, what a rollback restores and what a record contains.

## Permissions

| Method and path | Permission | What it does |
| --- | --- | --- |
| `/api/perms/...` | `perms`, or `config` to write | The permission store; see the [Permissions HTTP API](permissions-http.md) |
| `GET /api/permissions` | `perms` | Deprecated alias, kept for one release: `{enabled, members, truncated}`: up to 1000 users with global group memberships in the permission store as `{uuid, name, groups}`, with `name` the online or last-seen name |
| `POST /api/permissions/members/{player}/{group}` | `config` | Add a player (UUID, or a known player name) to an existing permission group with `{}`; returns `{changed, revision}` |
| `DELETE /api/permissions/members/{player}/{group}` | `config` | Remove a player from a permission group with `{}`; returns `{changed, revision}` |

The `/api/permissions` membership routes predate `/api/perms` and stay as
aliases for one release. Use `PUT /api/perms/users/{uuid}/nodes` and `DELETE
/api/perms/users/{uuid}/nodes/inherit:{group}` instead. `config` also allows
every read that `perms` allows.

Memberships are global `inherit` nodes in the
[permission store](permissions.md). Changing one creates a permission revision
with actor `operator:<name>` and an operator audit record `group_membership`
with a `uuid/group` target. Everyone belongs to `default` implicitly, so it
can't be assigned.
