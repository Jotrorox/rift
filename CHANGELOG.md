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

### Added

- `rift admin node plan` suggests moves that even out memory across nodes,
  from their `memory_mb` and the host memory their agents measure. Each move
  is a `rift admin move` command that passes the checks of a real move, with
  the players who would have to leave; `--spread <percent>` sets how far
  apart nodes may be (20 points by default). It only suggests: Rift still
  never moves a server on its own. Also at `GET /api/nodes/plan`. See
  [docs/nodes.md](docs/nodes.md#even-out-the-nodes).
- The instance database can live in PostgreSQL instead of the SQLite file:
  `rift.config.database = { backend = "postgres" }` with the connection URL in
  `RIFT_DATABASE_URL` (or `url_env`), optional `ca` for TLS and `schema` to
  share one database between networks. Every component (instances, group
  states, node schedules, permissions and their audit log, the player
  directory) works the same on both backends. One Rift owns a database at a
  time through an advisory lock; with `standby = true` a second controller
  waits and takes over when the first stops, adopting the nodes' running
  servers. `rift database copy` moves the data from SQLite to PostgreSQL and
  back, and `rift check --strict` checks a PostgreSQL database read-only. See
  [docs/database.md](docs/database.md).
- `extensions.storage = { database = true }` keeps the extension store in the
  instance database (SQLite, PostgreSQL or MariaDB) instead of a file of its
  own.
- MariaDB (10.5 or newer) works as the instance database too:
  `rift.config.database = { backend = "mariadb" }` with a `mariadb://` URL in
  `RIFT_DATABASE_URL`. It supports the same settings and features as
  PostgreSQL: TLS with `require_ssl=true` or `ca`, `schema` naming a database
  on the server, one owner at a time through a named lock with
  `standby = true` for a waiting second controller, `rift database copy` to
  and from SQLite, and `rift check --strict`. See
  [docs/database.md](docs/database.md#use-mariadb).
- Built-in backups of persistent instances on nodes. With
  `rift.config.backups` set, Rift snapshots each such instance every
  `interval_ms` (6 hours by default) into a folder on the proxy host or an S3
  bucket (`backups.s3`, also MinIO, R2 and other S3-compatible services) and
  keeps the newest `keep` snapshots. Files are stored by SHA-256, so after the
  first snapshot only changed files are uploaded. A running server's world is
  flushed with `save-off` and `save-all flush` and copied on its node first,
  so saving pauses only for that local copy. `rift admin backup <instance>`
  takes a snapshot now, `rift admin backups [instance]` lists them,
  `rift admin backups forget` deletes them, and
  `rift admin restore <instance> [snapshot] [--node <node>]` restores an
  instance in place or onto another node, which recovers the instances of a
  dead node in one command. Agents need the new `backup` feature: upgrade them
  together with the controller. See [backups](docs/backups.md).
- `tab_list.players = "group"` lists the players on the other instances of a
  player's service group, so a BedWars round shows every BedWars player but
  not the lobby. `tab_list.groups` limits this to some service groups; players
  elsewhere see only their own server, which stays the default. The
  dashboard's Tab list page offers it too. See
  [service group players](docs/server-list.md#service-group-players).
- `tab_list.names = "all"` gives the entries backends list themselves the
  same rank names and order as Rift's own entries for players on other
  servers, so a backend without a tab plugin (such as Pumpkin) shows
  prefixes and suffixes too. `tab_list.format` arranges the name, for example
  `"{prefix}{name}{suffix} &8{server}"`, with `{group}` for the primary
  group's display name. The dashboard's Tab list page edits both.
- `rift admin move <instance> <node>` moves a persistent instance to another
  node on purpose: it stops the instance, copies its folder between the node
  agents through the proxy with a SHA-256 check per file, records the new node
  in the instance database and starts it again. `--no-copy` re-homes an
  instance whose folder was restored on the new node, for example after its
  node died; `--keep-source` keeps the old folder. Instances never move
  automatically. Both agents must advertise the new `move` feature: upgrade them
  together with the controller.
- `rift admin move <server> <node>` also moves named servers of
  `managed_servers`, folder included. The server keeps its port, and the move
  is refused when that port is taken on the new node. The configuration file
  is not edited: the instance database remembers the new node and backend
  across reloads and restarts until the configuration changes the server's
  `node` or backend, for example to the new values. `rift admin servers`
  shows the node the configuration still names as `configured_node`. See
  [docs/nodes.md](docs/nodes.md#move-a-named-server).
- Moves can be started and followed from the dashboard: servers on nodes have
  a Move button that asks for the new node, and the server list shows the
  copy's progress, rate and time left, with a button to cancel it. The web API
  has `POST /api/servers/{name}/move` (an operation to poll) and
  `DELETE /api/servers/{name}/move` to cancel, and `/api/servers` and
  `rift admin servers` report a running move under `move`.
  `rift admin move <server> --cancel` cancels from the command line.
- An interrupted move resumes instead of starting over. When a copy fails,
  is cancelled or the proxy stops during it, the files that arrived stay on
  the new node, and the next move of the same server to that node sends only
  the files that are missing or changed since; it recognises a copied file by
  its size and modification time, which moved files now keep. What the source
  deleted meanwhile is dropped. Both agents need the new `move_resume`
  feature; with older agents a move starts over as before.
- Plugins can ship their own messages in a `messages.toml` beside
  `plugin.toml` or `init.lua`, with translations under `[languages.<tag>]`.
  A `message` object, `{ message = "invited", args = { from = name } }`,
  sends one wherever text goes and is rendered in the language of the player
  who sees it; one `broadcast` reaches every player in their own language.
  Operators reword a plugin's message in `text.messages` and `text.languages`
  by its qualified key, such as `["party:invited"]`, and `rift:queue.full`
  names one of Rift's own messages from a plugin or `rift.lua`. `rift check`
  checks every template and replacement without running plugin code and
  prints each plugin's message and language counts. See
  [plugin messages](docs/text.md#plugin-messages).
- A markup or message argument can be a style, `{ style = "<#55ffff><bold>" }`,
  that wraps whatever the template puts between `<team>` and `</team>`, so an
  operator who rewords a message keeps a color the plugin computes. See
  [style arguments](docs/text.md#style-arguments).
- Markup has `<transition:red:blue:0.5>` for one color along the stops, a
  phase on `<gradient>` to animate it, `<rainbow:!>` for a reversed rainbow,
  and `<sprite:item/diamond>` and `<head:Notch>`, which clients from 1.21.9
  show. See [markup tags](docs/text.md#markup-tags).
- The plugin contract `rift:plugin@0.2.1` adds the `text` interface:
  `render`, `plain`, `to-legacy` and `to-mini` convert text with markup,
  legacy codes and messages resolved in a given language, for bus payloads,
  HTTP responses and logs, or to check a configured template in `init`. A
  plugin that calls it sets `min_api = "0.2.1"`; components built against
  0.2.0 keep loading. Lua gets `rift.text.render(text, language)`,
  `rift.text.message(key, args)` and an optional `language` on
  `rift.text.plain`, `to_legacy` and `to_mini`. See
  [the host API](docs/plugins.md#host-api) and
  [`rift.text`](docs/text.md#rifttext-in-lua).

### Changed

- **Breaking for plugins:** the plugin contract is `rift:plugin@0.3.0`, which
  adds [inventory menus](docs/menus.md) for WebAssembly plugins:
  `players.menu`, `update-menu` and `close-menu`, and the `menu` hook in
  `handler` with its `menu-event`. A new hook is a breaking change, so Rift
  now loads only `api = "0.3"` plugins, for every engine, and refuses a 0.2
  manifest with a pointer to the guide. The move needs no change in a
  plugin's source: persistent Lua plugins set `api = "0.3"`, WebAssembly
  plugins also rebuild against a 0.3.0 SDK. See
  [Moving from 0.2](docs/plugins.md#moving-from-02).
- A markup argument now wins over a theme style of the same name. Before, the
  theme style was applied and the argument never shown, so a plugin's
  `<info>` argument vanished on a network whose theme has an `info` style.
- `<rainbow>` takes a phase from -1 to 1 instead of 0 to 1.
- `transition`, `sprite` and `head` are built-in tags now, so a theme style
  or a markup argument can no longer have one of these names.
- `text.messages` and each `text.languages` table hold up to 1024 entries
  instead of 256, since replacements for plugin messages count too.
  `text.theme` stays at 256 styles.

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
- Agents report their host's available memory, CPU count and load, and the
  free disk below `--root` in every state snapshot. Placement skips a node
  whose host has less available memory than an instance needs, uses the
  smaller of unallocated `memory_mb` and real available memory to rank
  `least_memory`, and skips a node with less free disk than its new
  `min_free_disk_mb` (default 1024, `0` disables). `/api/nodes`,
  `rift.control.nodes` and the dashboard's Nodes panel show the measurements,
  and a failed placement names why each node was skipped.
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
- The Files page and `/api/files/servers` read and write files of up to
  64 MiB on node servers, instead of 512 KiB. Larger files cross the message
  bus in 512 KiB parts; uploads are staged beside the target and renamed into
  place after the last part. Uploading files over 512 KiB needs an agent
  that advertises the `file_parts` feature; the controller refuses them for
  older agents with `413`.
- `rift admin warm [template]`, `POST /api/nodes/warm` and a dashboard button
  have every node download its groups' templates and server software ahead of
  time, so the first instance on a new or freshly synced node starts without
  waiting. Agents need the new `warm` feature; the controller tells older
  agents to upgrade instead of sending the command.

### Integration and operations

- `web.tls` serves the dashboard and API over HTTPS on `web.listen`, with the
  messaging certificate (`tls = true`) or its own files, so remote nodes need
  no reverse proxy for templates. The certificate is reloaded on SIGHUP.
- `/api/files/` requests may take two minutes instead of ten seconds.

### Database

- New `rift.sqlite3` component: node schedules, schema 1. Older releases
  ignore it.
- New component `extension_store` (table `extension_entries`), created only
  with `extensions.storage = { database = true }`. Older releases ignore it.
- New component `moved_servers`, schema 1: where `rift admin move` put named
  servers. Older releases ignore it and run those servers where the
  configuration says, without their folder there.

### SDKs

- Rust `rift-plugin` 0.3.0 implements `rift:plugin@0.3.0`. Its `menu` module
  builds menus, patches and buttons (`Menu::chest(id)`, `hopper`,
  `dispenser`, `anvil`, `Button::new(item)`, `MenuPatch`) for
  `players::show_menu`, `players::update_menu` and `players::close_menu`, and
  `Plugin::menu` receives a `MenuEvent` whose `kind()` is a typed
  `MenuKind`, with `payload::<T>()` and `state::<T>()` to decode the JSON.
- Rust `rift-plugin` 0.2.1, Go `github.com/Jotrorox/rift/sdk/go` v0.2.1 and
  the C SDK implement `rift:plugin@0.2.1`. They build messages
  (`text::message`, `rift.Msg`, `rift_msg`), style arguments and escaped
  markup, and components with translations (arguments and a fallback), key
  bindings, sprites, heads, a font, a shadow, a decoration turned off,
  `change_page` clicks and item and entity hovers. They wrap the `text`
  interface: `text::render`, `plain`, `to_legacy` and `to_mini` in Rust,
  `rift.RenderText`, `PlainText`, `ToLegacy` and `ToMini` in Go, and
  `rift_text_render`, `rift_text_to_plain`, `rift_text_to_legacy` and
  `rift_text_to_mini` in C. See [Rust](docs/plugins-rust.md#text),
  [Go](docs/plugins-go.md#text) and [C](docs/plugins-c.md#rich-text-builders).

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
