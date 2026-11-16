# Running plugins

For operators of persistent [plugins](plugins.md): how to see what is loaded,
what Rift does when a plugin fails or the configuration reloads, and where to
find state, logs and metrics.

## Check what will run

`rift check` lists every plugin with its mode and grants, before anything
starts:

```text
plugin (configuration script): mode script, sandboxed; hooks login, join
plugin hello-lua 0.1.0: mode persistent, engine lua, trusted; restart on-failure; capabilities filesystem=data(advisory) network=no(rift.http only) env=all(os.getenv only) ffi=false native_modules=false memory_limit_mb=256; Lua io and os are not sandboxed
plugin (bus responders): mode remote; hooks login
```

## Admin commands

`rift admin plugins` uses the
[local administration](admin.md)
service.

| Command | Needs | What it does |
| --- | --- | --- |
| `rift admin plugins list` | `status` | Shows name, mode, engine, status, restarts, calls and last error of every plugin |
| `rift admin plugins status hello-lua` | `status` | Shows one plugin |
| `rift admin plugins logs hello-lua` | `logs` | Prints the last 200 [log lines](#logs) |
| `rift admin plugins restart hello-lua` | `plugins` | [Restarts](#stop-and-restart-a-plugin) the plugin |
| `rift admin plugins run ban Mallory` | `plugins` | Runs a plugin [command](plugin-events.md#commands) registered with `console` |

Each plugin entry, here and in the HTTP API, has these fields:

| Field | Content |
| --- | --- |
| `name`, `engine`, `version` | The script and the bus responders are listed as `(script)` and `(remote)` |
| `mode` | `script`, `persistent` or `remote` |
| `status` | `starting`, `running`, `failed` or `stopped`; `active` for the script and the bus responders |
| `restarts` | Automatic and operator restarts since Rift started |
| `calls` | Awaited hooks plus queued observer events, timer ticks and bus messages |
| `last_error`, `hooks`, `commands`, `capabilities` | The last error, and what the plugin registered and was granted |
| `restartable` | `true` for persistent plugins |

## Startup

Each persistent plugin runs on its own OS thread, one call at a time: engines
need no locking, and the guest is never called concurrently. Its
[background workers](plugin-events.md#background-work) are further instances
on threads of their own.

Rift starts plugins in order once the control service answers and before any
listener accepts players, and waits for every `init`. **A plugin that fails to
start fails startup.**

## Failures and restarts

A handler error (a Lua error, a returned error) fails that call; the plugin
keeps running.

A trap, a panic, running out of memory, an error outside a handler, or (for
WebAssembly) guest code interrupted at its deadline marks the instance
**failed**. `failure.restart` in the [manifest](plugins.md#plugintoml) then
decides:

| `restart` | After a runtime failure | After `init` fails on a reload |
| --- | --- | --- |
| `never` | stays failed | stays failed |
| `on-failure` | restarts | stays failed until the next configuration change |
| `always` | restarts | keeps retrying |

- Restarts back off from 1 s, doubling to 60 s. An instance that ran for 60 s
  before failing starts over at 1 s.
- An invalid registration or a command that collides with another owner is
  never retried.
- While a plugin is failed or restarting, its awaited calls apply `on_failure`,
  but only for hooks its last registration listed: a failed plugin that
  registered `http` and `timer` does not deny logins.

## Calls while a plugin starts

A plugin's registration is its claim on hooks and commands.

- **A new plugin**, at startup or added by a reload, has claimed nothing while
  it is in `init`, so Rift does not ask it yet. Startup waits for every `init`
  anyway.
- **A restarted or replaced plugin** keeps its predecessor's registration until
  its own `init` returns. Calls to the hooks it claimed queue behind that
  `init` and apply `on_timeout` if it is slow. Every other hook is unaffected.
- Plugins initialize concurrently but claim commands in `rift.plugin()` order.
  A plugin waits for the one before it to claim, at most until that one's
  predecessor wait and `init` deadline (plus 30 s for creating its engine) have
  passed.

## Reloads

A configuration reload compares the plugin set by name:

| Change | What happens |
| --- | --- |
| Nothing (same files, manifest and configuration table) | Untouched; state and commands survive. |
| Only the configuration table, `hot_reload = true`, running | `reload(config)` is called within `deadlines.reload`; on error the plugin keeps its previous configuration. |
| Only the configuration table, otherwise | The instance is replaced. |
| The manifest or any file in the folder except `data/` and hidden (dot) files | The instance is replaced. The folder may not contain symbolic links, hidden ones included. |
| Removed from the configuration | Stopped. |
| New | Started. |

- **One generation at a time.** A replacement starts only after its
  predecessor's thread exited, also when a plugin is added again while its
  removed generation is still stopping. A predecessor that is still running
  1 s after its shutdown grace period ended (stuck in code its engine cannot
  interrupt) is abandoned with a log line, and the new instance starts anyway.
- **Command conflicts.** A reload that gives the configuration script a command
  registered by a plugin that stays configured (unchanged, hot-reloaded or
  replaced) is refused and leaves the running set as it is.
- **Existing sessions** see the current plugin set. A command is looked up when
  the player sends it, so commands a plugin registers after the session began
  (added by a reload, or still starting when the player joined) reach the
  plugin instead of the backend. The player's command tree shows them with the
  backend's next command list.

## Stop and restart a plugin

**Stopping.** `shutdown(reason)` runs within `deadlines.shutdown`. A plugin
that does not return in time is told to stop (engines interrupt it where they
can) and its thread is abandoned. It receives no further events either way.

**Operator restarts.** `rift admin plugins restart <name>`, the dashboard's
Restart button, `POST /api/plugins/<name>/restart` and `rift.control.plugins`
stop a plugin and start a fresh instance in the same position, with the same
files and its configuration table as currently configured (also while a hot
reload to it is still running). Calls in the meantime to hooks the plugin
registered queue behind the new `init` or time out under the manifest; other
hooks are unaffected.

## Dashboard and HTTP API

The [dashboard](http.md)'s Plugins page shows every plugin's state, restarts,
calls and last error, a live log tail, and a Restart button.

| Method and path | Web permission |
| --- | --- |
| `GET /api/plugins` | `read` |
| `GET /api/plugins/{name}` | `read` |
| `GET /api/plugins/{name}/logs` | `logs` |
| `POST /api/plugins/{name}/restart` with `{}` (returns `202`) | `plugins` |

All four require unrestricted operators: plugins act on the whole proxy, so
group-scoped operators get `403`, and `plugins` cannot be granted to a scoped
operator.

## Logs

Rift keeps the last 200 lines per persistent plugin in memory, the **log
tail**: its `log` calls (and `print` in Lua), captured standard output and
error of WebAssembly guests, lifecycle changes, and calls that timed out or
failed with the policy that applied.

The same lines go to Rift's standard error. The tail keeps every line; Rift's
log shows `debug` and `trace` lines only with
[`RIFT_LOG_LEVEL=debug`](monitoring.md#log-levels), which `rift dev` sets.

## Bus subjects

**`rift.control.plugins`** takes `{"action":"list"}`,
`{"action":"status","name":"…"}`, `{"action":"logs","name":"…"}` or
`{"action":"restart","name":"…"}` and answers like the other
[control subjects](messaging-subjects.md#control-subjects). It is authorized like
them: grant `publish` on `rift.control.plugins` only to principals with
`control = true` that may restart plugins.

**`rift.events.plugins`** reports lifecycle changes as
`{"event":"started"|"failed"|"restarting"|"stopped","plugin":"…","version":"…","restarts":n,"error":"…"}`
(`error` only for `failed`), and
`{"event":"restart_requested","plugin":"…","version":"…","actor":"…"}`
for an operator restart. Plugins receive these through their `event` hook too.

## Audit log

The [operator audit log](http.md#authentication-and-access) records plugin
changes with the plugin name, never error text. The HTTP and admin requests
themselves are audited as usual.

| Record | Actor |
| --- | --- |
| `plugin started`, `plugin stopped`, `plugin restarting`, `plugin failed`, `plugin reloaded`, `plugin reload_failed` | `system` |
| `plugin restart_requested` | The operator: `local-admin` for the admin socket; on the bus `bus:<login>`, `plugin:<name>` for another plugin and `rift.control` for the configuration script (names no web operator can have) |

## Metrics

On the status service's `/metrics`:

| Metric | Labels | Meaning |
| --- | --- | --- |
| `rift_plugin_calls_total` | `plugin`, `hook`, `outcome` | Calls by outcome: `ok`, `error` and `timeout` for awaited hooks; `queued` and `dropped` for observers, timer ticks and bus messages |
| `rift_plugin_call_seconds` | `plugin`, `hook` | Histogram of awaited hook latency as the caller saw it, including timeouts |
| `rift_plugin_restarts_total` | `plugin` | Automatic and operator restarts |
| `rift_plugin_up` | `plugin` | 1 while the plugin is running |

Labels are bounded: `plugin` takes only configured names (series of a removed
plugin disappear at the next reload), and `hook` and `outcome` are fixed sets.
