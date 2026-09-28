# Rift

A small TCP reverse proxy for Minecraft Java Edition. Rust, Tokio, one backend.
No Minecraft packet parsing: the backend handles login, encryption, compression,
and gameplay. Tokio is the only direct dependency; the binary needs no Java.

## Run

```sh
cargo build --release --locked
./target/release/rift 0.0.0.0:25565 127.0.0.1:25566
```

Players connect to port `25565`; Rift connects to the server on `25566`.
With no arguments, Rift uses those same addresses. Addresses must be IP literals
with ports; IPv6 works too: `rift '[::]:25565' '[::1]:25566'`.
Use `--help` for usage. Stop with Ctrl-C (active connections close).

For a backend on the same machine, set these in `server.properties`:

```properties
server-ip=127.0.0.1
server-port=25566
online-mode=true
prevent-proxy-connections=false
```

Keep Paper's BungeeCord/Velocity forwarding disabled. This is transparent TCP:
the backend sees Rift's IP address, with no player IP forwarding. There is no
multi-server routing, protocol translation, or Bedrock/UDP support.

## Implementation

- One async task per connection on Tokio's multithreaded runtime.
- `TCP_NODELAY` on both sockets for small-packet latency.
- Two reusable 32 KiB relay buffers per connection, with backpressure and half-close support.
- Five-second backend connection timeout. No idle timeout for established sessions.
- At most 4,096 active connections, including pending backend connections;
  excess clients are immediately closed. The OS file descriptor limit must allow
  two sockets per client plus headroom.
- No per-packet logging, serialization, shared traffic lock, or unbounded queue.

## Tests

```sh
cargo fmt --all --check
cargo test --locked --all-targets
cargo test --locked --all-targets --release
cargo clippy --all-targets --locked -- -D warnings
python3 -m unittest discover -s tests -p 'test_*.py' -v
python3 tests/minecraft.py --accept-eula --jobs 3
python3 tests/minecraft.py --accept-eula --jobs 3 --compression disabled
python3 tests/bench.py --report target/benchmark.json
```

Rust is pinned in `rust-toolchain.toml`; install it with rustup. The Python scripts
need Python 3.11+ and use only the standard library. Vanilla and Paper also need
Java 21; the pinned Pumpkin binary needs Linux x86_64. Server downloads require
internet access on the first run. `--accept-eula` accepts
the [Minecraft EULA](https://aka.ms/MinecraftEULA) for the local test servers.
CI passes this flag for its disposable fixtures. Servers bind only to loopback,
use offline mode, and stop with their proxies when testing finishes.

The harness pins the [official Mojang server](https://www.minecraft.net/en-us/download/server)
at **1.21.11** and [Paper](https://docs.papermc.io/misc/downloads-service/) at
**1.21.11 build 132**, plus [Pumpkin **0.2.0+26.3-26.51**](https://github.com/Pumpkin-MC/Pumpkin/releases/tag/0.2.0%2B26.3-26.51)
for Minecraft **26.3**. Versions, URLs and publisher checksums live in
[`tests/servers.json`](tests/servers.json); every download and cache hit is verified.
Each run creates a fresh world under `target/minecraft/runs/`, retains logs and a
`result.json`, and checks:

- Direct/proxied status equality and exact ping payloads.
- 64 status requests with 16 concurrent clients.
- Direct and proxied login, compression in both directions, configuration,
  teleport acknowledgement, world chunks, and two play keepalive exchanges.
- Recovery when the backend starts after Rift, and clean handling of its shutdown.

Use `--server paper` to select one backend (repeat the option to select several),
`--binary /path/to/rift` to test an existing build, and `--report path.json` to
collect results. On macOS or Windows, select `--server vanilla --server paper`.
`--jobs 3` runs the backends concurrently; the default of one limits local resource
use. Downloads are cached separately from worlds. Do not run Python with `-O`,
which disables test assertions; the scripts reject it.

Rust tests additionally check simultaneous bulk transfer byte for byte, slow
readers, fragmented concurrent sessions, half-closes in both directions, connection
refusal, and CLI startup failures. Microsoft account authentication and
encrypted gameplay are not exercised by the offline integration fixtures.

## CI and releases

[`CI`](.github/workflows/ci.yml) runs on pull requests, pushes to `master`, merge
queues, manual dispatch, and weekly. Its independent jobs run in parallel:

| Job | Required checks |
| --- | --- |
| Quality | Rust formatting, Clippy with warnings denied, offline harness tests, actionlint |
| Dependencies | `cargo audit --deny warnings` against the current RustSec database |
| Regression | Debug and release tests on Linux x86_64, macOS ARM64, Windows x86_64; package smoke tests |
| Minecraft | Six jobs: vanilla, Paper, Pumpkin × compression enabled/disabled |
| Benchmark | Transfer correctness plus downloadable latency/throughput measurements |

All matrix jobs finish even when a sibling fails. The final **CI ready** check fails
if any required job fails, is cancelled, or is skipped. Configure branch protection
to require **CI ready** and require pull requests before merging; workflow files
cannot enable repository protection by themselves. Fork PRs need no custom secrets
or write token. Actions are pinned by commit, Cargo uses `--locked`, and Dependabot
proposes weekly Cargo and action updates.

The Minecraft jobs upload reports, proxy/server logs and crash reports even on
failure. CI caches only verified server downloads, never test worlds. Logs,
benchmark reports, and native release packages are retained for 14 days.
Benchmark numbers are informational because shared runners have variable load;
functional assertions still fail the job. Compare JSON reports from repeated runs
on the same hardware before setting a performance budget.

[`Release`](.github/workflows/release.yml) runs when a `v*` tag is pushed. The tag
must exactly match `Cargo.toml` (for example, version `0.2.0` requires `v0.2.0`).
It reruns the entire CI workflow on that tag, then publishes the tested Linux,
macOS and Windows archives with `SHA256SUMS` to a GitHub Release. Only the final
publishing job receives write permission; it uses the workflow's built-in token.
This distributes the standalone proxy; running servers are managed separately.

When updating a server fixture, update its URL, version, checksum and protocol in
`tests/servers.json`, adjust `PROTOCOLS` and packet handling in `tests/minecraft.py`
against the upstream protocol/source, and run both compression modes locally.
Pumpkin's protocol source is pinned by `source_commit` in the fixture file. Keep
full gameplay checks required when upgrading; a status ping alone is insufficient.
Update `rust-toolchain.toml` explicitly when upgrading Rust, then run the full suite.

## Benchmark

The benchmark compares direct TCP with Rift using a local Python echo server.
It reports median/p95 round-trip latency and median throughput over three runs
with one and sixteen clients. Throughput counts echoed payload once, although it
travels in both directions. These are loopback measurements including Python
overhead, not a Minecraft player-capacity estimate.

Example release-build results from the development machine (Linux, 2026-09-28):

| Route | RTT median / p95 | 1 client | 16 clients combined |
| --- | --- | --- | --- |
| Direct | 12.5 / 19.5 µs | 2,180 MiB/s | 1,957 MiB/s |
| Rift | 32.0 / 36.8 µs | 1,200 MiB/s | 1,204 MiB/s |

That run added 19.5 µs to median round-trip latency. Results vary with the host
and other running workloads.
