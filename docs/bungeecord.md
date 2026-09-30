# BungeeCord plugin-message compatibility

Existing Bukkit/Paper plugins can request server transfers and query players
through the BungeeCord plugin channel. Enable the bridge explicitly:

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

This initial compatibility subset does not implement `Forward`,
`ForwardToPlayer`, `Message`, `MessageRaw`, `KickPlayer`, or `KickPlayerRaw`.
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

## Trust and testing

Enable compatibility only when the configured backend servers and their plugins
are trusted: a backend can query online players and request transfers for another
player. Client-originated messages on either compatibility channel are consumed
before they can reach backend plugins, including messages that impersonate a
proxy response. Other plugin channels continue to pass through. With the bridge
disabled, compatibility channels also pass through unchanged.

Run the independent socket acceptance test after building Rift:

```sh
cargo build --locked --release
python3 tests/bungeecord_wire.py --binary target/release/rift
```

It checks all supported queries and transfers, case handling, channel aliases,
backend registration, malformed messages, client spoofing, access rejection,
ordinary channel passthrough, disabled defaults, and compressed and uncompressed
backend connections. Protocol codec tests additionally cover the packet mappings
for each supported Minecraft protocol.

Wire and plugin API references:
[Paper plugin messaging](https://docs.papermc.io/paper/dev/plugin-messaging/) and
[Velocity's BungeeCord responder](https://github.com/PaperMC/Velocity/blob/dev/3.0.0/proxy/src/main/java/com/velocitypowered/proxy/connection/backend/BungeeCordMessageResponder.java).
