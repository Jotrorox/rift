# Authenticated, encrypted gameplay

This check covers signed-in gameplay that the offline fixtures cannot verify.
Paper coverage was completed on 2026-09-28 across two complementary runs; see
[the recorded results](OPERATIONS_RESULTS.md). For new executions, require
explicit operator confirmations and reviewed evidence for every claimed phase.
A scoped recovery/drain pass does not substitute for the other phases.

Requirements: Linux, Python 3.11+, Java 21, network access to Minecraft's session
services, and a licensed Minecraft Java **1.21.11** client signed in through its
launcher. Use a client on the harness machine, or forward the printed loopback
port over SSH. The harness never requests or stores Microsoft passwords/tokens.

```sh
python3 tests/manual_online.py --accept-eula --server paper
# Repeat for vanilla when checking both server implementations:
python3 tests/manual_online.py --accept-eula --server vanilla
```

If an operator intentionally reconnects during the recovery-continuity phase,
retain that run's partial evidence and repeat the remaining phases with
`python3 tests/manual_online.py --accept-eula --server paper --recovery-only`.
This creates a fresh lobby session and a report scoped to `recovery_and_drain`;
its pass does not claim that the skipped reload/outage/fallback phases ran again.
During recovery, stay connected until explicitly instructed to disconnect.

The EULA flag accepts the [Minecraft EULA](https://aka.ms/MinecraftEULA) for these
temporary servers. The runner prints the randomly assigned Rift address and a
results directory under `target/minecraft/runs/manual-online-*`. It enables all
operational settings and keeps both game servers and metrics on loopback.

Both servers use `online-mode=true`, `enforce-secure-profile=true`,
`prevent-proxy-connections=false`, and compression threshold 256. Paper forwarding
is left disabled. See the [Paper server.properties reference](https://docs.papermc.io/paper/reference/server-properties/)
for these settings. An unauthenticated login probe must receive an Encryption
Request with `should_authenticate=true` directly and through Rift. This checks
negotiation only; completing signed-in gameplay is the separate human check.

Follow the prompts; enter `PASS` only after observing the requested behavior:

1. Join the primary with the signed-in client. Move or fly into new chunks, place
   and break blocks, and send chat. Record your profile name at the prompt. The
   runner requires a completed login in the backend log and an authenticated UUID
   different from the offline UUID.
2. Stay connected and continue playing through 12 route reloads, four rejected
   reloads and 3,072 status requests. Verify no kick, reconnect screen, rollback or
   world switch. Continue moving and interacting afterward. The runner checks
   that the primary logged no new login during stress and the lobby logged none, and saves
   Rift memory/socket samples with the same budgets as the automated scenario.
3. Disconnect and reconnect when instructed to enter the lobby. Keep playing
   while the runner stops the primary and sends another status burst. The lobby
   player must remain connected. Stopping a player's own backend would necessarily
   disconnect that player; this is not a session migration test.
4. Reconnect once while the primary is down. The same authenticated profile must
   reach the fallback lobby. Keep playing during primary recovery; recovery must
   leave the existing lobby session in place.
5. After the shutdown signal, continue movement, block interactions and chat.
   Confirm within the 120-second drain deadline while still connected. Rift must
   refuse new connections, serve metrics and advance traffic counters for the
   established encrypted session. Then disconnect normally when instructed;
   Rift must exit successfully without forcing the shutdown.

The runner records timestamped operator confirmations, profile UUID, encryption
probe outcomes, resource budgets and drain metrics in `result.json`, alongside
`resources.jsonl`, proxy logs, backend logs and configuration. Any failed prompt,
assertion or interruption leaves `passed: false` and stops the fixture processes.
Retain these artifacts and note client version, operator, date and any observed
disconnects when reporting the result. Each `PASS` requires an explicit operator
confirmation; an assistant may relay that confirmation to the runner but must
never infer it from logs or supply it on the operator's behalf without confirmation.

The offline suite separately exercises rate/capacity rejection and forced drain
deadline/second-signal paths with real logged-in clients. This manual check adds
Microsoft authentication and an actual encrypted gameplay stream; it cannot be
replaced by status pings, negotiation probes or an offline UUID.
