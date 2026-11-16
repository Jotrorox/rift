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

- The Files page and `/api/files/servers` read and write files of up to
  64 MiB on node servers, instead of 512 KiB. Larger files cross the message
  bus in 512 KiB parts; uploads are staged beside the target and renamed into
  place after the last part. Uploading files over 512 KiB needs an updated
  agent; older agents refuse them with `413` rather than writing part of the
  file.
- `/api/files/` requests may take two minutes instead of ten seconds.

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
