# Operating a Rift release

Releases provide standalone executables for Linux x86_64, macOS ARM64 and Windows
x86_64. Run `rift --license` to view the embedded licenses. Documentation and
examples are available in the repository. The [README](../README.md) covers
protocol support and backend configuration; this guide covers daily administration.

## First start

Download the executable for your OS/CPU. On Linux, rename it and make it executable:

```sh
mv rift-linux-x86_64 rift
chmod +x rift
./rift --version
./rift init rift.lua
# Edit listener and backend addresses for your server.
./rift check rift.lua
./rift --config rift.lua
```

`rift init [path]` writes a commented configuration (default `./rift.lua`) and
refuses to overwrite an existing file. `rift check [path]` evaluates and validates
it without opening listeners or contacting backends; the older `--check path`
spelling also works. Correct the field/path named by any error and rerun the
check before starting. Configuration scripts are trusted local code.

On macOS, rename `rift-macos-aarch64` to `rift`, run `chmod +x rift`, then use the
same commands. On Windows, rename `rift-windows-x86_64.exe` to `rift.exe` and
run it with `.\rift.exe`.
`--version` identifies the Cargo package version; nightly builds may share that
version, so retain the release tag when reporting issues.

The generated configuration forwards port 25565 to loopback port 25566.
Configure that backend as described in the README. For public Paper networks,
enable online authentication and Velocity modern forwarding using
[`examples/online.lua`](../examples/online.lua). Rift verifies accounts; private
Paper backends run `online-mode=false` with `proxies.velocity.online-mode=true`.
The generated starter retains unauthenticated offline behavior until these
settings are enabled.
[`examples/network.lua`](../examples/network.lua) adds hostname routing, fallback,
admission controls, health probes, status caching and metrics. Adapt its hostnames
and backend addresses before use.

Look for `rift: listening on ...`, then connect a Minecraft client through the
public address. With metrics enabled,
`curl --fail http://127.0.0.1:9090/metrics` should succeed. A successful config
check does not establish backend reachability or application readiness.

## Enable operational administration

Set these fields through `rift.config`, `rift.setup({ ... })`, or a legacy
returned Lua configuration table before starting:

```lua
admin = {
    listen = "127.0.0.1:9091",
    token_env = "RIFT_ADMIN_TOKEN",
    permissions = { "status", "maintenance", "drain", "transfer", "reload", "shutdown" },
},
maintenance = false,
draining = {}, -- For example, { "survival" } blocks new attachments to survival.
metrics = "127.0.0.1:9090",
login_rate_limit = {
    per_ip_per_second = 20,
    per_ip_burst = 40,
    global_per_second = 200,
    global_burst = 400,
    max_ips = 65536,
},
```

Set `RIFT_ADMIN_TOKEN` to a randomly generated secret of 32–1024 bytes without control characters
in both the server and the administrator's environment. For a local shell:

```sh
export RIFT_ADMIN_TOKEN="$(openssl rand -hex 32)"
./rift check rift.lua
./rift --config rift.lua
# In a separate shell with the same secret:
./rift admin status
```

The `admin` endpoint speaks JSON lines over TCP, accepts only loopback bind addresses
and requires the token on every request. Permissions are explicit: remove operations a credential must not
perform, or use `{ "status" }` for a read-only endpoint. Permission and credential
changes require a restart. There is one configured credential and permission set,
not a multi-user identity store. `rift check` validates the configuration shape;
the server verifies the environment secret when it starts. Keep secrets out of
Lua files, command arguments and committed Compose files.

This endpoint is separate from the optional `web` HTTP dashboard/API and its
bearer token. They can run together on distinct ports. See
[HTTP administration](#http-administration) for browser-based configuration
editing and the separate read-only status website.

The CLI always reads its secret from `RIFT_ADMIN_TOKEN`, even if the server uses
a different `token_env` variable. It defaults to `127.0.0.1:9091`; use
`rift admin --address 127.0.0.1:9092 status` for another loopback port.
`status` reports connected players, their login names and connection IDs, backend state and
maintenance state. Commands fail with an actionable error when authentication,
permission checks or validation fail. Use the local CLI through SSH for remote
administration.

Connection `rate_limit` runs before handshake parsing. `login_rate_limit` runs
at login admission and has the same token-bucket fields and defaults; it allows
status requests without spending login tokens. Both are opt-in, apply globally
and per IP, and leave established traffic untouched. Clients sharing a NAT also
share an IP allowance. A routing hook can enforce additional IP/listener policy.
Online mode verifies player names before network access checks; offline-mode
names are unauthenticated and cannot establish account ownership.

## Inspect health and failed connections

Watch player counts, backend health, login latency, error growth and memory
alongside the process/service status. Prometheus metrics are served at
`GET /metrics` on the configured metrics listener. That listener has no
authentication; use loopback or a trusted monitoring network.

| Need | What to inspect |
| --- | --- |
| Player load | `rift_players_online`, `rift_backend_players_online`, `rift_connections_active` |
| Backend availability | `rift_backend_up{backend="name"}`, admin `status`, probe failures |
| Maintenance | `rift_maintenance_mode`, `rift_backend_draining{backend="name"}` |
| Login performance | `rift_login_duration_seconds` histogram together with login/connection errors |
| Connection failures | `rift_connection_errors_total`, `rift_backend_connect_failures_total`, structured events |
| Capacity/access pressure | `rift_connections_rate_limited_total`, `rift_logins_rate_limited_total`, `rift_connections_access_rejected_total` |
| Memory | `rift_process_resident_memory_bytes` and `rift_process_resident_memory_available` on Linux |
| Changes | `rift_reloads_total`, `rift_reload_failures_total`, `rift_player_transfers_total`, `rift_player_transfer_failures_total` |

A backend's TCP health indicates reachability, not a successful Minecraft login.
Backends initially remain eligible before the first probe and always report up
when probing is disabled. Check draining state separately: a reachable backend
can intentionally refuse new attachments. Counters survive reloads and reset
when the process restarts. Linux process RSS is not a container memory limit or
an estimate of memory per player. Login duration measures TCP acceptance through
first settled play, including routing and handshake; unsuccessful logins do not
contribute to the successful-login histogram. The resident-byte gauge is omitted
when availability is zero.

Connection failures produce JSON lines on stderr, with `connection_id`, `backend`,
`backend_address`, `stage`, `failure`, `error_kind`, `duration_ms` and a readable
`message`. Backend fields are null if failure occurred before route selection.
Startup, reload and shutdown messages remain plain text. For a systemd service:

```sh
sudo journalctl -u rift --since '10 minutes ago' -o cat --no-pager \
  | jq -R 'fromjson? | select(.event == "connection_failed" or .event == "connection_rejected")'
```

Use `connection_id` to join related events within one process. A
`backend_attempt_failed` can be followed by successful fallback; it does not
alone mean the player disconnected. `admin_command` events record operation
results without logging credentials.

| Error/stage | Operator action |
| --- | --- |
| Config filename, field or backend reference | Fix the named item and run `rift check` again |
| Listener bind failure | Check the IP belongs to this host and the port is free |
| `dns` / `dns_error` | Resolve the configured backend name from the Rift host/container |
| `connect` / `connect_error` | Check backend listener, firewall and backend address/port |
| `connect` / `no_eligible_backend` | Check probes, fallback configuration and draining state |
| `handshake` / `handshake_error` | Check client protocol, accidental health probes and connection deadlines |
| `on_route` / script error or overload | Inspect the named script; simplify/bound the hook or reduce arrival rate |
| Session/login failure | Check supported versions, authentication-service access, matching Velocity secrets on every Paper backend and the backend's own log |
| Rate-limit/capacity rejection | Check legitimate player traffic and NAT concentration before changing limits |
| Admin permission/authentication failure | Check the CLI secret and the configured operation permission |

## Linux systemd service

The supplied [unit](../examples/rift.service) uses an unprivileged `rift` account
and root-owned configuration. Run these commands from a repository checkout on
a systemd host, with the downloaded executable saved as `rift` and a generated
`rift.lua`. Create the account only if it does not exist:

```sh
sudo useradd --system --user-group --no-create-home --shell /usr/sbin/nologin rift
sudo install -m 0755 rift /usr/local/bin/rift
sudo install -d -o root -g rift -m 0750 /etc/rift
sudo install -o root -g rift -m 0640 rift.lua /etc/rift/rift.lua
sudoedit /etc/rift/rift.lua
sudo -u rift /usr/local/bin/rift check /etc/rift/rift.lua
sudo install -m 0644 examples/rift.service /etc/systemd/system/rift.service
sudo systemctl daemon-reload
sudo systemctl enable --now rift
sudo systemctl status rift
sudo journalctl -u rift -n 50 --no-pager
```

If administration is enabled, create `/etc/rift/admin.env` before starting,
owned by root with mode 0600, containing `RIFT_ADMIN_TOKEN=<your-secret>`.
The unit's optional `EnvironmentFile` loads it without making the secret part
of the process command line. Supply the same secret in your CLI environment.

The unit validates before starting, restarts on failures with a five-second
delay, and caps restart bursts. An explicit stop stays stopped. Its file limit
is 8192; each active session needs client/backend sockets plus runtime headroom.
If you raise `max_connections`, size both file descriptors and memory for it.
The service needs no writable data directory. LuaJIT uses executable memory,
so do not add `MemoryDenyWriteExecute=true` to this unit.

## Container example

The supplied [Dockerfile](../examples/Dockerfile) packages the Linux release
binary using the release runner's Ubuntu 24.04 runtime baseline. The
[Compose example](../examples/compose.yaml) runs as UID/GID 65532 with a read-only
filesystem, no Linux capabilities, a read-only config directory and a 40-second
stop deadline. Download the Linux x86_64 executable as `rift` into a repository
checkout, make it executable with `chmod +x rift`, then run from that checkout:

```sh
mkdir config
./rift init config/rift.lua
# Edit config/rift.lua; make it readable by container UID 65532.
./rift check config/rift.lua
export RIFT_IMAGE=rift:my-release
# Set RIFT_ADMIN_TOKEN here too if administration is enabled.
docker compose -f examples/compose.yaml build
docker compose -f examples/compose.yaml run --rm rift check /etc/rift/rift.lua
docker compose -f examples/compose.yaml up -d
docker compose -f examples/compose.yaml logs --tail=50 rift
```

The image build runs `rift --version` to detect a wrong architecture or
incompatible runtime before startup. For a source build, set
`RIFT_BINARY=target/release/rift`; ensure its architecture/libc match the image.
The example fixes `platform: linux/amd64` to match the released Linux executable.

Loopback in a container refers to that container. Use private, reachable backend
addresses or service DNS names on a shared container network. To expose metrics
through the example's host-loopback port mapping, set
`metrics = "0.0.0.0:9090"` inside the container. Keep admin on
`127.0.0.1:9091` and run commands inside it:

```sh
docker compose -f examples/compose.yaml exec rift rift admin status
# After editing and validating the host config:
docker compose -f examples/compose.yaml exec rift rift check /etc/rift/rift.lua
docker compose -f examples/compose.yaml kill -s SIGHUP rift
docker compose -f examples/compose.yaml logs --tail=50 rift
docker compose -f examples/compose.yaml stop
```

Compose passes the server token into the container; `exec` uses that environment.
A directory bind mount lets atomic configuration-file replacements become visible
inside the container. Increase `stop_grace_period` if you increase
`shutdown_timeout_ms`. The exec-form entrypoint receives SIGTERM directly; the
[Dockerfile reference](https://docs.docker.com/reference/dockerfile/#entrypoint)
and [Compose service reference](https://docs.docker.com/reference/compose-file/services/#stop_grace_period)
describe these lifecycle settings.

## Reload configuration

Save a candidate in the same directory, validate it as the service user, then
replace the live file atomically. Keep a backup for rollback:

```sh
sudo cp -p /etc/rift/rift.lua /etc/rift/rift.lua.previous
sudo cp -p /etc/rift/rift.lua /etc/rift/rift.lua.next
sudoedit /etc/rift/rift.lua.next
sudo -u rift /usr/local/bin/rift check /etc/rift/rift.lua.next
# Continue only after validation succeeds.
sudo mv /etc/rift/rift.lua.next /etc/rift/rift.lua
sudo systemctl reload rift
sudo journalctl -u rift --since '1 minute ago' --no-pager
```

`ExecReload` checks the file, then sends SIGHUP. Signal delivery is asynchronous:
**a successful `systemctl reload` does not prove Rift accepted the reload**.
Confirm `rift: configuration reloaded` or an increase in `rift_reloads_total`.
When configured, `rift admin reload` offers the same validated reload operation.
On rejection, the old snapshot remains active; inspect the error, restore the
backup or fix the file, and reload again. `rift check` validates a candidate in
isolation and cannot detect changes incompatible with an already running process.

| Change | Reload behavior |
| --- | --- |
| Routes, backend addresses, fallback, hooks | New connections use the new snapshot; existing sessions keep their sockets and routing |
| Connection/login limits, configured maintenance/draining | Apply to new admission/attachments; current sessions continue |
| Health probes, status cache | Rebuilt after successful reload |
| Shutdown deadline | New value applies to the later shutdown drain |
| `web`, `status`, standalone `metrics`, web bearer token | Can enable, disable or move live; replacement sockets bind before commit |
| Gameplay listeners, operational `admin` bind/token variable/permissions | Restart required; incompatible reload rejects the entire candidate |

Already accepted connections keep their original configuration snapshot while
initial login completes. A route edit never transfers a connected player.
Explicit administrator transfers are separate operations. Runtime maintenance
and backend-drain overrides survive reloads until another admin command changes
them or the process restarts. `off` is itself an override, not a reset to the
configured value. Keep persistent policy in the Lua file. Lowering `max_connections` preserves
sessions and rejects new admission until the total falls below the new limit.

Outside systemd, use `kill -HUP <pid>` on Unix or Ctrl-Break on Windows.

## Maintenance and transfers

With the relevant admin permissions, block new logins and drain a backend:

```sh
rift admin maintenance on
rift admin drain survival on
rift admin status
# Use an online player's connection ID from status and a configured backend name.
rift admin transfer 42 lobby
rift admin status
```

Maintenance leaves existing players connected and still permits server-list
status. Backend draining preserves players already attached, prevents new
attachments to that backend (including fallback and explicit transfers), and
allows eligible configured fallbacks to accept new players. Wait for the backend
player count to reach zero or transfer supported clients before stopping it.

Transfers retain the client socket on Minecraft 1.8.9–26.3 (50 explicitly mapped release protocols) while
attaching the target backend and checking the same UUID/name. Completion requires
the replacement world's Join Game, with a 30-second operation cap. Other client
versions and invalid targets are rejected without moving the player. The target
must permit the player under both the session's original access rules and the
current administrator-request configuration. A denied or failed replacement
login leaves the original backend attached. If a transfer fails after the client
transition begins, Rift disconnects the player to avoid continuing a damaged
session. Transfer success does not move inventories between independent servers.

With a [network configuration](../examples/online.lua), a
backend transport failure tries the configured hubs and fallbacks. Access rules,
backend bans and draining still apply; explicit backend kicks are terminal.

After backend work, inspect its health and reopen admission:

```sh
rift admin drain survival off
rift admin maintenance off
rift admin status
```

These runtime controls are useful for a maintenance window. To preserve policy
across process restarts, change `maintenance`/`draining` in the configuration and
validate/reload it as well. A graceful full-process stop is available through
`rift admin shutdown`, `systemctl stop rift`, Compose `stop`, or Ctrl-C.

## Shutdown, upgrade and rollback

SIGTERM stops admission and drains sessions for `shutdown_timeout_ms` (30 seconds
by default). Players still connected at the deadline are disconnected. A second
SIGTERM/SIGINT forces remaining sessions closed immediately. Look for
`rift: draining ...` followed by `rift: shutdown complete`;
`shutdown deadline reached` means the drain expired. Metrics remain available
during the drain. **Keep the supervisor stop deadline longer than Rift's drain**:
40 seconds in the examples. Apply systemd edits with `systemctl daemon-reload`.
See systemd's [`ExecReload` and `TimeoutStopSec` documentation](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html).

Download the new executable into a separate directory, rename it to `rift` and
run `chmod +x rift`. Record both release tags and retain the previous executable.
From the candidate directory:

```sh
# Validate compatibility before touching the running installation.
sudo -u rift ./rift check /etc/rift/rift.lua
./rift --version
sudo install -d -m 0700 /var/backups/rift
sudo cp -p /usr/local/bin/rift /var/backups/rift/rift.previous
sudo cp -p /etc/rift/rift.lua /var/backups/rift/rift.lua.previous
# Announce/prepare the maintenance window, then allow graceful drain.
sudo systemctl stop rift
sudo install -m 0755 ./rift /usr/local/bin/rift
sudo systemctl start rift
sudo systemctl status rift
sudo journalctl -u rift -n 50 --no-pager
```

Confirm metrics, backend health and a real client login before declaring the
upgrade complete. Preserve `admin.env` securely when changing its format or secret.
If validation/startup or the client check fails, restore the matched binary/config:

```sh
sudo systemctl stop rift
sudo install -m 0755 /var/backups/rift/rift.previous /usr/local/bin/rift
sudo install -o root -g rift -m 0640 /var/backups/rift/rift.lua.previous /etc/rift/rift.lua
sudo -u rift /usr/local/bin/rift --check /etc/rift/rift.lua
sudo systemctl start rift
sudo journalctl -u rift -n 50 --no-pager
```

For containers, retain the previous versioned image and matching config, build a
new image under a different `RIFT_IMAGE` tag, and run its one-shot `check` command
before stopping the running container. Stop gracefully, then run `up -d` with the
new tag. Roll back by restoring the previous config, selecting the previous
image tag, validating it and running `up -d --no-build`. Avoid rebuilding or
reusing the previous tag until the upgrade is verified. A process replacement
disconnects sessions; configuration reload alone preserves them.

## Sizing

The default connection cap is 1024; it is not a measured player-capacity claim.
Monitor your own traffic before increasing limits. Packet buffers grow with
packet sizes; memory, CPU, file descriptors and backend capacity all constrain
safe player counts.

## HTTP administration

The optional [web dashboard and status services](http.md) provide a bundled
website, revision-checked configuration API, separate read-only status/metrics,
and Lua HTTP extensions. Start with `examples/admin.lua`. These HTTP services
are separate from the `rift admin` JSON-line control endpoint; both can run
simultaneously on distinct ports. Website saves, HTTP reloads, `rift admin reload`
and signal reloads share one serialized configuration transaction path.

The `web` configuration controls the dashboard and HTTP API, with its own
`web.token` bearer credential. Operational `admin.permissions` do not restrict
the HTTP API: a web credential grants the enabled HTTP API capabilities,
including configuration editing. The `status` server is read-only and
unauthenticated. All services are disabled unless configured.

The supplied systemd and Compose examples intentionally keep configuration
read-only to the process. Their dashboards can inspect and validate source and
reload edits made by an external administrator, but browser saves need write
access to both the selected file and its parent directory for atomic replacement.
To enable browser saves, provision a dedicated writable configuration directory
for the service account, allow that path through systemd's `ProtectSystem` with
`ReadWritePaths`, or use a writable Compose directory mount. Keep the CLI token
environment file outside a service-writable config directory.

An HTTP save validates the complete candidate and reserves changed service
sockets before committing the source and runtime. Invalid configuration, socket
collisions, stale revisions and write failures retain the previous file and
working runtime. A rejected reload preserves the runtime but does not undo an
edit already written by an external editor. Changing routes affects new
connections; existing sessions keep their sockets and backend attachment.
