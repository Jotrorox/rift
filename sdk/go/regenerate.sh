#!/usr/bin/env bash
# Regenerate the Go bindings in gen/ from ../../wit with the pinned
# wit-bindgen-go (fetched through the Go module proxy; no install needed).
#
#   sdk/go/regenerate.sh          # rewrite gen/
#   sdk/go/regenerate.sh --check  # fail if the committed bindings are stale
set -euo pipefail

WIT_BINDGEN_GO=go.bytecodealliance.org/cmd/wit-bindgen-go@v0.7.0

sdk="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
wit="$sdk/../../wit"

out="$sdk/gen"
if [[ "${1:-}" == "--check" ]]; then
    out="$(mktemp -d)/gen"
    trap 'rm -rf "$(dirname "$out")"' EXIT
else
    rm -rf "$out"
fi

# World `plugin`, then `plugin-chat` on top of it: the same packages plus
# rift/plugin/chat, the optional `chat` export. Only a plugin that imports
# sdk/go/rift/chat links that package, and it is built with
# `--wit-world plugin-chat`.
for world in plugin plugin-chat; do
    go run "$WIT_BINDGEN_GO" generate \
        --world "$world" \
        --package-root github.com/Jotrorox/rift/sdk/go/gen \
        --out "$out" \
        "$wit"
done

if [[ "${1:-}" == "--check" ]]; then
    if ! diff -r "$sdk/gen" "$out"; then
        echo "error: sdk/go/gen is stale; run sdk/go/regenerate.sh" >&2
        exit 1
    fi
    echo "sdk/go/gen is up to date"
fi
