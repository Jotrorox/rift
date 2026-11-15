# C plugins

A C (or C++) plugin is a WebAssembly component built with
[wasi-sdk](https://github.com/WebAssembly/wasi-sdk/releases) and the SDK in
[`sdk/c`](../sdk/c): generated bindings, weak defaults for every hook and a
header of helpers. Read the [plugin overview](plugins.md) for how plugins are
enabled, dispatched and restarted, and [`sdk/c/README.md`](../sdk/c/README.md)
for the SDK's files and its CMake integration.

## Build

wasi-sdk 27 or newer links a component directly for `wasm32-wasip2`:

```sh
$WASI_SDK_PATH/bin/clang --target=wasm32-wasip2 -mexec-model=reactor -O2 \
  -Wl,--strip-debug -I sdk/c -o my_plugin.wasm my_plugin.c \
  sdk/c/rift_plugin.c sdk/c/rift_plugin_defaults.c sdk/c/rift_plugin_component_type.o
```

`sdk/c/build.sh -o my_plugin.wasm my_plugin.c` runs the same line
(`WASI_SDK_PATH` defaults to `/opt/wasi-sdk`). With CMake, configure with
`-DCMAKE_TOOLCHAIN_FILE=$WASI_SDK_PATH/share/cmake/wasi-sdk-p2.cmake`,
`include(sdk/c/RiftPlugin.cmake)` and call `rift_add_plugin(my_plugin my_plugin.c)`.
`-mexec-model=reactor` builds a library without `main`; `--strip-debug` drops
the DWARF wasi-libc ships with, which otherwise triples a small plugin's size.
Check the result with `wasm-tools component wit my_plugin.wasm`.

## A minimal plugin

```c
#include "rift.h"

bool exports_rift_plugin_handler_init(rift_string_t *config, rift_registration_t *ret, rift_string_t *err) {
  (void)err;
  rift_string_free(config);
  rift_registration_init(ret);
  rift_registration_add_command(ret, "hello", "hello.use", RIFT_PLUGIN_TYPES_SUGGEST_KIND_PLAYERS);
  rift_registration_add_hook(ret, RIFT_PLUGIN_TYPES_HOOK_COMMAND);
  return true;  // or set *err to an owned string and return false
}

void exports_rift_plugin_handler_command(rift_context_t *ctx, rift_decision_t *ret) {
  rift_decision_message_fmt(ret, "Hello, " RIFT_STR_FMT "!", RIFT_STR_ARG(ctx->name));
  rift_context_free(ctx);
}
```

```toml
# plugins/my-plugin/plugin.toml
name = "my-plugin"
version = "0.1.0"
api = "0.2"
engine = "wasm"
entry = "my_plugin.wasm"

[capabilities]
filesystem = ["data"]
network = false
env = []
```

Then `rift.plugin("my-plugin")` in `rift.lua`.
[`examples/plugins/hello-c`](../examples/plugins/hello-c) is the complete
example (`make WASI_SDK_PATH=/path/to/wasi-sdk`), behaving exactly like the
Rust, Go and Lua examples.

## Mapping the contract

| Contract | C |
| --- | --- |
| `handler` exports | `exports_rift_plugin_handler_<hook>` with the exact signatures in `rift_plugin.h`. `rift_plugin_defaults.c` provides weak versions of all of them; define only the hooks you need. |
| `registration` | `rift_registration_init`, `rift_registration_add_hook`, `rift_registration_add_command`, `rift_registration_add_command_fixed`; [aliases and the console](#commands). |
| `text` | `rift_text_plain("...")` and `rift_text_json(json)`; see [text](#text). |
| `decision` | `rift_decision_continue`, `rift_decision_deny(ret, text)`, `rift_decision_message(ret, text)`, `rift_decision_server`, `rift_decision_queue`, `rift_decision_leave_queue`; `rift_decision_deny_fmt` and `rift_decision_message_fmt` format plain text. |
| `suggest`, `status` | `rift_suggestions_add(ret, text, tooltip_or_NULL)`; `rift_status_override_init`, `rift_status_override_add_sample` and owned texts in `motd` and `version`. |
| host functions | `rift_plugin_<interface>_<function>`, for example `rift_plugin_players_tab_list`, `rift_plugin_store_increment`, `rift_plugin_bus_publish`, `rift_plugin_timers_every`. `rift.h` wraps the common ones and logs their errors: `rift_message`, `rift_action_bar`, `rift_title`, `rift_kick`, `rift_broadcast`, `rift_show_dialog`, `rift_player_lookup`, `rift_player_search`, `rift_set_language`, `rift_add_group`, `rift_background_*`. |
| `chat`, `kick` | `exports_rift_plugin_handler_chat` and `exports_rift_plugin_handler_kick`, delivered only when the hook list names them; see [chat and kick](#chat-and-kick). |
| `background` | `rift_background_defer`, `rift_background_resolve`, `rift_background_run`, `rift_background_is_worker`; see [background work](#background-work). |
| `result<T, string>` | The function returns `bool` and fills a `ret` and an `err` out-parameter. |
| `option<T>` parameters | Pointers; `NULL` is `none`. |
| `log` | `rift_log(RIFT_INFO, "format %d", value)`. |
| configuration JSON | `rift_json_get_string(&config, "key", &out)` reads one top-level string; use cJSON or jsmn for more. |
| `dialog`, `players.dialog` | [Dialogs](dialogs.md): `rift_show_dialog(&target, json)` and `rift_plugin_players_clear_dialog`; clicks reach `exports_rift_plugin_handler_dialog(rift_dialog_response_t *)`, which owns its argument (free it with `rift_dialog_response_free`). The weak default ignores clicks. No registration needed. |

## Text

Everything a player reads is a `rift_text_t`: plain text of 1..=1024 bytes,
shown exactly as written, or a JSON text component of at most 32 KiB
([rich text](extensions.md#rich-text)). Messages, action bars, titles, tab
lists, boss bars, kick reasons, `deny` and `message` decisions, suggestion
tooltips and the server-list fields all take one.

```c
rift_message(&ctx->uuid, rift_text_plain("Welcome back."));
rift_action_bar(&ctx->uuid, rift_text_json("{\"text\":\"Lobby\",\"color\":\"green\"}"));
rift_decision_deny(ret, rift_text_plain("You are banned."));

rift_text_t title = rift_text_json("{\"mini\":\"<green>Lobby\"}");
rift_text_t subtitle = rift_text_plain("Have fun");
rift_title(&ctx->uuid, &title, &subtitle, 10, 70, 20);  // either may be NULL
```

`rift_text_plain`, `rift_text_json` and `rift_text_plain_string(&ctx->name)`
are borrowed views, like `rift_str`. Host functions borrow them and the
`rift.h` helpers copy what they keep, so literals are fine in both places. A
text you store yourself in a value a handler returns must be owned:
`rift_text_dup(text)` or `rift_text_plain_fmt("%d online", n)`.

```c
bool exports_rift_plugin_handler_status(exports_rift_plugin_handler_ping_t *ping, rift_status_override_t *ret) {
  rift_status_override_init(ret);
  ret->motd = rift_some_text(rift_text_dup(rift_text_json("{\"mini\":\"<gold>My network\"}")));
  rift_status_override_add_sample(ret, rift_text_plain("§7Hello"));
  rift_plugin_types_ping_free(ping);
  return true;
}
```

## Commands

`rift_registration_add_command` returns the command spec. Add other names
with `rift_command_add_alias` (at most 16) and let operators run the command
from the console with `rift_command_set_console`:

```c
rift_command_spec_t *find = rift_registration_add_command(ret, "find", "find.use", RIFT_PLUGIN_TYPES_SUGGEST_KIND_PLAYERS);
rift_command_add_alias(find, "whereis");
rift_command_set_console(find, true);
```

The hooks always see `find` in `ctx->command`, whichever name was typed. For
a console run `ctx->console` is true, `ctx->name` is `CONSOLE`, `ctx->uuid` is
empty and `ctx->connection_id` is 0; a `message` decision is printed for the
operator. The spec pointer is valid until the next command is added.

## Chat and kick

`exports_rift_plugin_handler_chat` sees chat messages, and the commands that
reach the backend, before the backend does ([the hook](plugins.md#chat));
`exports_rift_plugin_handler_kick` runs when a backend kicks a player in play.
Both are delivered only to a plugin whose hook list names them: an empty list
means every hook except these two.

```c
rift_registration_add_hook(ret, RIFT_PLUGIN_TYPES_HOOK_CHAT);
rift_registration_add_hook(ret, RIFT_PLUGIN_TYPES_HOOK_KICK);
```

```c
void exports_rift_plugin_handler_chat(rift_chat_attempt_t *attempt, rift_decision_t *ret) {
  if (attempt->kind == RIFT_PLUGIN_TYPES_CHAT_KIND_MESSAGE && is_muted(&attempt->ctx.uuid)) {
    rift_decision_deny(ret, rift_text_plain("You are muted."));
  } else {
    rift_decision_continue(ret);
  }
  rift_chat_attempt_free(attempt);
}

void exports_rift_plugin_handler_kick(rift_context_t *ctx, rift_decision_t *ret) {
  // ctx->server is the backend that kicked, ctx->reason its reason.
  rift_decision_server(ret, "lobby");
  rift_context_free(ctx);
}
```

`attempt->text` is the message, or the command line without its slash; for a
command, `attempt->ctx.command` is the root name and `attempt->ctx.args` the
rest. `chat` answers `continue` or `deny`. `kick` answers `continue` (leave it
to `network.on_kick`), `deny` (disconnect with the text) or `server`. Rift
asks a plugin that lists `chat` about every message of every player; with
signed chat, the first deny costs that player their
[chat signing](plugins.md#chat).

## Background work

A plugin runs one call at a time, so a hook that blocks holds up every other
hook. A decision hook can defer its answer instead, hand the slow part to a
background worker and return. Workers are further instances of the same
component, enabled in the manifest:

```toml
[background]
workers = 2
```

A worker has its own memory and globals. It never gets `init` or any hook:
only `work_init` once, with the plugin configuration, and then one `work`
call per job. `work` may block (30 s unless the manifest sets
`deadlines.work`); its result returns to the main instance through
`job_done`.

```c
#include "rift.h"

#define PENDING 64
static struct { bool used; uint64_t handle, ticket; } pending[PENDING];

// Main instance: defer the answer and queue the lookup.
void exports_rift_plugin_handler_login(rift_context_t *ctx, rift_decision_t *ret) {
  uint64_t ticket, handle;
  rift_decision_continue(ret);  // ignored once the hook is deferred
  for (int i = 0; i < PENDING; i++) {
    if (pending[i].used) continue;
    if (rift_background_defer(&ticket)) {
      if (rift_background_run(ctx->uuid.ptr, ctx->uuid.len, &handle)) {
        pending[i].used = true;
        pending[i].handle = handle;
        pending[i].ticket = ticket;
      } else {
        rift_decision_t allow;
        rift_decision_continue(&allow);
        rift_background_resolve(ticket, &allow);
      }
    }
    break;
  }
  rift_context_free(ctx);
}

// Worker: the slow part. `payload` is the UUID queued above.
bool exports_rift_plugin_handler_work(rift_bytes_t *payload, rift_bytes_t *ret, rift_string_t *err) {
  (void)err;
  bool banned = ban_database_has(payload->ptr, payload->len);  // may block
  rift_bytes_free(payload);
  *ret = rift_bytes_dup(banned ? "1" : "0", 1);
  return true;  // or set *err to an owned string and return false
}

// Main instance: answer the deferred login.
void exports_rift_plugin_handler_job_done(uint64_t handle, rift_job_outcome_t *outcome) {
  for (int i = 0; i < PENDING; i++) {
    if (!pending[i].used || pending[i].handle != handle) continue;
    rift_decision_t decision;
    if (!outcome->is_err && outcome->val.ok.len == 1 && outcome->val.ok.ptr[0] == '1') {
      rift_decision_deny(&decision, rift_text_plain("You are banned."));
    } else {
      rift_decision_continue(&decision);
    }
    rift_background_resolve(pending[i].ticket, &decision);  // frees the decision
    pending[i].used = false;
    break;
  }
  rift_job_outcome_free(outcome);
}
```

- `rift_background_defer` works in `login`, `initial_server`,
  `before_transfer`, `command`, `chat` and `kick`, once per call. A hook that
  does not defer answers through `ret` as usual.
- The hook's deadline keeps running while the job does. An answer after it
  counts as a missed deadline (`on_timeout`), and `rift_background_resolve`
  then returns false.
- `rift_background_run` fails without workers in the manifest and when 1024
  jobs wait already. `job_done` reports a failed job with `outcome->is_err`
  and the reason in `outcome->val.err`.
- `rift_background_is_worker()` tells the two kinds of instance apart in code
  they share. `job_done` needs no entry in the hook list.

## Players, languages and groups

`rift_plugin_players_get` and `_list` return `rift_player_info_t` with the
player's `ip`, profile `properties`, `visible_to`, the client's `locale` and
the `language` Rift's own messages use. For players who are offline:

```c
rift_player_record_t record;
if (rift_player_lookup(&ctx->args, &record)) {  // a UUID or the last name
  rift_log(RIFT_INFO, RIFT_STR_FMT " was last seen at %llu on " RIFT_STR_FMT, RIFT_STR_ARG(record.name),
           (unsigned long long)record.last_seen, RIFT_STR_ARG(record.last_server));
  rift_player_record_free(&record);
}

rift_player_record_list_t found;
if (rift_player_search("ste", 10, &found)) {    // most recently seen first
  for (size_t i = 0; i < found.len; i++) use(&found.ptr[i]);
  rift_player_record_list_free(&found);
}
```

`rift_set_language(&ctx->uuid, "de_de")` sets the language of Rift's own
messages for a player, online or not; `NULL` goes back to the client's
locale. It is remembered across sessions, so `ctx->language` has it from
`login` on. Without one, `ctx->language` is the client's locale once the
client has sent it, which is after `login`.

`rift_add_group(&ctx->uuid, "vip", expires_at)` adds the user to a group
until `expires_at` Unix seconds, replacing an earlier expiry; `0` adds them
for good:

```c
rift_add_group(&ctx->uuid, "vip", (uint64_t)rift_plugin_meta_time() + 30 * 86400);
```

## Rich text builders

`rift.h` writes JSON text components, escaping every string. Each builder
returns a malloc'd, NUL-terminated string; wrap it with `rift_text_json` and
free it with `free()`:

```c
// Rift markup, written as {"mini": "...", "args": {...}}; see docs/text.md.
rift_mini_t m = rift_mini("<gold>Welcome, <player>!</gold> <gray><click:run_command:/spawn>[Spawn]");
char *name = rift_string_cstr(&ctx->name);
rift_mini_arg(&m, "player", name);              // shown as written, never parsed
char *rank = rift_legacy("&c[Admin]");          // {"legacy": "&c[Admin]", "marker": "&"}
rift_mini_arg_component(&m, "rank", rank);      // a component argument
char *welcome = rift_mini_finish(&m);
rift_message(&ctx->uuid, rift_text_json(welcome));
free(welcome), free(rank), free(name);

// A component field by field (1.21.5 spellings; Rift converts for older clients).
rift_component_t c = rift_component("[Accept]");
rift_component_color(&c, "green");
rift_component_bold(&c);
rift_component_run_command(&c, "/party accept Alex");
rift_component_hover_text(&c, "Join Alex's party");
rift_component_extra_text(&c, " or ignore it");
char *accept = rift_component_finish(&c);
rift_decision_message(ret, rift_text_json(accept));  // copies
free(accept);

// A subtitle alone, from legacy codes.
char *sub = rift_legacy_marker("§7Have fun", "§");
rift_text_t subtitle = rift_text_json(sub);
rift_title(&ctx->uuid, NULL, &subtitle, 10, 70, 20);
free(sub);
```

The component setters are `rift_component_color`, `_bold`, `_italic`,
`_underlined`, `_strikethrough`, `_obfuscated`, `_run_command`,
`_suggest_command`, `_open_url`, `_copy_to_clipboard`, `_hover_text` (plain),
`_hover` (a component as JSON), `_insertion`, `_extra` (a child as JSON),
`_extra_text` and `_field` (any other key, raw JSON); call each at most once.
`rift_string_quote` turns a `rift_string_t` into an owned JSON string.

Markup is only read from an explicit `mini` object: a bare JSON string, as in
`rift_text_json("\"<red>hi\"")`, is shown as written, and so is all plain
text. Pass player input with `rift_mini_arg`, never by formatting it into the
markup. The tags are listed in [Text and markup](text.md).

## Memory ownership

These rules come from wit-bindgen's C generator and cause most C plugin bugs:

- **Handler arguments are yours.** Free them before returning
  (`rift_context_free`, `rift_chat_attempt_free`, `rift_bus_message_free`,
  `rift_job_outcome_free`, or the generated `rift_plugin_types_*_free`).
- **Values you return are freed by the bindings** with `free()`. Every
  non-empty string or list in a return value must come from `malloc`: use
  `rift_string_dup`, `rift_string_fmt`, `rift_text_dup`, `rift_decision_*`,
  `rift_suggestions_add` and `rift_registration_*`. Returning
  `rift_str("literal")` or a `rift_text_plain` view corrupts the heap.
- **Arguments you pass to host functions are borrowed**; literals are fine.
- **Results from host functions are yours**, including error strings: free
  them with `rift_string_free`, `rift_bytes_free`, `rift_string_list_free`,
  `rift_player_record_free` or the generated helpers.

## Gotchas

- **The bindings are generated for world `core`, not `plugin`.** wit-bindgen's
  C generator (seen with 0.41 and 0.63) emits `*_free` helpers that silently
  skip `option<string>`, `list<string>` and `list<u8>` fields whose types are
  shared with WASI, which leaks memory on every hook. Generating world `core`
  (the Rift imports and the handler export without WASI) avoids the shared
  types; wasi-libc brings its own WASI bindings, so the built component still
  satisfies world `plugin` and imports exactly the WASI interfaces libc uses.
  Keep this when regenerating: `sdk/c/regenerate.sh` does it for you.
- **wasi-sdk's WASI patch version differs from Rift's.** The component imports
  WASI 0.2 at the patch version wasi-sdk targets; Rift links WASI 0.2
  semver-compatibly, so this needs no action.
- **A worker shares no memory with the main instance.** Globals set in
  `init` are not set in a worker; read the configuration again in
  `work_init`, and pass everything a job needs in its payload.
- **Traps restart the instance.** `abort()`, an out-of-bounds access or an
  allocation failure ends the call; Rift applies `on_failure` and restarts the
  instance per `failure.restart`. A hook still running at its deadline is
  interrupted the same way.
- **Older toolchains** (wasi-sdk before 22, or `--target=wasm32-wasip1`) emit
  core modules; componentize them with `wasm-tools component new
  module.wasm --adapt wasi_snapshot_preview1.reactor.wasm -o my_plugin.wasm`.
