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
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
python3 tests/minecraft.py --accept-eula
python3 tests/bench.py
```

The Python scripts use only the standard library. The Minecraft integration test
also needs Java 21 and internet access on the first run. `--accept-eula` accepts
the [Minecraft EULA](https://aka.ms/MinecraftEULA) for the local test servers.
Jars, worlds, and logs stay under ignored `target/minecraft/`; both servers and
the proxy are stopped when the test ends.

The harness pins the [official Mojang server](https://www.minecraft.net/en-us/download/server)
at **1.21.11** and [Paper](https://docs.papermc.io/misc/downloads-service/) at
**1.21.11 build 132**, verifying the publishers' SHA-1/SHA-256 checksums.
It runs them one at a time on loopback in offline mode, checking:

- Direct/proxied status equality and exact ping payloads.
- 64 status requests with 16 concurrent clients.
- Direct and proxied login, compression in both directions, configuration,
  teleport acknowledgement, world chunks, and a play keepalive exchange.
- Recovery when the backend starts after Rift, and clean handling of its shutdown.

Rust tests additionally check simultaneous bulk transfer byte for byte, half-closes
in both directions, and connection refusal. Microsoft account authentication and
encrypted gameplay are not exercised by the offline integration fixtures.

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
