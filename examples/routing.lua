-- Run: rift --config examples/routing.lua
-- This initialization runs in a fresh Lua state for every routing call.
local maintenance = false

return {
    listeners = {
        public = "0.0.0.0:25565",
        creative = "0.0.0.0:25567",
    },
    backends = {
        lobby = "127.0.0.1:25566",
        creative = "127.0.0.1:25568",
    },
    routes = {
        public = "lobby",
        creative = "lobby",
    },
    on_route = function(connection)
        if maintenance then
            return { reject = true, reason = "Maintenance" }
        end
        if connection.listener == "creative" then
            return { backend = "creative" }
        end
        return nil -- Keep the configured default backend.
    end,
}
