# Install Rift

Rift is one executable for Linux (x86_64 and ARM64), macOS ARM64 and Windows
x86_64. This page covers downloading and verifying a release, the first start
and checking a configuration. On Linux, that is:

```sh
sha256sum --check --ignore-missing SHA256SUMS
tar xzf rift-linux-x86_64.tar.gz
./rift --version
./rift init
# Edit listener and backend addresses for your server.
./rift check
./rift
```

## Download

Take the archive for your OS and CPU, and `SHA256SUMS`, from the same
[release](https://github.com/Jotrorox/rift/releases).

| Asset | Contents |
| --- | --- |
| `rift-linux-x86_64.tar.gz`, `rift-linux-aarch64.tar.gz`, `rift-linux-x86_64-musl.tar.gz`, `rift-linux-aarch64-musl.tar.gz`, `rift-macos-aarch64.tar.gz`, `rift-windows-x86_64.zip` | `rift` (`rift.exe` on Windows), `README.md`, `LICENSE`, `THIRD_PARTY_NOTICES`, `docs/` and `examples/` |
| `rift-linux-x86_64`, `rift-linux-aarch64`, `rift-linux-x86_64-musl`, `rift-linux-aarch64-musl`, `rift-macos-aarch64`, `rift-windows-x86_64.exe` | The same executable on its own, for scripted installs |
| `SHA256SUMS` | SHA-256 checksums of every other asset |

- Every asset has a signed
  [build provenance attestation](#verifying-a-download).
- Keep the archive's files together, so relative links between the guides and
  examples keep working. The raw benchmark samples and the SDK sources stay
  online.
- Rename a standalone executable to `rift` (`rift.exe` on Windows) and,
  outside Windows, run `chmod +x rift`.

## Which Linux build

| Build | Runs on | Use it when |
| --- | --- | --- |
| `rift-linux-x86_64`, `rift-linux-aarch64` | glibc 2.35 or newer: Ubuntu 22.04, Debian 12, Fedora 36, RHEL 10 and later | the host qualifies; this is the faster build |
| `rift-linux-x86_64-musl`, `rift-linux-aarch64-musl` | any distribution; statically linked, no libc needed | Alpine, RHEL 8 and 9, Debian 11, Amazon Linux 2023, `scratch` or distroless images |

- `ldd --version` prints the host's glibc version.
- A glibc build on an older host fails at once with
  `version 'GLIBC_2.3x' not found`. Switch to the musl build for the same CPU.
- The musl build behaves the same, but musl's memory allocator is slower
  under many threads, so prefer the glibc build where it runs.
- Managed servers and plugins are separate processes or sandboxes and work
  with either build.

## Verifying a download

`SHA256SUMS` detects a corrupted or incomplete download:

| OS | Check |
| --- | --- |
| Linux | `sha256sum --check --ignore-missing SHA256SUMS` |
| macOS | `shasum -a 256 --check --ignore-missing SHA256SUMS` |
| Windows | Compare `Get-FileHash .\rift-windows-x86_64.zip -Algorithm SHA256` with the file's line in `SHA256SUMS` |

A provenance attestation also proves that GitHub Actions built the file from
this repository's release workflow. Verify any asset with the
[GitHub CLI](https://cli.github.com/) before you run it:

```sh
gh attestation verify rift-linux-x86_64.tar.gz --repo Jotrorox/rift \
  --signer-workflow Jotrorox/rift/.github/workflows/release.yml
```

The output names the source commit and the workflow run that built the file.
For a nightly build, use
`--signer-workflow Jotrorox/rift/.github/workflows/nightly.yml`.

## First start

1. Verify the archive and extract it into an empty directory:
   - Linux: `tar xzf rift-linux-x86_64.tar.gz`
   - macOS: `tar xzf rift-macos-aarch64.tar.gz`
   - Windows: `Expand-Archive .\rift-windows-x86_64.zip -DestinationPath rift`.
     Work inside the `rift` folder and run `.\rift.exe` where the next steps
     say `./rift`.
2. Run `./rift init`, then edit the listener and backend addresses in the
   `rift.lua` it writes. The generated configuration forwards port 25565 to
   loopback port 25566; [set up that backend](#set-up-a-backend).
3. Run `./rift check`. Correct the field or path an error names and check
   again.
4. Run `./rift`. Look for `rift: listening on ...`, then connect a Minecraft
   client through the public address.

A successful check does not establish that backends are reachable or ready.
With metrics enabled, `curl --fail http://127.0.0.1:9090/metrics` should
succeed.

## Commands

| Command | What it does |
| --- | --- |
| `rift init [path]` | Writes a commented configuration (default `./rift.lua`). Refuses to overwrite an existing file. |
| `rift check [path]` | Evaluates and validates it without opening listeners or contacting backends. `rift --check [path]` is the same. |
| `rift check --strict [path]` | Adds the [strict checks](#strict-checks). |
| `rift` | Loads `rift.lua` from the current working directory, or uses the default addresses `0.0.0.0:25565` and `127.0.0.1:25566` when it is absent. |
| `rift --config path/to/custom.lua` | Starts with another configuration file. |
| `rift 0.0.0.0:25565 127.0.0.1:25566` | Starts with a listener and a backend address, without a configuration file. |
| `rift --version` | Prints the Cargo package version. Nightly builds may share it, so keep the release tag when you report an issue. |
| `rift --license` | Prints the embedded licenses. |

Configuration scripts are trusted local code. With addresses on the command
line, Rift does not load `rift.lua`. Hostname routing works there too:

```sh
./rift 0.0.0.0:25565 \
  --route survival.example.com=127.0.0.1:25566 \
  --route '*.games.example.com=127.0.0.1:25567' \
  --default 127.0.0.1:25566
```

Routes prefer exact names, then the longest wildcard suffix, then the default.

## Set up a backend

The generated configuration expects a Minecraft server on `127.0.0.1:25566`.
Set that server's `server.properties`:

```properties
server-ip=127.0.0.1
server-port=25566
online-mode=false
prevent-proxy-connections=false
```

> [!WARNING]
> The generated starter does not authenticate players until you enable
> [online mode and forwarding](authentication.md). Keep backend ports private.

Two examples go further:

- [`examples/online.lua`](../examples/online.lua): online authentication and
  Velocity modern forwarding for a public Paper network.
- [`examples/network.lua`](../examples/network.lua): hostname routing,
  fallback, admission controls, health probes, status caching and metrics.
  Adapt its hostnames and backend addresses before use.

## Strict checks

`rift check --strict [path]` runs the plain check, then the checks that need
the host. Run it in the same environment as the service, for example with
`sudo -u rift --preserve-env=...` or
`systemd-run --property=EnvironmentFile=...`, since it reads that environment.

| Check | What passes |
| --- | --- |
| Secrets | Every environment variable the configuration names is set and has the length startup requires. Output names the variables, never their values. |
| Instance database | `instance_database` opens read-only, passes SQLite's `quick_check`, has a supported schema, and its registered instances fit the configuration. A missing database is fine when its directory is writable. With [PostgreSQL or MariaDB](database.md), the database connects read-only instead; a database without Rift's tables yet is fine, and one owned by a running Rift counts as running. |
| Extension store | An existing `extensions.storage.path` decodes; a missing one can be created. A store kept in the instance database (`database = true`) is covered by the instance database check. |
| Ports | Every gameplay listener, `admin.listen`, `web.listen`, `status.listen` and `metrics` can be bound over TCP, and `messaging.listen` over UDP. Each socket is released at once. |
| Reloads | Compared with the last deployment in the operator records (`web.records_directory`, or `<config>.operators`), no change needs a restart. The rules are those of a live reload: listener, admin, messaging, node, managed server, instance database, extension enablement/API version/storage/queue and `web.records_directory` changes need one. Modules are read from disk as they are now. |

| Secret variable | Required value |
| --- | --- |
| `forwarding.secret_env` (for `velocity` and `bungeeguard`) | 1–1024 bytes |
| `admin.token_env`, `messaging.principals[].token_env`, `nodes.*.token_env` | 32–1024 bytes |
| `web.token_env`, `web.operators.*.token_env` | 16–1024 bytes |
| `extensions.integrations.*.headers_env` | a valid header value |

The result depends on whether Rift is running:

- **Running** (the instance database is locked): port binds are skipped,
  since Rift holds them, and a reload incompatibility is an error.
- **Stopped:** an incompatible change is only reported, because the next
  start applies everything.

Strict checks create nothing; testing whether a directory is writable creates
a temporary file and removes it at once. The exit status is non-zero when any
strict check fails.

## Next steps

- [Authentication and forwarding](authentication.md): verify accounts.
- [Lua configuration](lua.md): routes, modules, callbacks and plugins.
- [Deploy with systemd or Docker](deployment.md): run Rift as a service.
