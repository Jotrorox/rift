-- Copy to ./rift.lua, or run: rift --config examples/rift.lua
-- Routes map listener names to backend names. Send SIGHUP (Unix) to reload.
-- See routing.lua for an on_route hook and network.lua for operational settings.
return {
    listeners = {
        public = "0.0.0.0:25565",
    },
    backends = {
        lobby = "127.0.0.1:25566",
    },
    routes = {
        public = "lobby",
    },
    limits = {
        -- 64 MiB of relay buffers at capacity; budget extra for sockets/runtime.
        max_connections = 1024,
        connect_timeout_ms = 5000,
        buffer_size = 32 * 1024,
    },
}
