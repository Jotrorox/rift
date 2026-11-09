#!/usr/bin/env bash
# Check that a plugin artifact is a component built against rift:plugin@0.1:
# it must export the handler interface and import the Rift interfaces the
# hello examples use. Any 0.1.x version is accepted, since Wasmtime matches
# them with each other. Needs wasm-tools on PATH.
#
#   sdk/check-component.sh examples/plugins/hello-c/hello_c.wasm
set -euo pipefail

wasm="$1"
wit="$(wasm-tools component wit "$wasm")"

missing=0
for line in \
    'export rift:plugin/handler' \
    'import rift:plugin/players' \
    'import rift:plugin/store' \
    'import rift:plugin/bus' \
    'import rift:plugin/timers' \
    'import rift:plugin/log'; do
    if ! grep -qE "^\s+${line}@0\.1\.[0-9]+;$" <<<"$wit"; then
        echo "error: $wasm lacks \`$line\`" >&2
        missing=1
    fi
done
if [[ "$missing" != 0 ]]; then
    exit 1
fi

# Everything else must be WASI 0.2 or Rift itself.
if unexpected="$(sed -n '/^world root {/,/^}/p' <<<"$wit" | grep -E '^\s+(import|export) ' | grep -vE '(rift:plugin/[a-z-]+@0\.1\.[0-9]+|wasi:[a-z-]+/[a-z-]+@0\.2\.[0-9]+);$')"; then
    echo "error: unexpected imports or exports in $wasm:" >&2
    echo "$unexpected" >&2
    exit 1
fi

wasm-tools validate "$wasm"
echo "$wasm: $(wc -c <"$wasm" | tr -d " ") bytes, exports $(grep -oE 'rift:plugin/handler@0\.1\.[0-9]+' <<<"$wit" | head -1)"
