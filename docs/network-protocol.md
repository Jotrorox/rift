# Network protocol

How Rift speaks the Minecraft Java protocol: supported releases, login,
forwarding, server switching, commands, compression, dialogs and menus. Read it when
you debug a protocol problem or work on `src/protocol/`.

| Feature | Versions |
| --- | --- |
| Network commands and backend replacement | Java **1.8.9–26.3** (66 releases, 50 protocols) |
| [Protocol translation](translation.md) between client and backend | 1.12–26.3. Outside that range, or with translation turned off, both use the same protocol. |
| Online authentication | Every supported version |
| Velocity modern forwarding | Backend on **1.13 or newer** (login plugin messages arrived in 1.13) |
| BungeeCord and BungeeGuard forwarding | Every supported version |

## Release matrix

All releases below have separate, checksum-verified Mojang server downloads in
[`tests/servers.json`](../tests/servers.json). Versions sharing a protocol share
packet mappings, but each server release runs the two-backend acceptance test.

| Java release | Protocol | Version-specific handling |
| --- | --- | --- |
| 1.8.9 | 47 | String login UUID, chat commands, byte Join Game dimension, position-based teleport acknowledgement |
| 1.9–1.9.4 | 107, 108, 109, 110 | Teleport IDs; Join Game dimension becomes an integer in 1.9.1; boss bars |
| 1.10–1.10.2 | 210 | Explicit packet mappings |
| 1.11–1.11.2 | 315, 316 | Title reset action changes |
| 1.12–1.12.2 | 335, 338, 340 | Keepalive changes from VarInt to long in 1.12.2 |
| 1.13–1.13.2 | 393, 401, 404 | Named Brigadier parsers, namespaced brand channel, login plugin messages |
| 1.14–1.14.4 | 477, 480, 485, 490, 498 | Join view distance; Respawn no longer carries difficulty |
| 1.15–1.15.2 | 573, 575, 578 | Seed hash and respawn-screen flag |
| 1.16–1.16.1 | 735, 736 | Binary login UUID, world list, dimension registry NBT, previous game mode |
| 1.16.2–1.16.5 | 751, 753, 754 | Separate hardcore flag, dimension NBT, VarInt player limit |
| 1.17–1.17.1 | 755, 756 | Separate border/title packets and client text-filter setting |
| 1.18–1.18.2 | 757, 758 | Simulation distance and server-listing setting |
| 1.19–1.19.2 | 759, 760 | Optional login profile key, signed commands, last-seen acknowledgement and death location |
| 1.19.3 | 761 | Chat session, player-list action bitset and numeric argument parsers |
| 1.19.4 | 762 | Packet bundles, time parser properties |
| 1.20–1.20.1 | 763 | Respawn portal cooldown |
| 1.20.2 | 764 | Configuration-based switching; JSON components and string dimension type |
| 1.20.3–1.20.4 | 765 | NBT components and resource-pack pop |
| 1.20.5–1.20.6 | 766 | Numeric dimension type, strict-error flag and separate unsigned command packet |
| 1.21, 1.21.1 | 767 | Login strict-error flag; Join Game has no sea level; no chat checksum |
| 1.21.2, 1.21.3 | 768 | Sea level, client particle setting and new teleport layout |
| 1.21.4 | 769 | Player Loaded acknowledgement |
| 1.21.5 | 770 | Chat checksum, resource-selector command parser, changed packet IDs |
| 1.21.6 | 771 | Hex-color/dialog parsers and changed serverbound IDs |
| 1.21.7, 1.21.8 | 772 | Explicit mapping for this protocol |
| 1.21.9, 1.21.10 | 773 | Changed clientbound IDs |
| 1.21.11 | 774 | Explicit mapping for this protocol |
| 26.1, 26.1.1, 26.1.2 | 775 | Changed clientbound/serverbound IDs |
| 26.2 | 776 | Login session UUID and Join Game online-mode flag |
| 26.3 | 777 | VarInt game modes, new command parsers, configuration/play IDs and teleport acknowledgement |

## Login and authentication

Setup is described under
[authentication and forwarding](authentication.md).

- Online mode completes a proxy-owned RSA challenge and AES-128/CFB8
  transition before a fixed HTTPS `hasJoined` request to Mojang. The
  client-supplied UUID is never used as proof of identity.
- Encryption wraps the entire client byte stream, including packet lengths and
  compressed frames. It keeps independent read/write cipher state across
  cancellation and backend replacement.
- Rift verifies the Mojang session and retains its UUID, canonical name and
  signed profile properties. The backend login exchange receives the same
  verified profile and socket peer IP on initial login and every replacement.
- Authentication service failures never fall back to offline identities.
- Omitted security settings retain the original offline mode, which requires
  forwarding disabled and `enforce-secure-profile=false`.

### Chat keys on 1.19–1.19.2

1.19–1.19.2 clients send a chat signing key in Login Start and sign the login
challenge with it instead of encrypting it. Rift checks that signature against
the key, as the vanilla server does, and passes the key on:

- in the backend's Login Start, as Velocity and BungeeCord do;
- when Paper asks for it, in Velocity forwarding versions 2 (1.19) and 3
  (1.19.1–1.19.2, with the owning Mojang UUID).

Rift does not check Mojang's signature over the key; a backend with
`enforce-secure-profile=true` does.

`authentication.signed_chat` decides whether backends get the chat key and
chat session at all; see [signed chat](authentication.md).

## Forwarding

### Backend handshake

The backend handshake carries the client's hostname and, for Forge clients, its
`\0FML\0`, `\0FML2\0`, `\0FML3\0` or `\0FORGE` marker (with Forge's optional
network version, found anywhere in the NUL-delimited list as Forge itself
reads it).

Rift drops any other NUL-delimited data the client sends after the hostname.
A client therefore cannot pass BungeeCord-style IP, UUID or skin data to an
offline backend that has `settings.bungeecord=true`.

A 1.20.5+ client may arrive with the transfer intent (3), sent there by
another server's Transfer packet. Rift accepts it and passes the intent on to
the backend, except after a backend switch and for an online-mode login,
whose backend handshakes carry the login intent (2). On a
[cluster](clustering.md#rolling-restarts) peer every backend handshake carries
the login intent: a player that a draining peer hands over arrives with the
transfer intent, which is no transfer from the backend's point of view, and
backends need no `accepts-transfers=true`. An older client that sends the
transfer intent is refused with `login.transfer_version`.

### BungeeCord and BungeeGuard

With `forwarding.mode = "bungeecord"` or `"bungeeguard"`, Rift itself writes
BungeeCord's layout into the backend handshake, NUL-separated: the client's
hostname, the socket peer IP, the undashed verified UUID and the verified
profile properties as JSON.

- A Forge marker travels as Bungee's `forgeClient` and `extraData` properties.
- BungeeGuard adds one `bungeeguard-token` property. A profile property of
  that name from anywhere else is dropped.
- The hostname may be up to 32767 characters, which Spigot and Paper read.
- A backend that ignores the forwarded data derives an offline UUID. Rift
  refuses that at Login Success with a pointer to `spigot.yml`.

### Velocity

Velocity `velocity:player_info` requests stay inside the proxy. Their responses
use HMAC-SHA256 over the negotiated version, socket peer IP, verified UUID, name
and profile properties, including property signatures.

Rift negotiates the version as Velocity does:

| Version | When |
| --- | --- |
| 4 | A 1.19.3+ player, when Paper asks for it |
| 2 or 3 | A 1.19–1.19.2 player with a chat key |
| 1 | Otherwise |

- Each backend must request forwarding before Login Success.
- A Velocity request while Rift uses BungeeCord forwarding fails the login.
- Client responses can only answer plugin queries actually relayed to that
  client. They cannot answer a Velocity query or reuse a completed query ID.
- Proxy-owned forwarding can be repeated on a safe initial transport retry,
  because it needs no new client exchange.

## Server switching

### Capabilities

Switching is an explicit capability in `src/protocol/switching.rs`, separate
from relay and configuration-state support. Session code selects it for
command handling, world validation, client information/brand replay,
resource-pack removal and bundle tracking.

| A capability maps | For |
| --- | --- |
| Control packet IDs, Join Game fields, signed-command checksum and Brigadier argument registry | Every protocol |
| Respawn, keepalive, tab-list and boss-bar packets, dimension layouts and named command argument parsers | Protocols before 1.20.2 |
| Clientbound Respawn and Game Event | Every protocol. Rift reads them (with Join Game) only for the player's `dimension` and `gamemode` permission contexts; a packet that does not parse exactly is relayed unchanged and skipped. |

The Respawn and Game Event IDs come from minecraft-data `protocol.json` for
each release through 26.1, and from the `GameProtocols` registration order of
the official 26.2 and 26.3 server artifacts listed under
[source references](#source-references) (Respawn `0x52`/`0x54`, Game Event
`0x26`/`0x27`).

### Preflight

A replacement performs an independent backend login, the preflight, before
changing the client.

- The old backend continues processing packets during preflight.
- Explicit login disconnects are access denials. A disconnect from the current
  backend is terminal.
- Before committing a ready replacement, Rift drains immediately ready
  old-server packets to a quiescent point, so a simultaneously queued ban
  cannot lose a selection race. This drain is limited to 128 events and 20 ms
  of elapsed work.
- A busy stream or incomplete packet cancels preflight while keeping the old
  attachment.
- Backpressured client frames are completed before checking the old backend
  again, under the replacement deadline.
- An unfinished backend bundle is closed before the configuration or legacy
  world transition.
- Compression belongs to each connection independently.
- The new backend must send a fresh Join Game; only its completed delivery
  marks a switch ready.

### Switch on 1.20.2 and newer

1. The new backend confirms the same UUID and exact name.
2. Rift sends Start Configuration and drains old play packets through
   Configuration Acknowledged. Old play packets drained during replacement
   never reach the new backend.
3. Rift forwards the new configuration exchange, and replays persistent client
   information and brand into it.
4. On 1.20.3+, resource packs are common-listener state, so Rift explicitly
   pops the old pack stack. Earlier clients have no pack-pop packet: packs
   remain until replaced by the destination or the client leaves the network.

Reconfiguration is the world reset, so Rift sends no extra Respawn and
replays no chat session. The official 1.21.11 client constructs a new
`ClientPacketListener` at configuration completion. Its `handleLogin` installs
the new entity ID and world and resets `chatSession`, the signed-message
encoder, `nextChatIndex`, the last-seen tracker and message-signature cache.
The client then establishes a fresh chat session when applicable.

Existing chat packets, signatures and acknowledgements pass unchanged within
each backend attachment.

### Switch before 1.20.2

1. Preflight additionally waits for a validated Join Game.
2. Rift sends a proxy-owned keepalive and drains client packets through its
   matching reply. This is the barrier.
3. Rift checks the old backend again for queued bans, removes tracked tab
   entries and boss bars, and resets the header/footer and titles.
4. The new Join Game installs the destination entity ID and dimension
   registry. Malformed or excessively nested registry NBT is rejected.
5. A following Respawn forces the world reload. Before 1.16 the Join Game
   dimension is temporarily changed so even overworld-to-overworld switches
   reload chunks.
6. Persistent client settings and brand are replayed as play packets.

Around the barrier:

- Old-server keepalive replies still reach that server during the barrier, so
  a cancelled switch does not cause a later keepalive timeout.
- After the barrier reply, no additional old-server packets are forwarded to
  the client. This prevents replies to late old-server packets from reaching
  the replacement backend.
- A bounded batch of late packets is inspected for bans and retained on the
  old attachment until the switch succeeds.
- An incomplete frame, compression change or exhausted inspection budget
  cancels the switch as busy; retained packets resume in order on rollback. An
  explicit disconnect remains terminal.
- A busy source retains its attachment if the client transition has not
  already required closing an unfinished packet bundle.

### Cancellation and retries

- Cancellation during preflight leaves the old session usable.
- Cancellation after `switch_in_progress()` becomes true requires
  disconnection, because transition writes may be partial.
- Runtime supplies deadlines for login/configuration and replacement.
- Initial transport failures can retry before Login Success. A login
  plugin/cookie exchange disables such retries, because its client responses
  cannot be replayed safely to another backend.

## Proxy commands

- On 1.13+, the backend command tree gains executable `server` and `hub`
  literals. `server` uses a greedy `brigadier:string`, which does not create a
  signed chat argument.
- Before 1.19, proxy commands are slash-prefixed chat packets.
- 1.19–1.19.2 use variable-length signatures and a different last-seen
  structure. Signatures are never removed or transplanted between backends.
- Unsigned commands may be intercepted. Commands carrying argument signatures
  are forwarded unchanged, including commands named `server` or `hub`.
- For a signed-command packet with zero argument signatures, consuming a proxy
  command preserves its last-seen offset using a separate acknowledgement
  packet.
- System messages carry position `1` through 1.19; 1.19.1 replaces it with an
  overlay boolean, which Rift sets to false.

### Suggestions

On 1.19.3+, `server <name>` and extension commands with `suggest` mark their
argument with `minecraft:ask_server`. Older clients get no `ask_server` marks,
so they never ask.

- Rift answers Command Suggestions Requests whose text (with or without a
  leading `/`) starts with `server `, `hub ` or a registered extension command
  and a space. It never forwards those to the backend.
- Every other request, including one for a command name still being typed,
  reaches the backend unchanged.
- Response ranges count UTF-16 code units, as Brigadier does.
- Tooltips are JSON text before 1.20.3 and anonymous network NBT from then on.

## Compression

Rift decompresses every packet to inspect it. A packet it neither translates
nor rewrites is forwarded in the frame it arrived in when both connections use
the same compression threshold. Other packets are compressed again:

- Each block is stored or written with fixed or dynamic Huffman codes,
  whichever is smallest. Re-encoding therefore adds at most 5 bytes per 64 KiB
  block plus 6 bytes of zlib framing, and incompressible packets keep fitting
  the 2 MiB frame limit.
- Rift's matcher is simpler than zlib's (one candidate per position), so its
  output runs roughly 10-30% larger than zlib level 6 on compressible data.
- A packet over 2 MiB uncompressed that the backend compressed to just under
  the limit can therefore still exceed it if Rift must rewrite or translate
  it. Rift then disconnects the player instead of sending an oversized frame.

## Dialogs

On 1.21.6+ (protocols 771 to 777) Rift shows [dialogs](dialogs.md) for
plugins. IDs come from the official server artifacts' packet reports for every
protocol. The layouts are the same in all of them (Mojang's 1.21.6 mappings
and the unobfuscated 26.3 server, inspected with `javap -c -private`):

| Protocols | Show Dialog (play) | Clear Dialog (play) | Custom Click Action (play) | Configuration: show / clear / click |
| --- | --- | --- | --- | --- |
| 771–772 (1.21.6–1.21.8) | `0x85` | `0x84` | `0x41` | `0x12` / `0x11` / `0x08` |
| 773–774 (1.21.9–1.21.11) | `0x8a` | `0x89` | `0x41` | `0x12` / `0x11` / `0x08` |
| 775–776 (26.1–26.2) | `0x8c` | `0x8b` | `0x44` | `0x12` / `0x11` / `0x08` |
| 777 (26.3) | `0x8f` | `0x8e` | `0x44` | `0x13` / `0x12` / `0x08` |

| Packet | Layout |
| --- | --- |
| Show Dialog (play) | A `Holder<Dialog>`. Rift always sends it inline: VarInt `0`, then the dialog as anonymous network NBT, converted from JSON the way vanilla's `JsonOps` to `NbtOps` conversion does. |
| Clear Dialog | No fields. |
| Custom Click Action | An identifier, a VarInt length of at most 65536 and an optional anonymous NBT tag. Rift decodes that tag with a 32-level nesting limit (vanilla accepts 16). A malformed click packet closes the session, as vanilla does. |

Routing:

- Clicks with a `rift:` identifier never reach a backend. One on the dialog
  Rift showed last becomes a dialog event for its owner; any other is dropped.
- Every other click passes through unchanged.
- A backend's own Show or Clear Dialog ends Rift's dialog, and so does
  configuration, which closes every screen.
- Dialogs reach the client only in play. Clicks drained during a server switch
  are dropped like commands.

## Menus

On 1.12+ (protocols 335 to 777) Rift shows [inventory menus](menus.md) for
plugins: chest, hopper, dispenser and anvil screens it writes to the client
in the client's protocol, whatever the backend runs. Rift's containers have
the IDs 101 to 127, above the 1 to 100 vanilla servers use. The client's
packets for them (`container_click`, `container_close`,
`container_button_click`, `container_slot_state_changed`, the anvil's
`rename_item` or, on 1.12, the `MC|ItemName` plugin message) and
`set_creative_mode_slot` while a menu is open never reach a backend. When a
click disturbs the player's inventory rows on the client, Rift asks the
backend to send the inventory again with a click on container 0 that changes
nothing, written in the backend's protocol past the translator; up to 1.16.5
Rift acknowledges the backend's rejection of that click itself.

The packets per protocol, container and state IDs, the inventory request per
backend version and how menus were tested are in
[how menus work](menus-internals.md).

## Tests

Every capability must have a pinned `switchable` fixture in
`tests/servers.json`. A Rust test checks the capability and fixture sets agree.
Expansion requires an explicit layout review, independent test-client mapping
and a passing real-server switch/recovery run.

| Test | What it covers |
| --- | --- |
| `tests/network_minecraft.py --accept-eula` and the CI compatibility matrix | The entire fixture set through lobby → survival → lobby, target-ban rollback and recovery after killing survival. Each transition checks fresh world, chunks, teleport, command tree (1.13+), game mode, border and accepted backend chat on the same compressed frontend socket. Also resource-pack cleanup, duplicate login rejection, crash recovery and terminal play bans. |
| Rust tests | Encrypted authentication, profile forwarding (including preserving 26.2+ session UUIDs), malformed control packets and each version's command parser registry. |
| Independent wire tests | All accepted protocols with compression disabled and thresholds 0, 64 and 256. |
| `tests/manual_online.py` | The authenticated Paper/client procedure, pinned to 1.21.11. Its baseline, LuckPerms/EssentialsX/EssentialsXChat and ViaVersion/ViaBackwards stacks record signed chat/command observations, resource-pack responses and UUID permission checks. |

The automated real-server test uses offline loopback servers and a protocol
test client, with compressed and uncompressed backends behind one compressed
frontend. The isolated fixtures explicitly disable the whitelist, including on
26.3 where its default would otherwise reject the generated test player names.

> [!NOTE]
> The offline fixtures do not certify every mod or plugin and do not replace
> the manual procedure with a signed-in graphical client. Its automated
> preflights prove startup and rejection behavior only; they never report
> authenticated acceptance.

The fixtures declare their Java runtime: 8 for 1.8.9–1.16.5, 17 for
1.17–1.20.4, 21 for 1.20.5–1.21.11 and 25 for 26.x. CI installs the matching
runtime. Locally, set `RIFT_JAVA_8`, `RIFT_JAVA_17`, `RIFT_JAVA_21` and
`RIFT_JAVA_25` to executable paths as needed (each defaults to `java`):

```sh
cargo build --locked --release
python3 tests/network_minecraft.py --accept-eula
# Or select one release:
python3 tests/network_minecraft.py --accept-eula --server vanilla-26.3
python3 tests/protocol_wire.py --binary target/release/rift
```

## Source references

Rift's implementation was written independently. Mojang methods were
inspected with `javap -c -private`; no decompiled Mojang code is included in
this repository.

Mojang:

- [Mojang release manifest](https://piston-meta.mojang.com/mc/game/version_manifest_v2.json):
  official release list, server URLs, checksums and Java requirements.
- [Mojang 1.21.11 client artifact](https://piston-data.mojang.com/v1/objects/ba2df812c2d12e0219c489c4cd9a5e1f0760f5bd/client.jar)
  and [official client mappings](https://piston-data.mojang.com/v1/objects/031a68bebf55d824f66d6573d8c752f0e1bf232a/client.txt):
  `ClientPacketListener.handleConfigurationStart`, `handleLogin`, `setKeyPair`,
  `ClientConfigurationPacketListenerImpl.handleConfigurationFinished`, and
  `LastSeenMessages.computeChecksum` / `Update.verifyChecksum`.
- [Official 26.2 server artifact](https://piston-data.mojang.com/v1/objects/823e2250d24b3ddac457a60c92a6a941943fcd6a/server.jar)
  and [official 26.3 server artifact](https://piston-data.mojang.com/v1/objects/33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c/server.jar):
  `version.json`, `GameProtocols`, `ConfigurationProtocols`, `ClientboundLoginFinishedPacket`,
  `ClientboundLoginPacket`, `CommonPlayerSpawnInfo`, `GameType`,
  `ServerboundAcceptTeleportationPacket` and `ArgumentTypeInfos`. 26.2 adds the
  session UUID and online-mode flag; 26.3 changes game-mode encoding and adds
  property-free command parsers through registry ID 61. The same
  `GameProtocols` registration order gives Command Suggestions Request `0x0f`
  and Response `0x0f` for both.

minecraft-data:

- [1.21.8 packet schema](https://github.com/PrismarineJS/minecraft-data/blob/master/data/pc/1.21.8/protocol.json)
  and [1.21.11 packet schema](https://github.com/PrismarineJS/minecraft-data/blob/master/data/pc/1.21.11/protocol.json):
  control IDs, SpawnInfo, command-tree argument properties and chat checksums.

Velocity:

- [System chat encoding](https://github.com/PaperMC/Velocity/blob/dev/3.0.0/proxy/src/main/java/com/velocitypowered/proxy/protocol/packet/chat/SystemChatPacket.java):
  the 1.19/1.19.1 position-to-overlay transition.
- [PlayerDataForwarding](https://github.com/PaperMC/Velocity/blob/dev/3.0.0/proxy/src/main/java/com/velocitypowered/proxy/connection/PlayerDataForwarding.java)
  and [backend LoginSessionHandler](https://github.com/PaperMC/Velocity/blob/dev/3.0.0/proxy/src/main/java/com/velocitypowered/proxy/connection/backend/LoginSessionHandler.java):
  modern forwarding payload, HMAC coverage and version negotiation.
- [ClientPlaySessionHandler](https://github.com/PaperMC/Velocity/blob/dev/3.0.0/proxy/src/main/java/com/velocitypowered/proxy/connection/client/ClientPlaySessionHandler.java):
  independent confirmation that configuration resets world/tab/bossbar state;
  for legacy clients, the Join Game/Respawn reset and persistent
  tab/boss/title cleanup.
- [SessionCommandHandler](https://github.com/PaperMC/Velocity/blob/dev/3.0.0/proxy/src/main/java/com/velocitypowered/proxy/protocol/packet/chat/session/SessionCommandHandler.java):
  signed arguments cannot be consumed; unsigned consumption preserves chat
  acknowledgement offsets.

Backends and [signed chat](authentication.md):

- Paper 1.21.11 build 132, bytecode read with `javap -c`, not run:
  `DedicatedServer.enforceSecureProfile` and
  `SignedMessageChain.Decoder.unsigned`.
- Pumpkin 0.2.0 at the commit pinned in `tests/servers.json`:
  `net/java/play/chat_message.rs`.
