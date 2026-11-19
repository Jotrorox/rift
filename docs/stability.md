# Stability and versioning

What a Rift version number promises, which interfaces it covers and what an
upgrade may change. Changes are recorded in the [changelog](../CHANGELOG.md).

## Release channels

| Channel | Tag | Use |
| --- | --- | --- |
| Stable | `v0.1.0` | Production. Marked as the latest release. |
| Release candidate | `v0.1.0-rc.1` | The stable pipeline, published as a prerelease to test a release before it is final. |
| Nightly | `nightly-daily-…`, `nightly-weekly-…` | The tested tip of `master`. No compatibility promise between two nightlies. |

Every channel passes the same CI and publishes the same archives with
`SHA256SUMS` and build provenance attestations. Stable releases and release
candidates also publish the Java artifacts.

## Version numbers

Rift follows [Semantic Versioning](https://semver.org/). It is before 1.0, so
the minor version carries the compatibility promise:

- A **patch** release (`0.1.0` to `0.1.1`) fixes bugs and security issues. It
  keeps every interface below compatible and does not change the database
  schema.
- A **minor** release (`0.1` to `0.2`) may change or remove an interface. The
  changelog lists each such change with what to do about it. An interface is
  marked deprecated for at least one minor release before it is removed,
  unless keeping it would be a security problem.
- Security fixes go to the latest stable release only.

## What is covered

| Interface | Version | Rule |
| --- | --- | --- |
| Lua configuration (`rift.lua`) and `extensions` hooks | `extensions.api_version` | A configuration that passes `rift check` on one release passes on later patch releases with the same behavior. |
| Command line and `rift admin` | Rift's version | Commands, flags and exit codes are kept. Human-readable output is not an interface; use the JSON forms in scripts. |
| Plugin contract | `rift:plugin@0.2.x` and `api = "0.2"` in `plugin.toml` | Additive on the `0.2` track: a component built against any `0.2` version loads on every later one. See [plugins](plugins.md). |
| Messaging protocol | 1 | Additive within a protocol version. See the [messaging protocol](messaging-protocol.md). |
| Permissions protocol | 1 | Additive within a protocol version. See the [permissions protocol](permissions-protocol.md#versioning). |
| Node agent protocol | Features the agent advertises | Additive: commands and fields are only added. The controller uses an addition only on agents that advertise its feature. See [controller and node agents](#controller-and-node-agents). |
| HTTP API | Rift's version | Documented endpoints and fields are kept; fields may be added. See the [HTTP API](http-api.md). |
| Java artifacts (`io.github.jotrorox.rift`) | Rift's version | Source and binary compatible across patch releases. |
| Rust, Go and C SDKs | The plugin contract version they implement | Source compatible on the `0.2` track. |

Not covered:

- log text
- the dashboard's markup
- metric label order
- the layout of files under `local/` and `temp/`
- internal Rust APIs of the `rift` crate
- anything a document calls experimental

## Controller and node agents

The controller and its [`rift node` agents](nodes.md) run the same binary, but
they do not have to be replaced at the same moment. Agents may be one release
apart from the controller while an upgrade rolls through the nodes:

| Agent | Supported | What works |
| --- | --- | --- |
| Same release as the controller | Yes | Everything |
| Older, from the same or the previous minor release | Yes | Everything the agent advertises. The rest is refused for that node, with a message that names it |
| Newer, from the same minor release | Yes, for example after rolling the controller back | Everything the controller knows |
| From a newer minor release than the controller | No | Roll the agents back as well |

Agents advertise features, not just a version, and the controller checks
features:

- Each agent advertises the [features](nodes.md#agents-older-than-the-controller)
  it supports in every state snapshot. The controller checks the feature
  before it sends a start setting or command that needs it, so it never
  relies on an agent's version.
- An operation that needs a missing feature fails on that node only, before
  anything changes, and says to upgrade the agent. Examples are a move, a
  backup, warming, a large file upload or a server with a bus login. Other
  operations keep working.
- New agent capabilities arrive as new features, in patch and minor releases
  alike.
- A different version is reported as `version_mismatch` in `GET /api/nodes`
  and flagged in the dashboard, as a reminder to finish the upgrade.
  `rift check --strict` reports agents that would refuse servers in the
  configuration.
- Agents from 0.1.0 advertise no features at all.
- Between two nightlies, features still protect each operation, but there is
  no compatibility promise.

Upgrade the controller first, then the agents one node at a time, as described
in [upgrade nodes](upgrading.md#upgrade-nodes).

## Database

`rift.sqlite3` only moves forward:

- A release that needs a new schema migrates the file at its first start, in
  one transaction.
- There is no downgrade. An older binary refuses a newer schema and does not
  start.
- Patch releases never change the schema. A minor release that does says so
  in the changelog.

Copy the file before every upgrade, as described under
[upgrade and rollback](upgrading.md).

## Minecraft versions and platforms

The Minecraft versions in the [README](../README.md) are tested against real
servers in CI. Support for a new Minecraft release arrives in a patch or minor
release. Dropping a Minecraft version, dropping a platform or raising the
minimum operating system version happens only in a minor release.

Each release ships these platforms:

- Linux x86_64 and aarch64, each as a glibc build and a statically linked
  musl build (see [which Linux build](install.md#which-linux-build))
- macOS aarch64
- Windows x86_64
- a container image for `linux/amd64` and `linux/arm64` (see
  [run in a container](deployment.md#run-in-a-container))

Building from source needs the Rust toolchain pinned in
`rust-toolchain.toml`, which may move in any release.
