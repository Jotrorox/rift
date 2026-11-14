# Operating a Rift release

Each release provides these assets for Linux x86_64, macOS ARM64 and Windows x86_64:

| Asset | Contents |
| --- | --- |
| `rift-linux-x86_64.tar.gz`, `rift-macos-aarch64.tar.gz`, `rift-windows-x86_64.zip` | `rift` (`rift.exe` on Windows), `README.md`, `LICENSE`, `THIRD_PARTY_NOTICES`, `docs/` and `examples/` |
| `rift-linux-x86_64`, `rift-macos-aarch64`, `rift-windows-x86_64.exe` | The same executable on its own, for scripted installs |
| `SHA256SUMS` | SHA-256 checksums of every other asset |

Every asset also has a signed [build provenance attestation](#verifying-a-download).
Keep the archive's files together so relative links between the guides and
examples keep working; the raw benchmark samples and the SDK sources stay
online. Run `rift --license` to view the embedded licenses. The
[README](../README.md) covers protocol support and backend configuration; this
guide covers daily administration.

## First start

Download the archive for your OS/CPU and `SHA256SUMS` from the same release,
[verify](#verifying-a-download) it, and extract it into an empty directory.
On Linux:

```sh
sha256sum --check --ignore-missing SHA256SUMS
tar xzf rift-linux-x86_64.tar.gz
./rift --version
./rift init
# Edit listener and backend addresses for your server.
./rift check
./rift
```

`rift init [path]` writes a commented configuration (default `./rift.lua`) and
refuses to overwrite an existing file. `rift check [path]` evaluates and validates
it without opening listeners or contacting backends; `rift --check [path]`
uses the same default. Running `./rift` loads `rift.lua` from the current working
directory when present, or uses the default addresses when it is absent. Use
`rift --config path/to/custom.lua` to select another file. Correct the field/path
named by any error and rerun the check before starting. Configuration scripts
are trusted local code.

### Strict checks

`rift check --strict [path]` runs the plain check and then the checks that
need the host, as the service account would see it. Run it in the same
environment as the service (for example `sudo -u rift --preserve-env=...` or
`systemd-run --property=EnvironmentFile=...`), since it reads that environment:

- **Secrets:** every variable the configuration names is set and has the length
  startup requires: `forwarding.secret_env` (1–1024 bytes, for `velocity` and `bungeeguard`), `admin.token_env`,
  `messaging.principals[].token_env` and `nodes.*.token_env` (32–1024 bytes),
  and `extensions.integrations.*.headers_env` (a valid header value). Output
  names the variables, never their values.
- **Instance database:** `instance_database` opens read-only, passes SQLite's
  `quick_check`, has a supported schema, and its registered instances fit the
  configuration. A missing database is fine when its directory is writable.
- **Extension store:** an existing `extensions.storage.path` decodes; a missing
  one can be created.
- **Ports:** every gameplay listener, `admin.listen`, `web.listen`,
  `status.listen` and `metrics` can be bound over TCP, and `messaging.listen`
  over UDP. Each socket is released at once.
- **Reloads:** the configuration is compared with the last deployment in the
  operator records (`web.records_directory`, or `<config>.operators`) using the
  same rules as a live reload: listener, admin, messaging, node, managed server,
  instance database, extension enablement/API version/storage/queue and
  `web.records_directory` changes need a restart. Modules are read from disk as
  they are now.

When the instance database is locked, Rift is running: port binds are skipped,
since Rift holds them, and a reload incompatibility is an error. When Rift is
stopped, an incompatible change is only reported, because the next start
applies everything. Strict checks create nothing; testing whether a directory is
writable creates a temporary file and removes it at once. Exit status is
non-zero when any strict check fails.

On macOS, check with `shasum -a 256 --check --ignore-missing SHA256SUMS`,
extract with `tar xzf rift-macos-aarch64.tar.gz`, then use the same commands.
On Windows, compare `Get-FileHash .\rift-windows-x86_64.zip -Algorithm SHA256`
with the file's line in `SHA256SUMS`, extract it with
`Expand-Archive .\rift-windows-x86_64.zip -DestinationPath rift`, and run
`.\rift.exe` inside the `rift` folder. If you use a standalone executable, rename
it to `rift` (`rift.exe` on Windows) and, outside Windows, run `chmod +x rift`.
`--version` identifies the Cargo package version; nightly builds may share that
version, so retain the release tag when reporting issues.

### Verifying a download

`SHA256SUMS` detects a corrupted or incomplete download. Provenance
attestations also prove that GitHub Actions built the file from this
repository's release workflow. With the [GitHub CLI](https://cli.github.com/),
verify any asset before running it:

```sh
gh attestation verify rift-linux-x86_64.tar.gz --repo Jotrorox/rift \
  --signer-workflow Jotrorox/rift/.github/workflows/release.yml
```

The output names the source commit and the workflow run that built the file.
For a nightly build, use `--signer-workflow Jotrorox/rift/.github/workflows/nightly.yml`.

The generated configuration forwards port 25565 to loopback port 25566.
Configure that backend as described in the README. For public Paper networks,
enable online authentication and Velocity modern forwarding using
[`examples/online.lua`](../examples/online.lua). Rift verifies accounts; private
Paper backends run `online-mode=false` with `proxies.velocity.online-mode=true`
(or BungeeGuard forwarding for networks with versions older than 1.13).
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
    permissions = { "status", "servers", "maintenance", "drain", "transfer", "reload", "shutdown", "plugins", "logs", "perms" },
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
./rift check
./rift
# In a separate shell with the same secret:
./rift admin status
```

The `admin` endpoint speaks JSON lines over TCP, accepts only loopback bind addresses
and requires the token on every request. Permissions are explicit: remove operations a credential must not
perform, or use `{ "status" }` for a read-only endpoint. Permission and credential
changes require a restart. There is one configured credential and permission set,
not a multi-user identity store. `rift check` validates the configuration shape;
the server verifies the environment secret when it starts, and `rift check
--strict` verifies it beforehand. Keep secrets out of
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

The `perms` permission grants `rift admin perms <command>`, the
[`/perms` grammar](permissions.md#commands) with console rights, plus `log`,
`compiled`, `doctor` and `backends`; import and export paths are resolved where
the CLI runs and read or written by the proxy.

The `servers` permission grants `rift admin servers`, `rift admin start <backend>`
and `rift admin stop <backend>` for configured [managed servers](managed-servers.md).
It also grants `rift admin groups`, `rift admin create <group>` and
`rift admin remove <instance>` for [service groups](managed-servers.md#service-groups-and-dynamic-instances),
`rift admin group start <group>`, `rift admin group stop <group>` and
`rift admin group mode <group> active|dev` for
[whole groups](managed-servers.md#start-stop-and-dev-mode-for-a-whole-group),
and `rift admin nodes` for [node agents](nodes.md).
Creation allocates a port and registers a stopped backend; removal waits for
child cleanup and refuses players or pending attachments.
Start and stop return an accepted response immediately; poll `servers` to verify
completion. A failed start appears in the server's `last_error` field. These
commands never accept arbitrary process commands or modify ordinary unmanaged
backends. `status` also includes the managed server state for read-only inspection.

`rift admin plugins list` and `rift admin plugins status <name>` show
[plugins](plugins.md#operator-surface) with their state, restarts, calls and
last error; they need `status`. `rift admin plugins logs <name>` shows a
plugin's last 200 log lines and needs `logs`, like the HTTP log endpoints. `rift admin plugins restart <name>` stops a persistent plugin and
starts a fresh instance with its current configuration; it needs the `plugins`
permission, returns once the restart is accepted, and is recorded in the
operator audit log with the plugin name.

Connection `rate_limit` runs before handshake parsing. `login_rate_limit` runs
at login admission and has the same token-bucket fields and defaults; it allows
status requests without spending login tokens. Both apply globally and per IP,
and leave established traffic untouched. `login_rate_limit` is opt-in.
`rate_limit` is on with the defaults above whenever `on_route` or `on_status` is
configured, because those hooks run a Lua VM for every connection or ping; set
`rate_limit = false` to turn it off, or a table to tune it. Without those hooks
it is opt-in. Clients sharing a NAT also
share an IP allowance. A routing hook can enforce additional IP/listener policy.
Online mode verifies player names before network access checks; offline-mode
names are unauthenticated and cannot establish account ownership.

## Server list: MOTD, favicon and player count

By default Rift answers server-list pings itself with the network's player count
(without players a [visibility rule](permissions.md#visibility) hides),
`limits.max_connections` as the maximum and the description `Rift`. With
`status_cache` it instead relays the routed backend's response. To set the
network's own entry, configure `server_list`:

```lua
rift.config.server_list = {
    -- A string (section-sign codes such as "\u{00A7}6" work) or a chat component table.
    motd = { text = "My Network", color = "gold", extra = { { text = "\nNow on 1.21", color = "gray" } } },
    maintenance_motd = "Back soon. Follow @mynetwork for updates.",
    version = { mini = "<gold>My Network</gold> <gray>1.8-1.21" }, -- Shown when a client's version is incompatible.
    max_players = 500,               -- Defaults to limits.max_connections.
    favicon = "icons/network.png",   -- 64x64 PNG up to 16 KiB, relative to rift.lua, or a data URL.
    sample = { { mini = "<green>Survival</green>, <aqua>Creative" }, "and Minigames" }, -- Hover text over the player count.
    hosts = {
        ["survival.example.com"] = { motd = "Survival", favicon = "icons/survival.png" },
        ["*.minigames.example.com"] = { motd = "Minigames", max_players = 200 },
    },
}
```

The player count is the number of players logged in through Rift across all
backends. Rift answers each ping directly and never contacts a backend, so
`server_list` cannot be combined with `status_cache`. Every field is optional.
`hosts` entries use the same patterns as hostname routes (exact names first,
then the longest `*.` suffix), and inherit any unset field from the top level.
During maintenance, `maintenance_motd` replaces `motd`. Pings for a hostname
with no route keep the reply "No server is configured for this hostname."
Each text field is a string or a chat component table, including the
`{ mini = ... }` and `{ legacy = ... }` forms of [styled text](text.md). Clients
read `version` and `sample` as strings, so Rift writes components there as `§`
codes, with hex colours as the nearest named colour; a plain string is sent as
written. The MOTD is rewritten for each pinging client like any component.
Rift's default MOTDs and version name are the `status.*` entries of its
[message catalogue](text.md#rifts-own-messages).
Rift reads favicon files when it loads or reloads the configuration, so replace
the file and reload to change the icon. A favicon can also be the image itself
as a `data:image/png;base64,...` URL. That is how the dashboard's Server list
page stores an uploaded icon: it accepts PNG, JPEG, WebP or GIF, scales the
image to fit 64×64 and saves it in the configuration, so no file has to exist
next to `rift.lua`. Rift rejects missing files, images that are not 64×64 PNGs,
and settings that would exceed the protocol's 32767-byte status limit.

For per-ping logic, register a `status` handler. It runs before Rift contacts
any backend and receives `listener`, `hostname`, `protocol`, `peer_addr`,
`peer_ip`, `backend` (the routed backend or group, or nil for an unmatched
hostname), `maintenance`, `online` and `max_players`. Return nil to keep the
default, or a table with any of `motd`, `version`, `protocol`, `online`,
`max_players`, `sample` and `favicon` (a `data:image/png;base64,...` URL):

```lua
rift.on("status", function(ping)
    if ping.maintenance then
        -- Protocol -1 makes every client show the version text in place of the count.
        return { motd = "Scheduled maintenance until 18:00 UTC", version = "Maintenance", protocol = -1 }
    end
    if ping.hostname == "event.example.com" then
        return { motd = { text = "Build contest is live!", color = "aqua" } }
    end
end)
```

Results apply on top of `server_list` (or Rift's default response). A
handler's result also replaces `status_cache` passthrough for that ping;
returning nil keeps passthrough. The handler runs on two workers of its own,
separate from the four `on_route` workers, so a flood of pings cannot delay
logins and busy routing cannot blank the server list. Both hooks wait up to
25 ms for a free worker before failing, and each has a 50 ms deadline and a
fresh VM for every call.
On an error, overload, an invalid result or an oversized response, Rift logs
a `status_hook_failed` event and serves the default document, so the server
list never goes blank because of a script.

## Tab list: header, footer and network-wide players

Set `tab_list` to give every player the same tab list header and footer on
every backend, and optionally to list everyone on the network:

```lua
rift.config.tab_list = {
    -- A string (section-sign codes work) or a chat component table.
    header = { text = "My Network", color = "gold", bold = true },
    footer = "\u{00A7}7{online} players online \u{00A7}8| \u{00A7}7You are on {server}",
    players = "network", -- "server" (the default) lists only the current backend's players.
}
```

Header and footer text may use `{player}` (the player's name), `{server}` (their
current backend), `{online}` (players connected through Rift) and
`{server_online}` (players on their backend); other text in braces is left as
it is. Rift sends the header and footer when the player joins, again after each
server switch, and within half a second of any change to the values they show.
While Rift sets a header and footer, it drops the ones backends send, such as
those from a tab list plugin. Setting only one of `header` and `footer` leaves
the other blank. Each is limited to 4096 bytes before placeholders are replaced.

With `players = "network"`, each player's list also shows the players on every
other backend. Backends keep listing their own players, with game mode, ping and
display names from their plugins; Rift only adds players the current backend
does not list, so a player is never listed twice and moving between servers
needs no cleanup. Entries Rift adds show the player's skin when authentication
is enabled and the default skin otherwise, with full signal bars and no game
mode or display name. A player whose
[visibility rule](permissions.md#visibility) hides them from a viewer is not
listed to that viewer, nor counted in `{online}` and `{server_online}`; their
own server's list is the backend's (a vanish plugin there hides them).

Configuration reloads apply to connected players. Removing `tab_list` removes
Rift's entries and blanks the header and footer, after which backends' own
updates show again. Extensions can set a player's header and footer with
`rift.players.tab_list` (see [online players](extensions.md#online-players)).
The tab list works on every Minecraft version Rift can switch players on
(1.8.9–26.3).

## Run behind a load balancer (PROXY protocol)

When players reach Rift through TCPShield, Cloudflare Spectrum, HAProxy or a
cloud TCP load balancer, every socket comes from the load balancer. Enable the
HAProxy PROXY protocol (v1 or v2) on the load balancer and tell Rift which
sources may send it:

```lua
rift.config.proxy_protocol = {
    -- Addresses or CIDR blocks of your load balancers. Required.
    trusted_proxies = { "10.0.0.0/8", "2001:db8::/32" },
    -- Optional. Omit to require the header on every listener.
    listeners = { "default" },
}
```

On an affected listener, Rift rejects connections from sources outside
`trusted_proxies` before reading any bytes, then requires a valid header within
the 5-second handshake deadline. Clients cannot connect directly or claim
another address. Rift reads the header before applying `rate_limit`, and it
then reports the player's address in `login_rate_limit`, `on_route`
`peer_ip`/`peer_addr`, extension `peer_ip`, Velocity forwarding and the `peer`
field of connection events. Rift ignores v2 TLVs. When the header is a v2
`LOCAL` command, which load balancer health checks often use, or a v1
`UNKNOWN`, Rift keeps the load balancer's address. Use your provider's
published ranges for TCPShield or Cloudflare. Keep the list as small as
possible: a trusted source can claim any client address. Rift reads the
setting for each new connection, so a reload applies it without reopening
listeners.


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
| Capacity/access pressure | `rift_connections_rate_limited_total`, `rift_logins_rate_limited_total`, `rift_connections_access_rejected_total`, `rift_connections_proxy_rejected_total` |
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
| `status_hook_failed` | Inspect the named `on_status` script and its returned fields; pings are still answered with the default |
| Session/login failure | Check supported versions, authentication-service access, matching Velocity secrets on every Paper backend and the backend's own log |
| `proxy_untrusted` / `proxy_header` | Add the load balancer to `proxy_protocol.trusted_proxies`, enable PROXY protocol on it, or block direct access to the listener |
| Rate-limit/capacity rejection | Check legitimate player traffic and NAT concentration before changing limits |
| Admin permission/authentication failure | Check the CLI secret and the configured operation permission |

## Linux systemd service

The supplied [unit](../examples/rift.service) uses an unprivileged `rift` account
and root-owned configuration. Run these commands on a systemd host from the
extracted Linux archive, after generating `rift.lua` there. Create the account
only if it does not exist:

```sh
sudo useradd --system --user-group --no-create-home --shell /usr/sbin/nologin rift
sudo install -m 0755 rift /usr/local/bin/rift
sudo install -d -o root -g rift -m 0750 /etc/rift
sudo install -o root -g rift -m 0640 rift.lua /etc/rift/rift.lua
printf '%s\n' 'rift.config.instance_database = "/var/lib/rift/rift.sqlite3"' \
  | sudo tee -a /etc/rift/rift.lua > /dev/null
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
`StateDirectory=rift` creates `/var/lib/rift`, owned by the service account with
mode 0700, and permits writes there under `ProtectSystem=strict`. The configured
`instance_database` stores SQLite registrations, its lock and journal in that
directory, which survives service stops and restarts. Every file-based startup
opens this database, even without service groups; the default location beside
the root-owned config is not writable. `rift check` validates configuration
without opening the database; run `sudo -u rift rift check --strict
/etc/rift/rift.lua` to verify that the database opens or can be created. LuaJIT uses
executable memory, so do not add `MemoryDenyWriteExecute=true` to this unit.

## Secrets and the processes Rift starts

Rift reads its secrets from environment variables: `admin.token_env`, every
`messaging.principals[].token_env`, every `nodes.*.token_env`,
`forwarding.secret_env` and the `headers_env` of
`extensions.integrations`. A `rift node` agent reads `--token-env` and
`--forwarding-secret-env`. Rift removes exactly these variables from the
environment of every managed server it starts, locally and through agents,
and from the default environment of WebAssembly plugins. Everything else is
still inherited, so a database password you export for a server plugin
keeps working; `env` and `inherit_env` on a managed server or service group
narrow that further (see [managed servers](managed-servers.md#configuration)).
A variable once used for a secret stays removed until Rift restarts, even if a
reload stops naming it, because its value is still in Rift's environment.

On Linux Rift also clears its dumpable flag (`PR_SET_DUMPABLE`) as the first
thing it does, in every mode (`rift`, `rift node`, `rift dev`, `rift admin`).
Processes of the same user, such as the Minecraft servers Rift starts, then
cannot read `/proc/<rift pid>/environ` or `/proc/<rift pid>/mem`, or attach
with `ptrace`, so they cannot recover the secrets from outside either. This has
two side effects:

- **No core dumps.** A crashing Rift writes no core file, because it would
  contain every secret. Debug a crash with a build run under a debugger
  started by root, or reproduce it on a host without production secrets.
- **No same-user debugging or descriptor inspection.** `gdb -p`, `strace -p`,
  `perf`, `lsof -p` and monitoring that lists `/proc/<pid>/fd` (open-file
  counts in process exporters) are refused when they run as Rift's user; run
  them as root (which has `CAP_SYS_PTRACE`) instead. `/proc/<pid>/status` and
  `stat` stay readable, so CPU and memory monitoring keeps working.

For a debugging session, start Rift with `RIFT_DUMPABLE=1` to skip this; Rift
logs a warning at startup. It is read once, before anything else, so a reload
cannot change it.

This is hygiene, not isolation. Managed servers still run as Rift's user: they
can read every file Rift can (the configuration, `rift.sqlite3`, the
messaging private key and anything under `local/`), write Rift's plugins and
templates, and signal Rift. On Linux, set
[`isolation`](managed-servers.md#isolation) on groups and managed servers to
confine them to their own directory; elsewhere, keep untrusted server plugins
off the host, or run servers on [nodes](nodes.md) under a separate account. macOS and Windows have no equivalent flag; there, a process
of the same user can read Rift's environment.

## Container example

The supplied [Dockerfile](../examples/Dockerfile) packages the Linux release
binary using the release runner's Ubuntu 24.04 runtime baseline. The
[Compose example](../examples/compose.yaml) runs as UID/GID 65532 with a read-only
root filesystem, no Linux capabilities, a read-only config directory, a writable
`rift-state` volume at `/var/lib/rift` and a 40-second stop deadline.
Run from the extracted Linux x86_64 archive:

```sh
mkdir config
./rift init config/rift.lua
printf '%s\n' 'rift.config.instance_database = "/var/lib/rift/rift.sqlite3"' >> config/rift.lua
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

The image creates `/var/lib/rift` with owner 65532:65532 and mode 0700; Docker
preserves these permissions when initializing the named volume. SQLite creates
its database, lock and journal there while `/etc/rift` stays read-only. The
volume survives container recreation and `docker compose down`; `down --volumes`
removes the stored registrations. An existing volume or replacement image must
also provide write access for UID/GID 65532. `rift check` alone does not verify
database write access (`rift check --strict` does); confirm the running container reaches `rift: listening`
in its logs after `up -d`.

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
| Health probes, status cache, `server_list` (favicon files are reread) | Rebuilt after successful reload |
| `tab_list` | Applied to connected players within half a second |
| Shutdown deadline | New value applies to the later shutdown drain |
| `web`, `status`, standalone `metrics`, web bearer token | Can enable, disable or move live; replacement sockets bind before commit |
| Gameplay listeners, operational `admin` bind/token variable/permissions | Restart required; incompatible reload rejects the entire candidate |
| `managed_servers` definitions or addresses of managed backends | Restart required; running processes and their lifecycle policy remain attached to the original definition |
| `service_groups`, `templates` and scaling policies | Apply live while each registered instance keeps its process definition, storage policy and an in-range port; otherwise the reload names the instance to remove first. Create and remove instances with admin or web operations |
| `instance_database` | Bundled SQLite file, default `rift.sqlite3` beside the config; restores registrations before scaling. Path changes require a restart |
| `nodes` and `messaging` | Restart required; agents keep their servers running while the controller restarts and are adopted again. See [multiple hosts](nodes.md) |
| Runtime service instances | SQLite preserves names, groups, nodes, ports, storage ownership and explicit stops across reloads and proxy restarts |

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

For a managed backend, check both players and attachments before stopping:

```sh
rift admin servers
rift admin stop survival
rift admin servers
# After maintenance, re-enable automatic wake and start the process:
rift admin start survival
rift admin servers
```

The `reservations` count includes pending and established backend attachments,
including connections whose protocol does not support player tracking. Tracked
players also have an attachment, so these counts overlap.
Manual stop refuses occupied servers and pauses automatic wake so an incoming
connection cannot undo maintenance. Start re-enables automatic wake according to
the configured policy. Idle shutdown leaves automatic wake available. Start and
stop acknowledgements mean the request was accepted; verify `running` or `stopped`
in the following `servers` response. Shutdown failures are reported there too.

Transfers retain the client socket on Minecraft 1.8.9–26.3 (50 explicitly mapped release protocols) while
attaching the target backend and checking the same UUID/name. Completion requires
the replacement world's Join Game, with a 30-second transition cap. A sleeping
managed destination may first take up to its configured startup deadline to
become ready; the original session continues forwarding during that wait. Other client
versions and invalid targets are rejected without moving the player. The target
must permit the player under both the session's original access rules and the
current administrator-request configuration. A denied or failed replacement
login leaves the original backend attached. If a transfer fails after the client
transition begins, Rift disconnects the player to avoid continuing a damaged
session. Transfer success does not move inventories between independent servers.

With a [network configuration](../examples/online.lua), a
backend transport failure tries the configured hubs and fallbacks. Access rules,
backend bans and draining still apply.

A backend that kicks a player in play (a round ending, a ban, Paper's `/stop`)
moves them to a hub or fallback the same way and shows the kick in chat as
`connection.kicked` ("You were kicked from survival: …"). With nowhere to go,
or when the kick came from the only hub, the player receives the backend's
kick unchanged. Set `network.on_kick = "disconnect"` to keep every backend kick
final:

```lua
rift.config.network = { hubs = { "lobby" }, on_kick = "disconnect" }
```

A [`kick` hook](extensions.md#callbacks-and-decisions) decides per kick before this
rule: it can name the server to send the player to or the text to disconnect
them with. Kicks during login, configuration or an ongoing transfer stay final,
as do kicks without a network configuration or on Minecraft versions without
server switching.

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

Verify and extract the new release archive into a separate directory. Record
both release tags and retain the previous archive. From the candidate directory:

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
the HTTP API: the legacy `web.token` grants full enabled API access. Named
`web.operators` can restrict permissions and service groups; the dashboard
provides managed-server logs/commands, structured definitions, deployment
history/rollback and audit records. See [HTTP access](http.md#authentication-and-access).
Configure `web.records_directory` on writable storage for retained records when
the configuration mount is read-only. The `status` server is read-only and
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
