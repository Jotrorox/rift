# Network protocol contract

Network commands and backend replacement support Java **1.8.9–26.3**
(66 releases, 50 protocols).
A client and a backend on different versions between 1.12 and 26.3 are
translated; see [protocol translation](translation.md). Outside that range, or
with translation turned off, both use the same protocol. Online authentication
works on every supported version. Velocity modern forwarding requires a backend
on **1.13 or newer** (login plugin messages arrived in 1.13); BungeeCord and
BungeeGuard forwarding work on all.
With online authentication enabled, Rift encrypts the client connection, verifies
the Mojang session and retains its UUID, canonical name and signed profile properties.
Paper backends run `online-mode=false` with Velocity modern forwarding enabled
and `proxies.velocity.online-mode=true`, or with `settings.bungeecord=true` for
BungeeCord forwarding; see the README for configuration.

1.19–1.19.2 clients send a chat signing key in Login Start and sign the login
challenge with it instead of encrypting it. Rift checks that signature against
the key, as the vanilla server does, and passes the key on: in the backend's
Login Start (as Velocity and BungeeCord do) and, when Paper asks for it, in
Velocity forwarding versions 2 (1.19) and 3 (1.19.1–1.19.2, with the owning
Mojang UUID). Rift does not check Mojang's signature over the key; a backend
with `enforce-secure-profile=true` does.
The backend login exchange receives the same verified profile and socket peer IP
on initial login and every replacement. Authentication service failures never
fall back to offline identities. Omitted security settings retain the original
offline mode, which requires forwarding disabled and `enforce-secure-profile=false`.

### Unsigned chat

`authentication.signed_chat` (default `true`) decides whether backends learn
the material they verify a player's chat signatures with. With `false`, Rift
withholds it on every backend attachment, translated or not:

| Client | Withheld |
| --- | --- |
| 1.19–1.19.2 | The chat key: the backend's Login Start is rewritten without it, and Velocity forwarding answers with version 1, as for a player who has no key. Rift still checks the client's signature over the login challenge. |
| 1.19.3 and newer | Every serverbound Player Session packet (chat session update), at login and whenever the client renews its key. |

Chat messages, commands, their signatures and last-seen acknowledgements are
forwarded as before. A backend without the key or session does not verify
them and does not track a signature chain: it takes the player's chat as
unsigned. That is what makes it safe for the proxy to drop a single chat
message or command of a player; with signed chat, a backend that misses one
signed message rejects every later one.

The consequences, for every player on the network: their chat is shown to
others as not secure (the client's unsigned-message marker), and chat
reporting does not work, because nothing they say is signed towards the
backend. Clients before 1.19 and offline-mode networks are unaffected; they
sign nothing.

Backends must not require signed chat:

- **Vanilla** (and Spigot) enforce `enforce-secure-profile` only with
  `online-mode=true`, which a backend behind a proxy does not have. From
  1.19.1 on the setting has no effect there; a 1.19 backend must keep
  `enforce-secure-profile=false` (see [translation](translation.md)).
- **Paper** also enforces it when its proxy settings say the proxy is in
  online mode (`proxies.velocity.online-mode: true`, or
  `proxies.bungee-cord.online-mode` with `settings.bungeecord`). Set
  `enforce-secure-profile=false` in `server.properties` on Paper backends,
  or players cannot chat: the server answers each message with "Chat disabled
  due to missing profile public key". Established from the bytecode of Paper
  1.21.11 build 132 (`DedicatedServer.enforceSecureProfile` and
  `SignedMessageChain.Decoder.unsigned`, `javap -c`), not from a run.
- **Pumpkin** 0.2.0 accepts chat from a player with no chat session in its
  default configuration (`allow_chat_reports = false`): it ignores Player
  Session packets and message signatures altogether and broadcasts chat
  unsigned, although it announces secure-chat enforcement. Established by a
  run against the pinned release binary (`tests/servers.json`,
  0.2.0+26.3-26.51, protocol 777): a player that never sent a Player Session
  chatted with and without a 256-byte signature, and both messages reached
  another player and the server log. With `allow_chat_reports = true`, which
  Pumpkin permits only together with its own `online_mode`, the source at the
  pinned commit (`net/java/play/chat_message.rs`) kicks a player whose message
  has no signature, and one without a session for an expired key; that
  configuration was not run and does not work with `signed_chat = false`.

Changing the setting applies to connections made afterwards and restarts
every persistent plugin, as a change of `online_mode` does.

Switching is an explicit capability in `src/protocol/switching.rs`, separate from
relay and configuration-state support. Each protocol maps its control packet IDs,
Join Game fields, signed-command checksum and Brigadier argument registry.
Session code selects the capability for command handling,
world validation, client information/brand replay, resource-pack removal and
bundle tracking. Pre-1.20.2 contracts also map Respawn, keepalive, tab-list and
boss-bar packets, dimension layouts and named command argument parsers.
Every capability also maps clientbound Respawn and Game Event, which Rift reads
(with Join Game) only for the player's `dimension` and `gamemode` permission
contexts; a packet that does not parse exactly is relayed unchanged and skipped.
Their IDs come from minecraft-data `protocol.json` for each release through
26.1 and from the `GameProtocols` registration order of the official 26.2 and
26.3 server artifacts below (Respawn `0x52`/`0x54`, Game Event `0x26`/`0x27`).

Every capability must have a pinned `switchable` fixture in `tests/servers.json`.
A Rust test checks the capability and fixture sets agree.
`tests/network_minecraft.py --accept-eula` and the CI compatibility matrix run the
entire fixture set through lobby → survival → lobby, target-ban rollback and
recovery after killing survival. Each transition checks fresh world,
chunks, teleport, command tree (1.13+), game mode, border and accepted backend
chat on the same compressed frontend socket. Expansion requires an explicit layout review,
independent test-client mapping and a passing real-server switch/recovery run.
These offline fixtures do not replace the authenticated Paper/client procedure
in [tests/MANUAL_ONLINE.md](../tests/MANUAL_ONLINE.md), which remains pinned to
1.21.11. Its baseline, LuckPerms/EssentialsX/EssentialsXChat and
ViaVersion/ViaBackwards stacks record signed chat/command observations,
resource-pack responses and UUID permission checks. Automated preflights prove
startup and rejection behavior only; they never report authenticated acceptance.

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

The automated real-server test uses offline loopback servers and a protocol test
client, with compressed and uncompressed backends behind one compressed frontend.
It verifies fresh worlds, chunks, teleports, command trees, chat, game modes,
borders, resource-pack cleanup, duplicate login rejection, target-ban rollback,
crash recovery and terminal play bans. Rust tests cover encrypted authentication,
profile forwarding (including preserving 26.2+ session UUIDs), malformed control
packets and each version's command parser registry. Independent wire tests cover
all accepted protocols with compression disabled and thresholds 0, 64 and 256.
These checks do not certify every mod/plugin or replace testing with a signed-in
graphical client; that manual procedure remains documented separately.

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

The isolated fixtures explicitly disable the whitelist, including on 26.3 where
its default would otherwise reject the generated test player names.

A replacement performs an independent backend login before changing the client.
The old backend continues processing packets during this preflight. Explicit
login disconnects are access denials, and a disconnect from the current backend
is terminal. Before committing a ready replacement, Rift drains immediately ready
old-server packets to a quiescent point so a simultaneously queued ban cannot lose
a selection race. This drain is limited to 128 events and 20 ms of elapsed work;
a busy stream or incomplete packet cancels preflight while keeping the old
attachment. Backpressured client frames are completed before checking the old
backend again, under the replacement deadline. On 1.20.2+, once the new backend
confirms the same UUID and exact name, Rift sends Start Configuration, drains old play packets through Configuration
Acknowledged, and forwards the new configuration exchange. Compression belongs
to each connection independently. The new backend must send a fresh Join Game;
only its completed delivery marks a switch ready.

Before 1.20.2, preflight additionally waits for a validated Join Game. Rift sends
a proxy-owned keepalive and drains client packets through its matching reply,
checks the old backend again for queued bans, removes tracked tab entries and
boss bars, and resets the header/footer and titles. The new Join Game installs
the destination entity ID and dimension registry. A following Respawn forces
the world reload; before 1.16 the Join Game dimension is temporarily changed so
even overworld-to-overworld switches reload chunks. Persistent client settings
and brand are replayed as play packets. Malformed or excessively nested registry
NBT is rejected. A busy source retains its attachment if the client transition
has not already required closing an unfinished packet bundle. Old-server
keepalive replies still reach that server during the barrier, so a cancelled
switch does not cause a later keepalive timeout. After the barrier reply, no
additional old-server packets are forwarded to the client. A bounded batch of
late packets is inspected for bans and retained on the old attachment until the
switch succeeds. An incomplete frame, compression change or exhausted inspection
budget cancels the switch as busy; retained packets resume in order on rollback.
An explicit disconnect remains terminal. This prevents
replies to late old-server packets from reaching the replacement backend.

For 1.20.2 and newer, reconfiguration is the world reset. The official 1.21.11
client constructs a new `ClientPacketListener` at configuration completion. Its `handleLogin`
installs the new entity ID and world and resets `chatSession`, the signed-message
encoder, `nextChatIndex`, the last-seen tracker and message-signature cache.
This is why no extra Respawn or stale chat-session replay is appropriate. The
client establishes a fresh chat session when applicable. Existing chat packets,
signatures and acknowledgements pass unchanged within each backend attachment.
Old play packets drained during replacement never reach the new backend.

Persistent client information and brand are replayed into the new configuration.
On 1.20.3+, resource packs are common-listener state, so Rift explicitly pops the
old pack stack. Earlier clients have no pack-pop packet: packs remain until
replaced by the destination or the client leaves the network. An unfinished
backend bundle is closed before the configuration or legacy world transition.

On 1.13+, the backend command tree gains executable `server` and `hub` literals.
`server` uses a greedy `brigadier:string`, which does not create a signed chat
argument. Unsigned commands may be intercepted. Commands carrying argument
signatures are forwarded unchanged, including commands named `server` or `hub`.
For a signed-command packet with zero argument signatures, consuming a proxy
command preserves its last-seen offset using a separate acknowledgement packet.

On 1.19.3+, `server <name>` and extension commands with `suggest` mark their
argument with `minecraft:ask_server`. Rift answers Command Suggestions Requests
whose text (with or without a leading `/`) starts with `server `, `hub ` or a
registered extension command and a space. It never forwards those to the backend.
Every other request, including one for a command name still being typed, reaches
the backend unchanged. Response ranges count UTF-16 code units, as Brigadier
does. Tooltips are JSON text before 1.20.3 and anonymous network NBT from then on.
Older clients get no `ask_server` marks, so they never ask.

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

Rift decompresses every packet to inspect it, but a packet it neither translates
nor rewrites is forwarded in the frame it arrived in when both connections use
the same compression threshold. Other packets are compressed again. Each block
is stored or written with fixed or dynamic Huffman codes, whichever is smallest,
so re-encoding adds at most 5 bytes per 64 KiB block plus 6 bytes of zlib
framing, and incompressible packets keep fitting the 2 MiB frame limit. Rift's
matcher is simpler than zlib's (one candidate per position), so its output runs
roughly 10-30% larger than zlib level 6 on compressible data. A packet over
2 MiB uncompressed that the backend compressed to just under the limit can
therefore still exceed it if Rift must rewrite or translate it; Rift then
disconnects the player instead of sending an oversized frame.

The backend handshake carries the client's hostname and, for Forge clients, its
`\0FML\0`, `\0FML2\0`, `\0FML3\0` or `\0FORGE` marker (with Forge's optional
network version, found anywhere in the NUL-delimited list as Forge itself
reads it). Rift drops any other NUL-delimited data the client sends after the
hostname, so a client cannot pass BungeeCord-style IP, UUID or skin data to an
offline backend that has `settings.bungeecord=true`.

With `forwarding.mode = "bungeecord"` or `"bungeeguard"`, Rift itself writes
BungeeCord's layout into the backend handshake: the client's hostname, the
socket peer IP, the undashed verified UUID and the verified profile properties
as JSON, NUL-separated. A Forge marker travels as Bungee's `forgeClient` and
`extraData` properties. BungeeGuard adds one `bungeeguard-token` property;
a profile property of that name from anywhere else is dropped. The hostname
may be up to 32767 characters, which Spigot and Paper read; a backend that
ignores the forwarded data derives an offline UUID, which Rift refuses at
Login Success with a pointer to `spigot.yml`.

Velocity `velocity:player_info` requests stay inside the proxy. Their responses
use HMAC-SHA256 over the negotiated version, socket peer IP, verified UUID, name
and profile properties, including property signatures. Rift negotiates the
version as Velocity does: 4 for a 1.19.3+ player when Paper asks for it, 2 or 3
with a 1.19–1.19.2 chat key, otherwise 1. With Velocity forwarding each backend
must request forwarding before Login Success; a Velocity request while Rift
uses BungeeCord forwarding fails the login. Client responses can only answer
plugin queries actually relayed to that client; they cannot answer a Velocity
query or reuse a completed query ID. Proxy-owned forwarding can be repeated on
a safe initial transport retry because it needs no new client exchange.

Before 1.19, proxy commands are slash-prefixed chat packets. 1.19–1.19.2 use
variable-length signatures and a different last-seen structure; signatures are
never removed or transplanted between backends.
System messages carry position `1` through 1.19; 1.19.1 replaces it with an
overlay boolean, which Rift sets to false.

Implementation references checked for this change:

- [Velocity system chat encoding](https://github.com/PaperMC/Velocity/blob/dev/3.0.0/proxy/src/main/java/com/velocitypowered/proxy/protocol/packet/chat/SystemChatPacket.java): the 1.19/1.19.1 position-to-overlay transition.

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

- [Mojang release manifest](https://piston-meta.mojang.com/mc/game/version_manifest_v2.json):
  official release list, server URLs, checksums and Java requirements.
- [Official 26.2 server artifact](https://piston-data.mojang.com/v1/objects/823e2250d24b3ddac457a60c92a6a941943fcd6a/server.jar)
  and [official 26.3 server artifact](https://piston-data.mojang.com/v1/objects/33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c/server.jar):
  `version.json`, `GameProtocols`, `ConfigurationProtocols`, `ClientboundLoginFinishedPacket`,
  `ClientboundLoginPacket`, `CommonPlayerSpawnInfo`, `GameType`,
  `ServerboundAcceptTeleportationPacket` and `ArgumentTypeInfos`, inspected with
  `javap -c -private`. 26.2 adds the session UUID and online-mode flag; 26.3 changes
  game-mode encoding and adds property-free command parsers through registry ID 61.
  The same `GameProtocols` registration order gives Command Suggestions Request
  `0x0f` and Response `0x0f` for both.
  No decompiled Mojang source is included in this repository.

- [Velocity legacy client transitions](https://github.com/PaperMC/Velocity/blob/dev/3.0.0/proxy/src/main/java/com/velocitypowered/proxy/connection/client/ClientPlaySessionHandler.java): Join Game/Respawn reset and persistent tab/boss/title cleanup.

## Dialogs

On 1.21.6+ (protocols 771 to 777) Rift shows [dialogs](dialogs.md) for
plugins. IDs come from the official server artifacts' packet reports for every
protocol; the layouts are the same in all of them (Mojang's 1.21.6 mappings and
the unobfuscated 26.3 server, inspected with `javap -c -private`):

| Protocols | Show Dialog (play) | Clear Dialog (play) | Custom Click Action (play) | Configuration: show / clear / click |
| --- | --- | --- | --- | --- |
| 771–772 (1.21.6–1.21.8) | `0x85` | `0x84` | `0x41` | `0x12` / `0x11` / `0x08` |
| 773–774 (1.21.9–1.21.11) | `0x8a` | `0x89` | `0x41` | `0x12` / `0x11` / `0x08` |
| 775–776 (26.1–26.2) | `0x8c` | `0x8b` | `0x44` | `0x12` / `0x11` / `0x08` |
| 777 (26.3) | `0x8f` | `0x8e` | `0x44` | `0x13` / `0x12` / `0x08` |

Show Dialog in play carries a `Holder<Dialog>`; Rift always sends it inline
(VarInt `0`, then the dialog as anonymous network NBT, converted from JSON the
way vanilla's `JsonOps` to `NbtOps` conversion does). Clear Dialog has no
fields. Custom Click Action carries an identifier, a VarInt length of at most
65536 and an optional anonymous NBT tag. Rift decodes that tag with a 32-level
nesting limit (vanilla accepts 16); a malformed click packet closes the
session, as vanilla does.

Clicks with a `rift:` identifier never reach a backend: one on the dialog Rift
showed last becomes a dialog event for its owner, any other is dropped. Every
other click passes through unchanged. A backend's own Show or Clear Dialog
ends Rift's dialog, and so does configuration, which closes every screen.
Dialogs reach the client only in play; clicks drained during a server switch
are dropped like commands.
