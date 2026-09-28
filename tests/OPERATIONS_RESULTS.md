# Real-server operational validation — 2026-09-28

All six server/compression combinations passed, plus a 100-reload Pumpkin soak.
The automated clients use offline authentication. **Manual authenticated,
encrypted gameplay on Paper also passed**, with coverage completed across two
complementary runs on the same date. The runnable procedure is in
[MANUAL_ONLINE.md](MANUAL_ONLINE.md).

Environment: Linux x86_64, Python 3.14.7, system OpenJDK 25.0.4.1, Rust 1.98.1.
CI continues to use Java 21. Fixtures are the checksum-verified versions in
`servers.json`: vanilla 1.21.11, Paper 1.21.11-132, Pumpkin 0.2.0+26.3-26.51.
These local runs used Rift runtime source at `55a0298`; the tested release binary SHA-256
is `25d88b89d0064223e33844787fb1505c9846e999a9491e26884d35c8d8a85f3a`.

Each default scenario performed 12 routing reloads, four rejected reloads, three
admission-policy reloads, 3,072 status requests during the reload loop and further
warmup/outage/admission traffic (3,397 accepted TCP connections before shutdown).
It verified live teleport acknowledgements and keepalives, preserved established
sessions, rate/capacity rejection, primary shutdown, fallback login, primary
recovery, and gameplay through graceful and forced shutdown paths.

Measurements below cover **Rift only**, sampled every 50 ms after warmup. Every
run started with 15 file descriptors and 9 sockets at the resource baseline.
“Quiet” is the maximum after status requests settled back to two gameplay
sessions. The resource assertions passed throughout; these finite runs are not
a proof of an unlimited-duration memory bound.

| Server | Compression | Reload rounds | Baseline / peak RSS (MiB) | Quiet FDs / sockets | Peak FDs / sockets | Result |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| Vanilla | enabled | 12 | 5.19 / 6.54 | 15 / 9 | 47 / 41 | PASS |
| Vanilla | disabled | 12 | 5.17 / 6.63 | 15 / 9 | 47 / 41 | PASS |
| Paper | enabled | 12 | 5.00 / 6.46 | 16 / 10 | 47 / 41 | PASS |
| Paper | disabled | 12 | 5.00 / 6.36 | 15 / 9 | 47 / 41 | PASS |
| Pumpkin | enabled | 12 | 5.51 / 6.78 | 15 / 9 | 28 / 20 | PASS |
| Pumpkin | disabled | 12 | 5.60 / 6.79 | 15 / 9 | 27 / 17 | PASS |
| Pumpkin, extended operations | enabled | 100 | 5.63 / 9.50 | 16 / 10 | 47 / 41 | PASS |

The extended run accepted 26,043 connections, applied 103 valid reloads including
admission changes, and rejected 34 invalid candidates. Its established primary
player used the hostname whose route repeatedly switched between primary and
lobby; console teleports on the original server continued to reach that player.
Peak RSS growth was 3.88 MiB, below the 16 MiB regression allowance. Settled
descriptors/sockets grew by at most one, below the allowances of eight/six.

Reproduce the required matrix and optional longer workload with:

```sh
cargo build --release --locked
python3 tests/minecraft.py --accept-eula --jobs 3 --report target/minecraft/enabled.json
python3 tests/minecraft.py --accept-eula --jobs 2 --compression disabled --report target/minecraft/disabled.json
python3 tests/minecraft.py --accept-eula --server pumpkin --operation-rounds 100
```

Local evidence is aggregated in `target/minecraft/operations-final.json`. The
corresponding run directories, in table order, are `vanilla-a25tw1ph`,
`vanilla-p8npa8o4`, `paper-qhhzmcoh`, `paper-8o8aiwv_`, `pumpkin-wy3_98qq`,
`pumpkin-vmosl2ew`, and `pumpkin-route-soak-r7jn2xvw` under
`target/minecraft/runs/`. Each retains `result.json`, backend/proxy logs and
`operations/resources.jsonl`. These generated artifacts are untracked; CI now
uploads the resource timeline and Lua configuration alongside its existing logs
and reports. An additional full 100-round Pumpkin run also passed before the
targeted hostname-continuity refinement.

The manual runner's preflight was exercised against actual online-mode vanilla
and Paper servers. Direct and proxied logins both received Encryption Request
with `should_authenticate=true`; evidence is in
`target/minecraft/online-preflight.json`. Those probes verify negotiation only.

The operator subsequently completed actual signed-in Java 1.21.11 gameplay
against online-mode Paper, with compression and secure profiles enabled. The
backend verified the same authenticated UUID on primary and lobby, distinct from
an offline UUID. The operator confirmed chunk loading, block interactions and
chat, uninterrupted gameplay through 12 reloads/four rejected candidates/3,072
status requests, lobby survival during primary outage, and a fresh authenticated
fallback login. Rift RSS during that stress phase grew from 4.86 to 5.86 MiB;
all quiet samples retained the baseline 13 descriptors and 7 sockets.

An intentional operator reconnect during primary recovery invalidated that
phase's continuity observation. The original report remains `passed: false`,
with its five completed phase confirmations preserved. A fresh run scoped to
`recovery_and_drain` then passed: the original lobby login survived primary
restart without a reconnect, gameplay and both traffic counters continued during
shutdown drain, and Rift exited normally after the operator disconnected, with
zero forced shutdowns. No single full manual run is claimed to have completed.

The combined evidence is `target/minecraft/manual-online-final.json`, with each
phase linked to its source result. The source directories are
`manual-online-_gw17j4b` (login, reloads, outage and fallback) and
`manual-online-fu_lexy7` (recovery and drain) under `target/minecraft/runs/`.
Both retain configurations, logs, encryption-probe results, profile UUID and
timestamped operator confirmations. Manual encrypted gameplay was checked on
Paper; vanilla's online-mode check remains negotiation-only.

The manual session also exposed a harness assumption that rejected intentional
reconnects made before stress. The runner now compares login counts before and
after stress and supports a clearly scoped `--recovery-only` repeat, without
discarding earlier evidence or presenting skipped phases as tested.

Validation also passed 22 Python harness tests, 64 Rust tests with
`--test-threads=1`, formatting, Clippy with warnings denied, and `git diff --check`.
The initial parallel Rust run hit `AddrInUse` in the pre-existing cache reload
test; its serialized rerun passed. Initial real-server runs exposed two harness
shutdown races: transient `/proc` permission errors on process exit and TCP
connect resets while polling a closing listener. Both were fixed and covered by
failure-detection tests before the successful reruns. No Rift runtime fix was
needed for the tested scenarios.
