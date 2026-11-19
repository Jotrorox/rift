# Admin CLI

`rift admin` runs operator commands against a running Rift: status, reloads,
maintenance, transfers, managed servers, plugins and permissions. It talks to
the `admin` endpoint, a loopback socket that is off until you configure it.

## Enable the endpoint

1. Add `admin` to the configuration, through `rift.config`,
   `rift.setup({ ... })` or a legacy returned Lua configuration table:

   ```lua
   admin = {
       listen = "127.0.0.1:9091",
       token_env = "RIFT_ADMIN_TOKEN",
       permissions = { "status", "servers", "maintenance", "drain", "transfer", "reload", "shutdown", "plugins", "logs", "perms" },
   },
   ```

2. Set the token for the server and for the administrator, then start Rift:

   ```sh
   export RIFT_ADMIN_TOKEN="$(openssl rand -hex 32)"
   ./rift check
   ./rift
   # In a separate shell with the same secret:
   ./rift admin status
   ```

- `listen` accepts only loopback addresses. The endpoint speaks JSON lines
  over TCP and requires the token on every request.
- The token is a random secret of 32–1024 bytes without control characters.
  The server reads it from the variable named by `token_env` and verifies it
  at startup. `rift check` validates only the configuration shape;
  `rift check --strict` verifies the secret beforehand.
- The CLI always reads `RIFT_ADMIN_TOKEN`, even if the server uses a different
  `token_env` variable.
- There is one credential and one permission set, not a multi-user identity
  store. Changing the credential or the permissions needs a restart.
- Keep secrets out of Lua files, command arguments and committed Compose
  files. Under systemd, use
  [`/etc/rift/admin.env`](deployment.md#run-under-systemd).

The CLI connects to `127.0.0.1:9091` by default. Use
`rift admin --address 127.0.0.1:9092 status` for another loopback port, and
run the local CLI through SSH for remote administration.

This endpoint is separate from the optional `web`
[dashboard and HTTP API](http.md) and its bearer token. Both can run together
on distinct ports.

## Permissions

`permissions` lists what the endpoint allows: `status`, `servers`,
`maintenance`, `drain`, `transfer`, `reload`, `shutdown`, `plugins`, `logs`,
`perms` and `cluster`. Remove the operations a credential must not perform,
or use `{ "status" }` for a read-only endpoint. `cluster` allows membership
changes, leadership transfers and backups of a [cluster](clustering.md);
reading the cluster's status needs only `status`.

## Commands

Prefix each command with `rift admin`.

| Command | Needs | What it does |
| --- | --- | --- |
| `status` | `status` | Reports connected players with their login names and connection IDs, backend state, maintenance state and managed server state |
| `reload` | `reload` | [Reloads the configuration](operations.md#reload-the-configuration) after validating it |
| `maintenance on\|off` | `maintenance` | Turns [maintenance mode](operations.md#maintenance-mode) on or off |
| `drain <backend> on\|off` | `drain` | Blocks or allows [new attachments](operations.md#drain-a-backend) to a backend |
| `transfer <connection-id> <backend>` | `transfer` | [Moves an online player](operations.md#transfer-a-player) to another backend |
| `shutdown` | `shutdown` | [Stops Rift gracefully](upgrading.md#shut-down-gracefully) |
| `shutdown all` | `shutdown` | Also stops managed servers with [`keep_running`](upgrading.md#servers-that-keep-running) |
| `servers` | `servers` | Lists [managed servers](managed-servers.md) with state, players, `reservations` and `last_error` |
| `start <backend>` | `servers` | Starts a managed server |
| `stop <backend>` | `servers` | [Stops a managed server](operations.md#stop-a-managed-backend-for-maintenance) |
| `groups` | `servers` | Lists [service groups](service-groups.md) |
| `create <group>` | `servers` | Allocates a port and registers a stopped backend in the group |
| `remove <instance>` | `servers` | Removes an instance after child cleanup; refuses while it has players or pending attachments |
| `move <server> <node> [--keep-source] [--no-copy]` | `servers` | [Moves a persistent instance or a named server](nodes.md#move-a-persistent-instance-to-another-node) and its folder to another node; returns when the move has finished |
| `move <server> --cancel` | `servers` | [Cancels a running move](nodes.md#interrupted-and-cancelled-moves) before its commit; the server stays on its old node |
| `backup <instance>` | `servers` | Takes a [backup](backups.md) snapshot of a persistent instance on a node now |
| `backups [instance]` | `servers` | Lists the backed-up instances, or one instance's snapshots |
| `backups forget <instance> <snapshot>\|all` | `servers` | Deletes snapshots and the files only they used |
| `restore <instance> [snapshot] [--node <node>]` | `servers` | [Restores an instance](backups.md#restore-an-instance) from its newest or a given snapshot, in place or onto another node; returns when it has finished |
| `group start <group>`, `group stop <group>` | `servers` | Starts or stops a [whole group](service-groups.md#start-stop-and-dev-mode-for-a-whole-group) |
| `group mode <group> active\|dev` | `servers` | Sets the group's mode |
| `nodes` | `servers` | Lists [node agents](nodes.md) |
| `node cordon <node>`, `node drain <node>`, `node uncordon <node>` | `servers` | [Takes a node out of service](nodes.md#take-a-node-out-of-service) or returns it |
| `node plan [--spread <percent>]` | `servers` | [Suggests moves](nodes.md#even-out-the-nodes) that even out memory across nodes; moves nothing |
| `templates` | `servers` | Lists templates and git template sources with their cache and sync state |
| `download <template> [refresh]` | `servers` | Downloads a template's [server software](templates.md#download-server-software-automatically) ahead of time; `refresh` replaces the cached jar |
| `sync <template-source>` | `servers` | Pulls new commits of a [git template source](templates.md#template-folders-from-git) |
| `warm [template]` | `servers` | Has every [node](nodes.md#warm-the-node-caches) download its groups' templates and server software now, or only those of one template |
| `plugins list`, `plugins status <name>` | `status` | Shows [plugins](plugin-operations.md) with state, restarts, calls and last error |
| `plugins logs <name>` | `logs` | Shows a plugin's last 200 log lines |
| `plugins restart <name>` | `plugins` | Stops a persistent plugin and starts a fresh instance with its current configuration; recorded in the operator audit log with the plugin name |
| `plugins run <command> [arguments]` | `plugins` | Runs a persistent plugin's [command](plugin-events.md#commands) that is registered with `console`, and prints its message |
| `perms <command>` | `perms` | Runs the [`/perms` grammar](permissions-commands.md) with console rights, plus `log`, `compiled`, `doctor` and `backends` |
| `cluster`, `cluster status [--leader]` | `status` | Shows this [cluster](clustering.md#operating-a-cluster) peer's view of members, roles, leader, term, indexes and lag; `--leader` fetches the leader's view, which knows every peer's lag |
| `cluster add <name> <host:port> [--witness]` | `cluster` | Adds a peer as a learner; see [membership commands](clustering.md#rift-admin-cluster) |
| `cluster promote <name>`, `cluster demote <name> [--force]` | `cluster` | Makes a caught-up learner a voter, or a voter a learner |
| `cluster remove <name> [--force]`, `cluster replace <name> [--force]` | `cluster` | Removes a peer, or re-adds one that lost its Raft file under a new id |
| `cluster leave <name>` | `cluster` | [Drains a peer](clustering.md#rolling-restarts): it hands leadership away, sends its players to other peers and exits |
| `cluster transfer-leader [<name>]` | `cluster` | Moves leadership to another voter |
| `cluster backup <file>` | `cluster` | Writes a [backup](clustering.md#backup-and-recovery) of the cluster's state on the proxy host |

- `start`, `stop` and `plugins restart` return once Rift accepts the request.
  Poll `servers` to verify completion; a failed start appears in the server's
  `last_error` field.
- The server commands never accept arbitrary process commands and never modify
  ordinary unmanaged backends.
- For `perms`, import and export paths are resolved where the CLI runs, and
  the proxy reads or writes the file. The same holds for `cluster backup`.
- On a cluster follower, `move`, `backup`, `backups forget`, `restore` and
  `warm` run on the leader for you, as do starts, stops, `create` and
  `remove`; `maintenance` and `drain` change only the peer you ask.
