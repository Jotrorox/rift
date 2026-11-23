# Deploy with systemd or Docker

This page turns a working `rift.lua` into a long-running service: a hardened
systemd unit on a Linux host, or a container started with Docker Compose.
[Install](install.md) Rift and generate the configuration first.

## Run under systemd

The supplied [unit](../examples/rift.service) uses an unprivileged `rift`
account and a root-owned configuration. Run these steps on a systemd host,
from the extracted Linux archive, after generating `rift.lua` there.

1. Create the account, unless it exists:

   ```sh
   sudo useradd --system --user-group --no-create-home --shell /usr/sbin/nologin rift
   ```

2. Install the executable and the configuration:

   ```sh
   sudo install -m 0755 rift /usr/local/bin/rift
   sudo install -d -o root -g rift -m 0750 /etc/rift
   sudo install -o root -g rift -m 0640 rift.lua /etc/rift/rift.lua
   ```

3. Move the instance database to the state directory, edit the configuration
   and check it as the service account:

   ```sh
   printf '%s\n' 'rift.config.instance_database = "/var/lib/rift/rift.sqlite3"' \
     | sudo tee -a /etc/rift/rift.lua > /dev/null
   sudoedit /etc/rift/rift.lua
   sudo -u rift /usr/local/bin/rift check /etc/rift/rift.lua
   ```

4. If the [admin endpoint](admin.md) is enabled, create `/etc/rift/admin.env`,
   owned by root with mode 0600, containing `RIFT_ADMIN_TOKEN=<your-secret>`.
   Supply the same secret in your CLI environment.

5. Install and start the unit:

   ```sh
   sudo install -m 0644 examples/rift.service /etc/systemd/system/rift.service
   sudo systemctl daemon-reload
   sudo systemctl enable --now rift
   sudo systemctl status rift
   sudo journalctl -u rift -n 50 --no-pager
   ```

### What the unit does

- **Start and restart.** It validates the configuration before starting,
  restarts on failure after a five-second delay, and caps restart bursts. An
  explicit stop stays stopped.
- **Secrets.** The optional `EnvironmentFile` loads `admin.env` without making
  the secret part of the process command line.
- **File limit.** It is 8192. Each active session needs a client and a backend
  socket, plus runtime headroom. If you raise `max_connections`, size both
  file descriptors and memory for it.
- **State.** `StateDirectory=rift` creates `/var/lib/rift`, owned by the
  service account with mode 0700, and permits writes there under
  `ProtectSystem=strict`. The directory survives stops and restarts.
- **Memory cgroup.** `Delegate=memory` gives Rift the unit's cgroup to place
  limited servers in. `DelegateSubgroup=rift` needs systemd 254 and is ignored
  by older versions.
- **Sandbox.** Rift and the managed servers it starts run without
  capabilities, without access to `/home`, device nodes, kernel logs or
  modules, other users' processes in `/proc`, or new namespaces, and with a
  private `/tmp`. Everything outside `/var/lib/rift` is read-only.
  `systemd-analyze security rift` lists what remains.

### Writable paths

The configured `instance_database` keeps its SQLite registrations, lock and
journal in `/var/lib/rift`. Every file-based startup opens this database, even
without service groups, and the default location beside the root-owned
configuration is not writable.

`rift check` does not open the database; run
`sudo -u rift rift check --strict /etc/rift/rift.lua` to verify that it opens
or can be created. With the instance database in
[PostgreSQL or MariaDB](database.md), put `RIFT_DATABASE_URL` in the unit's
environment file instead; the SQLite file is then not used.

Point `software_cache`, `template_storage` and server directories at paths
below the state directory, or add them with `ReadWritePaths=`. The
configuration stays read-only to Rift; to save it from the browser, see the
[web dashboard](http.md).

### Settings left out on purpose

Do not add these to the unit:

| Setting | Reason |
| --- | --- |
| `MemoryDenyWriteExecute=true` | LuaJIT and the JVMs of managed servers generate code |
| `ProtectKernelTunables=true`, `ProtectControlGroups=true` | Both make the unit's cgroup read-only, which disables [memory limits](server-limits.md#memory-limits) |
| `SystemCallFilter=` | Managed servers are arbitrary programs, and [isolation](server-limits.md#isolation) needs Landlock's system calls |

## Run a cluster

A [cluster](clustering.md) runs one Rift per host, each a peer with its own
`cluster.toml`. [examples/cluster](../examples/cluster/rift.lua) has the
configuration, a `cluster.toml` per peer, a witness and two units:
[rift-cluster.service](../examples/cluster/rift-cluster.service) for a peer
and [rift-witness.service](../examples/cluster/rift-witness.service) for a
witness.

A peer writes its own `rift.lua` when a deployment arrives, and keeps its
Raft log, snapshots and template contents beside it, so the configuration
lives in the state directory rather than in a read-only `/etc/rift`:

```sh
sudo install -d -o root -g rift -m 0750 /etc/rift/cluster
sudo install -o root -g rift -m 0640 ca.pem eu-2.pem eu-2.key /etc/rift/cluster/
sudo install -o root -g root -m 0600 cluster.env /etc/rift/cluster.env   # RIFT_CLUSTER_TOKEN=..., other secrets
sudo install -d -o rift -g rift -m 0700 /var/lib/rift
sudo install -o rift -g rift -m 0600 eu-2.toml /var/lib/rift/cluster.toml
sudo install -m 0644 examples/cluster/rift-cluster.service /etc/systemd/system/rift.service
sudo systemctl daemon-reload
sudo systemctl enable --now rift
```

- **First peer.** Put the existing `rift.lua` in `/var/lib/rift` too, and run
  `sudo -u rift rift cluster init --peer eu-1 --config /var/lib/rift/rift.lua`
  before the first start.
- **Other peers** start without a `rift.lua`. Add them from a member with
  `rift admin cluster add` and `promote`; they then receive the deployed
  configuration. The unit therefore runs no `rift check` before starting.
- **Stopping** a peer drains it: its 1.20.5+ players move to another peer,
  so `TimeoutStopSec` must exceed the five-second transfer wait plus
  `shutdown_timeout_ms`.
- **Players** reach the peers through DNS round robin, an SRV record
  (`_minecraft._tcp`) per peer, or a TCP load balancer. Behind a load
  balancer, enable the [PROXY protocol](hardening.md#run-behind-a-load-balancer)
  on every peer and make each peer's `public_address` reach that peer alone.
- **Firewall.** Open the UDP port of `listen` in `cluster.toml` between the
  peers only; the UDP port of `messaging.listen` to bus clients and node
  agents; and the game port (TCP) to players or the load balancer.
- **Secrets** go into `/etc/rift/cluster.env` on every peer, with the same
  values everywhere: `RIFT_CLUSTER_TOKEN`, the web token, the forwarding
  secret and the node tokens. `RIFT_ADMIN_TOKEN` may differ per host.

A cluster does not run in the Compose setup below as it stands: every peer
needs its own host (or at least its own state directory and addresses) and
its `cluster.toml`.

## Run in a container

Each release publishes a multi-platform image (`linux/amd64` and
`linux/arm64`), built from the [Dockerfile](../examples/Dockerfile) and the
release's own glibc executables, with a build provenance attestation:

```sh
docker pull ghcr.io/jotrorox/rift:0.2.0   # or :latest for the newest release
gh attestation verify oci://ghcr.io/jotrorox/rift:0.2.0 --repo Jotrorox/rift
```

- Release candidates are published under their own version tag
  (`0.2.0-rc.1`) and never as `latest`.
- The image contains Rift only and has no Java. Use it for the proxy and run
  game servers on [nodes](nodes.md) or other hosts, or build your own image on
  top of it for managed servers.
- To use it with the Compose example, set
  `RIFT_IMAGE=ghcr.io/jotrorox/rift:0.2.0` and start with
  `docker compose -f examples/compose.yaml up -d --no-build`.

### Build and start with Compose

The [Compose example](../examples/compose.yaml) runs as UID/GID 65532 with a
read-only root filesystem, no Linux capabilities, a read-only config
directory, a writable `rift-state` volume at `/var/lib/rift` and a 40-second
stop deadline. Run from the extracted Linux x86_64 archive:

```sh
mkdir config
./rift init config/rift.lua
printf '%s\n' 'rift.config.instance_database = "/var/lib/rift/rift.sqlite3"' >> config/rift.lua
# Edit config/rift.lua; make it readable by container UID 65532.
./rift check config/rift.lua
export RIFT_IMAGE=rift:my-release
# Set RIFT_ADMIN_TOKEN here too if administration is enabled.
docker compose -f examples/compose.yaml build
docker compose -f examples/compose.yaml run --rm rift check /etc/rift/rift.lua
docker compose -f examples/compose.yaml up -d
docker compose -f examples/compose.yaml logs --tail=50 rift
```

Confirm that the container reaches `rift: listening` in its logs after
`up -d`: `rift check` alone does not verify database write access
(`rift check --strict` does).

- The Dockerfile packages the glibc Linux release binary on Ubuntu 24.04. Any
  base with glibc 2.35 or newer works, and the musl binary runs on every base.
- The image build runs `rift --version` to detect a wrong architecture or an
  incompatible runtime before startup.
- For a source build, set `RIFT_BINARY=target/release/rift` and make sure its
  architecture and libc match the image.
- The example fixes `platform: linux/amd64`. Change it to `linux/arm64` when
  you build from the `aarch64` archive.

### Volumes

| Path | Mount | Notes |
| --- | --- | --- |
| `/etc/rift` | Read-only bind mount of `config` | A directory mount makes atomic configuration replacements visible inside the container |
| `/var/lib/rift` | Named volume `rift-state` | SQLite database, lock and journal. The image creates it with owner 65532:65532 and mode 0700; Docker preserves these when it initializes the volume |

The volume survives container recreation and `docker compose down`;
`down --volumes` removes the stored registrations. An existing volume or a
replacement image must also provide write access for UID/GID 65532.

### Addresses

Loopback in a container refers to that container. Use private, reachable
backend addresses or service DNS names on a shared container network.

To expose metrics through the example's host-loopback port mapping, set
`metrics = "0.0.0.0:9090"` inside the container. Keep admin on
`127.0.0.1:9091` and run its commands inside the container.

### Operate the container

```sh
docker compose -f examples/compose.yaml exec rift rift admin status
# After editing and validating the host config:
docker compose -f examples/compose.yaml exec rift rift check /etc/rift/rift.lua
docker compose -f examples/compose.yaml kill -s SIGHUP rift
docker compose -f examples/compose.yaml logs --tail=50 rift
docker compose -f examples/compose.yaml stop
```

Compose passes the server token into the container, and `exec` uses that
environment. Increase
[`stop_grace_period`](https://docs.docker.com/reference/compose-file/services/#stop_grace_period)
if you increase `shutdown_timeout_ms`. The exec-form
[entrypoint](https://docs.docker.com/reference/dockerfile/#entrypoint)
receives SIGTERM directly.

To upgrade a container, see [upgrading](upgrading.md#upgrade-a-container).
