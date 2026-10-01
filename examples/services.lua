-- Run: rift check examples/services.lua
-- Prepare examples/servers/lobby-1, lobby-2, etc. with EULA/config/plugins first.
-- Replace /absolute/paper.jar and configure authentication/forwarding on Paper.
local config = require("rift.config")
config.listeners = { public = "0.0.0.0:25565" }
config.backends = {}
config.routes = { public = "lobby" }
config.service_groups.lobby = {
    directory = "servers/{name}",
    command = { "java", "-Xms512M", "-Xmx1G", "-jar", "/absolute/paper.jar", "--port", "{port}", "nogui" },
    port_range = { 25600, 25700 },
    start_on_connect = true,
    idle_timeout_ms = 5 * 60 * 1000,
    start_timeout_ms = 120000,
    stop_timeout_ms = 30000,
    restart_delay_ms = 5000,
}
config.authentication = { online_mode = true }
config.forwarding = { mode = "velocity", secret_env = "RIFT_FORWARDING_SECRET" }
config.admin = {
    listen = "127.0.0.1:9091",
    token_env = "RIFT_ADMIN_TOKEN",
    permissions = { "status", "servers", "drain", "transfer", "reload", "shutdown" },
}
config.web = { listen = "127.0.0.1:8080" }
-- Set RIFT_ADMIN_TOKEN, start Rift, then use:
-- rift admin create lobby
-- rift admin create lobby
-- rift admin groups
-- rift admin remove lobby-2
