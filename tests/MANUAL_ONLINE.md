# Online authentication and Velocity forwarding acceptance

This procedure tests the current proxy-owned login and encrypted client stream
against two pinned **Paper 1.21.11 build 132** servers. It requires a licensed
Minecraft Java **1.21.11** client signed in through its launcher. No Microsoft
password, access token, or refresh token is requested or stored.

Install Python 3.11+, Rust, and a Java **JDK 21+** (`java` and `javac` on `PATH`).
Allow the proxy to reach Mojang's session service and the harness to reach the
public Minecraft profile lookup service. Run from the repository root:

```sh
python3 tests/manual_online.py --accept-eula --server paper
# Use an existing build and a fixed loopback port if preferred:
python3 tests/manual_online.py --accept-eula --binary target/release/rift --port 25565 --profile YourName
```

`--accept-eula` accepts the [Minecraft EULA](https://aka.ms/MinecraftEULA) for the
temporary servers. The harness prints the frontend address and retains its files
under `target/minecraft/runs/manual-online-*`. Use a client on the harness machine,
or forward that loopback port over SSH. With SSH, Paper should receive the tunnel's
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
checksum-verified Paper jar. It records the UUID, name, forwarded IP, complete
signed texture properties, and inventory that **Paper actually receives**.
It does not modify profiles or authenticate players. The harness compares the
backend UUID/name to Mojang's public account lookup, verifies the texture payload
belongs to that UUID, and checks that the same properties survive every transfer.
The local fixture expects `127.0.0.1`; `--expected-ip` changes the expected address.

Follow each prompt and type `PASS` only after observing the requested behavior:

1. Join the lobby. Check your usual skin with F5, load chunks, move, place/break
   blocks, and send chat. An earlier unauthenticated probe claiming your actual
   UUID/name must still receive a mandatory encryption challenge. This probe
   checks that claims do not bypass authentication; the cryptographic rejection
   paths and session-service failures are covered by the automated Rust tests.
2. The harness gives you **7 diamonds** in the lobby. Use `/server primary`
   without disconnecting. Check your skin, fresh chunks, block interactions and
   chat. The primary receives the same account UUID and signed properties.
3. The harness gives you **11 emeralds** on primary. Use `/hub` and check that the
   lobby's 7 diamonds return. Use `/server primary` again and check that primary's
   11 emeralds return. Keep those items unchanged until the test ends. These are
   two separate server inventories; this test does not claim inventory syncing
   between worlds.
4. Use `/hub`. The harness restarts the now-empty primary with an incorrect
   forwarding secret. Try `/server primary`: expect a clear error, remain in the
   lobby, and verify movement, blocks, chat and the 7 diamonds still work. The
   runner checks that Paper logged no new successful join, Rift recorded a failed
   transfer, and the original frontend connection remains open.
5. The harness restores the correct secret and restarts primary. Use
   `/server primary` and verify your 11 emeralds, usual skin and gameplay again.
   Disconnect normally only at the final prompt.

Rift's frontend accepted-connection counter must remain unchanged through all
transfers, with one active client and the expected backend player count. Each
world must save player data under the Mojang UUID. All visual/gameplay claims
require timestamped operator confirmations; profile/inventory assertions use the
Paper plugin's independently recorded data.

`result.json` records `passed: true` only when every phase completes. Any assertion,
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
Historical transparent-relay runs in [OPERATIONS_RESULTS.md](OPERATIONS_RESULTS.md)
do not establish acceptance of this implementation.
