# Servers that outlive the proxy

By default every local server stops with Rift. With `keep_running = true` on a
managed server or service group, its processes keep running while Rift shuts
down, restarts, is upgraded or crashes, and the next Rift adopts them, as with
servers on a [node](nodes.md):

```lua
rift.config.managed_servers.survival = {
    directory = "servers/survival",
    command = { "java", "-Xmx2G", "-jar", "paper.jar", "nogui" },
    keep_running = true,
}
```

Players lose their connection when the proxy goes away, but the worlds stay
loaded, and players rejoin as soon as the proxy is back.

- The setting works on Linux and macOS. Elsewhere, and for servers on a node
  (which keep running anyway), the configuration is rejected.
- It cannot be combined with `storage = "ephemeral"`; use `disposable` or
  `persistent`.
- It is part of the process definition, like `command`: named servers take it
  on a restart of Rift, groups while they have no instances.

A kept server does not depend on Rift's process:

- It leads its own session, so neither Ctrl-C in Rift's terminal nor Rift's
  death reaches it. A kill still reaches its whole process group.
- Output goes to `rift-server.log`. Rotation, the log endpoints and the
  dashboard console keep working after adoption.
- Console input arrives through the FIFO `rift-console` in the server
  directory. `rift admin`, the dashboard and a graceful `stop` write to it.
- [`isolation`](server-limits.md#isolation), `env` and `inherit_env` apply
  unchanged.

## Adoption

When Rift starts a kept server, it records the pid and its start time in
`rift-process.json` in the server directory. A starting Rift adopts the
recorded process only if that pid still has that start time, leads its own
session and runs in the server directory.

Rift never adopts or signals a process it did not start: it ignores any other
record, and a port held by anything else is still a start failure. The record
and the FIFO are removed when the server stops.

> [!WARNING]
> Do not delete `rift-process.json` while the server runs. Without it Rift
> cannot find the process again, and you have to stop the server by hand.

An adopted server is `starting` until its port accepts connections, within
`start_timeout_ms`, and then `running` with its pid. Idle stop, stop, drain,
removal and scaling apply as usual. Rift is not its parent, so two things
differ:

- The exit status is unknown. Every exit counts as a failure and is recovered
  within `restart_retries`, even where a child exiting with status 0 would
  become `stopped`. The restarted process is Rift's child again.
- Stop writes `stop` to the console and, after `stop_timeout_ms`, kills the
  process group.

Administrator intent still wins:

- A server stopped with `rift admin stop`, the dashboard or a group stop is not
  running, so there is nothing to adopt. For service instances the saved stop
  keeps it stopped after the restart.
- An instance that the database says is stopped but that is found running
  (Rift was killed between the two) is adopted and stopped.
- An instance whose removal was interrupted is stopped during startup
  recovery, before its files are deleted.
- A server that was still starting at an orderly shutdown is stopped, like any
  interrupted start.

A kept server runs with the command, environment and isolation it was started
with. To apply a changed definition, or to turn `keep_running` off, stop the
server and start it again. A server left running after you removed the setting
is not adopted, and its start fails because the port is in use.

Its bus login stays valid across the restart. Give `messaging.listen` a fixed
port so the address in `RIFT_BUS_ADDRESS` stays the same.

## Stop everything

`rift admin shutdown`, SIGTERM and Ctrl-C leave kept servers running. To stop
Rift and its kept servers together, use:

```sh
rift admin shutdown all
```

It drains sessions like `shutdown` and then stops every local server, kept or
not, with the usual `stop` and `stop_timeout_ms`. It sets no stop intent, so
the servers start again as configured with the next Rift. To stop single
servers first, use `rift admin stop <backend>` or
`rift admin group stop <group>`; those are remembered for service instances.

Under systemd, `KillMode=control-group` (the default, and the supplied unit's
setting) kills every process of the unit when it stops or restarts, kept
servers included. Set `KillMode=process` to let them survive
`systemctl restart rift`. `systemctl stop rift` then leaves them running too,
so stop them with `rift admin shutdown all`. In a container, the servers end
with the container whatever the setting says.
