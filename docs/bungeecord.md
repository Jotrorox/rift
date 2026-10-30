# BungeeCord plugin-message compatibility

Existing Bukkit/Paper plugins can request server transfers, query players,
forward plugin messages to other servers, and message or kick players through
the BungeeCord plugin channel. Enable the bridge explicitly:

```lua
rift.config.network = {
    bungeecord = true,
    access = {
        survival = { deny = { "RestrictedPlayer" } },
    },
}
```

Add `bungeecord = true` to an existing `network` table when one is already
configured. The default is `false`. This bridge uses the player's Minecraft
connection and requires neither Rift's QUIC messaging listener nor its custom
messaging protocol. It can transfer players without enabling network command
destinations through `initial` or `hubs`.
Changing the option during reload affects new client connections; existing
sessions retain the setting they accepted at login.

Rift supports `BungeeCord` on legacy connections and `bungeecord:main` on modern
connections, accepting either spelling for incoming messages. Replies and backend
channel registration use the name appropriate to the connection's Minecraft
version. Bukkit/Paper plugins should keep using the `BungeeCord` API channel;
the server maps that name for modern clients. Rift registers its channel with
each backend after entering play, including after a transfer.

## Supported requests

All string fields use Java `DataOutput.writeUTF` / `DataInput.readUTF`, including
their unsigned 16-bit byte length and modified UTF-8 encoding. They are not
Minecraft VarInt-prefixed strings. Integer fields are signed big-endian 32-bit
values except the unsigned 16-bit `ServerIP` port.

The first string in every request and response is the subchannel below. The
table lists the remaining fields in wire order.

| Subchannel | Request fields | Response fields |
| --- | --- | --- |
| `Connect` | Server name | No response; transfer the carrier player |
| `ConnectOther` | Player name, server name | No response; transfer the named player |
| `IP` | None | Carrier IP address, integer port |
| `IPOther` | Player name | Player name, IP address, integer port |
| `PlayerCount` | Server name or `ALL` | Server name or `ALL`, integer player count |
| `PlayerList` | Server name or `ALL` | Server name or `ALL`, comma-and-space-separated player names |
| `GetServers` | None | Comma-and-space-separated configured server names |
| `GetServer` | None | Carrier's current server name |
| `GetPlayerServer` | Player name | Player name, current server name |
| `UUID` | None | Carrier UUID as 32 hexadecimal digits without hyphens |
| `UUIDOther` | Player name | Player name, UUID as 32 hexadecimal digits without hyphens |
| `ServerIP` | Server name | Server name, configured IP address or hostname, unsigned short port |
| `Forward` | Server name, `ALL` or `ONLINE`; subchannel, unsigned short length, data | No response; the subchannel, length and data reach the target servers |
| `ForwardToPlayer` | Player name; subchannel, unsigned short length, data | No response; the subchannel, length and data reach the player's server |
| `Message` | Player name or `ALL`, text | No response; send a chat message |
| `MessageRaw` | Player name or `ALL`, JSON text component | No response; send a chat message |
| `KickPlayer` | Player name, text | No response; disconnect the player |
| `KickPlayerRaw` | Player name, JSON text component | No response; disconnect the player |

`GetPlayerServer` is the Velocity extension to the original BungeeCord queries.
Player and server lookups ignore ASCII case; responses use the configured server
name or the player's canonical name. `ALL` must be uppercase. Name lists are
sorted for consistent responses. Empty servers have count zero and an empty
player-list string. Missing players or servers produce no reply. `ServerIP`
returns configured hostnames without a DNS lookup and IPv6 addresses without
brackets. Player IP queries report the socket peer address known to Rift.

Player queries use identities registered at backend login success, which can
precede entering the world. UUID queries use the identity established at login:
the verified profile in online mode and the offline UUID otherwise.
Responses return to the requesting backend over the carrier player's connection;
they are not sent to the Minecraft client.

Transfers use Rift's existing access and capacity checks, extension hooks, and
backend replacement flow. Transfer hooks receive the reason `bungeecord`. A
`ConnectOther` request checks the target player's access, even when its carrier
has permission to enter that server. Unknown, inaccessible, draining or
unavailable destinations leave the player attached to the current backend.
Transfer requests have no success or failure reply, as in BungeeCord; plugins
can query player location when they need to confirm the result.
The reported backend changes only after the destination becomes ready.

## Rift requests

Rift adds two requests to the same channel for information the standard set
does not carry. They use the same envelope and string encoding. BungeeCord and
Velocity ignore unknown subchannels, so a plugin that sends them keeps working
behind those proxies; it just gets no reply there.

| Subchannel | Request fields | Response fields |
| --- | --- | --- |
| `RiftServers` | Server name, service group name or `ALL` | Target, integer entry count, then per running server: name, service group, state, integer players, integer player limit |
| `RiftInGroup` | Player name, permission group | Player name, permission group, boolean membership |

`RiftServers` lists only servers that are up, sorted by name, so a lobby
selector can show them without a separate query per server. `ALL` selects every
configured server, a service group name selects that group's instances, and a
server name selects that server. Lookups ignore ASCII case except for uppercase
`ALL`, as with `PlayerCount`; the response repeats the configured name. A group
without running instances returns zero entries, and an unknown target produces
no reply.

A managed server is up while its process is `running`; Rift leaves it out while
it is stopped, starting, stopping or failed. An unmanaged backend is up unless
health checks mark it down. Without `health_check` it always counts as up. The
state is `draining` for a server that is draining, through the `draining`
setting or the administrator `drain` command, and `running` otherwise. Draining
servers keep their players but accept no new ones.

The service group is the group a server is an instance of, or an empty string.
The player count matches `PlayerCount` for that server. The player limit is the
server's `extensions.queues` capacity, which Rift enforces, then the
per-instance capacity of its service group's queue, then the group's
`scaling.capacity_per_instance`, the occupancy automatic scaling plans for.
Zero means Rift knows no limit for the server; the backend's own `max-players`
still applies.

`RiftInGroup` answers whether a player belongs to a permission group defined in
`extensions.groups`. Membership follows the same rules as permission checks: a
player belongs to the groups added with `rift.permissions.add_group` or the
dashboard, to every group those inherit from, and to `default` and its parents
when `default` is defined. Memberships of groups the current configuration no
longer defines do not count, and an undefined group returns false. The player
name ignores ASCII case and the response uses the canonical name; the group
name is compared exactly and repeated as sent. The boolean is a single byte
(`DataInput.readBoolean`). Missing players produce no reply, and so does every
`RiftInGroup` request without extensions using `api_version = 2`, since
permission groups do not exist then.

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

`RiftInGroup` is written as `writeUTF("RiftInGroup")`, the player name and the
group name, and its reply is read with `readUTF`, `readUTF`, `readUTF` and
`readBoolean`.

## Forwarding, messages and kicks

`Forward` and `ForwardToPlayer` deliver a message to other servers' plugins, for
example cross-server chat. The data after the target must be a subchannel
string, an unsigned 16-bit byte length, and exactly that many bytes; Rift
delivers those fields unchanged as a `BungeeCord` plugin message. A plugin
message needs a player connection, so each target server receives the message
through one player connected there, the one connected longest first. Servers
without players are skipped; nothing is queued for later players. `ALL` and
`ONLINE` both select every server with players except the sender's own, as in
Velocity. A named server, including the sender's own, receives the message
through one of its players. `ForwardToPlayer` uses the named player's current
server. Forwarded messages travel only from proxy to backend and are never sent
to the Minecraft client.

`Message` sends text to one player, or to every player for uppercase `ALL`.
Legacy `§` formatting codes in the text work as in BungeeCord. `KickPlayer`
disconnects one player with a text reason; `ALL` is a player name there.
`MessageRaw` and `KickPlayerRaw` take a JSON text component instead, written
for the player's Minecraft version. Rift checks only that it is a JSON string,
list or object without `null` list entries, then sends it unchanged to clients
before 1.20.3 and converts it to NBT for 1.20.3 and newer, as the server does.
Invalid components are dropped.

These requests enter the target player's queue of pending proxy actions, the
same bounded queue that `rift.players` uses, and the player's own session sends
them. A full queue or an absent player drops the request, and a `Forward` whose
carrier has moved to another server before delivery is dropped as well. As in
BungeeCord, none of these requests has a reply. Players count once they are in
a world, so a player still logging in can neither carry nor receive them.

## Limits

Unknown or malformed compatibility messages are consumed without a reply.
Messages and responses are limited to 32,767 bytes; a response that would exceed
the limit is omitted. Configured server names must be unique when compared
without ASCII case while compatibility is enabled.

## Backend plugin example

Register the plugin's incoming and outgoing `BungeeCord` channel during plugin
startup. A standard Java payload can then request a transfer:

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

Cross-server chat forwards its own payload to every other server with players:

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

Enable compatibility only when the configured backend servers and their plugins
are trusted: a backend can query online players and their permission group
memberships, transfer, message or kick any player, and send plugin messages to
every other server. Client-originated messages on either compatibility channel are consumed
before they can reach backend plugins, including messages that impersonate a
proxy response. Other plugin channels continue to pass through. With the bridge
disabled, compatibility channels also pass through unchanged.

Run the independent socket acceptance test after building Rift:

```sh
cargo build --locked --release
python3 tests/bungeecord_wire.py --binary target/release/rift
```

It checks all supported queries and transfers, Rift's server list including
draining servers, forwarding to other backends,
legacy and JSON messages and kicks, case handling, channel aliases,
backend registration, malformed messages, client spoofing, access rejection,
ordinary channel passthrough, disabled defaults, and compressed and uncompressed
backend connections. Protocol codec tests additionally cover the packet mappings
for each supported Minecraft protocol.

Wire and plugin API references:
[Paper plugin messaging](https://docs.papermc.io/paper/dev/plugin-messaging/) and
[Velocity's BungeeCord responder](https://github.com/PaperMC/Velocity/blob/dev/3.0.0/proxy/src/main/java/com/velocitypowered/proxy/connection/backend/BungeeCordMessageResponder.java).
