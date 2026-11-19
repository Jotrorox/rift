# Limits, PROXY protocol and secrets

This page covers the settings that protect a public Rift: rate limits, the
connection cap per address, PROXY protocol behind a load balancer, how Rift
keeps its secrets from the processes it starts, and whether it stores player
addresses.

## Rate limits

Rift has two token-bucket limits with the same fields and defaults. This
example sets the login limit to the default values:

```lua
login_rate_limit = {
    per_ip_per_second = 20,
    per_ip_burst = 40,
    global_per_second = 200,
    global_burst = 400,
    max_ips = 65536,
},
```

| Setting | Runs | When unset |
| --- | --- | --- |
| `rate_limit` | On every connection, before handshake parsing | On with the defaults when `on_route` or `on_status` is configured, because those hooks run a Lua VM for every connection or ping; otherwise off |
| `login_rate_limit` | At login admission; status requests spend no login tokens | Off |

- Set `rate_limit = false` to turn it off, or a table to tune it.
- Both limits apply globally and per IP, and leave established traffic
  untouched. Clients sharing a NAT also share an IP allowance.
- A routing hook can enforce additional IP or listener policy.
- Online mode verifies player names before network access checks.
  Offline-mode names are unauthenticated and cannot establish account
  ownership.

## Connections per address

`limits.max_connections_per_address` (default 256) caps the open connections
of one IPv4 address or IPv6 /64, so a single host cannot hold every slot of
`max_connections`.

- A connection over the cap is rejected with the failure
  `address_capacity_exhausted`.
- Behind PROXY protocol, it counts the client address the trusted proxy
  reports.
- Raise it when many players legitimately share one address, and keep it below
  `max_connections`.

It limits open connections; `rate_limit` limits how fast new ones arrive.

## In a cluster

In a [cluster](clustering.md#what-stays-per-peer) of several proxies, these
limits stay **per peer**: each peer counts only the connections it accepts.

- `rate_limit` and `login_rate_limit` buckets, including the global ones,
  apply to each peer separately. Three peers behind DNS round robin together
  admit up to three times `global_per_second`; divide the value by the number
  of peers if it is meant for the whole network.
- `limits.max_connections` and `limits.max_connections_per_address` cap the
  open connections of one peer. A client can hold that many on each peer it
  reaches.
- They protect one process. The cap on players in the whole network is
  `cluster.max_players`, which the cluster enforces when a login's claim
  applies, and [queues](queues.md#in-a-cluster) count capacity network-wide.

## Run behind a load balancer

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

> [!WARNING]
> A trusted source can claim any client address. Keep the list as small as
> possible, and use your provider's published ranges for TCPShield or
> Cloudflare.

On an affected listener:

1. Rift rejects connections from sources outside `trusted_proxies` before
   reading any bytes. Clients cannot connect directly or claim another
   address.
2. Rift requires a valid header within the 5-second handshake deadline.
3. Rift reads the header before applying `rate_limit`. It then reports the
   player's address in `login_rate_limit`, `on_route` `peer_ip`/`peer_addr`,
   extension `peer_ip`, Velocity forwarding and the `peer` field of
   [connection events](monitoring.md#connection-events).

Rift ignores v2 TLVs. For a v2 `LOCAL` command, which load balancer health
checks often use, or a v1 `UNKNOWN`, Rift keeps the load balancer's address.

Rift reads the setting for each new connection, so a reload applies it without
reopening listeners.

## Secrets in environment variables

Rift reads its secrets from environment variables:

- `admin.token_env`
- every `messaging.principals[].token_env`
- every `nodes.*.token_env`
- `web.token_env` and every `web.operators.*.token_env`
- `forwarding.secret_env`
- the `headers_env` of `extensions.integrations`
- for a `rift node` agent, `--token-env` and `--forwarding-secret-env`

Rift removes exactly these variables from the environment of every managed
server it starts, locally and through agents, and from the default environment
of WebAssembly plugins.

- Everything else is still inherited, so a database password you export for a
  server plugin keeps working.
- `env` and `inherit_env` on a managed server or service group narrow that
  further; see [managed servers](managed-servers.md#configure-a-managed-server).
- A variable once used for a secret stays removed until Rift restarts, even if
  a reload stops naming it, because its value is still in Rift's environment.

## The dumpable flag

On Linux, Rift clears its dumpable flag (`PR_SET_DUMPABLE`) as the first thing
it does, in every mode (`rift`, `rift node`, `rift dev`, `rift admin`).
Processes of the same user, such as the Minecraft servers Rift starts, then
cannot read `/proc/<rift pid>/environ` or `/proc/<rift pid>/mem`, or attach
with `ptrace`, so they cannot recover the secrets from outside either.

This has two side effects:

- **No core dumps.** A crashing Rift writes no core file, because it would
  contain every secret. Debug a crash with a build run under a debugger
  started by root, or reproduce it on a host without production secrets.
- **No same-user debugging or descriptor inspection.** `gdb -p`, `strace -p`,
  `perf`, `lsof -p` and monitoring that lists `/proc/<pid>/fd` (open-file
  counts in process exporters) are refused when they run as Rift's user. Run
  them as root, which has `CAP_SYS_PTRACE`. `/proc/<pid>/status` and `stat`
  stay readable, so CPU and memory monitoring keeps working.

For a debugging session, start Rift with `RIFT_DUMPABLE=1` to skip this. Rift
logs a warning at startup. It reads the variable once, before anything else,
so a reload cannot change it.

macOS and Windows have no equivalent flag; there, a process of the same user
can read Rift's environment.

> [!NOTE]
> This is hygiene, not isolation. Managed servers still run as Rift's user:
> they can read every file Rift can (the configuration, `rift.sqlite3`, the
> messaging private key and anything under `local/`), write Rift's plugins and
> templates, and signal Rift.

To confine managed servers on Linux, set
[`isolation`](server-limits.md#isolation) on groups and managed servers.
Elsewhere, keep untrusted server plugins off the host, or run servers on
[nodes](nodes.md) under a separate account.

## Player addresses

Rift remembers every player who joined, in `rift.sqlite3` beside the
permission store: UUID, name, first and last seen, the last server, the
language a plugin set and the address the player last joined from. Plugins and
the configuration script read this player directory with
[`players.lookup` and `players.search`](plugin-events.md#the-player-directory-and-languages).

```lua
rift.config.players = { record_ip = false }
```

`record_ip` (default `true`) decides whether joins store the address. With
`false`, `last_ip` is empty: a join stores no address and clears the one
stored for that player before. The setting applies on reload.
