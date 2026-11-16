# BungeeCord compatibility

Existing Bukkit/Paper plugins can request server transfers, query players,
forward plugin messages to other servers, and message or kick players through
the BungeeCord plugin channel. Plugins written for Rift can use the typed
[network API](network-java.md) instead, which needs no carrier player and
answers every request.

## Enable the bridge

```lua
rift.config.network = {
    bungeecord = true,
    access = {
        survival = { deny = { "RestrictedPlayer" } },
    },
}
```

Add `bungeecord = true` to an existing `network` table when one is already
configured. The default is `false`.

- The bridge uses the player's Minecraft connection and requires neither
  Rift's QUIC messaging listener nor its custom messaging protocol.
- It can transfer players without enabling network command destinations
  through `initial` or `hubs`.
- Changing the option during reload affects new client connections; existing
  sessions retain the setting they accepted at login.
- Rift supports `BungeeCord` on legacy connections and `bungeecord:main` on
  modern connections, accepting either spelling for incoming messages.
  Replies and backend channel registration use the name appropriate to the
  connection's Minecraft version.
- Bukkit/Paper plugins should keep using the `BungeeCord` API channel; the
  server maps that name for modern clients.
- Rift registers its channel with each backend after entering play, including
  after a transfer.

> [!NOTE]
> Whatever the setting, Rift drops every message a client sends on the
> `BungeeCord` or `bungeecord:main` channel. Backend plugins treat that
> channel as the proxy's voice, so a client's message never reaches a
> backend.

## Supported requests

The first string in every request and response is the subchannel. The table
lists the remaining fields in wire order.

- All string fields use Java `DataOutput.writeUTF` / `DataInput.readUTF`,
  including their unsigned 16-bit byte length and modified UTF-8 encoding.
  They are not Minecraft VarInt-prefixed strings.
- Integer fields are signed big-endian 32-bit values, except the unsigned
  16-bit `ServerIP` port.

| Subchannel | Request fields | Response fields | Notes |
| --- | --- | --- | --- |
| `Connect` | Server name | None | Transfers the carrier player |
| `ConnectOther` | Player name, server name | None | Transfers the named player |
| `IP` | None | Carrier IP address, integer port | The socket peer address known to Rift |
| `IPOther` | Player name | Player name, IP address, integer port | As `IP` |
| `PlayerCount` | Server name or `ALL` | Server name or `ALL`, integer player count | Without players a [visibility rule](permissions.md#visibility) hides |
| `PlayerList` | Server name or `ALL` | Server name or `ALL`, comma-and-space-separated player names | Without hidden players |
| `GetServers` | None | Comma-and-space-separated configured server names | |
| `GetServer` | None | Carrier's current server name | |
| `GetPlayerServer` | Player name | Player name, current server name | The Velocity extension to the original BungeeCord queries |
| `UUID` | None | Carrier UUID as 32 hexadecimal digits without hyphens | |
| `UUIDOther` | Player name | Player name, UUID as 32 hexadecimal digits without hyphens | |
| `ServerIP` | Server name | Server name, configured IP address or hostname, unsigned short port | Hostnames without a DNS lookup, IPv6 addresses without brackets |
| `Forward` | Server name, `ALL` or `ONLINE`; subchannel, unsigned short length, data | None | The subchannel, length and data reach the target servers |
| `ForwardToPlayer` | Player name; subchannel, unsigned short length, data | None | The subchannel, length and data reach the player's server |
| `Message` | Player name or `ALL`, text | None | Sends a chat message |
| `MessageRaw` | Player name or `ALL`, JSON text component | None | Sends a chat message |
| `KickPlayer` | Player name, text | None | Disconnects the player |
| `KickPlayerRaw` | Player name, JSON text component | None | Disconnects the player |

### Lookups and replies

- Player and server lookups ignore ASCII case; responses use the configured
  server name or the player's canonical name. `ALL` must be uppercase.
- Name lists are sorted. Empty servers have count zero and an empty
  player-list string.
- Missing players or servers produce no reply.
- Player queries use identities registered at backend login success, which
  can precede entering the world.
- UUID queries use the identity established at login: the verified profile in
  online mode and the offline UUID otherwise.
- Responses return to the requesting backend over the carrier player's
  connection; they are not sent to the Minecraft client.

### Transfers

- Transfers use Rift's existing access and capacity checks, extension hooks,
  and backend replacement flow. Transfer hooks receive the reason
  `bungeecord`.
- A `ConnectOther` request checks the target player's access, even when its
  carrier has permission to enter that server.
- Unknown, inaccessible, draining or unavailable destinations leave the
  player attached to the current backend.
- Transfer requests have no success or failure reply, as in BungeeCord;
  plugins can query player location to confirm the result. The reported
  backend changes only after the destination becomes ready.

## Rift requests

Rift adds two requests to the same channel, with the same envelope and string
encoding. BungeeCord and Velocity ignore unknown subchannels, so a plugin
that sends them keeps working behind those proxies; it just gets no reply
there.

| Subchannel | Request fields | Response fields |
| --- | --- | --- |
| `RiftServers` | Server name, service group name or `ALL` | Target, integer entry count, then per running server: name, service group, state, integer players, integer player limit |
| `RiftInGroup` | Player name, permission group | Player name, permission group, boolean membership |

### `RiftServers`

`RiftServers` lists only servers that are up, sorted by name, so a lobby
selector can show them without a separate query per server.

| Target | Selects |
| --- | --- |
| `ALL` | Every configured server |
| A service group name | That group's instances; zero entries without running instances |
| A server name | That server |
| Unknown | No reply |

Lookups ignore ASCII case except for uppercase `ALL`, as with `PlayerCount`;
the response repeats the configured name.

A managed server is up while its process is `running`; Rift leaves it out
while it is stopped, starting, stopping or failed. An unmanaged backend is up
unless health checks mark it down. Without `health_check` it always counts as
up.

| Entry field | Value |
| --- | --- |
| Service group | The group the server is an instance of, or an empty string |
| State | `draining` for a server that is draining, through the `draining` setting, the administrator `drain` command or the server's own report on [`rift.server.state`](messaging-subjects.md#server-state). Otherwise the label the server reported there, such as `LOBBY` or `INGAME`, and `running` when it reported none |
| Players | Matches `PlayerCount` for that server |
| Player limit | The server's `extensions.queues` capacity, which Rift enforces, then the per-instance capacity of its service group's queue, then the group's `scaling.capacity_per_instance`, the occupancy automatic scaling plans for. Zero means Rift knows no limit; the backend's own `max-players` still applies |

Draining servers keep their players but accept no new ones.

A lobby selector can list a group's servers:

```java
ByteArrayOutputStream bytes = new ByteArrayOutputStream();
try (DataOutputStream out = new DataOutputStream(bytes)) {
    out.writeUTF("RiftServers");
    out.writeUTF("lobby");
}
player.sendPluginMessage(plugin, "BungeeCord", bytes.toByteArray());
```

and read the reply in its plugin-message listener:

```java
DataInputStream in = new DataInputStream(new ByteArrayInputStream(message));
if (in.readUTF().equals("RiftServers")) {
    String target = in.readUTF();
    int count = in.readInt();
    for (int i = 0; i < count; i++) {
        String name = in.readUTF();
        String group = in.readUTF();
        String state = in.readUTF();
        int players = in.readInt();
        int maxPlayers = in.readInt();
        // Show the server; "draining" servers accept no new players.
    }
}
```

### `RiftInGroup`

`RiftInGroup` answers whether a player is in a group of the
[permission store](permissions.md). The store always exists, with or without
extensions.

- Rift answers from the player's compiled subject in the contexts it knows
  for them: their server, dimension and game mode, and custom contexts
  reported by the backend or Lua providers.
- A player is in the groups they inherit directly, every group those inherit,
  and `default`, unless a negated `inherit` node blocks it.
- Memberships limited to other contexts, expired ones, and groups that no
  longer exist do not count.
- The player name ignores ASCII case and the response uses the canonical
  name; the group name ignores ASCII case and is repeated as sent.
- The boolean is a single byte (`DataInput.readBoolean`).
- Missing players produce no reply.

## Forwarding, messages and kicks

As in BungeeCord, none of these requests has a reply.

`Forward` and `ForwardToPlayer` deliver a message to other servers' plugins,
for example cross-server chat. The data after the target must be a subchannel
string, an unsigned 16-bit byte length, and exactly that many bytes; Rift
delivers those fields unchanged as a `BungeeCord` plugin message.

| Target | Receives the message |
| --- | --- |
| `ALL` or `ONLINE` | Every server with players except the sender's own, as in Velocity |
| A named server | That server, including the sender's own |
| `ForwardToPlayer` | The named player's current server |

- A plugin message needs a player connection, so each target server receives
  the message through one player connected there, the one connected longest
  first.
- Servers without players are skipped; nothing is queued for later players.
- Forwarded messages travel only from proxy to backend and are never sent to
  the Minecraft client.

Messages and kicks:

- `Message` sends text to one player, or to every player for uppercase `ALL`.
  Legacy `§` formatting codes in the text work as in BungeeCord.
- `KickPlayer` disconnects one player with a text reason; `ALL` is a player
  name there.
- `MessageRaw` and `KickPlayerRaw` take a JSON text component instead. Rift
  checks only that it is a JSON string, list or object without `null` list
  entries; invalid components are dropped. Rift sends it as JSON to clients
  before 1.20.3 and converts it to NBT for 1.20.3 and newer, as the server
  does.
- Hover and click events may use either the 1.21.5+ spelling (`click_event`,
  `hover_event`) or the older one; Rift rewrites them into the form the
  player's client reads (see [rich text](players-api.md#rich-text)).

These requests enter the target player's queue of pending proxy actions, the
same bounded queue that `rift.players` uses, and the player's own session
sends them.

- A full queue or an absent player drops the request.
- A `Forward` whose carrier has moved to another server before delivery is
  dropped as well.
- Players count once they are in a world, so a player still logging in can
  neither carry nor receive them.

## Limits

| Limit | Value |
| --- | --- |
| Messages and responses | 32,767 bytes; a response that would exceed the limit is omitted |
| Unknown or malformed compatibility messages | Consumed without a reply |
| Configured server names | Must be unique when compared without ASCII case while compatibility is enabled |

## Backend plugin example

Register the plugin's incoming and outgoing `BungeeCord` channel during
plugin startup. A standard Java payload can then request a transfer:

```java
ByteArrayOutputStream bytes = new ByteArrayOutputStream();
try (DataOutputStream out = new DataOutputStream(bytes)) {
    out.writeUTF("Connect");
    out.writeUTF("survival");
}
player.sendPluginMessage(plugin, "BungeeCord", bytes.toByteArray());
```

For queries, decode the response in the plugin-message listener with
`DataInputStream`: read the subchannel first, then the fields from the table.
A connected player is required to carry messages in both directions.

Cross-server chat forwards its own payload to every other server with
players:

```java
byte[] chat = message.getBytes(StandardCharsets.UTF_8);
ByteArrayOutputStream bytes = new ByteArrayOutputStream();
try (DataOutputStream out = new DataOutputStream(bytes)) {
    out.writeUTF("Forward");
    out.writeUTF("ALL");
    out.writeUTF("MyChat");
    out.writeShort(chat.length);
    out.write(chat);
}
player.sendPluginMessage(plugin, "BungeeCord", bytes.toByteArray());
```

The receiving servers' listeners read `"MyChat"` with `readUTF`, then
`readUnsignedShort` and that many bytes.

## Trust and testing

> [!WARNING]
> Enable compatibility only when the configured backend servers and their
> plugins are trusted: a backend can query online players and their
> permission group memberships, transfer, message or kick any player, and
> send plugin messages to every other server.

Client messages on either compatibility channel never reach backend plugins,
including messages that impersonate a proxy response. Other plugin channels
pass through. With the bridge disabled, what a backend sends on the
compatibility channels also passes through unchanged.

Run the independent socket acceptance test after building Rift:

```sh
cargo build --locked --release
python3 tests/bungeecord_wire.py --binary target/release/rift
```

It covers the requests on this page, channel aliases, backend registration,
malformed messages, client spoofing, access rejection, ordinary channel
passthrough, disabled defaults, and compressed and uncompressed backend
connections. Protocol codec tests additionally cover the packet mappings for
each supported Minecraft protocol.

Wire and plugin API references:
[Paper plugin messaging](https://docs.papermc.io/paper/dev/plugin-messaging/)
and
[Velocity's BungeeCord responder](https://github.com/PaperMC/Velocity/blob/dev/3.0.0/proxy/src/main/java/com/velocitypowered/proxy/connection/backend/BungeeCordMessageResponder.java).
