# Build hello_c.wasm with wasi-sdk 27 or newer:
#
#   make WASI_SDK_PATH=/path/to/wasi-sdk
#
# The artifact sits next to plugin.toml, which names it as the entry.
WASI_SDK_PATH ?= /opt/wasi-sdk
SDK := ../../../sdk/c
CC := $(WASI_SDK_PATH)/bin/clang
CFLAGS ?= -O2 -Wall -Wextra -Werror
# wasi-libc ships with DWARF; strip it unless you want to debug.
LDFLAGS ?= -Wl,--strip-debug

SDK_SOURCES := $(SDK)/rift_plugin.c $(SDK)/rift_plugin_defaults.c $(SDK)/rift_plugin_component_type.o

hello_c.wasm: hello.c $(SDK_SOURCES) $(SDK)/rift.h $(SDK)/rift_plugin.h
	$(CC) --target=wasm32-wasip2 -mexec-model=reactor $(CFLAGS) $(LDFLAGS) -I$(SDK) -o $@ hello.c $(SDK_SOURCES)

clean:
	rm -f hello_c.wasm

.PHONY: clean
