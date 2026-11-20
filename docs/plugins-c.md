# C plugins

A C (or C++) plugin is a WebAssembly component built with
[wasi-sdk](https://github.com/WebAssembly/wasi-sdk/releases) and the SDK in
[`sdk/c`](../sdk/c): generated bindings, weak defaults for every hook and a
header of helpers. This guide shows how the plugin contract looks in C.
[Plugins](plugins.md) covers enabling and trust;
[Plugin events and commands](plugin-events.md) says what each event and answer
means.

## Build

wasi-sdk 27 or newer links a component directly for `wasm32-wasip2`:

```sh
$WASI_SDK_PATH/bin/clang --target=wasm32-wasip2 -mexec-model=reactor -O2 \
  -Wl,--strip-debug -I sdk/c -o my_plugin.wasm my_plugin.c \
  sdk/c/rift_plugin.c sdk/c/rift_plugin_defaults.c sdk/c/rift_plugin_component_type.o
```

- `sdk/c/build.sh -o my_plugin.wasm my_plugin.c` runs the same line
  (`WASI_SDK_PATH` defaults to `/opt/wasi-sdk`).
- With CMake, configure with
  `-DCMAKE_TOOLCHAIN_FILE=$WASI_SDK_PATH/share/cmake/wasi-sdk-p2.cmake`,
  `include(sdk/c/RiftPlugin.cmake)` and call
  `rift_add_plugin(my_plugin my_plugin.c)`.
- `-mexec-model=reactor` builds a library without `main`. `--strip-debug`
  drops the DWARF wasi-libc ships with, which otherwise triples a small
  plugin's size.
- Check the result with `wasm-tools component wit my_plugin.wasm`.

[`sdk/c/README.md`](../sdk/c/README.md) describes the SDK's files and its
CMake integration.

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
| `chat`, `kick` | `exports_rift_plugin_handler_chat` and `exports_rift_plugin_handler_kick`; see [chat and kick](#chat-and-kick). |
| `background` | `rift_background_defer`, `rift_background_resolve`, `rift_background_run`, `rift_background_is_worker`; see [background work](#background-work). |
| `result<T, string>` | The function returns `bool` and fills a `ret` and an `err` out-parameter. |
| `option<T>` parameters | Pointers; `NULL` is `none`. |
| `log` | `rift_log(RIFT_INFO, "format %d", value)`. |
| `text` | `rift_text_render`, `rift_text_to_plain`, `rift_text_to_legacy`, `rift_text_to_mini` (since 0.2.1); see [converting text](#converting-text). |
| configuration JSON | `rift_json_get_string(&config, "key", &out)` reads one top-level string; use cJSON or jsmn for more. |
| `dialog`, `players.dialog` | [Dialogs](dialogs.md): `rift_show_dialog(&target, json)` and `rift_plugin_players_clear_dialog`; clicks reach `exports_rift_plugin_handler_dialog(rift_dialog_response_t *)`, which owns its argument (free it with `rift_dialog_response_free`). The weak default ignores clicks. No registration needed. |

## Text

Everything a player reads is a `rift_text_t`, the contract's
[`text`](plugin-events.md#text): plain text shown exactly as written, or a JSON
text component.

```c
rift_message(&ctx->uuid, rift_text_plain("Welcome back."));
rift_action_bar(&ctx->uuid, rift_text_json("{\"text\":\"Lobby\",\"color\":\"green\"}"));
rift_decision_deny(ret, rift_text_plain("You are banned."));

rift_text_t title = rift_text_json("{\"mini\":\"<green>Lobby\"}");
rift_text_t subtitle = rift_text_plain("Have fun");
rift_title(&ctx->uuid, &title, &subtitle, 10, 70, 20);  // either may be NULL
```

`rift_text_plain`, `rift_text_json` and `rift_text_plain_string(&ctx->name)`
are borrowed views, like `rift_str`.

- Host functions borrow them and the `rift.h` helpers copy what they keep, so
  literals are fine in both places.
- A text you store yourself in a value a handler returns must be owned:
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

[Rich text builders](#rich-text-builders) write the JSON for you.

## Commands

`rift_registration_add_command` returns the command spec. Add other names
with `rift_command_add_alias` (at most 16) and let operators run the command
from the console with `rift_command_set_console`:

```c
rift_command_spec_t *find = rift_registration_add_command(ret, "find", "find.use", RIFT_PLUGIN_TYPES_SUGGEST_KIND_PLAYERS);
rift_command_add_alias(find, "whereis");
rift_command_set_console(find, true);
```

- The spec pointer is valid until the next command is added.
- The hooks always see `find` in `ctx->command`, whichever name was typed.
- For a console run `ctx->console` is true, `ctx->name` is `CONSOLE`,
  `ctx->uuid` is empty and `ctx->connection_id` is 0; a `message` decision is
  printed for the operator ([commands](plugin-events.md#commands)).

## Chat and kick

`exports_rift_plugin_handler_chat` handles the
[`chat`](plugin-events.md#chat) event and `exports_rift_plugin_handler_kick`
the `kick` event. Rift delivers both only when the hook list names them: an
empty list means every hook except these two.

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

| Hook | Input | Answers |
| --- | --- | --- |
| `chat` | `attempt->text` is the message, or the command line without its slash. For a command, `attempt->ctx.command` is the root name and `attempt->ctx.args` the rest | `continue` or `deny` |
| `kick` | `ctx->server` and `ctx->reason` | `continue` (leave it to `network.on_kick`), `deny` (disconnect with the text) or `server` |

With signed chat, the first deny costs that player their
[chat signing](plugin-events.md#denying-costs-the-player-chat-signing).

## Background work

[Background work](plugin-events.md#background-work) explains the model and
its rules. Workers need `[background]` in the manifest.

In C a worker is another instance of the component with its own memory and
globals: globals set in `init` are not set there. It runs `work_init` once,
with the plugin configuration, and then one `work` call per job, so read the
configuration again in `work_init` and pass everything a job needs in its
payload. The result returns to the main instance through `job_done`.

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

- `rift_background_resolve` returns false when the answer came after the
  hook's deadline.
- `job_done` reports a failed job with `outcome->is_err` and the reason in
  `outcome->val.err`.
- `rift_background_is_worker()` tells the two kinds of instance apart in code
  they share.

## Players, languages and groups

[The player directory and languages](plugin-events.md#the-player-directory-and-languages)
describes the records and the language rules.

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

- `rift_set_language(&ctx->uuid, "de_de")` sets the language of Rift's own
  messages for a player, online or not; `NULL` goes back to the client's
  locale. `ctx->language` has it from `login` on.
- `rift_add_group(&ctx->uuid, "vip", expires_at)` adds the user to a group
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

// A message from the plugin's messages.toml, written as {"message": "...", "args": {...}}.
rift_msg_t msg = rift_msg("invited");
rift_msg_arg(&msg, "from", "Alex");
rift_msg_arg_style(&msg, "team", "<#55ffff><bold>");  // a style argument
char *invited = rift_msg_finish(&msg);
rift_message(&ctx->uuid, rift_text_json(invited));
free(invited);

// Other contents: a translation with an argument and a fallback.
rift_component_t t = rift_translate("multiplayer.player.joined");
rift_component_with_text(&t, "Alex");
rift_component_fallback(&t, "Alex joined");
char *joined = rift_component_finish(&t);
free(joined);

// Text you splice into markup yourself: < becomes \< and \ becomes \\.
char *safe = rift_text_escape(line);
free(safe);
```

- `rift_mini` takes `rift_mini_arg` (shown as written),
  `rift_mini_arg_component` and `rift_mini_arg_style` (a
  [style argument](text.md#style-arguments): markup made only of formatting tags).
  `rift_msg(key)` takes `rift_msg_arg`, `rift_msg_arg_component` and
  `rift_msg_arg_style`, and `rift_msg_finish` returns the JSON. A message
  comes from the plugin's `messages.toml`; see [plugin messages](text.md#plugin-messages). It is
  `rift_msg` because `rift_message` sends a player text.
- `rift_text_escape(text)` returns a malloc'd copy escaped for splicing into
  markup. An argument needs no escaping.
- The component builders, each `rift_component_*` setter called at most once:

  | Kind | Functions |
  | --- | --- |
  | Content | `rift_component(text)`, `rift_translate(key)` with `rift_component_with` (JSON), `_with_text` and `_fallback`, `rift_keybind(key)`, `rift_sprite(sprite)` with `rift_component_atlas`, `rift_head(name)` with `rift_component_hat(&c, false)` |
  | Style | `rift_component_color`, `_bold`, `_italic`, `_underlined`, `_strikethrough`, `_obfuscated`, `_decoration(&c, name, on)` (`false` turns it off; use it instead of `_bold` and the others, not as well), `_font`, `_shadow(&c, "#rrggbb", opacity)`, `_insertion` |
  | Click | `_run_command`, `_suggest_command`, `_open_url`, `_copy_to_clipboard`, `_change_page(&c, page)` |
  | Hover | `_hover_text` (plain), `_hover` (a component as JSON), `_hover_item(&c, id, count)`, `_hover_entity(&c, type, uuid, name_json)` (`name_json` may be NULL) |
  | Other | `_extra` (a child as JSON), `_extra_text`, `_field` (any other key, raw JSON) |

  Sprites and heads need 1.21.9 or newer; older clients drop them.
- `rift_string_quote` turns a `rift_string_t` into an owned JSON string.

Rift reads markup only from an explicit `mini` object. A bare JSON string, as
in `rift_text_json("\"<red>hi\"")`, is shown as written, and so is all plain
text. Pass player input with `rift_mini_arg`, never by formatting it into the
markup. The tags are listed in [Styled text](text.md).

## Converting text

`rift_text_render(text, language, &out, &err)`, `rift_text_to_plain`,
`rift_text_to_legacy` and `rift_text_to_mini` are the contract's `text`
interface (since 0.2.1: set `min_api = "0.2.1"` in `plugin.toml`). Rift
converts `text` with markup, legacy codes and the plugin's messages resolved
in `language` (`"de"`; `NULL` for the default) into compact JSON, the words,
`§` codes or markup. `rift_text_to_plain` is not `rift_text_plain`, which
makes a plain text view.

- `text` is borrowed, like every argument of a host function.
- On success the function returns true and `out` is yours: free it with
  `rift_string_free`.
- On failure it returns false with `out` empty, and the host's error is in
  `err`, yours to free; with `err` NULL the error is logged at warn level.

Use them for text that leaves through a bus payload, an HTTP response or a
log line, and in `init` to check a template from the configuration:

```c
// In exports_rift_plugin_handler_init(config_json, ret, err):
rift_string_t json;
if (!rift_text_render(rift_text_json(template_json), NULL, &json, err)) {
  return false;  // init fails with the host's error, which the bindings free
}
rift_string_free(&json);

// The plugin's own message `created` without formatting, in German.
rift_msg_t msg = rift_msg("created");
char *created = rift_msg_finish(&msg);
rift_string_t line;
if (rift_text_to_plain(rift_text_json(created), "de", &line, NULL)) {
  rift_log(RIFT_INFO, "German: " RIFT_STR_FMT, RIFT_STR_ARG(line));
  rift_string_free(&line);
}
free(created);
```

## Memory ownership

These rules come from wit-bindgen's C generator and cause most C plugin bugs:

| What | Who frees it |
| --- | --- |
| Handler arguments | You, before returning: `rift_context_free`, `rift_chat_attempt_free`, `rift_bus_message_free`, `rift_job_outcome_free`, or the generated `rift_plugin_types_*_free` |
| Values you return | The bindings, with `free()`. Every non-empty string or list in a return value must come from `malloc`: use `rift_string_dup`, `rift_string_fmt`, `rift_text_dup`, `rift_decision_*`, `rift_suggestions_add` and `rift_registration_*` |
| Arguments you pass to host functions | Nobody: they are borrowed, so literals are fine |
| Results from host functions, including error strings | You: `rift_string_free`, `rift_bytes_free`, `rift_string_list_free`, `rift_player_record_free` or the generated helpers |

> [!WARNING]
> Returning `rift_str("literal")` or a `rift_text_plain` view from a handler
> corrupts the heap.

## Gotchas

- **The bindings are generated for world `core`, not `plugin`.** For world
  `plugin`, wit-bindgen's C generator (seen with 0.41 and 0.63) emits `*_free`
  helpers that silently skip `option<string>`, `list<string>` and `list<u8>`
  fields whose types are shared with WASI, which leaks memory on every hook.
  World `core` has no WASI, and wasi-libc brings its own WASI bindings, so the
  built component still satisfies world `plugin`. Keep this when
  regenerating: `sdk/c/regenerate.sh` does it for you.
- **wasi-sdk's WASI patch version differs from Rift's.** The component imports
  WASI 0.2 at the patch version wasi-sdk targets; Rift links WASI 0.2
  semver-compatibly, so this needs no action.
- **Traps restart the instance.** `abort()`, an out-of-bounds access or an
  allocation failure ends the call; Rift applies `on_failure` and restarts the
  instance per `failure.restart`. A hook still running at its deadline is
  interrupted the same way.
- **Older toolchains** (wasi-sdk before 22, or `--target=wasm32-wasip1`) emit
  core modules; componentize them with `wasm-tools component new
  module.wasm --adapt wasi_snapshot_preview1.reactor.wasm -o my_plugin.wasm`.
