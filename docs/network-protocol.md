# Network protocol contract

Network commands and backend replacement are pinned to Java 1.21.11, protocol
774. Other accepted protocols retain ordinary forwarding. All network backends
must run offline mode with `enforce-secure-profile=false`; Rift does not implement
Mojang session authentication or secure-profile forwarding.

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

Implementation references checked for this change:

- [Mojang 1.21.11 client artifact](https://piston-data.mojang.com/v1/objects/ba2df812c2d12e0219c489c4cd9a5e1f0760f5bd/client.jar)
  and [official client mappings](https://piston-data.mojang.com/v1/objects/031a68bebf55d824f66d6573d8c752f0e1bf232a/client.txt):
  `ClientPacketListener.handleConfigurationStart`, `handleLogin`, `setKeyPair`,
  `ClientConfigurationPacketListenerImpl.handleConfigurationFinished`, and
  `LastSeenMessages.computeChecksum` / `Update.verifyChecksum`. Methods were
  inspected using `javap -c -private`; no decompiled Mojang code is included.
- [1.21.11 packet schema](https://github.com/PrismarineJS/minecraft-data/blob/master/data/pc/1.21.11/protocol.json):
  control IDs, SpawnInfo, command-tree argument properties and chat checksums.
- [Velocity ClientPlaySessionHandler](https://github.com/PaperMC/Velocity/blob/dev/3.0.0/proxy/src/main/java/com/velocitypowered/proxy/connection/client/ClientPlaySessionHandler.java):
  independent confirmation that configuration resets world/tab/bossbar state.
- [Velocity SessionCommandHandler](https://github.com/PaperMC/Velocity/blob/dev/3.0.0/proxy/src/main/java/com/velocitypowered/proxy/protocol/packet/chat/session/SessionCommandHandler.java):
  signed arguments cannot be consumed; unsigned consumption preserves chat
  acknowledgement offsets. Rift's implementation was written independently.
