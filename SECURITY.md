# Security policy

## Supported versions

Security fixes go into the newest release. Until 1.0 there are no maintained
older release lines: upgrade to the latest release to get a fix. Nightly
builds are for testing and get no separate fixes.

## Reporting a vulnerability

Report it privately, not in a public issue, pull request or discussion:
use [**Report a vulnerability**](https://github.com/Jotrorox/rift/security/advisories/new)
on the repository's Security tab. That opens a private advisory only the
maintainers can read.

Please include:

- the Rift version (`rift --version`) and platform;
- what an attacker needs (a network position, a player account, an operator
  token, a plugin, a managed server) and what they gain;
- steps or a proof of concept, with the relevant part of `rift.lua` and any
  secrets removed.

You can expect an acknowledgement within a week. Once the report is confirmed
we agree on a disclosure date with you, fix it in a release, publish the
advisory and credit you unless you prefer otherwise.

## Scope

In scope: the `rift` executable (proxy, `rift node` agent, admin endpoint, web
dashboard and HTTP API, message bus), the crates and SDKs in this repository,
the Java libraries and the release pipeline.

The trust boundaries Rift documents are part of that scope. Examples of
reports we want:

- a player or unauthenticated client reaching anything behind
  [authentication](docs/http.md#authentication-and-access), bypassing
  forwarding, or crashing or stalling the proxy;
- a scoped web operator or a bus principal acting outside its grants;
- a WebAssembly or script plugin escaping its sandbox or capabilities;
- a managed server with [`isolation`](docs/managed-servers.md#isolation)
  reading Rift's configuration, keys or another instance's files;
- a node agent or controller accepting commands from the wrong peer.

Not vulnerabilities in Rift:

- what a trusted party can do by design: `rift.lua` and persistent Lua
  plugins run with Rift's privileges, and an operator with the `config`
  permission controls the whole proxy;
- networks that run with `online_mode` off or forwarding without a secret,
  which `rift check` warns about;
- bugs in Minecraft servers, their plugins or Java itself.
