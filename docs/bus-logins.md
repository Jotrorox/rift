# Bus logins for servers

A backend plugin that talks to Rift's [message bus](messaging.md), such as
[RiftPerms](riftperms.md), needs a principal and a token. Listing
`server.lobby-1`, `server.lobby-2`, … under `messaging.principals` cannot
follow scaling, and one shared login per group would let any lobby speak for
any other. A `messaging` block makes Rift issue every instance its own login:

```lua
rift.config.messaging = {
    listen = "0.0.0.0:7443",
    advertise = "10.0.0.2:7443", -- what instances connect to; optional for local ones
    certificate = "certs/messaging.pem",
    private_key = "certs/messaging-key.pem",
}
rift.config.service_groups.lobby = {
    template = "lobby",
    port_range = { 25600, 25649 },
    messaging = { preset = "backend" }, -- plus publish = {...}, subscribe = {...} if needed
}
```

Each instance authenticates as `server.<instance>` with these grants:

- the `publish` and `subscribe` subjects of the block;
- with `preset = "backend"`, the
  [permission replica's subjects](messaging.md#backend-permission-principals)
  and the [network API](messaging-subjects.md#network-api), with which an
  instance lists servers and moves its own players;
- always, publish access to `_INBOX.>` to answer requests, and to
  `rift.server.state` to report its own
  [state, properties and drain](messaging-subjects.md#server-state);
- always, subscribe access to `_INBOX.server.<instance>.>` only, never to
  `_INBOX.>`. REQUEST uses a private broker inbox and needs no grant; manual
  reply subjects must sit in the instance's own namespace.

Rift sets these variables for the server process, last, so `env` cannot
override them:

| Variable | Value |
| --- | --- |
| `RIFT_SERVER_NAME` | The instance (or managed server) name, such as `lobby-3`; set for every managed server |
| `RIFT_SERVER_GROUP` | Its service group, such as `lobby` |
| `RIFT_BUS_ADDRESS` | On a [cluster](clustering.md) peer its own bus address (`bus_address` in its `cluster.toml`, else its `advertise` host with the listener's port); otherwise `messaging.advertise`, else `127.0.0.1` and the listener's bound port |
| `RIFT_BUS_ADDRESSES` | Set only when the bus has several addresses (the peers of a [cluster](clustering.md)): all of them, `RIFT_BUS_ADDRESS` first, as `host:port` entries separated by commas. The Java client prefers it; clients that read only `RIFT_BUS_ADDRESS` keep working against the first peer |
| `RIFT_BUS_SERVER_NAME` | `messaging.advertise_server_name`, else (on a cluster peer) the host of its `bus_address`, else the host of `advertise`, else (on a cluster peer) the host of its bus address, else `localhost` |
| `RIFT_BUS_CERT` | An absolute path to `rift-bus.pem` in the server directory: the messaging certificate, written at creation and refreshed on every start |
| `RIFT_BUS_TOKEN` | The instance's token |

RiftPerms, the Pumpkin adapter and the Rust client fall back to these
variables when `server`, the bus address, certificate or token are unset. They
fill the `group` [permission context](permissions.md#the-model) from
`RIFT_SERVER_GROUP`, so one template configuration serves every instance. Java
services can use `MessagingClient.builder().fromEnvironment()`.

## Tokens

A token is `ri1.<name>.<generation>.<HMAC>`, keyed by `local/instance-key`
beside the configuration (32 random bytes, mode 0600, created on first use).
Keep that file out of templates and backups you share.

- The generation increases each time an instance of that name is created and
  is kept in the instance database; named managed servers have generation 0.
  A recreated instance never inherits its predecessor's token.
- Rift stores no tokens. The bus verifies the HMAC and that the instance is
  still registered at that generation, so tokens survive proxy restarts.
- Removing an instance withdraws its login and closes its open bus connections
  at once.

Changes and nodes:

- Grants come from the definition when an instance registers. A reload that
  changes them applies to new connections.
- A `messaging` block needs `messaging.listen`. The first block added to a
  running Rift takes effect after a restart, like other messaging changes.
- Servers on [nodes](nodes.md#security) get their token in each start command.
  They need `messaging.advertise` set to an address the nodes reach; `rift
  check` warns otherwise.
- `rift check` also warns when a group's template ships RiftPerms but the group
  has no `messaging` block.
