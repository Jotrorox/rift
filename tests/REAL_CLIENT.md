# Headless real vanilla clients (`tests/real_client.py`)

The vanilla client decides whether a clientbound stream is valid: it disconnects
or logs an exception when it gets something it cannot handle. `real_client.py`
runs unmodified Mojang clients of many versions headlessly on a developer
machine, makes them join an address and reports whether they joined, stayed
connected and what they logged. It is meant to judge Rift's translating proxy;
it is validated against direct connections to pinned vanilla servers.

```sh
PY=/opt/homebrew/bin/python3          # any Python 3.11+, stdlib only
$PY tests/real_client.py --accept-eula setup 1.12.2 1.16.5 1.20.1 1.21.4 26.3
$PY tests/real_client.py --accept-eula join --version 1.20.1 --address 127.0.0.1:25565 [--stay 45] [--json]
$PY tests/real_client.py --accept-eula selftest [--versions 26.3 1.21.4 ...] [--stay 40] [--negative]
```

```python
import real_client
result = real_client.join("1.20.1", "127.0.0.1", 25577, stay=20)
result.ok, result.reason, result.failures, result.log_dir
```

`join` exits 0 only when the client joined and stayed the whole time without
logging network, packet or registry errors; otherwise it exits 1 with the reason
(server disconnect message, the client's exception with its first frames) and the
path of the kept logs. `--json` prints the `Result` as JSON.

## How it works

* **Files** come from Mojang's launcher metadata (version manifest, version JSON,
  client jar, libraries, asset index) and are SHA-1 verified. They are cached in
  `~/.cache/rift-via/clients/` (shared by all worktrees; set `RIFT_VIA_CACHE` to
  move `~/.cache/rift-via`). Sounds, music, `sounds.json` and non-English
  translations are not downloaded (~370 MB per asset index); the client gets a
  filtered index (`<id>-headless.json`) listing exactly what is present. Native
  libraries are not downloaded at all.
* **Headless**: the pinned HeadlessMC LWJGL agent (`tests/real_client_tools.json`,
  MIT, SHA-256 pinned) rewrites every `org.lwjgl` class so GLFW/SDL, OpenGL,
  OpenAL and STB calls return stub values: no window, no GPU, no input, no audio.
  A small helper agent compiled into the cache at setup (`HELPER_AGENT` in the
  script, built against HeadlessMC's jar with the cached JDK's `javac`) chains
  HeadlessMC's `MacosMenuAgent` (26.x crashes in native menu-bar code otherwise)
  and, for 1.13-1.16, makes the class that reads PNG sizes parse the IHDR chunk
  in Java (the stubbed `stbi_info_from_callbacks` fails, every texture would be
  missing and chunk meshing crashes on null block models). A silent narrator
  shadows `NarratorOSX`/`NarratorMac`: the first-launch accessibility screen
  otherwise speaks through macOS speech synthesis. Nothing touches networking or
  packet handling.
* **Offline account**: made-up name (unique per run), offline UUID, token `0`,
  `--userType legacy` (or `--offlineDeveloperMode` on 26.x, which has no user
  type; without it chat is disabled "by profile" and never logged). All HTTP the
  client does (auth, profile keys, skins, realms, telemetry) goes through a dead
  SOCKS proxy (`--proxyHost 127.0.0.1 --proxyPort 9`), so nothing reaches Mojang.
* **Connecting**: `--quickPlayMultiplayer` (1.20+) or `--server/--port` (older).
  Every run has its own game directory with a pre-written `options.txt`
  (render distance 2, 10 FPS, no sound, no narrator, no onboarding, no
  multiplayer warning, no telemetry, ...).
* **Relay**: the client connects to a loopback relay that forwards bytes
  unchanged except that the handshake's address and port are rewritten to the
  real target (the backend sees what a direct connection sends). It follows
  compression and login state with the per-protocol packet IDs of
  `tests/minecraft.py` and reports the Join Game packet, keep-alives the client
  echoed and disconnect packets (with the decoded reason). For clients that use
  `--server` (1.13-1.19) it holds the connection until the client's first
  resource reload has created its atlases: they otherwise join and mesh chunks
  before models are baked (1.16.5 crashes). Negative tests use it to corrupt one
  play packet. `join(..., relay=False)` connects directly (log-only evidence;
  liveness via `lsof`). Online-mode (encrypted) targets cannot be observed.
* **Outcome**: joined = Join Game packet seen by the relay (or, without relay,
  the client's own join chat line / "Loaded N advancements"). Failed when the
  client exits, the connection closes (by the server: its disconnect reason; by
  the client: usually after a decoder error), a disconnect packet arrives, the
  client logs a failure, an expected chat line is missing, or the client stopped
  echoing keep-alives.
* **Log classification** (`classify()`): log4j output is captured with a pattern
  including thread and logger. INFO is ignored except chat/disconnect lines.
  WARN/ERROR/FATAL entries are matched against `NOISE` (allowlist of headless
  noise: offline account / blocked Mojang services, stubbed audio and narrator,
  stubbed rendering/window/input, assets not downloaded, OS/JVM probing) first,
  unless their stack trace goes through the network stack; then against
  `FAILURE` (decoder/encoder exceptions, "Internal Exception", packet handling,
  registries, tags, codecs, chunk/entity data, crash reports, any warning on a
  Netty thread, any FATAL). The rest is "unknown": printed for triage, fatal
  only with `--strict`.
* **Safety**: at most 2 clients machine-wide (`flock` on
  `~/.cache/rift-via/locks/client-*`, `RIFT_REAL_CLIENT_SLOTS` to change),
  file locks around downloads and helper builds, atomic downloads, `-Xmx1G`,
  each client in its own process group, killed on exit, Ctrl-C, SIGTERM or
  timeout, plus a watchdog process that kills the group if the harness dies.
  On macOS `caffeinate -i -w <pid>` keeps idle sleep from freezing runs, and
  every run checks with `lsappinfo` that the client never registered as a GUI
  app.

## Self-test

`selftest` starts the matching pinned vanilla server from `tests/servers.json`
(offline mode, jar cached in `~/.cache/rift-via/servers/`), joins it directly,
then via the console runs `/say <nonce>`, `/give`, `/tp`, `/time`, `/weather`,
`/summon`, `/gamemode`, `/title`, `/xp`, `/effect` and a second `/say`; it
requires both chat lines in the client log, the server's "joined the game" and
no "lost connection"/"left the game" before the harness stopped the client.
`--negative` adds a version-mismatch run (must fail with the server's outdated
message) and, per version, a run where the relay replaces one clientbound play
packet's ID with 0x3FFF (must fail with the client's decoder error).
