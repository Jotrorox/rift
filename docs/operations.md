# Day-to-day operations

This page covers routine work on a running Rift: reloading the configuration,
maintenance windows, draining backends, moving players, and what happens when
a backend kicks a player or fails. For other tasks, start here:

| I want to… | Page |
| --- | --- |
| Install Rift and start it for the first time | [Install](install.md) |
| Run commands against a running proxy | [Admin CLI](admin.md) |
| Run Rift under systemd or Docker | [Deployment](deployment.md) |
| Read metrics and logs, or find why a connection failed | [Monitoring and logs](monitoring.md) |
| Set rate limits, run behind a load balancer, protect secrets | [Limits, PROXY protocol and secrets](hardening.md) |
| Stop, upgrade or roll back Rift | [Shutdown, upgrade and rollback](upgrading.md) |
| Manage Rift from a browser | [Web dashboard](http.md) |
| Let Rift start and stop Minecraft servers | [Managed servers](managed-servers.md) |

The commands below use the [admin CLI](admin.md) and the paths of the
[systemd setup](deployment.md#run-under-systemd).

## Reload the configuration

A reload applies a new `rift.lua` and keeps every session. Save a candidate in
the same directory, validate it as the service user, then replace the live
file atomically. Keep a backup for rollback:

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

> [!WARNING]
> A successful `systemctl reload` does not prove Rift accepted the reload:
> `ExecReload` checks the file, then sends SIGHUP, and signal delivery is
> asynchronous. Confirm `rift: configuration reloaded` or an increase in
> `rift_reloads_total`.

`rift admin reload` runs the same validated reload. Outside systemd, use
`kill -HUP <pid>` on Unix or Ctrl-Break on Windows.

### When a reload is rejected

The old snapshot stays active. Inspect the error, restore the backup or fix
the file, and reload again. `rift check` validates a candidate in isolation
and cannot detect changes incompatible with an already running process.

One case differs: if the configuration applies but its plugins cannot be
activated, the error says so. The new revision is live, the previous plugins
keep running, and the reload counts as a failure in the metrics. Fix the
plugin and reload again.

### What a reload changes

| Change | Reload behavior |
| --- | --- |
| Routes, backend addresses, fallback, hooks | New connections use the new snapshot; existing sessions keep their sockets and routing |
| Connection/login limits, configured maintenance/draining | Apply to new admission/attachments; current sessions continue |
| Health probes, status cache, `server_list` (favicon files are reread) | Rebuilt after successful reload |
| `tab_list` | Applied to connected players within half a second |
| Shutdown deadline | New value applies to the later shutdown drain |
| `web`, `status`, standalone `metrics`, web bearer token | Can enable, disable or move live; replacement sockets bind before commit |
| Gameplay listeners, operational `admin` bind/token variable/permissions | Restart required; incompatible reload rejects the entire candidate |
| `managed_servers` definitions or addresses of managed backends | Restart required; running processes and their lifecycle policy remain attached to the original definition. A server with `keep_running` survives that restart with the process it has; stop and start it to apply a changed command or environment |
| `service_groups`, `templates` and scaling policies | Apply live while each registered instance keeps its process definition, storage policy and an in-range port; otherwise the reload names the instance to remove first. Create and remove instances with admin or web operations |
| `instance_database` | Bundled SQLite file, default `rift.sqlite3` beside the config; restores registrations before scaling. Path changes require a restart |
| `nodes` and `messaging` | Restart required; agents keep their servers running while the controller restarts and are adopted again. See [multiple hosts](nodes.md) |
| Runtime service instances | SQLite preserves names, groups, nodes, ports, storage ownership and explicit stops across reloads and proxy restarts |

- A connection Rift already accepted keeps its original snapshot while its
  login completes.
- A route edit never transfers a connected player. Explicit
  [transfers](#transfer-a-player) are separate operations.
- Lowering `max_connections` preserves sessions and rejects new admission
  until the total falls below the new limit.

## Run a maintenance window

With the relevant [admin permissions](admin.md#permissions), block new logins,
drain a backend and move its players:

```sh
rift admin maintenance on
rift admin drain survival on
rift admin status
# Use an online player's connection ID from status and a configured backend name.
rift admin transfer 42 lobby
rift admin status
```

Wait for the backend's player count to reach zero, or transfer the remaining
players, before you stop it. After the work, check the backend's health and
reopen admission:

```sh
rift admin drain survival off
rift admin maintenance off
rift admin status
```

These commands set runtime overrides of two configuration fields:

```lua
maintenance = false,
draining = {}, -- For example, { "survival" } blocks new attachments to survival.
```

- An override survives reloads until another admin command changes it or the
  process restarts. `off` is itself an override, not a reset to the configured
  value.
- To keep a policy across restarts, change `maintenance` or `draining` in the
  Lua file and reload.

## Maintenance mode

Maintenance refuses new logins. Connected players stay, and the server list
still answers.

To let staff in during maintenance, name a permission node:

```lua
maintenance_permission = "rift.maintenance.bypass", -- Omit to turn everyone away.
```

- Players for whom the node resolves to `true` (granted directly, through a
  group or by a wildcard such as `rift.*`) log in as usual. Everyone else gets
  the maintenance message.
- Only global grants count, as for
  [server access](permissions.md#server-access) at login.
- Without the option, Rift refuses logins before the player is identified.
  With it, Rift reads the login and, in online mode, authenticates it first,
  so each refused login costs a Mojang session lookup. The login rate limit
  still applies beforehand.

> [!WARNING]
> In offline mode names are unverified, so anyone claiming a staff name
> passes.

## Drain a backend

Draining a backend keeps the players already attached and prevents new
attachments to it, including fallback and explicit transfers. Eligible
configured fallbacks accept the new players instead.

## Transfer a player

`rift admin transfer <connection-id> <backend>` moves one online player.

- Transfers work on Minecraft 1.8.9–26.3 (50 explicitly mapped release
  protocols). Rift keeps the client socket while it attaches the target
  backend and checks the same UUID and name.
- Completion requires the replacement world's Join Game, with a 30-second
  transition cap.
- A sleeping managed destination may first take up to its configured startup
  deadline to become ready. The original session keeps forwarding during that
  wait.
- The target must permit the player under both the session's original access
  rules and the configuration current when the administrator asks.
- A successful transfer does not move inventories between independent servers.

| Case | Result |
| --- | --- |
| Other client version or invalid target | Rejected without moving the player |
| Replacement login denied or failed | The original backend stays attached |
| Failure after the client transition began | Rift disconnects the player, to avoid continuing a damaged session |

## Stop a managed backend for maintenance

For a [managed backend](managed-servers.md), check both players and
attachments before stopping:

```sh
rift admin servers
rift admin stop survival
rift admin servers
# After maintenance, re-enable automatic wake and start the process:
rift admin start survival
rift admin servers
```

- `reservations` counts pending and established backend attachments, including
  connections whose protocol does not support player tracking. Tracked players
  also have an attachment, so the counts overlap.
- A manual stop refuses an occupied server and pauses automatic wake, so an
  incoming connection cannot undo maintenance.
- `start` re-enables automatic wake according to the configured policy. Idle
  shutdown leaves automatic wake available.
- An acknowledgement means Rift accepted the request. Verify `running` or
  `stopped` in the next `servers` response; shutdown failures are reported
  there too.

## Backend kicks and failures

These rules need a [network configuration](../examples/online.lua). Access
rules, backend bans and draining still apply.

| Event | What Rift does |
| --- | --- |
| Backend transport failure | Tries the configured hubs and fallbacks |
| A backend kicks a player in play (a round ending, a ban, Paper's `/stop`) | Moves the player to a hub or fallback the same way and shows the kick in chat as `connection.kicked` ("You were kicked from survival: …") |
| Nowhere to go, or the kick came from the only hub | The player receives the backend's kick unchanged |
| Kick during login, configuration or an ongoing transfer | Final |
| Kick without a network configuration, or on a Minecraft version without server switching | Final |

Set `network.on_kick = "disconnect"` to keep every backend kick final:

```lua
rift.config.network = { hubs = { "lobby" }, on_kick = "disconnect" }
```

A [`kick` hook](extensions.md#callbacks-and-decisions) decides per kick before
this rule: it can name the server to send the player to or the text to
disconnect them with.

## Sizing

The default connection cap is 1024; it is not a measured player-capacity
claim. Monitor your own traffic before increasing limits. Packet buffers grow
with packet sizes; memory, CPU, file descriptors and backend capacity all
constrain safe player counts.
