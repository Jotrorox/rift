# Operating a Rift release

The archives contain `rift` (`rift.exe` on Windows), `LICENSE`, `README.md`,
`examples/` and `docs/`. Rift is licensed under the [BSD-2-Clause License](../LICENSE).
Keep the examples alongside these documents so relative links keep working.

## First start

Download the archive for your OS/CPU and `SHA256SUMS` from the same release.
On Linux, check and extract it in an empty directory:

```sh
sha256sum --check --ignore-missing SHA256SUMS
tar xzf rift-linux-x86_64.tar.gz
./rift --version
cp examples/rift.lua rift.lua
# Edit listener and backend addresses for your server.
./rift --check rift.lua
./rift --config rift.lua
```

On macOS use `shasum -a 256 rift-macos-aarch64.tar.gz` and compare the hash with
its entry in `SHA256SUMS`; extract with `tar xzf`. On Windows compare
`Get-FileHash .\rift-windows-x86_64.zip -Algorithm SHA256` with its entry,
then use `Expand-Archive .\rift-windows-x86_64.zip` and run `rift.exe` inside it.
`--version` identifies the Cargo package version; nightly builds may share that
version, so retain the release tag and its `CHANGELOG.md` when reporting issues.

The basic example forwards port 25565 to a backend on loopback port 25566.
Configure that backend as described in the README. `examples/routing.lua` adds
a Lua hook; `examples/network.lua` adds hostname routing, fallback, admission,
health probes, status caching and loopback metrics. Adapt the example's hostnames
and backends before use. Validation does not test backend reachability.

Look for `rift: listening on ...` (and `rift: metrics on ...` if enabled), then
connect a Minecraft client through the public address. With metrics enabled,
`curl --fail http://127.0.0.1:9090/metrics` should succeed. Watch connection errors,
backend failures and rejected connections as well as active connections.

## Linux systemd service

The supplied [unit](../examples/rift.service) uses an unprivileged `rift` account
and root-owned configuration. Run these commands from the extracted archive on
a systemd host; create the account only if it does not exist:

```sh
sudo useradd --system --user-group --no-create-home --shell /usr/sbin/nologin rift
sudo install -m 0755 rift /usr/local/bin/rift
sudo install -d -o root -g rift -m 0750 /etc/rift
sudo install -o root -g rift -m 0640 examples/rift.lua /etc/rift/rift.lua
sudoedit /etc/rift/rift.lua
sudo -u rift /usr/local/bin/rift --check /etc/rift/rift.lua
sudo install -m 0644 examples/rift.service /etc/systemd/system/rift.service
sudo systemctl daemon-reload
sudo systemctl enable --now rift
sudo systemctl status rift
sudo journalctl -u rift -n 50 --no-pager
```

The unit validates before starting, restarts on failures with a five-second
delay, and caps restart bursts. An explicit stop stays stopped. Its file limit
is 8192; each active relay needs two sockets plus listener/runtime headroom.
If you raise `max_connections`, size both file descriptors and memory for it.
The service needs no writable data directory. LuaJIT uses executable memory,
so do not add `MemoryDenyWriteExecute=true` to this unit.

## Reload configuration

Save a candidate in the same directory, validate it as the service user, then
replace the live file atomically. Keep a backup for rollback:

```sh
sudo cp -p /etc/rift/rift.lua /etc/rift/rift.lua.previous
sudo cp -p /etc/rift/rift.lua /etc/rift/rift.lua.next
sudoedit /etc/rift/rift.lua.next
sudo -u rift /usr/local/bin/rift --check /etc/rift/rift.lua.next
# Continue only after validation succeeds.
sudo mv /etc/rift/rift.lua.next /etc/rift/rift.lua
sudo systemctl reload rift
sudo journalctl -u rift --since '1 minute ago' --no-pager
```

`ExecReload` checks the file, then sends SIGHUP. Signal delivery is asynchronous:
**a successful `systemctl reload` does not prove Rift accepted the reload**.
Confirm `rift: configuration reloaded` or an increase in `rift_reloads_total`.
On `rift: reload rejected`, the old configuration is still active; inspect the
error, restore the backup or fix the file, and reload again. `--check` cannot
detect changes that are incompatible with the running process.

Listener names/addresses and the metrics bind address require a restart.
Routes, backends, limits and the shutdown deadline can reload. Existing sessions
keep their original sockets and routing; new sessions use the new settings.
Outside systemd, use `kill -HUP <pid>` on Unix or Ctrl-Break on Windows.

## Shutdown, upgrade and rollback

```sh
sudo systemctl stop rift
sudo journalctl -u rift -n 50 --no-pager
```

SIGTERM stops admission and drains sessions for `shutdown_timeout_ms` (30 seconds
by default). Players still connected at the deadline are disconnected; arrange
a maintenance window if needed. A second SIGTERM/SIGINT forces remaining sessions
closed immediately. Look for `rift: draining ...` followed by
`rift: shutdown complete`; `shutdown deadline reached` means the drain expired.
The unit gives Rift 40 seconds before systemd sends SIGKILL. **If you increase
`shutdown_timeout_ms`, increase `TimeoutStopSec` beyond it**, including when you
reload the application setting. Apply unit edits with `systemctl daemon-reload`.
Console runs use Ctrl-C to drain and a second Ctrl-C to force closure.

For an upgrade, verify and extract the new archive separately, record the old
release tag/version, and run the new binary's `--check /etc/rift/rift.lua` as the
service user. Keep the old binary and configuration. Stop the service, install
the new binary with mode 0755, then start it and check logs, metrics and a client
connection. To roll back, stop, restore the previous binary and matching config,
and start again. A restart disconnects sessions; reload only updates configuration.

## Sizing

See the [local pilot and default rationale](pilot.md). It is a reproducible
loopback exercise, not a production player-capacity claim. Monitor your own
traffic before increasing limits. Optional rate limits, probes, caching and
metrics remain opt-in; in particular, users sharing one NAT also share one IP
token bucket.

Service behavior follows systemd's
[`ExecReload` and `TimeoutStopSec` documentation](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html).
