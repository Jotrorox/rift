# Monitoring and logs

Rift serves Prometheus metrics, writes a JSON line for each failed connection
and keeps a plain-text log on stderr. Use this page to set up monitoring and
to find out why a connection failed.

## Metrics

Set a metrics listener in `rift.lua`:

```lua
metrics = "127.0.0.1:9090",
```

Rift serves Prometheus metrics at `GET /metrics` on that listener;
`curl --fail http://127.0.0.1:9090/metrics` should succeed.

> [!WARNING]
> The metrics listener has no authentication. Use loopback or a trusted
> monitoring network.

Watch these alongside the process or service status:

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
| [Cluster](clustering.md#metrics) health | `rift_cluster_is_leader`, `rift_cluster_peer_up`, `rift_cluster_replication_lag`, `rift_cluster_diverged`, `rift_cluster_config_reload_failed` |

- **Backend health** is TCP reachability, not a successful Minecraft login.
  Backends stay eligible before the first probe and always report up when
  probing is disabled.
- **Draining** is separate: a reachable backend can intentionally refuse new
  attachments.
- **Counters** survive reloads and reset when the process restarts.
- **Login duration** measures TCP acceptance through first settled play,
  including routing and handshake. Unsuccessful logins do not contribute to
  the histogram.
- **Memory** is the Linux process RSS, not a container memory limit or an
  estimate of memory per player. Rift omits the resident-byte gauge when
  availability is zero.

### In a cluster

Every peer of a [cluster](clustering.md) serves its own metrics; scrape each
one. The series above count the peer's own connections and players; add them up
across peers for the network. A peer also serves `rift_cluster_*` series: Raft term, leadership and lease, commit and applied index, log
size, divergence, each peer's reachability, replication lag (on the leader),
proposal latency and outcomes, bus forwarding and drops, and each peer's
configuration state. [Clustering](clustering.md#metrics) lists them with
alerting suggestions, and [log lines](clustering.md#log-lines) lists the
`rift: cluster: ...` lines worth watching, such as
`lost contact with the leader`, `this replica diverged` and
`leaving: transferred N players to other peers and disconnected M`.

## Connection events

Connection failures produce JSON lines on stderr. Startup, reload and shutdown
messages remain plain text (see [log levels](#log-levels)).

| Event | Meaning |
| --- | --- |
| `connection_failed` | A connection ended with an error |
| `connection_rejected` | Rift refused a connection, for example for a rate limit, capacity or an untrusted proxy |
| `backend_attempt_failed` | One backend attempt failed. A fallback can still succeed, so this alone does not mean the player disconnected |
| `admin_command` | The result of an [admin command](admin.md), without credentials |

Each connection event has `connection_id`, `backend`, `backend_address`,
`stage`, `failure`, `error_kind`, `duration_ms` and a readable `message`.
Backend fields are null if the failure occurred before route selection. Use
`connection_id` to join related events within one process.

For a systemd service:

```sh
sudo journalctl -u rift --since '10 minutes ago' -o cat --no-pager \
  | jq -R 'fromjson? | select(.event == "connection_failed" or .event == "connection_rejected")'
```

Rift writes at most 20 `connection_rejected` lines per second, so a flood of
rejected connections cannot fill the log. The next line written carries
`suppressed`, the number left out. The rejection counters in the metrics count
every one.

## Find the cause of a failure

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
| Rate-limit/capacity rejection (`rate_limited`, `login_rate_limited`, `capacity_exhausted`, `address_capacity_exhausted`) | Check legitimate player traffic and NAT concentration before changing [limits](hardening.md) |
| Admin permission/authentication failure | Check the CLI secret and the configured operation permission |

## Log levels

Rift writes its log to stderr, one line per message, each starting with
`rift: `. The level is part of the text:

| Level | Line | Meaning |
| --- | --- | --- |
| `error` | `rift: error: ...` | Work was lost or a guarantee no longer holds: an audit record that could not be written, a plugin that failed, a panic |
| `warn` | `rift: warning: ...` | Something was refused, dropped or retried and Rift carried on |
| `info` | `rift: ...` | Lifecycle: listeners, reloads, nodes, servers, shutdown |
| `debug` | `rift: debug: ...` | Detail for tracing one problem, including plugins' `debug` and `trace` lines |

Two environment variables, read once at startup, control it. Any other value
stops Rift at startup.

| Variable | Values | Default |
| --- | --- | --- |
| `RIFT_LOG_LEVEL` | `error`, `warn`, `info`, `debug`: the least severe level still written | `info` |
| `RIFT_LOG_FORMAT` | `plain`, or `journal` to put the syslog priority (`<3>`, `<4>`, `<6>`, `<7>`) in front of each line | `plain` |

- Under systemd, add both to the unit with `Environment=` lines or to
  `/etc/rift/admin.env`.
- With `RIFT_LOG_FORMAT=journal`, the journal stores the priority instead of
  showing it, so `journalctl -u rift -p warning` lists warnings and errors
  only, and errors are highlighted.
- Plugin log lines (`rift: plugin <name>: ...`) follow the same filter by
  their own level. A plugin's [log tail](plugin-operations.md) keeps
  every line regardless.
- The JSON connection events and the output of commands such as `rift check`
  and `rift admin` are not filtered.

## Panics

A panic in one of Rift's tasks is logged as a single
`rift: error: panic in thread <name> at <file>:<line>: <message>` line. The
task that panicked is gone, but the process keeps running: connections still
drain and managed servers keep their supervisor. Treat the line as a bug to
report and plan a restart. Set `RUST_BACKTRACE=1` to have the backtrace follow
it line by line.
