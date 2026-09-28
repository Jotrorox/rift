# Web administration, status and Lua HTTP extensions

Run `rift --config examples/admin.lua`, then open <http://127.0.0.1:8080>.
The admin website shows listeners, backends, routes and live counters, edits the
complete Lua source, validates changes, saves and applies them, and reloads edits
made on disk. The status website runs separately at <http://127.0.0.1:9090>.
HTML, CSS and JavaScript are bundled with `include_str!`: no asset directory,
Node, package manager, CDN or separate frontend server is needed at runtime.
Axum is the only additional direct Rust dependency, with its `http1`, `json` and
`tokio` features enabled and default features disabled.

This guide covers `web` and `status` HTTP services. The operational `admin`
endpoint used by `rift admin` is a separate loopback JSON-line protocol with an
environment-provided secret and explicit operation permissions; see the
[operator guide](operations.md#enable-operational-administration). Both can run
on distinct ports. `admin.permissions` do not govern HTTP requests: the web
credential grants the enabled HTTP API capabilities, including configuration
editing. Changes to the operational `admin` configuration require restart.

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
requires a token of 16–4096 printable, non-whitespace ASCII characters. When a
token is configured, every `/api` and `/ext` request requires:

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
| `GET /api/status` | Runtime version, uptime, revision, actual listeners, backends, health, routes, fallbacks, limits, counters and enabled services |
| `GET /api/metrics` | Counter values as JSON |
| `GET /api/config` | `{source, revision, writable}` for the active configuration |
| `POST /api/config/validate` | Validate `{source}` including live listener restrictions; does not save or bind sockets |
| `PUT /api/config` | Validate, atomically save and apply `{source, revision}` |
| `POST /api/reload` | Reload the selected file, with an empty JSON object `{}` |
| `GET /status` | Public JSON on the separate status server |
| `GET /metrics` | Prometheus text on the status server when enabled, or the standalone metrics server |

Configuration saves preserve the source exactly, including comments, functions,
computed values and formatting. The source is the single editable configuration;
there is no generated JSON override file. Send the opaque revision from the last
`GET /api/config` with every save. A stale revision or a detected external file
change returns `409`; fetch/reload and review before retrying. Changes on disk
are applied only by an explicit reload or signal, not by a file watcher. The
website polls active state and preserves unsaved editor changes when another
client applies a revision.

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
Scripts have no filesystem, sockets, module loading, OS or debug access. They
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
