#!/usr/bin/env bash
# Regenerate the C bindings from ../../wit with the pinned wit-bindgen.
#
#   sdk/c/regenerate.sh          # rewrite rift_plugin.{c,h} and the type object
#   sdk/c/regenerate.sh --check  # fail if the committed bindings are stale
#
# The bindings target world `core` (the Rift imports and the handler export).
# wasi-libc already carries its own WASI bindings, and a plugin built with
# wasi-sdk satisfies world `plugin`: its WASI imports are the subset libc uses.
# Generating world `plugin` would also duplicate every WASI binding, and with
# wit-bindgen up to at least 0.63 it produces `*_free` helpers that skip
# option<string>, list<string> and list<u8> fields shared with WASI.
set -euo pipefail

WIT_BINDGEN_VERSION=0.63.0

sdk="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
wit="$sdk/../../wit"

if ! version="$(wit-bindgen --version 2>/dev/null)"; then
    echo "error: wit-bindgen not found; cargo install wit-bindgen-cli --locked --version $WIT_BINDGEN_VERSION" >&2
    exit 1
fi
if [[ "$version" != "wit-bindgen-cli $WIT_BINDGEN_VERSION" ]]; then
    echo "error: found $version, expected wit-bindgen-cli $WIT_BINDGEN_VERSION" >&2
    exit 1
fi

args=(c --world core --rename-world rift_plugin --out-dir "$sdk")
if [[ "${1:-}" == "--check" ]]; then
    args+=(--check)
fi
wit-bindgen "${args[@]}" "$wit"
