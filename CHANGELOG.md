# Changelog

Notable changes to Rift, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow the
[stability policy](docs/stability.md). Every release that changes the
`rift.sqlite3` schema says so under **Database**, because an older binary
refuses a newer schema: see
[upgrade and rollback](docs/upgrading.md).

The SDKs are versioned on their own, after the plugin contract they implement,
and are listed under **SDKs**.

## [Unreleased]

### Nodes

- `rift.config.nodes` changes apply on reload instead of needing a restart.
  Added nodes may log in at once, removed nodes lose their bus session, and
  labels, `memory_mb`, `max_instances` and `token_env` change in place. A
  reload that would remove a node still running servers, or change its
  address under them, is refused and names the servers.
- The controller checks which start settings a node agent applies before it
  starts a server there. It refuses servers whose `env`, `inherit_env`, bus
  login, `isolation = "required"` or `required` `memory_limit` the agent would
  silently drop, starts best-effort settings without and warns, and places new
  group instances only on agents that can run them. Agents from 0.1.0 report
  no such settings: upgrade them together with the controller.
- Outdated or mismatched agents are logged on connect, flagged in the
  dashboard's Nodes panel, reported by `GET /api/nodes` (`features`,
  `missing_features`, `version_mismatch`) and checked by
  `rift check --strict` against a running controller.
- Take a node out of service before maintenance:
  `rift admin node cordon|drain|uncordon <node>` and
  `POST /api/nodes/{name}/cordon|drain|uncordon`. A cordoned node gets no new
  instances. A draining node also stops sending new players to its
  disposable instances and retires each one once it is empty, while
  replacements start on other nodes. Persistent instances and named servers
  stay. `/api/nodes` reports each node's `schedule` and `drained`.
- The controller hands each node the forwarding secret (Velocity secret or
  BungeeGuard token) in its `prepare` commands, sealed with AES-256-GCM under a
  key derived from that node's token. Nodes no longer need
  `RIFT_FORWARDING_SECRET`; if it is still set and differs, the agent uses the
  controller's secret and logs a warning. It remains a fallback for older
  controllers, and agents advertise the `forwarding_secret` feature.
- `rift node init <name>` prints everything needed to add a node: its token,
  `rift.lua` entry, environment file and a systemd unit with
  `KillMode=control-group` and `Delegate=memory`. It checks offline that the
  messaging certificate names the address the agent dials and that the web
  API is reachable over HTTPS, and lists the ports to open.

### Integration and operations

- `web.tls` serves the dashboard and API over HTTPS on `web.listen`, with the
  messaging certificate (`tls = true`) or its own files, so remote nodes need
  no reverse proxy for templates. The certificate is reloaded on SIGHUP.

### Database

- New `rift.sqlite3` component: node schedules, schema 1. Older releases
  ignore it.

## [0.1.0] - 2026-11-16

The first stable release, after three release candidates (`v0.1.0-rc.1` to
`v0.1.0-rc.3`) published as prereleases from the same pipeline.

### Proxy

- Hostname routing through one port, backend DNS resolution, PROXY protocol
  v1/v2 from trusted load balancers and structured connection diagnostics.
- Online authentication with Velocity modern forwarding and BungeeCord
  forwarding, for every supported client version.
- Server switching, hubs, queues and recovery when a backend fails or kicks a
  player, for Minecraft 1.8.9 to 26.3.
- Protocol translation between Minecraft versions from 1.12 to 26.3.
- A proxy-owned server list (MOTD, favicon, status hook), tab list header and
  footer, a network-wide player list, boss bars, dialogs and styled text with a
  message catalogue.
- BungeeCord plugin-message channel compatibility.

### Configuration and plugins

- Validated Lua configuration (`rift.lua`), `rift check`, atomic reloads with
  history and rollback.
- One plugin contract (`rift:plugin@0.2.0`) with script, persistent Lua,
  WebAssembly and remote execution modes; login, transfer, chat and kick
  hooks; commands with suggestions, aliases and a console sender; background
  workers for slow lookups; per-plugin storage quotas.
- A player directory (name, UUID, first and last seen, address, last server)
  for plugins, and a per-player language that selects Rift's own messages.
- A network-wide permission store with groups, tracks, contexts, a LuckPerms
  import and `/server` access by permission node.

### Managed servers

- On-demand local servers, service groups with scaling and bounded recovery,
  templates with downloaded server software, persistent and ephemeral instance
  storage, Landlock confinement and per-instance bus logins.
- `rift node` agents to run game servers on more hosts.
- `memory_limit` on managed servers and service groups: a hard memory limit
  on a server's whole process tree, enforced on Linux with a cgroup v2 group
  per server and applied by node agents too. `{ mb = ..., required = true }`
  refuses to start a server whose limit cannot be enforced.
- `template_sources`: template folders checked out from a git repository
  into `template_storage`, fetched on first use and updated with
  `rift admin sync <source>` or `POST /api/template-sources/{name}/sync`.

### Integration and operations

- A message bus over QUIC with durable streams, shared by Rust, Lua and
  server plugins; typed Java APIs for permissions and the network.
- An embedded web dashboard and HTTP API with scoped operator tokens and audit
  records; a shareable status page. `web.token_env` and
  `web.operators.<name>.token_env` read tokens from environment variables.
- Release archives and standalone executables for Linux x86_64 and aarch64
  (glibc and musl), macOS aarch64 and Windows x86_64 with SHA-256 checksums
  and build provenance attestations.
- A container image for `linux/amd64` and `linux/arm64` at
  `ghcr.io/jotrorox/rift`, with build provenance.
- A hardened example systemd unit that delegates its cgroup so memory limits
  work, and `SECURITY.md` with the vulnerability reporting process.
- Documentation in short, task-focused pages, shipped in every archive and
  published as a website with a page index and a page filter. The site build
  fails on a broken link.

### Database

- `rift.sqlite3` instance schema 2, service groups schema 1, bus logins
  schema 1, permissions schema 1 and player directory schema 1. Files written
  by earlier development builds are migrated at the first start.

### SDKs

- Rust `rift-plugin` 0.2.0 on [crates.io](https://crates.io/crates/rift-plugin),
  git tag `sdk/rust/v0.2.0`.
- Go `github.com/Jotrorox/rift/sdk/go` v0.2.0: git tag `sdk/go/v0.2.0`.
- C SDK in `sdk/c` of the release tag.
- Java `io.github.jotrorox.rift` artifacts at the proxy's version, from
  `https://jotrorox.github.io/rift/maven` and GitHub Packages.

[Unreleased]: https://github.com/Jotrorox/rift/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Jotrorox/rift/releases/tag/v0.1.0
