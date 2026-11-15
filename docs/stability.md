# Stability and versioning

What a Rift version number promises, which interfaces it covers, and what an
upgrade may change. Changes are recorded in the
[changelog](../CHANGELOG.md).

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
- A **minor** release (`0.1` to `0.2`) may change or remove an interface. Each
  such change is listed in the changelog with what to do about it. An
  interface is marked deprecated for at least one minor release before it is
  removed, unless keeping it would be a security problem.
- Security fixes go to the latest stable release only.

## What is covered

| Interface | Version | Rule |
| --- | --- | --- |
| Lua configuration (`rift.lua`) and `extensions` hooks | `extensions.api_version` | A configuration that passes `rift check` on one release passes on later patch releases with the same behaviour. |
| Command line and `rift admin` | Rift's version | Commands, flags and exit codes are kept. Human-readable output is not an interface; use the JSON forms in scripts. |
| Plugin contract | `rift:plugin@0.2.x` and `api = "0.2"` in `plugin.toml` | Additive on the `0.2` track: a component built against any `0.2` version loads on every later one. See [plugins](plugins.md). |
| Messaging protocol | 1 | Additive within a protocol version. See the [messaging protocol](messaging-protocol.md). |
| Permissions protocol | 1 | Additive within a protocol version. See the [permissions protocol](permissions-protocol.md#versioning). |
| HTTP API | Rift's version | Documented endpoints and fields are kept; fields may be added. See [HTTP](http.md). |
| Java artifacts (`io.github.jotrorox.rift`) | Rift's version | Source and binary compatible across patch releases. |
| Rust, Go and C SDKs | The plugin contract version they implement | Source compatible on the `0.2` track. |

Not covered: log text, the dashboard's markup, metric label order, the layout
of files under `local/` and `temp/`, internal Rust APIs of the `rift` crate,
and anything a document calls experimental.

## Database

`rift.sqlite3` only moves forward. A release that needs a new schema migrates
the file at its first start, in one transaction. There is no downgrade: an
older binary refuses a newer schema and does not start. Patch releases never
change the schema; a minor release that does says so in the changelog. Copy
the file before every upgrade, as described under
[upgrade and rollback](operations.md#shutdown-upgrade-and-rollback).

## Minecraft versions and platforms

The Minecraft versions in the [README](../README.md) are tested against real
servers in CI. Support for a new Minecraft release arrives in a patch or minor
release. Dropping a Minecraft version, a platform or raising the minimum
operating system version happens only in a minor release.

Release executables are built for Linux x86_64, macOS aarch64 and Windows
x86_64. Building from source needs the Rust toolchain pinned in
`rust-toolchain.toml`, which may move in any release.
