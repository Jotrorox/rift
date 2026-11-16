# Lua websites and endpoints

The optional `on_http` callback in your Lua configuration answers HTTP
requests under `/ext` on the [web service](http.md). Use it for network
dashboards, maintenance information, deployment metadata, diagnostic pages and
application-specific API responses.

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

- The callback handles every HTTP method at `/ext` and `/ext/*`. Paths outside
  this namespace remain owned by Rift.
- `/ext` has the same bearer authentication and browser-origin protections as
  the [HTTP API](http-api.md). A caller needs the `extensions` permission or a
  full-access token; see
  [Authentication and access](http.md#authentication-and-access).
- The routes remain available with `web.api = false`.
- Hook and source changes become visible on the next successful reload.
- The dashboard's extension console can exercise methods, paths and request
  bodies without leaving the dashboard.

## Read the request

The request table contains:

| Field | Content |
| --- | --- |
| `method`, `path` | As sent |
| `query` | The raw query string without `?` |
| `body` | UTF-8 text |
| `headers` | Lowercase names. Authorization, cookie and proxy authorization headers are removed |
| `context` | The status object with operator detail, as a Lua table |

`status.exposure` and hidden `status.backends` do not apply to `context`, and
each backend has `display` and `players`. It omits managed lifecycle details.
JSON arrays become one-based Lua arrays. The context is an isolated snapshot;
modifying it cannot change the live runtime.

With [`rift.config.plugins`](players-api.md) set, the handler can
also list, message, kick and transfer players through `rift.players`.

## Check the caller

Rift requires only `extensions` for `/ext`, so a handler that changes anything
must check the caller itself. `context.operator` is the authenticated caller,
in the shape of `GET /api/access`:

| Field | Value |
| --- | --- |
| `name` | `admin` for `web.token`, `local` for the generated loopback token, otherwise the `web.operators` key |
| `permissions` | Absent (`nil`; JSON `null` for plugins) for full access, otherwise an array of permission names |
| `groups` | Absent when the caller is not limited to service groups. `extensions` cannot be combined with `groups`, so it is absent for every caller that reaches the handler |

```lua
local function allows(operator, permission)
    if operator.permissions == nil then return true end -- Full access.
    for _, held in ipairs(operator.permissions) do
        if held == permission then return true end
    end
    return false
end

on_http = function(request)
    if request.method == "POST" and request.path == "/ext/announce" then
        if not allows(request.context.operator, "console") then
            return { status = 403, body = "console permission required" }
        end
        -- ...
    end
end,
```

## Return a response

Return `nil` for a `404`, or
`{body = "...", status = 200, content_type = "text/plain"}`. Only `body` is
required.

- `status` must be an integer from 200 to 599; 204, 205 and 304 require an
  empty body.
- The MIME value is validated, and arbitrary response headers are
  unavailable.

> [!WARNING]
> HTML from a handler inherits the service's Content Security Policy: external
> resources and inline scripts and styles are blocked. Escape any
> request-derived text before you put it into HTML.

## Limits

HTTP hooks reuse Rift's restricted Lua VM, with a fresh state per invocation.

| Limit | Value |
| --- | --- |
| Request and response bodies | 256 KiB each |
| Memory | 8 MiB |
| Instruction budget | 100,000 |
| Deadline | 50 ms |
| HTTP script workers | Four, shared across reloads, independently of routing hook capacity |

Lua execution errors return `500`, busy Lua capacity `503` and an async Lua
deadline `504`.

Scripts can import local modules and folder plugins from their saved snapshot;
see the [Lua configuration](lua.md). They have no filesystem, sockets,
dynamic or native module loading, OS or debug access. They cannot call
arbitrary URLs or mutate the running configuration; configuration writes use
the revision-checked [HTTP API](http-api.md#configuration).

## Run handlers from Rust

For Rust integrations, `rift::http_script::{HttpScript, HttpRequest,
HttpResponse, HttpError}` provides an owned, thread-safe interface.

- Obtain the script from `Config::from_lua(...).on_http`.
- Async hosts call `script.execute(request).await`.
- The synchronous `evaluate` entry point accepts an explicit deadline and is
  intended for callers that own their worker policy.
