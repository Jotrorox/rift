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

function(rift_add_plugin name)
    add_executable(${name} ${ARGN}
        $<TARGET_OBJECTS:rift_plugin_sdk>
        "${RIFT_C_SDK_DIR}/rift_plugin_component_type.o")
    target_include_directories(${name} PRIVATE "${RIFT_C_SDK_DIR}")
    set_target_properties(${name} PROPERTIES SUFFIX ".wasm")
    # A reactor has no `main`. wasi-libc ships with DWARF; keep it only for
    # debug builds.
    target_link_options(${name} PRIVATE -mexec-model=reactor
        $<$<NOT:$<CONFIG:Debug>>:-Wl,--strip-debug>)
endfunction()
