-- Copy to ./rift.lua, or run: rift --config examples/rift.lua
-- Routes map listener names to backend names. This config runs only at startup.
-- See routing.lua for an optional per-connection on_route hook.
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
        max_connections = 4096,
        connect_timeout_ms = 5000,
        buffer_size = 32 * 1024,
    },
}
