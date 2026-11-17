# Templates and server software

A template names the files Rift copies into every new instance of a
[service group](service-groups.md): a server jar, plugins, config folders and a
map. A template can also name server software for Rift to download, and take
its folders from a git repository.

```lua
local config = require("rift.config")
config.templates.arena = {
    server_jar = "assets/paper.jar",
    plugins = { "assets/plugins/game.jar" },
    configs = "assets/configs",
    map = "assets/maps/arena",
}
config.service_groups.games = {
    template = "arena",
    storage = "disposable",
    directory = "servers/{name}",
    command = { "java", "-Xmx1G", "-jar", "server.jar", "nogui" },
    port_range = { 25600, 25649 },
    start_on_connect = true,
}
```

Define templates under `rift.config.templates` and name one from one or more
groups. [examples/templates.lua](../examples/templates.lua) uses one template
for a persistent survival group and a disposable game group;
[examples/cloud.lua](../examples/cloud.lua) is a complete setup with
downloaded software.

## Local asset templates

| Template field | Required | Instance destination |
| --- | --- | --- |
| `server_jar` | One of `server_jar` or `software` | `server.jar` in the working root |
| `software` | One of `server_jar` or `software` | A downloaded jar, copied to `server.jar`; see [server software](#download-server-software-automatically) |
| `plugins` | No | Each jar's filename under `plugins/` |
| `configs` | No | A directory, or a list of up to 16 directories, overlaid onto the working root in order |
| `map` | No | Contents of this directory under `world/` |
| `accept_eula` | No (`false`) | Writes `eula=true` to `eula.txt` |

Config layers are copied first, in order, so later folders override files from
earlier ones (like CloudNet template priorities). The jar, plugins and map
follow and replace matching files from the config layers.

Rules for sources:

- Paths are absolute, or relative to the configuration file's directory. They
  do not expand `{name}`, `{group}` or `{port}`.
- Assets must exist on the host when an instance is created. Rift reads them
  without modifying them. `rift check` checks templates structurally only.
- Template names contain ASCII letters, digits, underscores or hyphens, up to
  128 bytes.
- `plugins` is a dense array of at most 128 paths with distinct filenames.
  These Minecraft server plugins are separate from Rift's Lua folder plugins.
- Source trees contain ordinary files and directories. Provisioning rejects
  symlinks (including path ancestors), special files and ownership markers.
- Sources and instance directories must not overlap. Keep source assets
  separate from the generated `servers/` directories.

### What provisioning writes

In the instance's `server.properties`, provisioning sets `server-ip` to the
group's loopback address, `server-port` to the allocated port and
`level-name=world`. It preserves the other settings.

For a local `server_jar`, supply `online-mode=false`, the forwarding
configuration and any other settings your network requires in `configs`.
Templates with `software` set the
[proxy-related settings](#settings-rift-writes-for-downloaded-software)
themselves.

Rift writes `eula=true` only for templates with `accept_eula = true`; setting
it records that you accept the
[Minecraft EULA](https://aka.ms/MinecraftEULA). Otherwise provide your own
`eula.txt` in a config layer or in each instance directory before the start.

> [!NOTE]
> `autostart=false` avoids an immediate start at creation, but
> `start_on_connect=true` can still start the process when an eligible player
> arrives. Complete the setup before you route players to the group.

### Instances and their template

- A failed copy leaves no registered instance and no partially provisioned
  directory.
- Provisioning does not adopt an arbitrary existing server directory; it
  recognizes its own by their ownership metadata. For a world you prepared by
  hand, use a [named managed server](managed-servers.md) or a persistent group
  without a template.
- Editing template assets does not update existing instances. Persistent reuse
  keeps server files and world changes instead of copying the seed map again;
  disposable recreation copies the source assets again (see
  [storage policies](service-groups.md#choose-a-storage-policy)).

Templates can change live through Lua or the dashboard's
[structured editor](http.md); see
[change a group](service-groups.md#change-a-group) for what existing instances
must keep.

## Template folders from git

Template folders can live in a git repository. Name the repository under
`template_sources`; Rift checks it out into `template_storage/<name>`
(`local/templates/<name>` by default), and templates use the folders in it
like any others:

```lua
local config = require("rift.config")
config.template_sources.network = {
    git = "https://github.com/example/network-templates.git",
    ref = "main", -- a branch or tag; or rev = "<full commit id>" to pin
}
config.templates.lobby = {
    software = { type = "paper", version = "1.21.8" },
    configs = { "local/templates/network/Global/server", "local/templates/network/Lobby/default" },
    plugins = { "local/templates/network/plugins/hub.jar" },
    accept_eula = true,
}
```

| Field | Required | Meaning |
| --- | --- | --- |
| `git` | Yes | `https://…`, `ssh://…`, `user@host:path` or `file://…`. Plain `http://` and `git://` are refused, as is a password or token in the URL |
| `ref` | No | A branch or tag. Without `ref` and `rev`, the repository's default branch |
| `rev` | No | A full commit id (40 or 64 lowercase hex digits). The checkout must be exactly this commit; not combined with `ref` |

Rift fetches a source:

- the first time an instance needs one of its folders, before it reserves a
  port or name;
- again after you change the source's definition, for example to a new `rev`.

It does **not** follow a branch on its own, so instances never change because
someone pushed. `rift check` and configuration loading never fetch. Pin `rev`
where the contents must not change without a configuration change.

### Sync new commits

Pull new commits with `rift admin sync network` or
`POST /api/template-sources/network/sync`. The next instances use them;
existing instances keep their files. With [nodes](nodes.md), follow up with
`rift admin warm` so the nodes fetch the new files before the next
instance needs them. `rift admin templates` and
`GET /api/templates` list each source with its commit and whether it is
synced.

A sync fetches only the wanted commit and swaps the folder at the end, so a
failed fetch leaves the previous checkout in place.

- The folder holds the repository's files only. Git's own data is kept in
  `software_cache` as `git-<name>` and never reaches an instance or a node.
- Symbolic links in the repository arrive as plain files (provisioning rejects
  links). Submodules are not fetched.

> [!WARNING]
> A sync replaces the whole folder, including edits made there through the
> dashboard's Files page. Change the repository instead.

### Git and credentials

Rift runs the host's `git` as Rift's user, with that user's git configuration
but without Rift's secrets in its environment. Install `git` on the
controller; [nodes](nodes.md#templates-and-software) fetch template folders
from the controller and need none.

For a private repository, give that user an SSH deploy key or a
[credential helper](https://git-scm.com/docs/gitcredentials). A missing
credential fails the sync instead of prompting. Under the supplied systemd
unit the service account has no home directory, so point
`Environment=GIT_SSH_COMMAND=ssh -i /etc/rift/deploy_key -o UserKnownHostsFile=/etc/rift/known_hosts`
at files it can read.

## Download server software automatically

Instead of a local `server_jar`, a template can name server software. Rift
downloads it on first use, verifies it and caches it, similar to CloudNet's
version installer:

```lua
local config = require("rift.config")
config.templates.lobby = {
    software = { type = "paper", version = "1.21.8" }, -- or version = "latest"
    configs = { "local/templates/Global/server", "local/templates/Lobby/default" },
    accept_eula = true,
}
config.service_groups.lobby = {
    template = "lobby",
    storage = "disposable", -- Instances live in temp/services/{name}.
    port_range = { 25600, 25649 },
    memory_mb = 2048,
}
```

| `type` | Source | `version` | Verification |
| --- | --- | --- | --- |
| `paper`, `folia` | PaperMC Fill API, latest build of the version | Release such as `1.21.8`, or `latest` (default) | SHA-256 published by PaperMC |
| `purpur` | Purpur API, latest build of the version | Release or `latest` | HTTPS only, unless pinned with `sha256` |
| `vanilla` | Mojang version manifest | Release or `latest` | SHA-1 published by Mojang |
| `fabric` | Fabric meta server launcher with the latest stable loader and installer | Minecraft release or `latest` | HTTPS only, unless pinned with `sha256` |
| `url` | `url = "https://…"` with the required `sha256 = "…"` | Not used | The pinned SHA-256 |

Install the Java version the server needs on the Rift host, for example Java
21 or newer for current Paper releases.

Pin any exact version with `sha256`, for example
`{ type = "purpur", version = "1.21.8", sha256 = "…" }`:

- A download whose digest differs fails and leaves no jar behind.
- Purpur and Fabric publish no SHA-256 for their artifacts, so pin them where
  integrity matters.
- A pin names a single build, so a refresh fails once upstream ships a newer
  one, until you update the pin.
- `latest` cannot be pinned. It resolves to the newest release when the jar is
  first downloaded.

### The software cache

Downloads are cached under `software_cache` (default `local/versioncache`
beside the configuration) and reused for every later instance.

- An entry is `{type}-{version}/server.jar` with a `rift-software.json` record
  of the resolved version, build and digest.
- A pinned entry's name also carries the start of its digest
  (`purpur-1.21.8-0123456789ab`, or a hash of URL and digest for `url`), so
  changing a pin never reuses a jar checked against another digest.
- Every reuse first checks the jar against its recorded SHA-256 (and the pin).
  A missing record or an altered jar is downloaded again.
- Instance creation downloads a missing jar before it reserves a port or name.
- `rift check` and configuration loading never download anything.
- A download is limited to 512 MiB and must use HTTPS; plain HTTP is accepted
  only for loopback mirrors. Rift stops following redirects after five hops.
- Downloads are cancelled when Rift shuts down. A failed or interrupted
  download leaves no jar behind.

### Download ahead of time

Fetch or update a jar before an instance needs it with any of:

- the dashboard's **Download**/**Update** button on the Service groups page;
- `rift admin download lobby [refresh]`;
- `POST /api/templates/lobby/download`;
- `rift.control.download` on the message bus;
- `rift.servers.download("lobby")` from Lua.

Refreshing replaces the cached jar for new instances; existing instances keep
the jar they were created with. `rift admin templates` and
`GET /api/templates` report the cache state.

### Settings Rift writes for downloaded software

For downloaded software Rift also prepares the server to run behind the proxy.
A config layer that sets the property or supplies the file wins.

| Condition | Rift writes |
| --- | --- |
| Always | `online-mode=false` in `server.properties` |
| No layer sets `white-list` | `white-list=false` (new 26.x servers otherwise turn their whitelist on; use `network.access` to restrict players) |
| Offline network without `forwarding` | `enforce-secure-profile=false` |
| `forwarding = { mode = "velocity", ... }` | `config/paper-global.yml` with Velocity forwarding enabled and the secret from `secret_env`, created with mode `0600` |
| `mode = "bungeecord"` or `"bungeeguard"` | `spigot.yml` with `settings.bungeecord: true` |
| `mode = "bungeeguard"` | Also the token in `plugins/BungeeGuard/config.yml` (mode `0600`) |

The forwarding rows apply to Paper, Folia and Purpur instances. Templates with
a local `server_jar` receive only the
[endpoint settings](#what-provisioning-writes).

> [!WARNING]
> With `bungeeguard`, add the BungeeGuard plugin jar to the template's
> `plugins`. Without it the backend accepts the forwarded data unchecked.

## CloudNet-style folders

With the default paths, the layout matches CloudNet v4:

| Folder | Contents |
| --- | --- |
| `local/templates/` | Template folders used as `configs` layers (`template_storage`) |
| `local/versioncache/` | Downloaded server software (`software_cache`) |
| `local/services/{name}` | Persistent instances (default group `directory`) |
| `temp/services/{name}` | Disposable and ephemeral instances (default group `directory`) |

Both `software_cache` and `template_storage` accept a different path relative
to the configuration. The dashboard's **Files** page (and `/api/files`)
browses and edits `template_storage` and every managed server's directory; see
[template and server files](http.md#template-and-server-files).
