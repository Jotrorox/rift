# Shutdown, upgrade and rollback

Replacing the Rift process disconnects every session; a
[configuration reload](operations.md#reload-the-configuration) alone preserves
them. This page explains how Rift shuts down, how to upgrade to a new release
and how to go back.

## Shut down gracefully

A graceful stop is available through `rift admin shutdown`,
`systemctl stop rift`, Compose `stop`, or Ctrl-C.

1. SIGTERM stops admission and drains sessions for `shutdown_timeout_ms`
   (30 seconds by default). Metrics remain available during the drain.
2. Players still connected at the deadline are disconnected.
3. A second SIGTERM or SIGINT forces the remaining sessions closed
   immediately.

Look for `rift: draining ...` followed by `rift: shutdown complete`.
`shutdown deadline reached` means the drain expired.

> [!WARNING]
> Keep the supervisor's stop deadline longer than Rift's drain: 40 seconds in
> the examples.

Apply systemd edits with `systemctl daemon-reload`. See systemd's
[`ExecReload` and `TimeoutStopSec` documentation](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html).

## Servers that keep running

Local managed servers stop with Rift, after the drain. Servers with
[`keep_running`](kept-servers.md) do not:

- They keep running through a shutdown, an upgrade or a crash, and the next
  Rift adopts them. A restart costs players their connection but not the
  loaded worlds.
- This needs `KillMode=process` in the systemd unit. With the default
  `KillMode=control-group`, the unit's stop kills them.
- `rift admin shutdown all` stops Rift together with its kept servers. It
  needs the `shutdown` permission.

## Upgrade

1. [Verify](install.md) the new release archive and extract it into a separate
   directory. Record both release tags and retain the previous archive.
2. From the candidate directory:

   ```sh
   # Validate compatibility before touching the running installation.
   sudo -u rift ./rift check /etc/rift/rift.lua
   ./rift --version
   sudo install -d -m 0700 /var/backups/rift
   sudo cp -p /usr/local/bin/rift /var/backups/rift/rift.previous
   sudo cp -p /etc/rift/rift.lua /var/backups/rift/rift.lua.previous
   # Announce/prepare the maintenance window, then allow graceful drain.
   sudo systemctl stop rift
   # Copy the database only while Rift is stopped; a new release may migrate it.
   sudo cp -p /var/lib/rift/rift.sqlite3 /var/backups/rift/rift.sqlite3.previous
   sudo install -m 0755 ./rift /usr/local/bin/rift
   sudo systemctl start rift
   sudo systemctl status rift
   sudo journalctl -u rift -n 50 --no-pager
   ```

3. Confirm metrics, backend health and a real client login before declaring
   the upgrade complete.

Preserve `admin.env` securely when changing its format or secret.

## Upgrade nodes

With [nodes](nodes.md), upgrade the controller first and then the agents, one
node at a time. The controller checks each agent's features before it uses
them, so a new controller runs older agents and only refuses what they lack.
An older controller would not know about a newer agent's features. The
[version skew policy](stability.md#controller-and-node-agents) lists the
supported combinations.

1. Upgrade the controller as above. Agents reconnect by themselves and their
   servers keep running. `rift admin nodes` and the dashboard's Nodes panel
   flag each agent as `version_mismatch` until it is upgraded. An operation
   that needs a feature the agent lacks is refused on that node, with a
   message that says so.
2. Upgrade each node in turn. Restarting the agent stops its servers
   (`KillMode=control-group`), so drain the node first:

   ```sh
   # On the controller: no new instances, disposable games empty out.
   rift admin node drain node2
   rift admin nodes                     # wait for "drained": true
   rift admin stop survival             # persistent instances and named servers, if any
   # On the node, with the verified release extracted as for the controller:
   sudo install -m 0755 ./rift /usr/local/bin/rift
   sudo systemctl restart rift-node
   sudo journalctl -u rift-node -n 50 --no-pager
   # On the controller: version matches, nothing missing.
   rift admin nodes                     # "version_mismatch": false, "missing_features": []
   rift admin start survival
   rift admin node uncordon node2
   ```

   A server that is not stopped first is restarted by the controller once
   the agent is back, but its players are disconnected.
3. Once every node is done, run `rift check --strict` against the controller.
   It reports agents that would still refuse servers in the configuration.

Agents keep no database, so they need no backup. To roll back within the same
minor release, the controller can go back on its own. To roll back across a
minor release, roll back the agents as well: an older controller does not
support agents from a newer minor release.

## Roll back

If validation, startup or the client check fails, restore the matched binary,
config and database:

```sh
sudo systemctl stop rift
sudo install -m 0755 /var/backups/rift/rift.previous /usr/local/bin/rift
sudo install -o root -g rift -m 0640 /var/backups/rift/rift.lua.previous /etc/rift/rift.lua
sudo install -o rift -g rift -m 0600 /var/backups/rift/rift.sqlite3.previous /var/lib/rift/rift.sqlite3
sudo -u rift /usr/local/bin/rift --check /etc/rift/rift.lua
sudo systemctl start rift
sudo journalctl -u rift -n 50 --no-pager
```

## The database only moves forward

`instance_database` (`rift.sqlite3`) holds instance registrations and the
permission store. A new release migrates its schema in place at the first
start, and there is no downgrade.

- The previous binary refuses the migrated file with
  `instance database <path>: unsupported schema version N of <component>` and
  does not start.
- Rolling back the binary therefore means restoring the copy taken during the
  upgrade, which discards registrations and permission changes made since.
- Take the copy with Rift stopped. The file uses a rollback journal, so a
  stopped Rift leaves one complete file and no `-wal` companion.
- Where the path differs, use the configured `instance_database`. The `.lock`
  file beside it holds no data and needs no backup.
- The [changelog](../CHANGELOG.md) names each release that changes the schema.
- With the instance database in [PostgreSQL or MariaDB](database.md), take a
  `pg_dump` or `mariadb-dump --single-transaction` instead (it works while
  Rift runs); rolling back means restoring it.

## Upgrade a container

1. Retain the previous versioned image and its matching config.
2. Build the new image under a different `RIFT_IMAGE` tag and run its one-shot
   `check` command before stopping the running container.
3. Stop gracefully.
4. Copy `rift.sqlite3` out of the stopped container's volume, as above.
5. Run `up -d` with the new tag.

To roll back, restore the previous config and database copy, select the
previous image tag, validate it and run `up -d --no-build`. Avoid rebuilding
or reusing the previous tag until the upgrade is verified.

The Compose commands are in
[deployment](deployment.md#build-and-start-with-compose).
