# Network protocol contract

Network commands and backend replacement support Java **1.21.8 (772)** and
**1.21.11 (774)**. Clients and backends must use the same version; Rift does not
translate protocols. Other accepted protocols retain ordinary forwarding. With online
authentication enabled, Rift encrypts the client connection, verifies the Mojang
session and retains its UUID, canonical name and signed profile properties.
Paper backends run `online-mode=false` with Velocity modern forwarding enabled
and `proxies.velocity.online-mode=true`; see the README for configuration.
The backend login exchange receives the same verified profile and socket peer IP
on initial login and every replacement. Authentication service failures never
fall back to offline identities. Omitted security settings retain the original
offline mode, which requires forwarding disabled and `enforce-secure-profile=false`.

Switching is an explicit capability in `src/protocol/switching.rs`, separate from
relay and configuration-state support. These two versions share the Join Game,
signed-command checksum and Brigadier parser layouts; clientbound Join Game and
system-chat IDs differ. Session code selects the capability for command handling,
world validation, client information/brand replay, resource-pack removal and
bundle tracking.

Every capability must have a pinned `switchable` fixture in `tests/servers.json`.
A Rust test checks the two sets agree. `tests/network_minecraft.py --accept-eula`
and CI run the entire fixture set through lobby → survival → lobby, target-ban
rollback and recovery after killing survival. Each transition checks fresh world,
chunks, teleport, command tree, game mode, border and accepted backend chat on the
same compressed frontend socket. Expansion requires an explicit layout review,
independent test-client mapping and a passing real-server switch/recovery run.
These offline fixtures do not replace the authenticated Paper/client procedure
in `tests/MANUAL_ONLINE.md`, which remains pinned to 1.21.11.

A replacement performs an independent backend login before changing the client.
The old backend continues processing packets during this preflight. Explicit
login disconnects are access denials, and a disconnect from the current backend
is terminal. Before committing a ready replacement, Rift drains immediately ready
old-server packets to a quiescent point so a simultaneously queued ban cannot lose
a selection race. This drain is limited to 128 events and 20 ms of elapsed work;
a busy stream or incomplete packet cancels preflight while keeping the old
attachment. Backpressured client frames are completed before checking the old
backend again, under the replacement deadline. Once the new backend confirms the same UUID and exact name, Rift
sends Start Configuration, drains old play packets through Configuration
Acknowledged, and forwards the new configuration exchange. Compression belongs
to each connection independently. The new backend must send a fresh Join Game;
only its completed delivery marks a switch ready.

Reconfiguration is the world reset. The official 1.21.11 client constructs a
new `ClientPacketListener` at configuration completion. Its `handleLogin`
installs the new entity ID and world and resets `chatSession`, the signed-message
encoder, `nextChatIndex`, the last-seen tracker and message-signature cache.
This is why no extra Respawn or stale chat-session replay is appropriate. The
client establishes a fresh chat session when applicable. Existing chat packets,
signatures and acknowledgements pass unchanged within each backend attachment.
Old play packets drained during replacement never reach the new backend.

Persistent client information and brand are replayed into the new configuration.
Resource packs are common-listener state, so Rift explicitly pops the old pack
stack. An unfinished backend bundle is closed before Start Configuration.

The backend command tree gains executable `server` and `hub` literals.
`server` uses a greedy `brigadier:string`, which does not create a signed chat
argument. Unsigned commands may be intercepted. Commands carrying argument
signatures are forwarded unchanged, including commands named `server` or `hub`.
For a signed-command packet with zero argument signatures, consuming a proxy
command preserves its last-seen offset using a separate acknowledgement packet.

Cancellation during preflight leaves the old session usable. Cancellation after
`switch_in_progress()` becomes true requires disconnection, because transition
writes may be partial. Runtime supplies deadlines for login/configuration and
replacement. Initial transport failures can retry before Login Success; a login
plugin/cookie exchange disables such retries because its client responses cannot
be replayed safely to another backend.

Online mode completes a proxy-owned RSA challenge and AES-128/CFB8 transition
before a fixed HTTPS `hasJoined` request to Mojang. The client-supplied UUID is
never used as proof of identity. Encryption wraps the entire client byte stream,
including packet lengths and compressed frames, and keeps independent read/write
cipher state across cancellation and backend replacement.

Velocity `velocity:player_info` requests stay inside the proxy. Their responses
use HMAC-SHA256 over the negotiated version, socket peer IP, verified UUID, name
and profile properties, including property signatures. Rift supports forwarding
version 1 and the version 4 format used by modern Paper; versions 2/3's legacy
1.19 player-key fields are outside Rift's supported client versions. Each backend
must request forwarding before Login Success. Client responses can only answer
plugin queries actually relayed to that client; they cannot answer a Velocity
query or reuse a completed query ID. Proxy-owned forwarding can be repeated on
a safe initial transport retry because it needs no new client exchange.

Implementation references checked for this change:

- [Velocity PlayerDataForwarding](https://github.com/PaperMC/Velocity/blob/dev/3.0.0/proxy/src/main/java/com/velocitypowered/proxy/connection/PlayerDataForwarding.java)
  and [backend LoginSessionHandler](https://github.com/PaperMC/Velocity/blob/dev/3.0.0/proxy/src/main/java/com/velocitypowered/proxy/connection/backend/LoginSessionHandler.java):
  modern forwarding payload, HMAC coverage and version negotiation.
- [Mojang 1.21.11 client artifact](https://piston-data.mojang.com/v1/objects/ba2df812c2d12e0219c489c4cd9a5e1f0760f5bd/client.jar)
  and [official client mappings](https://piston-data.mojang.com/v1/objects/031a68bebf55d824f66d6573d8c752f0e1bf232a/client.txt):
  `ClientPacketListener.handleConfigurationStart`, `handleLogin`, `setKeyPair`,
  `ClientConfigurationPacketListenerImpl.handleConfigurationFinished`, and
  `LastSeenMessages.computeChecksum` / `Update.verifyChecksum`. Methods were
  inspected using `javap -c -private`; no decompiled Mojang code is included.
- [1.21.8 packet schema](https://github.com/PrismarineJS/minecraft-data/blob/master/data/pc/1.21.8/protocol.json)
  and [1.21.11 packet schema](https://github.com/PrismarineJS/minecraft-data/blob/master/data/pc/1.21.11/protocol.json):
  control IDs, SpawnInfo, command-tree argument properties and chat checksums.
- [Velocity ClientPlaySessionHandler](https://github.com/PaperMC/Velocity/blob/dev/3.0.0/proxy/src/main/java/com/velocitypowered/proxy/connection/client/ClientPlaySessionHandler.java):
  independent confirmation that configuration resets world/tab/bossbar state.
- [Velocity SessionCommandHandler](https://github.com/PaperMC/Velocity/blob/dev/3.0.0/proxy/src/main/java/com/velocitypowered/proxy/protocol/packet/chat/session/SessionCommandHandler.java):
  signed arguments cannot be consumed; unsigned consumption preserves chat
  acknowledgement offsets. Rift's implementation was written independently.
