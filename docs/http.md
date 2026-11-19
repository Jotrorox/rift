# Web dashboard

Rift can serve three optional HTTP services: a browser dashboard with an API, a
read-only status site and a Prometheus metrics endpoint. This page covers how
to enable them, who may use the dashboard and what it does.

```sh
rift --config examples/admin.lua
```

With this [example](../examples/admin.lua), open <http://127.0.0.1:8080> for
the dashboard and <http://127.0.0.1:9090> for the status site. Rift prints a
[token](#authentication-and-access) for the dashboard to standard error.

| Setting | Serves | Access | Details |
| --- | --- | --- | --- |
| `web` | The dashboard, the `/api` endpoints and Lua `/ext` routes | Bearer token | [HTTP API](http-api.md), [Permissions HTTP API](permissions-http.md), [Lua websites and endpoints](http-lua.md) |
| `status` | The status page, `/status` JSON, `/badge.svg` and `/metrics` | Read-only, no login | [Status page](status-page.md) |
| `metrics` | Prometheus `/metrics` only | Read-only, no login | [Monitoring and logs](monitoring.md) |

The dashboard assets are embedded in the binary; no Node installation or
separate frontend server is needed at runtime.

> [!NOTE]
> `rift admin` uses a different endpoint: a loopback JSON-line protocol with
> its own secret and permissions (see [Admin CLI](admin.md)). Both can run on
> distinct ports, and `admin.permissions` do not govern HTTP requests.

## Configure the services

Add these optional fields to your Lua configuration:

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

A service is disabled when omitted or set to `false`. `web` and `status` also
accept `enabled = false`, which retains the other settings in the source. An
enabled empty table uses the defaults above.

| Field | Controls |
| --- | --- |
| `web.ui` | The dashboard HTML and assets |
| `web.api` | The built-in `/api` endpoints, which the bundled dashboard needs. Lua `/ext` routes remain available with `api = false`, which is useful for custom Lua sites |
| `status.ui` | The status HTML and assets; a table also [styles them](status-page.md) |
| `status.metrics` | The status server's Prometheus endpoint. The status JSON stays available whenever the server is enabled |
| `metrics` | The standalone metrics listener. With `false`, internal traffic counters and authenticated `/api/metrics` remain available |
| `web.tls` | HTTPS on `web.listen`; see [Serve HTTPS](#serve-https) |

## Reload the services

The `web`, `status` and `metrics` settings, their addresses and `web.token`
hot-reload. Rift binds new service sockets before it commits a change, and
never interrupts accepted proxy sessions.

- An unchanged address with port `0` keeps its assigned port. Actual bound
  addresses appear in logs and status JSON.
- Moving a service onto a port still occupied by another Rift service is
  rejected; use a spare port or separate reloads.
- Group and template changes apply live when existing instances retain their
  process definitions and storage policy; remove affected instances before
  changing those settings.
- Gameplay listener names and addresses, the operational `admin` bind, token
  variable and permissions, static `managed_servers` definitions and
  `web.records_directory` require a restart.

Turning off or moving the dashboard server may cut your browser connection;
the save response still finishes. Re-enable it by editing the Lua file and
sending SIGHUP (Unix) or Ctrl-Break (Windows), or restart Rift.

## Serve HTTPS

`web.tls` makes the web listener itself speak HTTPS:

```lua
rift.config.web = {
    listen = "0.0.0.0:8443",
    token_env = "RIFT_WEB_TOKEN",
    tls = true, -- the messaging certificate and key
    -- tls = { certificate = "certs/web.pem", private_key = "certs/web-key.pem" },
}
```

`tls = true` reuses `messaging.certificate` and `messaging.private_key`, which
[node agents](nodes.md) already trust through their `--ca` file, so nodes need
no reverse proxy for templates. The certificate must name the address clients
use, as for the bus. A table names other PEM files (an unencrypted key),
relative to Rift's working directory; a certificate from a public CA suits
browsers better. Both files are read again on every reload, so after renewing
a certificate, reload (SIGHUP) instead of restarting; open connections keep the
old one. A file that cannot be read rejects the reload and keeps the running
listener, and `rift check --strict` loads them too. The listener serves only
HTTPS while `tls` is set: a plain `http://` request fails its handshake. The
status and metrics listeners stay plain HTTP.

## Authentication and access

Every `/api` and `/ext` request requires a bearer token:

```http
Authorization: Bearer your-token
```

| Token | Configured as | Grants |
| --- | --- | --- |
| Administrator | `web.token` or `web.token_env` | Full access, as `admin` |
| Named operator | `web.operators.<name>` | Its `permissions`, within its `groups` |
| Generated | Nothing configured, on a loopback bind | Full access until Rift exits, as `local` |

- A non-loopback bind requires `web.token` (or `web.token_env`) or at least one
  named operator token.
- Tokens contain 16–4096 printable, non-whitespace ASCII characters and must
  be unique. Operator names must be distinct from reserved system/admin audit
  identities.
- Permission and credential changes apply on the next request after reload.
- `GET /api/access` reports the current identity and grants without
  credentials.

On a loopback bind without `web.token` and named operators, Rift generates a
random token at startup and prints it once to standard error:

```text
rift: web token for this run: 3f9c…(64 hex digits)… (enter it under API access; set web.token to choose a permanent one)
```

It stays valid across reloads, and is accepted only with a loopback IP or
`localhost` `Host` header. Loopback alone is not a credential, because any
local process could use it, including plugins on managed servers.

### Named operators

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
| `console` | Send commands with the managed server's console privileges, and message or kick players who are on a server within scope (`/api/players/{player}/…`) |
| `servers` | Start/stop servers and whole groups, switch group modes and create/remove instances within scope |
| `config` | Read, validate, edit and reload complete Lua source and structured definitions. It exposes any secrets in the source and can change credentials |
| `deploy` | View configuration deployment metadata. Restoring a retained deployment also needs `config`, because the restored source replaces the whole configuration, including its tokens and operators |
| `audit` | View operator records; group-scoped operators see only their own records |
| `extensions` | Invoke configured Lua `/ext` handlers |
| `files` | Browse, download, upload, edit and delete files in template folders (unrestricted scope only) and managed server directories within scope |
| `perms` | Read the [permission store](permissions.md) through `/api/perms` and the dashboard's permission views: groups, players, compiled subjects, traces, registered nodes, the change log and exports. Writing needs `config` |
| `plugins` | Restart persistent [plugins](plugin-operations.md) (unrestricted scope only); reading plugin state needs `read`, plugin logs `logs` |

### Group scopes

| `groups` | Server actions and logs are allowed on |
| --- | --- |
| omitted | All groups and static managed servers |
| a list | Instances of those groups |
| an empty list | No server targets |

Scoped listings omit other groups and servers, backend routing and
configuration paths. Global counters and listener information remain visible.
`config`, `deploy`, `extensions`, `plugins` and `perms` require unrestricted
scope, because they affect or expose the whole proxy.

### Tokens from the environment

The Lua source appears in `/api/config`, deployment history and backups. To
keep tokens out of it, name an environment variable instead:

```lua
rift.config.web = {
    listen = "0.0.0.0:8080",
    token_env = "RIFT_WEB_TOKEN",
    operators = {
        observer = { token_env = "RIFT_WEB_TOKEN_OBSERVER", permissions = { "read", "logs" } },
    },
}
```

- `token_env` replaces `token` on the web service or on an operator. Set one
  of the two, and give every token its own variable.
- Rift reads the variable from its own environment; restart it with a new
  value to rotate the token.
- While a variable is unset or holds no valid token, that token is refused
  and none is generated in its place. Rift says so at startup and on reload,
  and `rift check --strict` verifies that each variable is set.
- Like Rift's [other secrets](hardening.md), the variables are removed from
  the environment of every managed server.
- With the supplied systemd unit, put them in the root-owned
  `EnvironmentFile`.

### Browser and network protection

- The HTML shell and static assets contain no private configuration and load
  without authentication. The dashboard holds the token in memory and forgets
  it on page reload.
- API responses never include a separate token field, but `/api/config`
  returns the Lua source with any credentials it contains. Treat the source
  as administrative data.
- Requests with mismatched `Origin` headers or cross-site Fetch Metadata are
  rejected, and mutations require JSON. There is no CORS allowance or
  cookie-based login.
- For remote administration, use HTTPS: [built in](#serve-https), at a
  trusted reverse proxy, or through an SSH tunnel. When proxying, preserve the
  browser's `Host` header and use a token.

The status and metrics servers are unauthenticated; see
[what the status site reveals](status-page.md#choose-what-the-site-reveals).

## What the dashboard can do

| Sidebar section | What you can do |
| --- | --- |
| Overview, Network | See listeners, backends, routes and live counters; edit the server list and tab list |
| Servers | See lifecycle state, start and stop servers, create instances from service groups, remove them with explicit file retention/deletion labels, follow logs, send console commands and manage [files](#template-and-server-files) |
| Configuration | Edit, validate, save and apply the complete Lua source, and reload edits made on disk; edit groups and templates field by field; restart plugins; exercise `/ext` methods, paths and request bodies |
| Permissions | Edit permission groups, players and nodes, and read their change history |
| Activity | Browse configuration deployment history, roll back and read operator audit records |

The dashboard polls active state and preserves unsaved editor changes when
another client applies a revision. It follows a selected server log every
second in a bounded 128 KiB view. It polls through long operations, displays
failures and cleanup warnings, and identifies unresolved operations if polling
fails.

## Let the browser save configuration

A save needs write access to the configuration file and its parent directory.
The supplied [systemd and Compose examples](deployment.md) intentionally keep
configuration read-only: their dashboards can inspect and validate the source
and reload external edits, but cannot save. To enable browser saves:

- **systemd**: provision a dedicated writable configuration directory for the
  service account and allow it through `ProtectSystem` with `ReadWritePaths`.
- **Compose**: use a writable directory mount.
- Keep the CLI token environment file outside a service-writable
  configuration directory.

A save runs in this order:

1. Rift validates the complete candidate and reserves changed service sockets.
2. It writes a new sibling file, preserves the original access permissions,
   flushes it and atomically replaces the selected file.
3. It applies the new runtime. Changed routes affect new connections; existing
   sessions keep their sockets and backend attachment.

Invalid configuration, socket collisions, stale revisions and write failures
retain the previous file, runtime and services. A rejected reload preserves
the runtime too, but does not undo an edit already written by an external
editor.

- A save preserves the source exactly, including comments, functions, computed
  values and formatting. There is no generated JSON override file.
- The selected path is canonicalized at startup, so a symlink selects its
  target.
- Changes on disk are applied only by an explicit reload or signal, not by a
  file watcher.
- Saves and signal, HTTP API and `rift admin reload` requests share one
  serialized transaction path with a bounded queue, outside gameplay I/O.

> [!WARNING]
> External file conflict checks are best-effort: an editor writing during the
> final check-and-rename interval can race a save. Use the API for competing
> automated writers, or finish and reload an external edit before editing in
> the dashboard. File replacement is atomic, but does not promise power-loss
> durability of the directory entry.

## Keep history and audit records

Records default to a private `<config filename>.operators` directory beside
the configuration. Set `web.records_directory` to writable storage when the
configuration mount is read-only.

- Unix directories use mode `0700`, files `0600`. Retained deployment sources
  may contain credentials; protect and back up the directory as
  administrative data.
- With no file-backed configuration, or an unwritable default directory,
  records stay in memory and the APIs report `durable: false`. An explicitly
  configured unwritable records directory fails startup.
- A post-commit history write failure is reported in Rift's stderr; the
  already-applied configuration remains active.

### Deployment history and rollback

Deployment history retains the latest 64 successful source configurations,
including startup, HTTP saves, CLI/signal reloads and rollbacks.

A rollback checks the supplied active revision and external disk edits,
validates the candidate, reserves sockets, then atomically saves and applies
it. Rejection preserves the working file and runtime. Restoring creates a new
history entry.

A restore uses the retained Lua source and the current local module and
plugin files. It does not restore asset contents, executable files, runtime
instances or worlds.

### Audit records

HTTP mutations record the named actor, operation, target, timestamp and
admission result. Rift also records:

- player messages and kicks, as `message_player` and `kick_player` with the
  player as the target, without their text;
- changing requests on the [message bus](messaging-subjects.md#player-api),
  by `bus:<login>` for a bus login, `plugin:<name>` for a persistent plugin,
  and `rift.control` or `rift.players` for the configuration script;
- scope denials, local CLI commands and signal reload results.

Requests without a valid token are summarized: the first in each minute
writes an `authentication` record by `anonymous` whose `count` is the number
of such requests since the previous one. `GET /api/audit` reports those not
yet written as `unrecorded_denials`. Requests to the local administrator port
without its token are summarized the same way.

- The API retains the latest 1024 audit records; the journal compacts at
  8 MiB. The latest 64 denial summaries are kept apart, so a flood of bad
  requests cannot push operator actions out.
- Bodies, console text, authorization headers and query strings are excluded.
- A lifecycle `accepted` means queued; follow server state for completion.
- An intent record precedes each HTTP or CLI mutation, and the operation is
  refused if the intent cannot be written. A timeout can leave an intent
  without a final result.
- These are local records, not a tamper-proof external audit log.

### In a cluster

On a [cluster](clustering.md) peer, deployment history and operator audit
records are replicated, so `GET /api/deployments`, rollbacks and
`GET /api/audit` give the same answer on every peer (`"replicated": true`):

- `PUT /api/config`, structured edits, rollbacks and a reload of an edited
  `rift.lua` are deployments: Rift evaluates the source as every peer would,
  refuses it with `409` if a shared section depends on `rift.peer` (the
  message names the peer and section), commits it, applies it on the peer
  that took the request and writes it to every other peer's `rift.lua`,
  which then reloads. A peer whose reload fails keeps its configuration and
  shows the reason in `GET /api/cluster` under `configuration`.
- Unauthenticated request summaries stay with each peer.
- `rift cluster init` moves the records directory's `deployments.json` and
  `audit.jsonl` into the cluster; afterwards the files are no longer
  written.

## Template and server files

The Files page and `/api/files` serve two kinds of folders, like CloudNet's
template storage: `template_storage` (default `local/templates` beside the
configuration) and each managed server's working directory. Point template
`configs` layers at folders under `template_storage` to edit them from the
dashboard; see [Templates and server software](templates.md).

- Paths are relative to the selected folder. `.`/`..` segments, backslashes,
  colons, control characters and symlinks anywhere below the root are
  rejected.
- Rift-owned `.rift-*` files (such as the instance ownership marker) cannot be
  written or deleted.
- Uploads are written to a temporary file and renamed into place. Downloads
  are sent as `application/octet-stream` attachments. Both are limited to
  64 MiB, also for servers on a [node](nodes.md), where files over 512 KiB
  cross the message bus in parts. A download fails with `409` if the file
  changes between parts.
- Folder listings return at most 2000 entries.
- Writes to a running server's directory take effect when the server reads
  them, usually on reload or restart.
- File mutations are audited as `PUT files` and `DELETE files`.

> [!WARNING]
> Writing a server's plugins or configuration runs code with that server's
> operating-system identity. Grant `files` only to operators you would also
> trust with `console`.

[Node agents](nodes.md) authenticate to `GET /api/files/templates` with their
node token (`rift.config.nodes.<name>.token_env`) to mirror template folders.
That token can only read template files; every other request, and every
write, is refused with `403`. Node reads are not audited, like other reads.

On a [cluster](clustering.md#templates-and-other-files) peer, template writes
and deletes change the replicated template manifest: the response comes once
a majority of voting peers hold the uploaded file and the change is
committed, and every peer then writes the same `template_storage`. A write
fails with `500` while fewer than a majority of voters are reachable.
Top-level names starting with a dot are refused. Reads (and node agents)
use the peer's own copy, so any peer serves the same templates.

## Limits

Each HTTP server has these limits. Gameplay admission and Lua routing capacity
remain independent of them.

| Limit | Value |
| --- | --- |
| Open connections | 64 |
| Slowest accepted socket | 256 KiB of headers, request body and response in any 10-second window |
| Socket lifetime | One hour |
| Time for a retiring server to finish responses | Three seconds |

Idle and trickling connections therefore last at most ten seconds, while a
file upload or import on a slow link (from about 26 KiB/s) can finish. Each
request must finish within ten seconds, except `/api/files/` transfers, which
have two minutes.

## Development checks

- `cargo test --locked --all-targets` and
  `cargo clippy --locked --all-targets -- -D warnings` cover the Rust side,
  including real-socket HTTP and proxy integration tests.
- `node src/web_assets/tests.js` runs the frontend behavior harness: API
  interactions and editor state transitions, not browser rendering. It uses
  only Node's standard library, and nothing else in Rift needs Node.

CI runs both suites.
