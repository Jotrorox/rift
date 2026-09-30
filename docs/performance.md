# Performance against Velocity

The comparison runs Rift and Velocity sequentially against the same synthetic
Minecraft backend on the same machine. It measures proxy process CPU, resident
memory, successful-operation latency, delivered throughput and failures under
the same offered workload. It is a loopback protocol benchmark, not a player
capacity estimate.

## Published measurements

Rift used less process CPU and resident memory in these runs. The echo p95
ranges overlap; these measurements do not establish a meaningful echo-latency
ranking. Both proxies completed **108,288 measured operations each with zero
failures** across the two profiles.

Variability matters: one 64-client Rift login trial had a **135.73 ms p99**
(135.99 ms maximum), including a whole 64-login wave above 100 ms. One Velocity
64-client warmup missed **7 scheduled echoes** out of 115,200 warmup echoes for
that profile across all three trials. Those warmup failures remain in the raw
reports but are excluded from measured-phase totals below. The measurements
cannot identify whether the outliers originated in the proxy, driver or host.

Measured September 30, 2026. Each profile uses three trials per proxy, 30 seconds
of active echo warmup, 20 seconds of measured echo traffic, and 2,048 measured
logins in waves matching the client count. A separate 2,048-login warmup precedes
echo warmup. The offered echo rates are 320/s and 1,280/s, respectively.

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

[Artifact checksums and settings](performance/2026-09-30-index.json) and
[process logs and exact configurations](performance/2026-09-30-process-logs.tar.gz)
accompany the raw samples. Regenerate either table with
`python3 scripts/render_performance.py docs/performance/2026-09-30-loopback-16.json.gz`
(or the 64-client report).

The September 30, 2026 run uses an Intel Core Ultra 5 125U with 14 logical CPUs,
15.1 GiB RAM, Debian Linux kernel `7.2.6+deb14-amd64`, and Python 3.14.7. This is a
shared developer desktop with ordinary applications running; it is not an
isolated performance lab. No compilation or test suites run during the measured
windows. Both products inherit the same CPU affinity (logical CPUs 0–13).

Rift is the locked release build of source commit
`c1768262f010f331fec30ebe2f02f3417fa7cce3`, using Rust 1.98.1. The working tree
contains the added benchmark, tests and documentation; the proxy source is
unchanged. Velocity is **4.2.0 build 30**, using **Temurin 25.0.4.1+1** with
`-Xms1024M -Xmx1024M -XX:ActiveProcessorCount=2 -Dio.netty.eventLoopThreads=2`.
Rift uses `TOKIO_WORKER_THREADS=2`. These settings match event-loop workers;
they do not cap either process to two CPUs or equalize auxiliary threads.

The Java runtime came from the official
[Temurin release archive](https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.4.1%2B1/OpenJDK25U-jre_x64_linux_hotspot_25.0.4.1_1.tar.gz),
verified with SHA-256
`1731a34baadec5479258ea0202e4d5d865d2efeee60cb0c7d7eb056fe96ca219`.
Raw reports include the executable hashes and the full Java release metadata.

## What is equivalent

- Minecraft Java 1.8.9 (protocol 47), offline login, one backend, no player
  forwarding, encryption, compression, user plugins or Lua callbacks.
- Both clients complete login and receive a valid Join Game packet before
  exchanging custom plugin messages. Every echo is checked against its payload.
- The same Python standard-library backend and load generator, loopback TCP,
  `TCP_NODELAY` and a negotiated 1460-byte TCP MSS on Linux.
- The same client count, message size, offered messages per client per second,
  deadlines, connection-burst size and number of login attempts.
- Login and packet rate limits are disabled. Velocity connection logging,
  BungeeCord plugin-channel handling and bStats telemetry are disabled; Rift uses
  no metrics listener. This isolates a common forwarding workload.

Velocity normally enables compression and login throttling. These are deliberate
benchmark settings, not deployment defaults. See the upstream
[configuration reference](https://docs.papermc.io/velocity/configuration/).
The exact generated Lua and TOML, commands and JVM options are retained in each
report. The jar is pinned by version, build, size and SHA-256 in
[tests/velocity.json](../tests/velocity.json), using the official
[PaperMC downloads service](https://docs.papermc.io/misc/downloads-service/).

## Reproduce

Use Linux, Python 3.11 or newer, the pinned Rust toolchain and Java 25. Build and
run the correctness checks before measurement, then stop other builds and tests.
The downloader verifies cached files on every invocation and refuses a corrupt
cache entry. The comparison verifies the jar again before starting it.

```sh
cargo build --release --locked
python3 scripts/fetch_velocity.py

# Point this at the Java runtime being compared and retain its version.
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

The work directory must be new; it preserves each process log and generated
configuration. Failed requests remain in reports and are valid measurements.
Use `--require-success` for a correctness check that must exit nonzero on any
warmup or measured request failure. Infrastructure or configuration errors
always fail the run and leave an incomplete report. The table renderer refuses
incomplete reports. A quick CI run uses two clients, one trial and one second
each of warmup and measurement; it cannot support performance conclusions.

## Reading the measurements

CPU is the difference in the proxy's user plus system CPU time from Linux
`/proc`, divided by the measured wall-clock interval. **100% is one logical CPU**;
values can exceed 100%. Backend and load-generator CPU are recorded separately.
Startup and warmup CPU are excluded.
The table shows the median and range of each trial's mean CPU usage, not peak
CPU. Login bursts finish much faster than the 20-second echo windows; their CPU
values describe those short bursts. This host's process CPU counters advance
in 10 ms units. Raw reports retain CPU seconds and the exact resource window.

Memory is process RSS in MiB (1,048,576 bytes), including native JVM memory as well
as Java heap. It is not allocated heap, an allocation count, or whole-machine
memory. The sampled peak can miss spikes between samples. CPU and RSS require
Linux; unsupported platforms must not be presented as zero resource usage.
RSS includes memory retained from startup, warmup and earlier phases; excluding
their CPU time does not subtract their memory footprint.

Latency percentiles use nearest rank and include successful operations only.
For echo traffic, the table measures time from the scheduled send slot to the
verified reply, including load-generator dispatch delay. The raw reports also
record send-to-reply RTT and dispatch delay separately. Login latency starts at
TCP connection initiation and ends after Join Game and a verified echo.
Failures and their reasons are recorded separately, including timeouts, rejected
connections and incorrect echoes. A run with no successful operations has null
latencies. Zero observed failures describes only the attempts in that run.
Each client has at most one outstanding echo. Missed send slots and all
remaining slots of a failed session count as failures rather than reducing the
offered load. Echo connection setup is recorded separately and excluded from
its measured resource window; connection bursts include setup and teardown.
Read CPU alongside achieved throughput and failure rate: doing less successful
work can reduce CPU consumption.

Each trial starts fresh proxy and backend processes. Both products receive the
same active warmup; their order alternates between trials. Summary values must
be interpreted alongside trial ranges, not as statistically significant rankings.
CI runs a short correctness smoke test and saves its artifacts; its numbers are
not substituted for the longer published measurements.

## Scope and limitations

This fixture exercises login, the transition to play, and bidirectional plugin
payload forwarding. It does not simulate terrain generation, chunk traffic,
player movement, real-server tick scheduling, modern configuration phases,
signed chat, Mojang authentication, encryption, compression, backend switching,
or third-party plugins. A 1.8.9 loopback result cannot establish performance for
those workloads or for a real network. Fixed-rate tests do not measure maximum
throughput or maximum player count.

Login throughput can also be limited by the single Python load generator. Its
resource measurements and those of the separate backend process are retained
beside the proxy measurements. Small differences in scheduled echo latency
should be read alongside dispatch-delay and RTT samples.

JIT compilation, garbage collection, CPU frequency, operating-system scheduling
and other applications can affect repeated measurements. The reports identify
the CPU, operating system, runtime versions, worker settings, binary hashes and
source revision so another machine can reproduce the methodology. Compare
results only with matching workload and runtime settings, and run longer tests
on deployment hardware before making capacity decisions.

## Validation

[Validation evidence](performance/2026-09-30-validation.json) records 114 Python
tests passing under both 3.12 and 3.14, 346 Rust tests passing in each of debug
and release mode, 15 frontend checks, 200 protocol sessions, switching/recovery,
BungeeCord and Linux packaging checks. Formatting, Clippy and actionlint passed.
The exact CI comparison smoke also passed locally under Python 3.12 with
`--require-success` and the pinned Velocity jar. CI now runs that smoke and
preserves its report, configurations and logs on every benchmark job.

The existing Rift-only Lua burst smoke separately recorded 37 admission
failures among 576 attempts at its tested loads. Those observations are retained
in the validation evidence; the comparison above has no Lua callbacks.
