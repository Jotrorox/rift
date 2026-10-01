# Web administration, status and Lua HTTP extensions

Run `rift --config examples/admin.lua`, then open <http://127.0.0.1:8080>.
The admin website shows listeners, backends, routes and live counters, edits the
complete Lua source, validates changes, saves and applies them, and reloads edits
made on disk. Its managed servers panel reports lifecycle state and requests
server starts and stops, creates instances from service groups, and removes
instances with explicit file retention/deletion labels. It also provides live server
logs and console commands, field-based group/template editing, configuration
deployment history, rollback and operator audit records. The status website runs separately at <http://127.0.0.1:9090>.
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
`status.ui` controls the status HTML/assets, and `status.metrics` controls its
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

## Authentication and access

Loopback web binds may omit `token` for local development. A non-loopback bind
requires `web.token` or at least one named operator token. Tokens contain 16–4096
printable, non-whitespace ASCII characters. When credentials are configured,
every `/api` and `/ext` request requires:

```http
Authorization: Bearer your-token
```

The HTML shell and static assets contain no private configuration and load
without authentication. Enter the token in the website's connection form; it is
held in memory and forgotten on page reload. API responses never include a
separate token field, but authenticated `/api/config` returns the original Lua
source, including any credentials it contains. The source is administrative data.

Tokenless access accepts only loopback IP or `localhost` Host headers. Requests
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

Omitting `groups` permits all groups and static managed servers. A list restricts
server actions and logs to instances of those groups; an empty list permits no
server targets. Scoped listings omit other groups/servers, backend routing and
configuration paths. Global counters and listener information remain visible.
`config`, `deploy` and `extensions` require unrestricted scope because Lua and
configuration changes can affect the whole proxy. `config` exposes any secrets
in the Lua source and can change credentials. The legacy `web.token` and
credential-free loopback access retain full privileges. Once any named operator
is configured, loopback requests also require a valid token. Tokens must be
unique, names must be distinct from reserved system/admin audit identities,
and permission/credential changes apply on the next request after reload.
`GET /api/access` reports the current identity and grants without credentials.

The separate status and metrics servers are read-only and unauthenticated. Their
JSON includes backend/listener addresses, health eligibility, counters and
service settings, but no Lua source, token or configuration filesystem path.
Bind these servers to the monitoring interface appropriate for that information.
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
| `GET /api/servers` | `{servers: [...]}` with managed server lifecycle state and usage |
| `POST /api/servers/{name}/start` | Request a managed server start with `{}`; returns `202` when accepted |
| `POST /api/servers/{name}/stop` | Request an unused managed server stop and pause automatic wake with `{}`; returns `202` when accepted |
| `GET /api/servers/{name}/logs?cursor={byte_offset}` | Up to 64 KiB of log text with the next byte cursor, `truncated` and `has_more`; omit cursor to tail the log |
| `POST /api/servers/{name}/console` | Write one `{command: "list"}` to a running managed server; commands are limited to 4096 bytes without control characters |
| `GET /api/definitions` | Structured `templates`, `service_groups` and the active `revision`; includes private paths and executable arguments |
| `PUT /api/definitions/{section}/{name}` | Apply `{revision, definition}`; section is `templates` or `service_groups`, and `null` removes a definition |
| `GET /api/deployments` | Retained deployment metadata, active revision, retention limit and durability flag; never includes source |
| `POST /api/deployments/{id}/rollback` | Validate and restore a retained configuration with `{revision}` |
| `GET /api/audit` | Latest operator records and durability flag |
| `GET /api/groups` | `{groups: [...]}` with group names, inclusive port ranges, instance names, template and storage policy |
| `POST /api/groups/{name}/instances` | Provision and register an instance with `{}`; returns `201` with instance details (autostart follows group policy) |
| `DELETE /api/instances/{name}` | Stop and deregister an unused instance with `{}`; returns `200` with `{name, removed, files_removed}` after child cleanup |
| `GET /api/metrics` | Counter values as JSON |
| `GET /api/config` | `{source, revision, writable}` for the active configuration |
| `POST /api/config/validate` | Validate `{source}` including live listener restrictions; does not save or bind sockets |
| `PUT /api/config` | Validate, atomically save and apply `{source, revision}` |
| `POST /api/reload` | Reload the selected file, with an empty JSON object `{}` |
| `GET /status` | Public JSON on the separate status server |
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
backends without a proxy restart. Port exhaustion or an occupied instance
returns `409`; unknown groups/instances return `404`. Instance registration
survives configuration reloads and ends when the proxy restarts. Instance
removal waits for its shutdown deadline. Persistent instances retain their world
and other files; disposable instances delete their generated directory after
the child exits. Stop/start preserves files for both storage policies. Successful
file deletion returns `files_removed: true`; persistent removal returns `false`.
If deletion fails after the child is reaped, the backend is still removed and
the response includes `files_removed: false` and `cleanup_error`; inspect the
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
result. Authentication/scope denials, local CLI commands and signal reload results
are also recorded. Bodies, console text, authorization headers and query strings
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
`?`), UTF-8 `body`, lowercase `headers`, and `context` (the public status object
as a Lua table). Authorization, cookie and proxy authorization headers are
removed. JSON arrays become one-based Lua arrays. The context is an isolated
snapshot; modifying it cannot change the live runtime.

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
