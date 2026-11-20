# Plugin events and commands

Every event of the [plugin contract](plugins.md), what a plugin may answer, and
the rules around answers: deadlines, failures, dispatch order and commands.
This page is written for persistent plugins. `rift.lua` receives the same
events; its callback syntax and limits are in
[Hooks in `rift.lua`](extensions.md).

## All events

A plugin answers an **awaited** event within a deadline. An **observer** event
is queued for the plugin and has no answer.

| Event | When it fires | Answer | Default deadline | Default `on_timeout` |
| --- | --- | --- | --- | --- |
| `route` | A connection was accepted, before any bytes are read | keep the policy, pick a backend or group, or reject | 50 ms | deny (close the connection) |
| `login` | After Mojang authentication, before any backend DNS lookup or connection | continue or deny | 1000 ms | deny |
| `initial_server` | After an allowed login | continue, deny or a server | 1000 ms | deny |
| `before_transfer` | Before each eligible target connection attempt | continue or deny | 300 ms | deny |
| `command` | A player runs a command the plugin owns | message, server, queue, leave-queue or continue | 1000 ms | deny (the command reports an error) |
| `suggest` | A player completes an argument of such a command | completions | 50 ms | continue (no completions) |
| `status` | A server-list ping, before any backend contact | fields to replace in that one ping | 50 ms | continue |
| `http` | An authenticated request to `/ext` or `/ext/*` on the web server | a response, or none | 5000 ms | continue |
| `chat` | A chat message or backend command ([below](#chat)); only plugins that list it | continue or deny | 50 ms | deny (the message is dropped) |
| `kick` | A backend kicked the player in play ([fallback on kick](operations.md#run-a-maintenance-window)); only plugins that list it | continue, deny (disconnect with this text) or a server to move the player to | 300 ms | deny (`network.on_kick` applies) |
| `join` | Once, after the first usable Join Game reaches the client | none | observer | – |
| `after_transfer` | An admitted transfer attempt completed or failed | none | observer | – |
| `disconnect` | Session teardown, including an authenticated login that never joined | none | observer | – |
| `message` | A message on a subscription the plugin made | none | observer | – |
| `timer` | A tick of a timer the plugin started | none | observer | – |
| `event` | A `rift.events.*` publication | none | observer | – |
| `dialog` | A click on a plugin action of a [dialog](dialogs.md) the plugin showed | none | observer | – |
| `menu` | A click, anvil text or the end of an [inventory menu](menus.md) the plugin showed ([fields](menus.md#events)) | none | observer | – |
| `job_done` | A [background job](#background-work) the plugin started finished | none | observer | – |
| `init`, `reload`, `shutdown` | [Lifecycle](plugin-operations.md) | registration / ok / none | 30 s / 5 s / 10 s | init fails the plugin; reload keeps the old configuration |
| `work_init`, `work` | Background workers only | ok / the job's result | 30 s / 30 s (`deadlines.work`) | the job fails; `job_done` reports why |

Observers are interrupted after 60 s. A manifest's
[`[deadlines]`](plugins.md#plugintoml) changes these defaults. Callbacks in
`rift.lua` have their own fixed [limits](extensions.md#limits-and-state). The
WIT writes event names with hyphens (`initial-server`); manifests, Lua and the
bus use underscores.

## Choose the events a plugin receives

Every event is an export in the contract with a default in every SDK. A plugin
lists the events it wants in the registration that `init` returns. Rift skips
the rest, so unused events cost nothing.

- **An empty `registration.hooks` list delivers every event except `chat` and
  `kick`**, which a plugin always lists itself.
- `dialog`, `menu` and `job_done` need no entry: they go to the plugin that
  showed the dialog or menu or started the job.
- The Lua engine computes the list from `rift.on`, `rift.command`, timers and
  subscriptions.

## Deadlines and failures

When a plugin misses its deadline, `on_timeout` applies. When it fails (a
handler error, a trap, an invalid answer, or the plugin is failed or
restarting), `on_failure` applies.

- `deny` fails the event the way an error in `rift.lua` would: the login or
  transfer is refused, a route closes the connection, a command reports an
  error.
- `continue` skips this plugin and asks the next one.

Rift never treats an error as permission to proceed unless a manifest says so.

Each plugin has a queue of 1024 undelivered calls. When it is full, awaited
calls count as timeouts at once and observer events are dropped: a slow plugin
stalls only itself.

## Player events need online mode

`login`, `initial_server`, `before_transfer`, `command`, `suggest`, `join`,
`after_transfer`, `disconnect`, `chat` and `kick` carry a verified identity, so
they need `authentication.online_mode`. Without it:

- Persistent plugins still run `route`, `status`, `http`, timers, bus messages
  and events.
- A plugin whose registration lists a player event, registers a command or has
  an empty hook list fails to start and is not retried, so a ban list cannot
  silently allow everyone.
- `rift check` and `rift check --strict` warn when persistent plugins are
  configured without it.

Turning `online_mode` on or off restarts every persistent plugin.

## Text

Wherever a plugin gives Rift text for a player (a deny reason, a command's
message, a suggestion tooltip, a title, the server-list MOTD, version and
sample lines), the contract has one type, `text`:

- `plain(string)`: 1–1024 bytes, shown exactly as written.
- `json(string)`: a JSON text component of at most 32 KiB, for colors, hover
  text and click events, the `mini`, `legacy` and `message`
  [authoring objects](text.md) included.

A `message` object, such as `{"message": "invited", "args": {"from": "Steve"}}`,
sends one of the plugin's own messages (a key without `:`) or any catalog's
by qualified key. Rift renders it at the call: a decision or tooltip in the
language of the player the event is about, a player action in the target's,
a broadcast in each recipient's, and a `status` answer in the default
([whose language](text.md#whose-language)).

Lua writes a string or a component table ([rich text](players-api.md#rich-text)).
Clients see the status version and sample lines as `§` codes.

Rift validates answers exactly like Lua results and bus replies: text within
those limits, servers must exist, and an answer an event does not accept is a
failure of the plugin. One invalid entry fails the whole answer.

## Who calls `http`

Every web credential with the `extensions` permission reaches `/ext`, an
operator holding little else as much as the full token. Rift does not ask for
more; the handler decides.

The request's context (`context-json` in WebAssembly) is the status object the
configuration script's [`on_http`](http-lua.md)
receives, including its `operator` member: `{name, permissions, groups}` as
`GET /api/access` reports the caller. `permissions` is `null` (absent in Lua)
for full access, and `groups` is `null` when the caller is not limited to
service groups. Check `operator` before a handler changes anything.

## Chat

The `chat` event sees what a player types into the chat box before the backend
does: chat messages, and the commands that would reach the backend. It is the
event for mutes and chat filters.

- `continue`: the backend receives the packet exactly as the client sent it, in
  its place before everything the client sent afterwards.
- `deny` with a [text](#text): Rift drops the packet and shows the player the
  text as a system message.
- Any other answer is a failure of the plugin.

Commands that Rift or a plugin owns (`/server`, `/hub`, `/perms`, registered
commands) never come to the event.

| Mode | Registering | Input |
| --- | --- | --- |
| Configuration script | `chat = function(ctx) ... end` in `rift.config.plugins` ([callbacks](extensions.md#callbacks-and-decisions)) | `ctx.kind` (`"message"` or `"command"`), `ctx.text`, and for a command `ctx.command` and `ctx.args` |
| Persistent Lua | `rift.on("chat", fn)` | the same `ctx` |
| WebAssembly | listing `chat` in `registration.hooks` | `chat-attempt`: `ctx`, `kind`, `text` |

`text` is the message, or the command line without its leading slash.
`ctx.command` is the command's root name and `ctx.args` the rest, as for
`command`.

- **Asking for chat is explicit.** Rift asks every plugin that handles chat
  about every message of every player, so an empty hook list does not include
  it.
- **Order.** The configuration script, then persistent plugins in
  `rift.plugin()` order. The first deny wins, and there is no bus responder.
- **Deadline.** 50 ms (`deadlines.chat`, 1–10 000 ms). With `deny`, the default
  policy, a missed deadline or a failure drops the message and the player sees
  the `chat.failed` [message](text.md).
- While a server switch is under way, the old backend's chat is dropped without
  asking the event, as its commands are.

### Denying costs the player chat signing

A backend that verifies a player's chat rejects everything after one withheld
message. So the player whose message is denied gives up chat signing, in one of
two ways ([unsigned chat](authentication.md#signed-chat)):

- **With signed chat**, the default, players sign until the event first denies
  a signed message or command of theirs. Rift then logs that player in to
  their server again without their signing material: they see the world load
  once, as for a server switch, and their chat is unsigned (shown to others as
  not secure, and not reportable) until they reconnect to the proxy.
  Everything the event lets through stays signed, and nobody else is affected.
- **With `authentication.signed_chat = false`**, no backend ever learns
  anybody's signing material, so nobody sees a reload. Choose it when the
  event denies often.

Packets without a signature (commands without signed arguments, and all chat
of clients before 1.19) always come to the event. Packets the backend verifies
are forwarded without asking while a server switch is under way, and on a
client protocol without server switching.

> [!WARNING]
> Paper backends need `enforce-secure-profile=false` in `server.properties`
> either way. Otherwise they refuse every chat message of a player without a
> signing key, and a muted player cannot chat after the mute ends.

## Background work

A plugin runs one call at a time. A lookup that takes 200 ms inside `login`
keeps every other event of that plugin waiting, and `chat` has 50 ms.
Stretching the deadlines does not help; moving the slow part off the plugin's
thread does:

1. The hook calls `background.defer()`, which returns a ticket. Rift now
   ignores what the hook returns and waits for
   `background.resolve(ticket, decision)` instead.
2. The hook hands the slow part to a worker with `background.run(payload)`,
   which returns a handle, and returns. The plugin is free for the next call.
3. A **background worker** runs `work(payload)`: the database query, the HTTP
   request. It may block.
4. The plugin receives `job_done(handle, outcome)` with the worker's result or
   its error, and calls `resolve` with the decision.

```toml
[background]
workers = 2
```

Each language has a shorter way to write this: see
[Lua](plugins-lua.md#background-work), [Rust](plugins-rust.md#background-work),
[C](plugins-c.md#background-work) and [Go](plugins-go.md#background-work).

- **Workers are separate instances.** Each of the `workers` (1–16) is another
  instance of the plugin on its own thread, with its own memory: a second
  instance of the component, or for Lua a state running `background.entry`
  (default `worker.lua`). A worker never gets `init` or any event. It gets
  `work_init(config)` once before its first job, with the plugin's
  configuration, and then jobs.
- **Payloads and results** are bytes, at most 4 MiB.
- **A worker may call the host API** (`store`, `permissions`, `bus.request`,
  `log` and so on), but receives no timer ticks or bus messages and cannot
  `defer`, `resolve` or `run`.
- **`defer` works in the decision events** `login`, `initial_server`,
  `before_transfer`, `command`, `chat` and `kick`, once per call. The deferred
  answer accepts what the event accepts.
- **The deadline keeps running.** An answer that arrives after the event's
  deadline is a missed deadline: `on_timeout` applies, and `resolve` returns
  false. So a deferred `login` still needs a deadline longer than its lookup,
  but that deadline no longer delays `chat`.
- **Failures.** A handler error in `work`, a job over `deadlines.work` (default
  30 s) or a worker that traps reports an error through `job_done`. A trapped
  worker starts over at its next job. At most 1024 jobs wait at a time; `run`
  fails beyond that, and without a `[background]` table.
- **Workers stop with the plugin.** A restart or reload drops waiting jobs, and
  whoever waits for a deferred answer gets `on_failure`.

## Test player hooks without Mojang

Player events need `online_mode`, which asks Mojang's session server about
every login, so an automated test has no account to log in with. For tests
only, build Rift with the Cargo feature `test-session-server`:

```sh
cargo build --locked --features test-session-server
RIFT_TEST_SESSION_URL=http://127.0.0.1:8080/session/minecraft/hasJoined target/debug/rift
```

Such a build asks the server in `RIFT_TEST_SESSION_URL` instead of Mojang. The
test answers `GET <url>?username=<name>&serverId=<hash>` with status 200 and
the profile to log in
(`{"id": "<32 hex digits>", "name": "<name>", "properties": []}`), or with 204
to refuse the login. [`tests/session_server.rs`](../tests/session_server.rs)
has a login bot and such a server.

- The feature is off by default. Release and nightly binaries are built
  without it and never read the variable; no configuration option exists.
- The URL must be `http://` or `https://` on a loopback IP address
  (`127.0.0.1`, `[::1]`; not `localhost`). Anything else stops Rift at startup.
- Rift logs a warning at startup whenever the variable is in effect. Without
  the variable, a build with the feature asks Mojang as usual.
- The bot must still perform the encryption handshake, as a premium client
  does. A real Minecraft client with an offline account (the headless harness
  in [`tests/real_client.py`](../tests/real_client.py)) still cannot join: it
  fails its own request to Mojang before Rift asks anyone.

## Dispatch order

Rift asks the script first (its callback, then its `rift.on` handlers), then
persistent plugins in `rift.plugin()` order, then the bus responder in
`rift.config.plugins.decisions`.

| Events | Who is asked | The chain stops at |
| --- | --- | --- |
| `route`, `status`, `http` | Script, then persistent plugins | The first answer other than `continue` (or the default) |
| `login`, `initial_server`, `before_transfer`, `kick` | Script, persistent plugins, then the bus responder | The first answer other than `continue`, so a deny anywhere is final |
| [`chat`](#chat) | Script, then every plugin that handles it | The first deny |
| `command`, `suggest` | The command's one [owner](#commands) | – |
| `join`, `after_transfer`, `disconnect` | Every plugin that wants them; the script's observers run separately | – |
| `event` | Every plugin that registered it | – |
| `message`, `timer` | Only the plugin that made the subscription or timer | – |
| `dialog` | Whoever showed the dialog: the script or one plugin | – |
| `menu` | Whoever showed the menu: the script or one plugin | – |
| `job_done` | The plugin that started the job | – |

## Commands

Each command has exactly one owner: the configuration script or one persistent
plugin. A name registered twice, by two plugins or by a plugin and the script,
is a load error.

A command's `permission` is the node a player needs to see and run it. An empty
permission opens the command to every player, in every engine and in the
configuration script. Naming rules, the command tree and tab completion are in
[permissions and commands](extensions.md#permissions-and-commands).

**Aliases.** A persistent plugin's command may list up to 16 `aliases`, each a
name like the command's own (1–64 bytes of `a-z0-9_-`; `server` and `hub` are
reserved). An alias is claimed like a name, appears in the player's command
tree and completes the same way. The hooks always see the command's own name in
`ctx.command`, whichever name the player typed.

**The console.** A command registered with `console` also runs for operators:

```sh
rift admin plugins run ban Mallory griefing
```

The `command` event then sees `ctx.console` set, `ctx.name` `CONSOLE`, an empty
`ctx.uuid` and connection id 0. No permission is checked: the caller needed the
`plugins` admin permission. The command's `message` answer is printed
(`continue` prints nothing); any other answer needs a player and is an error.
Commands without `console` are refused, so a command that assumes a player
never runs without one.

## The player directory and languages

Rift remembers every player who joined, online or not, in `rift.sqlite3` beside
the permission store.

| Function | Result |
| --- | --- |
| `players.lookup(target)` | The record of a UUID, or of the name a player last joined with (case-insensitive): `uuid`, `name`, `first-seen` and `last-seen` (Unix seconds), `last-ip`, `last-server`, `online`, `language`. `none` for a player Rift never saw. |
| `players.search(prefix, limit)` | Records whose name starts with `prefix`, most recently seen first, at most `limit` (1–100). |
| `players.set-language(target, language?)` | Sets the language Rift's own messages use for a player, online or not; `none` goes back to the client's locale. Returns whether the player is known. |

In Lua these are `rift.players.lookup`, `search` and `set_language`
([functions](players-api.md#functions)).

A player has a record from their first join on, so `login` of a first-time
player finds none. `players = { record_ip = false }` in the configuration keeps
addresses out of the directory: `last-ip` is then empty, and an address stored
earlier is cleared at the player's next join. The default is `true`.

**Languages.** A client reports its locale only after login, so the `login`
event never has one, and a network's language switcher needs somewhere to keep
a player's choice. `set-language` stores a tag of 2–16 bytes of `a-z0-9_`
(`de`, `de_de`; `de-DE` is normalized) with the player's record. From then on:

- `ctx.language` carries it in every event from `login` on, in later sessions
  too. Without a stored choice it is the client's locale once that is known.
- `player-info.language` is the same value; `player-info.locale` stays what
  the client reported.
- Rift's own messages to that player (queue notices, refusals, `/server`
  output) come from [`text.languages`](text.md#languages): the exact tag, then
  its language (`de` for `de_at`), then `text.messages`.
- [Plugin messages](text.md#plugin-messages) sent to that player follow the
  same order ([whose language](text.md#whose-language)).
