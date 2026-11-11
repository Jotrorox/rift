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

void exports_rift_plugin_handler_command(rift_context_t *ctx, rift_action_t *ret) {
  memset(ret, 0, sizeof *ret);
  ret->tag = RIFT_PLUGIN_TYPES_ACTION_MESSAGE;
  ret->val.message = rift_string_fmt("Hello, " RIFT_STR_FMT "!", RIFT_STR_ARG(ctx->name));
  rift_context_free(ctx);
}
```

```toml
# plugins/my-plugin/plugin.toml
name = "my-plugin"
version = "0.1.0"
api = "0.1"
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
| `handler` exports | `exports_rift_plugin_handler_<hook>` with the exact signatures in `rift_plugin.h`. `rift_plugin_defaults.c` provides weak no-op versions; define only the hooks you need. |
| `registration` | `rift_registration_init`, `rift_registration_add_hook`, `rift_registration_add_command`, `rift_registration_add_command_fixed`. |
| `action` | `rift_action_continue`, `rift_action_deny`, `rift_action_server`, `rift_action_message`, `rift_action_queue`, `rift_action_leave_queue`. |
| `decisions` export | `exports_rift_plugin_decisions_<hook>` answers with `rift_decision_deny_json` or `rift_decision_message_json`, a JSON text component ([rich text](extensions.md#rich-text)); the weak defaults forward to the handler hooks. |
| `styled` export | `exports_rift_plugin_styled_suggest` (build the list with `rift_rich_suggestions_add(ret, text, tooltip_json)`) and `exports_rift_plugin_styled_status` answer with JSON text tooltips, version and sample lines (since 0.1.4). The weak defaults forward to the handler's `suggest` and `status`; hosts before 0.1.4 call only those. |
| host functions | `rift_plugin_<interface>_<function>`, for example `rift_plugin_players_message`, `rift_plugin_players_message_json`, `rift_plugin_store_increment`, `rift_plugin_bus_publish`, `rift_plugin_timers_every`. `rift_message` and `rift_message_json` wrap the chat message calls; `rift_title_json(&target, title_json, subtitle_json_or_NULL, fade_in, stay, fade_out)` (since 0.1.4) wraps `players.title-json`; `rift_locale(&target, &out)` (since 0.1.5) wraps `players.locale` and returns false while the client's language is not known, which is always the case in `login` and, for clients before 1.20.2, in `join` ([timing](plugins.md#host-api)). |
| rich text | `rift_mini`, `rift_legacy`, `rift_component` builders (below). |
| `result<T, string>` | The function returns `bool` and fills a `ret` and an `err` out-parameter. |
| `option<T>` parameters | Pointers; `NULL` is `none`. |
| `log` | `rift_log(RIFT_INFO, "format %d", value)`. |
| configuration JSON | `rift_json_get_string(&config, "key", &out)` reads one top-level string; use cJSON or jsmn for more. |
| `dialogs` export, `players.dialog` | [Dialogs](dialogs.md): `rift_show_dialog(&target, json)` and `rift_plugin_players_clear_dialog`; clicks reach `exports_rift_plugin_dialogs_dialog(rift_dialog_response_t *)`, which owns its argument (free it with `rift_dialog_response_free`). The weak default ignores clicks. No registration needed. |

## Rich text builders

`rift.h` writes JSON text components, escaping every string. Each builder
returns a malloc'd, NUL-terminated string; free it with `free()`:

```c
// Rift markup, written as {"mini": "...", "args": {...}}; see docs/text.md.
rift_mini_t m = rift_mini("<gold>Welcome, <player>!</gold> <gray><click:run_command:/spawn>[Spawn]");
char *name = rift_string_cstr(&ctx->name);
rift_mini_arg(&m, "player", name);              // shown as written, never parsed
char *rank = rift_legacy("&c[Admin]");          // {"legacy": "&c[Admin]", "marker": "&"}
rift_mini_arg_component(&m, "rank", rank);      // a component argument
char *welcome = rift_mini_finish(&m);
rift_message_json(&ctx->uuid, welcome);
free(welcome), free(rank), free(name);

// A component field by field (1.21.5 spellings; Rift converts for older clients).
rift_component_t c = rift_component("[Accept]");
rift_component_color(&c, "green");
rift_component_bold(&c);
rift_component_run_command(&c, "/party accept Alex");
rift_component_hover_text(&c, "Join Alex's party");
rift_component_extra_text(&c, " or ignore it");
char *accept = rift_component_finish(&c);
rift_decision_message_json(ret, accept);       // copies
free(accept);

// A title with a legacy subtitle.
char *sub = rift_legacy_marker("§7Have fun", "§");
rift_title_json(&ctx->uuid, "{\"mini\":\"<green>Lobby\"}", sub, 10, 70, 20);
free(sub);
```

The component setters are `rift_component_color`, `_bold`, `_italic`,
`_underlined`, `_strikethrough`, `_obfuscated`, `_run_command`,
`_suggest_command`, `_open_url`, `_copy_to_clipboard`, `_hover_text` (plain),
`_hover` (a component as JSON), `_insertion`, `_extra` (a child as JSON),
`_extra_text` and `_field` (any other key, raw JSON); call each at most once.
`rift_string_quote` turns a `rift_string_t` into an owned JSON string.

Markup is only read from an explicit `mini` object: a bare string, as in
`rift_message_json(&t, "<red>hi")`, is shown as written. Pass player input
with `rift_mini_arg`, never by formatting it into the markup. The tags are
listed in [Text and markup](text.md).

## Memory ownership

These rules come from wit-bindgen's C generator and cause most C plugin bugs:

- **Handler arguments are yours.** Free them before returning
  (`rift_context_free`, `rift_bus_message_free`, or the generated
  `rift_plugin_types_*_free`).
- **Values you return are freed by the bindings** with `free()`. Every
  non-empty string or list in a return value must come from `malloc`: use
  `rift_string_dup`, `rift_string_fmt`, `rift_action_*` and
  `rift_registration_*`. Returning `rift_str("literal")` corrupts the heap.
- **Arguments you pass to host functions are borrowed**; literals are fine.
- **Results from host functions are yours**, including error strings: free
  them with `rift_string_free`, `rift_bytes_free`, `rift_string_list_free` or
  the generated helpers.

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
- **Traps restart the instance.** `abort()`, an out-of-bounds access or an
  allocation failure ends the call; Rift applies `on_failure` and restarts the
  instance per `failure.restart`. A hook still running at its deadline is
  interrupted the same way.
- **Older toolchains** (wasi-sdk before 22, or `--target=wasm32-wasip1`) emit
  core modules; componentize them with `wasm-tools component new
  module.wasm --adapt wasi_snapshot_preview1.reactor.wasm -o my_plugin.wasm`.
