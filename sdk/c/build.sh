#!/usr/bin/env bash
# Build a Rift plugin component from C sources with wasi-sdk.
#
#   sdk/c/build.sh -o plugin.wasm my_plugin.c [more.c ...] [extra clang flags]
#
# RIFT_CHAT=1 builds a plugin that handles the chat hook: it defines RIFT_CHAT
# and links the bindings in chat/, which export `rift:plugin/chat`. Rift
# starts such a plugin only with `authentication.signed_chat = false`.
#
# WASI_SDK_PATH selects the wasi-sdk installation (default /opt/wasi-sdk);
# CFLAGS defaults to -O2 -Wall -Wextra and LDFLAGS to -Wl,--strip-debug
# (wasi-libc ships with DWARF). wasi-sdk 22+ links a component
# directly for the wasm32-wasip2 target, so the output is ready to load.
set -euo pipefail

sdk="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
wasi_sdk="${WASI_SDK_PATH:-/opt/wasi-sdk}"
clang="$wasi_sdk/bin/clang"

if [[ ! -x "$clang" ]]; then
    echo "error: $clang not found; set WASI_SDK_PATH to a wasi-sdk (27 or newer) installation" >&2
    exit 1
fi

bindings="$sdk"
chat=()
if [[ -n "${RIFT_CHAT:-}" && "${RIFT_CHAT}" != "0" ]]; then
    bindings="$sdk/chat"
    chat=(-DRIFT_CHAT)
fi

# shellcheck disable=SC2086 # CFLAGS and LDFLAGS are flag lists.
exec "$clang" --target=wasm32-wasip2 -mexec-model=reactor ${CFLAGS:--O2 -Wall -Wextra} ${LDFLAGS:--Wl,--strip-debug} \
    ${chat[@]+"${chat[@]}"} -I"$sdk" \
    "$@" \
    "$bindings/rift_plugin.c" "$sdk/rift_plugin_defaults.c" "$bindings/rift_plugin_component_type.o"
