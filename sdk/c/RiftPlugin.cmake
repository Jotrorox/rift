# CMake support for Rift C plugins.
#
# Configure with wasi-sdk's own wasm32-wasip2 toolchain file:
#
#   cmake -B build -DCMAKE_TOOLCHAIN_FILE=$WASI_SDK_PATH/share/cmake/wasi-sdk-p2.cmake
#   cmake --build build
#
# and in CMakeLists.txt:
#
#   include(path/to/sdk/c/RiftPlugin.cmake)
#   rift_add_plugin(my_plugin my_plugin.c)   # produces my_plugin.wasm
#
# A plugin that handles the chat hook uses rift_add_chat_plugin() instead: it
# defines RIFT_CHAT and links the bindings in chat/, which export
# `rift:plugin/chat`. Rift starts such a plugin only with
# `authentication.signed_chat = false`.
#
# The target is a component ready to load: wasi-sdk links components
# directly for wasm32-wasip2.

if(NOT CMAKE_C_COMPILER_TARGET STREQUAL "wasm32-wasip2")
    message(FATAL_ERROR
        "Rift plugins need the wasm32-wasip2 target; configure with "
        "-DCMAKE_TOOLCHAIN_FILE=<wasi-sdk>/share/cmake/wasi-sdk-p2.cmake")
endif()

set(RIFT_C_SDK_DIR "${CMAKE_CURRENT_LIST_DIR}" CACHE INTERNAL "Rift C plugin SDK")

add_library(rift_plugin_sdk OBJECT
    "${RIFT_C_SDK_DIR}/rift_plugin.c"
    "${RIFT_C_SDK_DIR}/rift_plugin_defaults.c")
target_include_directories(rift_plugin_sdk PUBLIC "${RIFT_C_SDK_DIR}")

add_library(rift_plugin_chat_sdk OBJECT EXCLUDE_FROM_ALL
    "${RIFT_C_SDK_DIR}/chat/rift_plugin.c"
    "${RIFT_C_SDK_DIR}/rift_plugin_defaults.c")
target_include_directories(rift_plugin_chat_sdk PUBLIC "${RIFT_C_SDK_DIR}")
target_compile_definitions(rift_plugin_chat_sdk PUBLIC RIFT_CHAT)

function(rift_plugin_target name sdk type)
    add_executable(${name} ${ARGN} $<TARGET_OBJECTS:${sdk}> "${type}")
    target_include_directories(${name} PRIVATE "${RIFT_C_SDK_DIR}")
    set_target_properties(${name} PROPERTIES SUFFIX ".wasm")
    # A reactor has no `main`. wasi-libc ships with DWARF; keep it only for
    # debug builds.
    target_link_options(${name} PRIVATE -mexec-model=reactor
        $<$<NOT:$<CONFIG:Debug>>:-Wl,--strip-debug>)
endfunction()

function(rift_add_plugin name)
    rift_plugin_target(${name} rift_plugin_sdk
        "${RIFT_C_SDK_DIR}/rift_plugin_component_type.o" ${ARGN})
endfunction()

function(rift_add_chat_plugin name)
    rift_plugin_target(${name} rift_plugin_chat_sdk
        "${RIFT_C_SDK_DIR}/chat/rift_plugin_component_type.o" ${ARGN})
    target_compile_definitions(${name} PRIVATE RIFT_CHAT)
endfunction()
