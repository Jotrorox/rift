# Authenticated Paper compatibility acceptance

This procedure tests the current proxy-owned login and encrypted client stream
against two pinned **Paper 1.21.11 build 132** servers. It requires a licensed
Minecraft Java **1.21.11** client signed in through its launcher. No Microsoft
password, access token, or refresh token is requested or stored.

Install Python 3.11+, Rust, and a Java **JDK 21** (`java` and `javac` on `PATH`).
If your default Java differs, set `RIFT_JAVA_21` to the Java 21 executable; the
compiler must support `--release 21`. Early-access JVM builds are rejected by Paper.
Allow the proxy to reach Mojang's session service and the harness to reach the
public Minecraft profile lookup service. Run from the repository root:

```sh
python3 tests/manual_online.py --accept-eula --server paper
# Use an existing build and a fixed loopback port if preferred:
python3 tests/manual_online.py --accept-eula --binary target/release/rift --port 25565 --pack-port 25570 --profile YourName
# Repeat the complete scenario for each plugin combination:
python3 tests/manual_online.py --accept-eula --plugin-stack essentials
python3 tests/manual_online.py --accept-eula --plugin-stack protocol
```

The matrix is explicitly scoped to **Paper 1.21.11 build 132** and a native
1.21.11 client. It does not extend the offline release matrix's real-server
claims to authenticated clients of every version.

| Stack | Pinned plugins | Additional observations |
| --- | --- | --- |
| `baseline` | RiftOnlineProbe fixture | Signed chat/commands, packs, forwarding and inventories |
| `essentials` | LuckPerms 5.5.85, EssentialsX 2.22.0, EssentialsXChat 2.22.0 | UUID-based permission grant/revoke, signed messages alongside chat formatting and command plugins |
| `protocol` | ViaVersion 5.12.0, ViaBackwards 5.12.0 | Native protocol coexistence with packet translation plugins loaded |

Plugin URLs and SHA-256 checksums are pinned in
[`online_plugins.json`](online_plugins.json). Cache hits are reverified, missing
or corrupt jars fail, and every backend must report all selected plugins enabled
with the exact expected versions, including after restarts. The Via stack does
**not** certify translation, other client versions or every plugin feature.
Pins cover the plugin jars; plugins may fetch their own runtime dependencies.
The probe is a test fixture, not a plugin for production deployment.

`--accept-eula` accepts the [Minecraft EULA](https://aka.ms/MinecraftEULA) for the
temporary servers. The harness prints the frontend address and retains its files
under `target/minecraft/runs/manual-online-*`. Use a client on the harness machine,
or forward that loopback port over SSH. Also forward the printed HTTP resource-pack
port (use `--pack-port` to choose one). Use **English (US)** in the client and set
the server entry's **Server Resource Packs** to **Prompt** before joining.
With SSH, Paper should receive the tunnel's
loopback peer address, not the address of the remote workstation.

The backend configuration follows [Paper's modern forwarding documentation](https://docs.papermc.io/velocity/player-information-forwarding/):
`server.properties` uses `online-mode=false` and `enforce-secure-profile=true`;
`spigot.yml` disables BungeeCord;
`config/paper-global.yml` enables Velocity with `online-mode=true` and a shared
secret. Rift enables online authentication and Velocity forwarding together.
The random secret is provided to Rift through `RIFT_FORWARDING_SECRET` and is
written only to disposable Paper configuration files inside the private run
directory. Both backends and the metrics endpoint bind to loopback. Backend
compression differs between the two servers to exercise independent framing.

A test-only plugin is compiled against API libraries embedded in the
checksum-verified Paper jar and Brigadier from the pinned Mojang jar. It records the UUID, name, forwarded IP, complete
signed texture properties, and inventory that **Paper actually receives**.
It does not modify profiles or authenticate players. The harness compares the
backend UUID/name to Mojang's public account lookup, verifies the texture payload
belongs to that UUID, and checks that the same properties survive every transfer.
The local fixture expects `127.0.0.1`; `--expected-ip` changes the expected address.

The probe also records Paper's `AsyncChatEvent` and a real signed-message command
argument (`/riftsigned`). At each checkpoint, send the random marker, run the
signed command, then send the next marker. The runner requires matching signed
content, a nonempty signature, the authenticated UUID and no chat cancellation.
An unsigned message or ordinary command preprocess event cannot satisfy these
checks. Paper supplies the accepted signed-message evidence; the probe does not
independently reverify Mojang/player cryptography. Visual delivery still needs
the operator's confirmation, including with EssentialsXChat active.

Two generated language-only resource packs visibly rename diamonds and emeralds
to `RIFT LOBBY ...` or `RIFT PRIMARY ...`. A loopback HTTP server serves only those
two zips; it cannot expose configurations or other files. Reports include both
hashes, unique request UUIDs and Paper's player-correlated status callbacks.
Format 75.0 matches the pinned [Minecraft 1.21.11 resource-pack format](https://www.minecraft.net/en-us/article/minecraft-java-edition-1-21-11).

Follow each prompt and type `PASS` only after observing the requested behavior:

1. Join the lobby. Check your usual skin with F5, load chunks, move, place/break
   blocks, and send chat. An earlier unauthenticated probe claiming your actual
   UUID/name must still receive a mandatory encryption challenge. This probe
   checks that claims do not bypass authentication; the cryptographic rejection
   paths and session-service failures are covered by the automated Rust tests.
2. The harness gives you **7 diamonds** in the lobby. Complete the signed chat
   and signed command prompts and accept the optional lobby pack; its name must
   be visible in your inventory. The Essentials stack also verifies LuckPerms
   grant/revoke through Paper's permission API without changing operator status.
   Use `/server primary`
   without disconnecting. Check your skin, fresh chunks, block interactions and
   chat. The primary receives the same account UUID and signed properties.
   The lobby pack must be removed: creative inventory item names return to normal.
3. The harness gives you **11 emeralds** on primary. Use `/hub` and check that the
   lobby's 7 diamonds return. Use `/server primary` again and check that primary's
   11 emeralds return. Keep those items unchanged until the test ends. These are
   two separate server inventories; this test does not claim inventory syncing
   between worlds. Accept the required primary pack before `/hub`; returning to
   the lobby must remove it. Repeat signed chat and signed commands after each
   transfer. Paper must report `ACCEPTED` then `SUCCESSFULLY_LOADED` for each
   accepted pack, not just an operator confirmation.
4. Use `/hub` and accept the lobby pack again. The harness restarts the now-empty primary with an incorrect
   forwarding secret. Try `/server primary`: expect a clear error, remain in the
   lobby, and verify movement, blocks, chat and the 7 diamonds still work. The
   runner checks that Paper logged no new successful join, Rift recorded a failed
   transfer, and the original frontend connection remains open. The lobby pack
   must remain active; signed chat/commands and permissions must still work.
5. The harness restores the correct secret and restarts primary. Use
   `/server primary` and verify your 11 emeralds, usual skin and gameplay again.
   The lobby pack must be removed and signed chat/commands must work again.
   Disconnect normally only at the requested prompt.
6. Reset the server entry's resource-pack setting to **Prompt** and rejoin the
   lobby on a separate authenticated connection. Refuse the optional pack: Paper
   must report `DECLINED`, gameplay and signed messages must continue. Refuse the
   subsequent required pack and confirm disconnection. The runner requires an
   observed Paper pack request and frontend closure. Vanilla can disconnect
   before sending `DECLINED` for a required pack, so that callback is recorded
   when present but is not required; this outcome includes an operator observation.
   This separate connection is deliberate: vanilla remembers consent per connection.

Rift's frontend accepted-connection counter must remain unchanged through all
transfers, with one active client and the expected backend player count. Each
world must save player data under the Mojang UUID. All visual/gameplay claims
require timestamped operator confirmations; profile/inventory assertions use the
Paper plugin's independently recorded data.

In a full run, `result.json` records `passed: true` and
`authenticated_acceptance: "passed"` only when every phase completes. Reports
include the Rift binary hash, server fixture and plugin pins, observed plugin
versions, signed-message records and resource-pack responses. Results are saved
after each confirmation/evidence phase. Any assertion,
failed prompt, interruption, or unavailable public profile lookup leaves
`passed: false` and stops the fixture processes. Evidence includes `proxy.log`,
Paper logs, and `plugins/RiftOnlineProbe/profiles.jsonl` in each backend directory.
These files include public profile data; Paper configuration files include the
disposable secret. Share only the needed artifacts.

An assistant may relay an operator's explicit `PASS` to the runner; it must not
infer a visual result from logs or enter `PASS` on the operator's behalf. Preserve
partial results if the operator reconnects or a phase fails, and repeat the full
scenario after fixing the problem. A successful negotiation probe alone is not
proof of account authentication, skins, or playable encrypted traffic.

Authentication outages are tested deterministically by the Rust authentication
suite using an injected local session-service fixture (timeouts, error statuses,
malformed responses, and rejection). The production client uses Mojang's HTTPS
endpoint. This manual harness never redirects real account authentication to a
mock endpoint and does not require disrupting Mojang or the operator's network.

## Automated preflight and regression evidence

Without an account or graphical client, run:

```sh
python3 tests/manual_online.py --accept-eula --binary target/release/rift --preflight --plugin-stack baseline
python3 tests/manual_online.py --accept-eula --binary target/release/rift --preflight --plugin-stack essentials
python3 tests/manual_online.py --accept-eula --binary target/release/rift --preflight --plugin-stack protocol
# Check the observer's real Paper callbacks with an unsigned fixture client:
python3 tests/online_probe_wire.py --accept-eula --plugin-stack essentials
```

CI runs these same preflights with two real Paper backends, secure profiles and
Velocity forwarding. It compiles the observer, checks exact enabled plugins,
requires Paper to reject invalid forwarding signatures and checks Rift's mandatory
authentication challenge. Successful preflight reports use
`scope: "paper_plugin_preflight"`, `passed: true`, and
`authenticated_acceptance: "not_run"`. They are never signed-in acceptance results.
CI uploads reports and logs, excluding Paper configurations containing the
disposable forwarding secret. Full manual results can contain public profile data
and chat markers; review artifacts before sharing them.

CI also runs `online_probe_wire.py` for each stack. This unsigned offline client
checks snapshots, permissions, chat/command signature reporting, console-only
controls, resource-pack request/removal packet UUIDs and all eight status callbacks.
Its resource-pack replies are synthetic; it does not download or render packs.
Reports use `scope: "offline_observer_runtime_only"` and
`authenticated_acceptance: "not_run"`. Unsigned messages must be reported as
unsigned, so the signed-in runner cannot mistake them for signed evidence.

The Rust session suite additionally tests real encrypted streams with a local
session-service fixture, synthetic chat signatures, version-specific chat/session
and command packets, switching, resource-pack responses and cleanup. Python tests
exercise missing/unsigned/mismatched evidence, partial writes, safe pack serving,
corrupt plugin downloads and disabled/wrong-version plugins. These deterministic
checks cannot establish real Mojang authentication or client rendering. A new
plugin or version needs a fresh full authenticated run; adding a pin alone is not
compatibility evidence.
