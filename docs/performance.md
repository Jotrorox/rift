# Performance against Velocity

Rift and Velocity run one after the other against the same synthetic Minecraft
backend on the same machine, under the same offered workload. The comparison
records proxy CPU, resident memory, latency, throughput and failures. It is a
loopback protocol benchmark, not a player capacity estimate.

## Results

Rift used less CPU and memory in every profile. Echo latency ranges overlap, so
these runs do not rank the proxies on echo latency. Both proxies completed all
**108,288 measured operations with zero failures**.

Two outliers are worth noting. One 64-client Rift login trial had a **135.73 ms
p99** (135.99 ms maximum) from a single slow 64-login wave. One Velocity
64-client warmup missed **7 of 115,200** scheduled echoes; warmup is excluded
from the tables but kept in the raw reports. These measurements cannot tell
whether the outliers came from the proxy, the load generator or the host.

Each profile ran three trials per proxy: 2,048 warmup logins, 30 seconds of echo
warmup, 20 seconds of measured echoes, then 2,048 measured logins in waves the
size of the client count.

### 16 concurrent clients

16 clients, 20 echoes/s/client, 1024 payload bytes, 3 trials.

Medians of per-trial values; parentheses show minimum–maximum. Failure counts pool all measured trials.

| Workload | Proxy | Mean CPU % | Peak RSS MiB | p50 ms | p95 ms | p99 ms | Successes/s | Failed/attempted |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| echo | Rift | 1.45 (1.35–1.65) | 8.86 (8.62–8.99) | 1.73 | 2.58 (2.44–2.75) | 3.01 (2.87–3.12) | 319.83 | 0/19,200 (0.00%) |
| echo | Velocity | 2.40 (2.35–2.40) | 354.40 (349.92–359.96) | 1.74 | 2.59 (2.24–2.64) | 3.04 (2.53–3.12) | 319.82 | 0/19,200 (0.00%) |
| login | Rift | 54.00 (53.77–54.75) | 8.86 (8.62–8.99) | 2.66 | 3.17 (3.09–3.27) | 3.38 (3.37–4.08) | 4488.75 | 0/6,144 (0.00%) |
| login | Velocity | 208.12 (207.69–215.69) | 349.27 (345.22–353.46) | 3.57 | 4.44 (4.12–5.11) | 5.25 (4.48–5.93) | 3411.67 | 0/6,144 (0.00%) |

[Raw JSON, gzip compressed](performance/2026-09-30-loopback-16.json.gz).

### 64 concurrent clients

64 clients, 20 echoes/s/client, 1024 payload bytes, 3 trials.

Medians of per-trial values; parentheses show minimum–maximum. Failure counts pool all measured trials.

| Workload | Proxy | Mean CPU % | Peak RSS MiB | p50 ms | p95 ms | p99 ms | Successes/s | Failed/attempted |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| echo | Rift | 4.15 (4.10–4.40) | 10.15 (9.80–10.52) | 3.50 | 4.67 (4.56–4.74) | 5.37 (5.29–5.43) | 1279.17 | 0/76,800 (0.00%) |
| echo | Velocity | 5.85 (5.55–6.20) | 392.15 (385.08–406.56) | 3.55 | 4.77 (4.69–4.90) | 5.47 (5.28–5.56) | 1279.23 | 0/76,800 (0.00%) |
| login | Rift | 58.30 (45.91–63.82) | 10.15 (9.80–10.52) | 8.96 | 12.17 (10.39–13.76) | 12.57 (10.82–135.73) | 5232.61 | 0/6,144 (0.00%) |
| login | Velocity | 215.78 (214.42–216.95) | 380.96 (367.83–384.95) | 11.81 | 14.06 (13.92–14.15) | 18.83 (18.40–23.34) | 4171.63 | 0/6,144 (0.00%) |

[Raw JSON, gzip compressed](performance/2026-09-30-loopback-64.json.gz).

Also published: [artifact checksums and settings](performance/2026-09-30-index.json),
[process logs and generated configurations](performance/2026-09-30-process-logs.tar.gz)
and [validation evidence](performance/2026-09-30-validation.json).

## Setup

Measured September 30, 2026 on a shared developer desktop, not an isolated lab:
Intel Core Ultra 5 125U (14 logical CPUs), 15.1 GiB RAM, Debian kernel
`7.2.6+deb14-amd64`, Python 3.14.7. No builds or tests ran during measurement.

| | Rift | Velocity |
|---|---|---|
| Version | commit `c1768262`, locked release build, Rust 1.98.1 | 4.2.0 build 30 on [Temurin 25.0.4.1+1](https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.4.1%2B1/OpenJDK25U-jre_x64_linux_hotspot_25.0.4.1_1.tar.gz) |
| Workers | `TOKIO_WORKER_THREADS=2` | `-XX:ActiveProcessorCount=2 -Dio.netty.eventLoopThreads=2` |
| Other | | `-Xms1024M -Xmx1024M` |

The worker settings match event-loop threads only; neither process is limited
to two CPUs. The Velocity jar is pinned by SHA-256 in
[tests/velocity.json](../tests/velocity.json) and downloaded from the
[PaperMC downloads service](https://docs.papermc.io/misc/downloads-service/).
Raw reports record executable hashes, Java release metadata, commands and
generated configuration.

Both proxies run the same workload:

- Minecraft Java 1.8.9 (protocol 47), offline login, one backend.
- No player forwarding, encryption, compression, plugins or Lua callbacks.
- Login and packet rate limits disabled. Velocity connection logging, BungeeCord
  channel handling and bStats are disabled; Rift has no metrics listener.
- The same Python backend and load generator over loopback TCP with
  `TCP_NODELAY` and a 1460-byte MSS.
- Each client logs in, receives Join Game, then exchanges plugin messages; every
  echo is checked against its payload.

Velocity enables compression and login throttling by default; they are disabled
here to compare plain forwarding. See the
[Velocity configuration reference](https://docs.papermc.io/velocity/configuration/).

## Reproduce

Requires Linux, Python 3.11+, the pinned Rust toolchain and Java 25. Stop other
builds and tests before measuring.

```sh
cargo build --release --locked
python3 scripts/fetch_velocity.py

RIFT_BENCH_JAVA=/path/to/java25/bin/java
for clients in 16 64; do
  python3 tests/compare_velocity.py \
    --binary target/release/rift \
    --velocity-jar target/velocity/velocity-4.2.0-30.jar \
    --java "$RIFT_BENCH_JAVA" \
    --trials 3 --clients "$clients" --burst-size "$clients" \
    --warmup 30 --duration 20 --rate 20 --payload-bytes 1024 \
    --attempts 2048 --timeout 5 --workers 2 --java-heap-mib 1024 \
    --report "target/comparison-$clients.json" \
    --work-dir "target/comparison-$clients-logs"
  python3 scripts/render_performance.py "target/comparison-$clients.json"
done
```

`fetch_velocity.py` verifies the cached jar on every run, and the comparison
verifies it again before starting. `--work-dir` must not exist yet; it keeps
every process log and generated configuration.

Request failures are recorded as results, not errors. Add `--require-success` to
exit nonzero on any failed request. Infrastructure or configuration errors abort
the run, and the renderer refuses the resulting incomplete report.

CI runs the same comparison with two clients, one trial and one second each of
warmup and measurement, and uploads its report and logs. It checks correctness
only; its numbers are not performance results.

## Reading the measurements

**CPU** is proxy user plus system time from `/proc` over the measured interval.
100% is one logical CPU. The table shows the mean for each trial, not peak.
Login bursts are much shorter than the 20-second echo windows, so their CPU
figures describe short bursts. Counters advance in 10 ms steps. Backend and
load-generator CPU are recorded separately in the raw reports.

**Memory** is peak sampled RSS in MiB, including JVM native memory. It includes
memory retained from startup and warmup, and sampling can miss short spikes.

**Latency** uses nearest-rank percentiles over successful operations. Echo
latency runs from the scheduled send slot to the verified reply, so it includes
load-generator dispatch delay; raw reports also record RTT and dispatch delay
separately. Login latency runs from TCP connect to Join Game plus one verified
echo.

**Failures** include timeouts, rejected connections, incorrect echoes and missed
send slots. Each client has at most one outstanding echo, and a failed session
counts all its remaining slots as failures, so offered load never drops. Read
CPU alongside throughput and failures: doing less work uses less CPU.

Each trial starts fresh proxy and backend processes, and the proxy order
alternates between trials. Treat the results as medians with ranges, not
statistically significant rankings.

## Limitations

The fixture covers login, the transition to play and plugin-message forwarding.
It does not cover chunk traffic, movement, tick scheduling, configuration
phases, signed chat, authentication, encryption, compression, backend switching
or plugins, and loopback is not a real network. Fixed-rate tests measure neither
maximum throughput nor maximum player count.

The single Python load generator may limit login throughput. JIT, garbage
collection, CPU frequency scaling, scheduling and other applications all add
variance. Compare only results with matching workload and runtime settings, and
test on deployment hardware before making capacity decisions.
