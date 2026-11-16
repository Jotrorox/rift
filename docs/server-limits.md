# Isolation and memory limits

[Managed servers](managed-servers.md) run as Rift's user and can use any
amount of memory. On Linux, two settings confine them without any privilege:
`isolation` restricts the files a server can reach, and `memory_limit` caps the
memory of its whole process tree. Both work on managed servers and service
groups, and `rift check` reports what the host gives each of them.

## Isolation

Without isolation, a server plugin can read everything Rift can:

- `rift.lua`, `rift.sqlite3` and the messaging key;
- `local/instance-key`, with which it could mint any instance's
  [bus token](bus-logins.md);
- other instances' directories, and their tokens from `/proc/<pid>/environ`.

On Linux 5.13 and newer, `isolation` confines a server with
[Landlock](https://docs.kernel.org/userspace-api/landlock.html):

```lua
rift.config.service_groups.lobby = {
    -- ...
    isolation = "landlock",
    -- or: isolation = { mode = "required", read = { "/srv/shared-maps" }, write = { "/srv/backups/lobby" } },
}
```

| `isolation` | Where Landlock is available | Elsewhere (macOS, Windows, Linux before 5.13 or without Landlock) |
| --- | --- | --- |
| `"off"` (default) | unconfined | unconfined |
| `"landlock"` | confined | **starts unconfined**, as Rift's user, and logs why on every start |
| `"required"` | confined | **refuses to start**, with the reason as the start failure |

Changing `isolation` changes the process definition, like `command`.

### What a confined server can reach

| Access | Paths |
| --- | --- |
| Read and write | Its own directory and `/tmp` |
| Read and execute | `/usr`, `/lib*`, `/bin`, `/sbin`, the Java installation its command resolves to, and every existing absolute path in its command line (such as a shared `/absolute/paper.jar`) |
| Read | The system files that name resolution, TLS, time zones and user lookups need; the random devices; its own `/proc/self`; the system-wide `/proc` and `/sys` files the JVM sizes itself from |
| Added by `read` | Paths it may read and execute |
| Added by `write` | Paths it may also write |

Everything else is invisible, including other processes' `/proc` entries and
the rest of `/etc`, where a packaged Rift keeps `rift.lua`.

- `/tmp` is shared by every confined server. Keep Rift's configuration and
  instance directories out of it.
- Relative `read` and `write` paths are relative to the configuration. On
  [nodes](nodes.md) they must be absolute paths on the node.
- On Linux 6.12 (Landlock ABI 6) the server also cannot signal processes
  outside its sandbox, Rift included.
- Networking is not restricted: plugins listen on their own ports (votes,
  maps, queries) and connect to databases.

### Verify isolation

`rift check` prints, for every server and group with `isolation`, what this
host gives it, or the fallback and what it means:

```text
isolation service_groups.lobby: landlock ABI 6 (filesystem, signals)
```

A plugin that writes outside its directory, such as a world on another disk or
a shared database file, needs that path in `write`. Check the server log after
you enable isolation. For servers on nodes, see
[node security](nodes.md#security).

## Memory limits

`memory_mb` on a template group sets the Java heap (`-Xms`/`-Xmx`) and what
placement counts against a node. It limits nothing: the JVM uses memory beyond
its heap, and a custom `command` can use any amount. `memory_limit` is the
enforced limit, on the server's whole process tree:

```lua
rift.config.service_groups.lobby = {
    -- ...
    memory_mb = 2048,    -- the heap
    memory_limit = 3072, -- the process: heap, metaspace, threads, native memory
    -- or: memory_limit = { mb = 3072, required = true },
}
```

- The value is in MiB, `64` through `16777216`, and works with any command.
- On a group with `memory_mb` it must be larger than the heap. Leave the JVM a
  quarter of the heap or 512 MiB beyond it, whichever is more, and more for
  servers with many plugins.
- The kernel kills a server that reaches its limit, all of its processes
  together. Rift recovers it like any other unexpected exit
  (`restart_retries`). Swap does not extend the limit.
- Java reads the limit from the cgroup and sizes its defaults from it.
- Changing `memory_limit` changes the process definition, like `command`.

| `memory_limit` | Linux with a delegated cgroup v2 group | Elsewhere (macOS, Windows, cgroup v1, no delegation, most containers) |
| --- | --- | --- |
| not set (default) | unlimited | unlimited |
| `3072` or `{ mb = 3072 }` | limited | **starts unlimited** and logs why on every start |
| `{ mb = 3072, required = true }` | limited | **refuses to start**, with the reason as the start failure |

### Delegate a cgroup to Rift

Rift enforces the limit with one cgroup v2 group per server. It needs only a
cgroup it may manage: the one it runs in, delegated to its user. The supplied
[systemd unit](../examples/rift.service) does that with `Delegate=memory`. For
a manual start, use
`systemd-run --user --scope -p Delegate=yes rift --config rift.lua`.

Rift moves itself into a `rift` subgroup and puts each limited server into
`rift-server-<name>` beside it. Nothing else may run in Rift's cgroup, because
Rift never moves a process it did not start.

A container usually mounts `/sys/fs/cgroup` read-only. Limit the container
instead, or run servers on [nodes](nodes.md). On a node the agent applies the
limit in its own delegated cgroup.

With [`keep_running`](kept-servers.md) under
systemd, use systemd 254 or newer. The unit's `DelegateSubgroup=rift` is what
lets a restarted Rift join a cgroup that still holds running servers.

### Verify a limit

`rift check` prints, for every server and group with a limit, what this host
gives it, or the fallback:

```text
memory_limit service_groups.lobby: 3072 MiB, cgroup v2 below /sys/fs/cgroup/system.slice/rift.service
```
