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
| `handler` exports | `exports_rift_plugin_handler_<hook>` with the exact signatures in `rift_plugin.h`. `rift_plugin_defaults.c` provides weak no-op versions; define only the hooks you need. |
| `registration` | `rift_registration_init`, `rift_registration_add_hook`, `rift_registration_add_command`, `rift_registration_add_command_fixed`. |
| `action` | `rift_action_continue`, `rift_action_deny`, `rift_action_deny_json`, `rift_action_server`, `rift_action_message`, `rift_action_message_json`, `rift_action_queue`, `rift_action_leave_queue`. The `_json` forms take a JSON text component ([rich text](extensions.md#rich-text)). |
| host functions | `rift_plugin_<interface>_<function>`, for example `rift_plugin_players_message`, `rift_plugin_players_message_json`, `rift_plugin_store_increment`, `rift_plugin_bus_publish`, `rift_plugin_timers_every`. `rift_message` and `rift_message_json` wrap the chat message calls. |
| `result<T, string>` | The function returns `bool` and fills a `ret` and an `err` out-parameter. |
| `option<T>` parameters | Pointers; `NULL` is `none`. |
| `log` | `rift_log(RIFT_INFO, "format %d", value)`. |
| configuration JSON | `rift_json_get_string(&config, "key", &out)` reads one top-level string; use cJSON or jsmn for more. |

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
