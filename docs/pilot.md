# Local release pilot

Run on 2026-09-28 on Linux x86_64 with 14 logical CPUs, Python 3.14.7 and the
pinned Rust 1.98.1 release build of Rift 0.1.0, extracted from the Linux release
archive. The
[raw report](pilot-results.json) includes the exact binary SHA-256, environment,
samples and counters. This is an automated loopback pilot with two Python TCP
backends and 16 simultaneous sessions; no external operators or real players
participated. It does not measure internet latency, authentication, gameplay
capacity or a long-running production workload.

## Reproduce

From a source checkout on Unix, using Python 3.12 or newer:

```sh
cargo build --release --locked
python3 scripts/package.py --platform linux-x86_64
mkdir -p target/pilot-release
tar xzf dist/rift-linux-x86_64.tar.gz -C target/pilot-release
python3 tests/pilot.py --binary target/pilot-release/rift --report target/pilot.json
```

Use `macos-aarch64` for packaging on an ARM Mac. RSS is recorded only on Linux.
The pilot binds ephemeral loopback ports, uses temporary configs, cleans up its
processes and needs no root privileges or server downloads. `--seconds 60`
extends the default ten-second soak. Do not run Python with `-O`; assertions
check correctness. CI runs the same pilot and retains its JSON with the benchmark
artifact; timing has no pass/fail threshold on shared runners.

## Workload and findings

Each buffer candidate uses a fresh proxy. Latency uses 100 warmup and 1000 timed
four-byte request/reply exchanges. Throughput is the median of three transfers:
64 MiB with one client, then 16 MiB each with 16 clients. Payload is counted once
although echo sends it in both directions. RSS is sampled with 16 open sessions
after those transfers and includes allocator/runtime memory; it excludes kernel
socket buffers and the Python processes.

| Buffer per direction | RTT p50 / p95 (µs) | One client (MiB/s) | 16 clients (MiB/s) | Rift RSS (KiB) |
| --- | --- | --- | --- | --- |
| 8 KiB | 52.8 / 185.4 | 398 | 366 | 5596 |
| 16 KiB | 55.4 / 103.9 | 482 | 356 | 6324 |
| 32 KiB | 72.5 / 252.5 | 543 | 350 | 7292 |
| 64 KiB | 62.2 / 186.1 | 562 | 408 | 8028 |

The operational phase deliberately lowers admission to 16 connections. It checks
that the seventeenth closes, an invalid reload preserves sessions, and a valid
reload switches new connections to a distinct backend while old connections keep
their original backend. During the ten-second soak it completed 9,104 checked
exchanges with zero connection errors or backend failures. Metrics recorded
exactly one capacity rejection, one rejected reload and one successful reload.

SIGTERM began a drain while all 16 connections were active. Existing traffic and
metrics remained usable until clients closed, and the process exited successfully
in 0.018 seconds without reaching its deadline. A separate run held a connection
open with a 250 ms shutdown timeout: Rift logged the deadline, closed the session
and exited successfully in 0.265 seconds. That short timeout only makes the pilot
quick; it is not the release default.

## Decisions applied to the release

| Setting | Release choice | Rationale and limits of the evidence |
| --- | --- | --- |
| `buffer_size` | Keep 32 KiB per direction | Larger buffers increased sampled RSS; throughput varied substantially. The small sweep does not justify doubling buffer memory or tuning for one host. |
| `max_connections` | Reduce 4096 to 1024 | The pilot verified admission at a small cap. Choose a conservative 64 MiB relay-buffer budget (`1024 × 2 × 32 KiB`) instead of 256 MiB. This is a sizing policy, not a measured 1024-player capacity. |
| `connect_timeout_ms` | Keep 5000 | Loopback cannot establish a useful WAN/DNS timeout; preserve that headroom. |
| `shutdown_timeout_ms` | Keep 30000 | Draining worked without interrupting active exchanges. Real players need more time than the synthetic clients, and long-lived sessions still need a deadline. |
| systemd `TimeoutStopSec` | 40 seconds | Allow the application deadline to finish before the service manager forces termination. |
| systemd `LimitNOFILE` | 8192 | Two sockets per relay at the default cap, with space for listeners, probes, metrics and the runtime. |
| Admission rates, health checks, cache and metrics | Remain opt-in | This pilot did not model shared NATs, backend probe cost or status freshness. The network example keeps explicit settings and loopback-only metrics. |

The basic example and built-in defaults now match the network example's 1024
connection cap. Existing explicit limits keep their values. Total memory exceeds
the relay-buffer budget due to the runtime, hooks, caches and kernel sockets;
measure those on the intended host before increasing capacity.

Packaging validates every shipped Lua example using the extracted executable and
checks its version against Cargo.toml. The systemd example was checked with
`systemd-analyze verify` using the built binary's path; no system service was
installed or exercised in this pilot. Follow the [operator guide](operations.md)
to verify readiness and reload acceptance on the target machine.
