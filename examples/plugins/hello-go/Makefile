# Build hello_go.wasm with TinyGo (0.42.0 recommended, see sdk/go/README.md)
# and wasm-tools on PATH:
#
#   make TINYGO=/path/to/tinygo
#
# The artifact sits next to plugin.toml, which names it as the entry.
TINYGO ?= tinygo
WIT := ../../../wit
SDK := ../../../sdk/go

hello_go.wasm: main.go go.mod go.sum $(wildcard $(SDK)/rift/*.go) $(wildcard $(WIT)/*.wit)
	$(TINYGO) build -target=wasip2 --wit-package $(WIT) --wit-world plugin -no-debug -o $@ .

clean:
	rm -f hello_go.wasm

.PHONY: clean
