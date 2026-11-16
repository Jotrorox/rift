# Server list and tab list

Set `server_list` to give your network its own entry in the multiplayer menu:
MOTD, favicon and player count. Set `tab_list` to give every player the same
tab list header and footer on every backend, and optionally to list everyone
on the network.

## Server list

Without `server_list`, Rift answers server-list pings itself with the
network's player count, `limits.max_connections` as the maximum and the
description `Rift`. With `status_cache`, it relays the routed backend's
response instead. To set the network's own entry:

```lua
rift.config.server_list = {
    -- A string (section-sign codes such as "\u{00A7}6" work) or a chat component table.
    motd = { text = "My Network", color = "gold", extra = { { text = "\nNow on 1.21", color = "gray" } } },
    maintenance_motd = "Back soon. Follow @mynetwork for updates.",
    version = { mini = "<gold>My Network</gold> <gray>1.8-1.21" }, -- Shown when a client's version is incompatible.
    max_players = 500,               -- Defaults to limits.max_connections.
    favicon = "icons/network.png",   -- 64x64 PNG up to 16 KiB, relative to rift.lua, or a data URL.
    sample = { { mini = "<green>Survival</green>, <aqua>Creative" }, "and Minigames" }, -- Hover text over the player count.
    hosts = {
        ["survival.example.com"] = { motd = "Survival", favicon = "icons/survival.png" },
        ["*.minigames.example.com"] = { motd = "Minigames", max_players = 200 },
    },
}
```

Rift answers each ping directly and never contacts a backend, so `server_list`
cannot be combined with `status_cache`. Every field is optional:

| Field | Default | Meaning |
| --- | --- | --- |
| `motd` | `Rift` | The description. |
| `maintenance_motd` | "The network is under maintenance." | Replaces `motd` during maintenance. |
| `version` | `Rift` | Version name, shown when a client's version is incompatible. |
| `max_players` | `limits.max_connections` | The maximum next to the player count. |
| `favicon` | none | A 64×64 PNG up to 16 KiB: a path relative to `rift.lua`, or a `data:image/png;base64,...` URL. |
| `sample` | none | Lines of hover text over the player count. |
| `hosts` | none | Entries for single hostnames, with the fields above. |

- **Player count:** the players logged in through Rift across all backends,
  without those a [visibility rule](permissions.md#visibility) hides.
- **Defaults:** the default MOTDs and version name are the `status.*` entries
  of Rift's [message catalogue](text.md#rifts-own-messages).
- **`hosts`:** keys use the same patterns as hostname routes (exact names
  first, then the longest `*.` suffix). An entry inherits any unset field from
  the top level.
- **Unrouted hostnames:** a ping for a hostname with no route keeps the reply
  "No server is configured for this hostname."
- **Limits:** Rift rejects settings that would exceed the protocol's
  32767-byte status limit.

### Text fields

`motd`, `maintenance_motd`, `version` and `sample` lines are each a string or
a chat component table, including the `{ mini = ... }` and `{ legacy = ... }`
forms of [styled text](text.md). Rift rewrites the MOTD for each pinging
client, like any component.

Clients read `version` and `sample` as strings, so Rift writes components
there as `§` codes, with hex colors as the nearest named color. A plain string
is sent as written.

### Favicon

Rift reads favicon files when it loads or reloads the configuration. To change
the icon, replace the file and reload. Rift rejects missing files and images
that are not 64×64 PNGs.

A favicon can also be the image itself, as a `data:image/png;base64,...` URL.
The dashboard's Server list page stores an uploaded icon that way: it accepts
PNG, JPEG, WebP or GIF, scales the image to fit 64×64 and saves it in the
configuration, so no file has to exist next to `rift.lua`.

### Answer pings from Lua

For per-ping logic, register a `status` handler. It runs before Rift contacts
any backend:

```lua
rift.on("status", function(ping)
    if ping.maintenance then
        -- Protocol -1 makes every client show the version text in place of the count.
        return { motd = "Scheduled maintenance until 18:00 UTC", version = "Maintenance", protocol = -1 }
    end
    if ping.hostname == "event.example.com" then
        return { motd = { text = "Build contest is live!", color = "aqua" } }
    end
end)
```

| Direction | Fields |
| --- | --- |
| Receives | `listener`, `hostname`, `protocol`, `peer_addr`, `peer_ip`, `backend` (the routed backend or group, or nil for an unmatched hostname), `maintenance`, `online`, `max_players` |
| Returns nil, or a table with any of | `motd`, `version`, `protocol`, `online`, `max_players`, `sample`, `favicon` (a `data:image/png;base64,...` URL) |

- A result applies on top of `server_list`, or of Rift's default response.
  It also replaces `status_cache` passthrough for that ping.
- Returning nil keeps the default, or the passthrough.
- On an error, overload, an invalid result or an oversized response, Rift
  logs a `status_hook_failed` event and serves the default document, so a
  script never blanks the server list.

The handler runs on two workers of its own, separate from the four `on_route`
workers. A flood of pings cannot delay logins, and busy routing cannot blank
the server list. Both hooks wait up to 25 ms for a free worker before failing,
and each call has a 50 ms deadline and a fresh VM.

## Tab list

Set `tab_list` in `rift.lua`:

```lua
rift.config.tab_list = {
    -- A string (section-sign codes work) or a chat component table.
    header = { text = "My Network", color = "gold", bold = true },
    footer = "\u{00A7}7{online} players online \u{00A7}8| \u{00A7}7You are on {server}",
    players = "network", -- "server" (the default) lists only the current backend's players.
}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `header`, `footer` | none | Text above and below the list. Each is limited to 4096 bytes before placeholders are replaced. |
| `players` | `"server"` | `"server"` lists only the current backend's players. `"network"` adds the players on every other backend. |

The tab list works on every Minecraft version Rift can switch players on
(1.8.9–26.3). Reloads apply to connected players. Removing `tab_list` removes
Rift's entries and blanks the header and footer, after which backends' own
updates show again.

### Header and footer

| Placeholder | Value |
| --- | --- |
| `{player}` | The player's name |
| `{server}` | Their current backend |
| `{online}` | Players connected through Rift |
| `{server_online}` | Players on their backend |

Other text in braces is left as it is.

- Rift sends the header and footer when the player joins, after each server
  switch, and within half a second of any change to the values they show.
- While Rift sets a header and footer, it drops the ones backends send, such
  as those from a tab list plugin.
- Setting only one of `header` and `footer` leaves the other blank.
- Plugins and `rift.lua` can set one player's header and footer with
  `rift.players.tab_list` (see [online players](players-api.md)).

### Network-wide players

With `players = "network"`, each player's list also shows the players on every
other backend.

- Backends keep listing their own players, with game mode, ping and display
  names from their plugins.
- Rift only adds players the current backend does not list, so a player is
  never listed twice and moving between servers needs no cleanup.
- Entries Rift adds show the player's skin when authentication is enabled and
  the default skin otherwise, with full signal bars and no game mode.

### Prefixes, suffixes and order

A player with a prefix or suffix in Rift's [permissions](permissions.md) is
listed with them around their name, so players on other servers show their
rank like the backend's own. There is no setting for this.

- **Colors:** the text takes `&` codes (see
  [legacy codes](text.md#legacy-codes)), and a color carries into the name
  and suffix as in a chat format: `&6[VIP] ` lists `[VIP] Alice` in gold, and
  `&6[VIP] &f` keeps the name white.
- **Updates:** Rift resolves both for the listed player on the server they
  are on, when they join, on every server switch and whenever their
  permissions change. Players who already see the entry get the update within
  half a second. A prefix scoped to a `dimension` or `gamemode` context
  follows only those moments.
- **Order:** from 1.21.2, these entries carry the weight of the player's
  primary group as their list order. The client sorts them by weight, highest
  first, ahead of entries without an order. Older clients have no list order
  and sort as they always did. Entries a backend lists keep that backend's
  order.
- **No prefix or suffix:** such a player, and every player on a network that
  does not use Rift's permissions, is listed by name alone, with no display
  name or order.

### Hidden players

A player whose [visibility rule](permissions.md#visibility) hides them from a
viewer is not listed to that viewer, nor counted in `{online}` and
`{server_online}`. Their own server's list is the backend's; a vanish plugin
there hides them.
