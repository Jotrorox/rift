# Authentication and forwarding

In online mode, Rift verifies each player's account with Mojang, handles
client encryption and forwards the verified identity to your backends. Set it
up before you open a network to the public. This page also covers backends
older than 1.13, offline mode and signed chat.

## Enable online mode

[`examples/online.lua`](../examples/online.lua) is a complete authenticated
lobby/survival network to start from.

1. Enable both settings in `rift.lua`:

   ```lua
   rift.config.authentication = { online_mode = true, timeout_ms = 10000 }
   rift.config.forwarding = { mode = "velocity", secret_env = "RIFT_FORWARDING_SECRET" }
   ```

2. Set `RIFT_FORWARDING_SECRET` in Rift's environment.
3. On each Paper backend, keep `online-mode=false` in `server.properties` and
   `settings.bungeecord=false` in `spigot.yml`, then configure
   `config/paper-global.yml`:

   ```yaml
   proxies:
     velocity:
       enabled: true
       online-mode: true
       secret: "the same secret as RIFT_FORWARDING_SECRET"
   ```

4. Restart Paper.

A failed verification never falls back to offline authentication.

## Settings

| Field | Default | Meaning |
| --- | --- | --- |
| `authentication.online_mode` | `false` | Verify accounts with Mojang. Must be enabled together with `forwarding`. |
| `authentication.timeout_ms` | `10000` | Time limit for the request to Mojang's session server, 1–60000. |
| `authentication.signed_chat` | `true` | Whether backends get what they need to verify chat signatures. See [signed chat](#signed-chat). |
| `forwarding.mode` | none | `"velocity"`, `"bungeeguard"` or `"bungeecord"`. Omit `forwarding` to disable it. |
| `forwarding.secret_env` | `RIFT_FORWARDING_SECRET` | Environment variable with the Velocity secret or BungeeGuard token, 1–1024 bytes. `"bungeecord"` has no secret and rejects this field. |

[`rift check --strict`](install.md#strict-checks) verifies that the secret is
set before you start Rift.

## Forwarding modes

Online mode works on every supported version, 1.8.9 through 26.3. Which
forwarding mode you can use depends on your oldest backend:

| Mode | Backends | Sends |
| --- | --- | --- |
| `velocity` | 1.13 or newer | Velocity modern forwarding, signed with the secret. Clients older than 1.13 are turned away at login. |
| `bungeeguard` | every version | BungeeCord forwarding, which every Spigot derivative reads, plus a token. |
| `bungeecord` | every version | The same data without a token. |

For a network with versions older than 1.13, use BungeeGuard:

```lua
rift.config.forwarding = { mode = "bungeeguard", secret_env = "RIFT_FORWARDING_SECRET" }
```

Set `settings.bungeecord: true` in each backend's `spigot.yml` and install the
[BungeeGuard](https://github.com/lucko/BungeeGuard) plugin with the same token
in `allowed-tokens`.

> [!WARNING]
> With `mode = "bungeecord"`, anyone who can reach a backend can join as any
> player. Use it only when backends accept connections from Rift alone.

## Offline mode

Without `authentication` and `forwarding`, Rift runs in offline mode. It is
the default and what `rift init` generates; `rift check` warns about it.

> [!WARNING]
> Offline mode does not verify names. Anyone who can reach a listener can join
> under any name, including an operator's.

- Keep backend ports private.
- Offline backends need `online-mode=false`, forwarding disabled and
  `enforce-secure-profile=false`.
- Plugin player hooks require online mode.

## Signed chat

`authentication.signed_chat` (default `true`) decides whether backends learn
the material they verify a player's chat signatures with. A backend without it
takes the player's chat as unsigned. That happens in two ways:

- **For every player**, when you set `signed_chat = false`.
- **For one player**, when a [`chat` hook](plugin-events.md#chat) denies one of
  their messages, until they reconnect.
  See [how the chat hook drops a message](#how-the-chat-hook-drops-a-message).

A player with unsigned chat is shown to others as not secure (the client's
unsigned-message marker) and cannot be reported, because nothing they say is
signed towards the backend. Clients before 1.19 and offline-mode networks are
unaffected; they sign nothing.

### What Rift withholds

With `signed_chat = false`, on every backend attachment, translated or not:

| Client | Withheld |
| --- | --- |
| 1.19–1.19.2 | The chat key: the backend's Login Start is rewritten without it, and Velocity forwarding answers with version 1, as for a player who has no key. Rift still checks the client's signature over the login challenge. |
| 1.19.3 and newer | Every serverbound Player Session packet (chat session update), at login and whenever the client renews its key. |

Chat messages, commands, their signatures and last-seen acknowledgements are
forwarded as before, but the backend neither verifies them nor tracks a
signature chain. That makes it safe for Rift to drop a single message or
command. With signed chat, a backend that misses one signed message rejects
every later one.

Changing the setting applies to connections made afterwards and restarts
every persistent plugin, as a change of `online_mode` does.

### Backends must not require signed chat

| Backend | What to do |
| --- | --- |
| Vanilla and Spigot | They enforce `enforce-secure-profile` only with `online-mode=true`, which a backend behind a proxy does not have. From 1.19.1 on the setting has no effect there; a 1.19 backend must keep `enforce-secure-profile=false` (see [translation](translation.md)). |
| Paper | Set `enforce-secure-profile=false` in `server.properties`. Paper also enforces it when its proxy settings say the proxy is in online mode (`proxies.velocity.online-mode: true`, or `proxies.bungee-cord.online-mode` with `settings.bungeecord`). Otherwise players cannot chat: the server answers each message with "Chat disabled due to missing profile public key". Read from the bytecode of Paper 1.21.11 build 132, not from a run. |
| Pumpkin 0.2.0 | Nothing with its default `allow_chat_reports = false`: it ignores Player Session packets and message signatures and broadcasts chat unsigned, although it announces secure-chat enforcement (run against the pinned release in `tests/servers.json`, 0.2.0+26.3-26.51, protocol 777). `allow_chat_reports = true`, which Pumpkin permits only with its own `online_mode`, kicks a player whose message has no signature, and one without a session for an expired key. It does not work with `signed_chat = false` (read from the source, not run). |

### How the chat hook drops a message

A plugin or the configuration script sees each chat message and backend
command first. Rift holds the packet. On a deny, it sends the backend only the
last-seen acknowledgement the packet carried, as it does for a proxy command.

With `signed_chat = true`, a packet the hook lets through reaches the backend
as the client signed it. Unsigned chat begins for one player when the hook
first denies a packet of theirs that the backend would have verified:

1. Rift drops the packet. The session stops giving out the player's signing
   material: the chat key and every later Player Session packet are withheld
   as in the table above.
2. The backend still has the old material and a chain that misses a link, so
   Rift closes that backend connection and logs the player in to the same
   server again, the way it recovers a player after an outage. The player
   sees the world load once, as for a server switch.
3. Until the player reconnects to the proxy, every backend takes their chat
   as unsigned. Their Paper backends need `enforce-secure-profile=false`.

The new login in step 2 is not a transfer: no `before_transfer` or
`after_transfer` runs and no queue slot is taken. If the server cannot be
reached, the player moves to a hub like after any outage. Signed packets the
client sends between the drop and the new login never reach the old backend.

In two cases Rift does not ask the hook about packets the backend verifies,
and they pass unseen:

- The client is on a protocol without switching support (see
  [network protocol](network-protocol.md)), so Rift cannot attach a backend
  again.
- The packet arrives while a switch is under way.
