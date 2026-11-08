# Alternative to the Makefile:
#
#   cmake -B build -DCMAKE_BUILD_TYPE=Release \
#     -DCMAKE_TOOLCHAIN_FILE=$WASI_SDK_PATH/share/cmake/wasi-sdk-p2.cmake
#   cmake --build build && cp build/hello_c.wasm .
cmake_minimum_required(VERSION 3.20)
project(hello_c C)

include(../../../sdk/c/RiftPlugin.cmake)
rift_add_plugin(hello_c hello.c)
