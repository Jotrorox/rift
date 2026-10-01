-- Run: rift check examples/services.lua
-- Automatic capacity: prepare the jar, EULA and backend settings before starting Rift.
-- For asset copying and disposable games, see examples/templates.lua.
-- Replace /absolute/paper.jar and configure authentication/forwarding on Paper.
local config = require("rift.config")
config.listeners = { public = "0.0.0.0:25565" }
config.backends = {}
config.routes = { public = "lobby" }
config.service_groups.lobby = {
    directory = "servers/{name}",
    command = { "java", "-Xms512M", "-Xmx1G", "-jar", "/absolute/paper.jar", "--port", "{port}", "nogui" },
    port_range = { 25600, 25700 },
    storage = "persistent", -- Removal retains world and server files.
    start_on_connect = true,
    start_timeout_ms = 120000,
    stop_timeout_ms = 30000,
    restart_delay_ms = 5000,
    restart_retries = 3,
    scaling = {
        min_instances = 1,
        max_instances = 8,
        spare_instances = 1,
        capacity_per_instance = 50,
        target_occupancy_percent = 80,
        queue_threshold = 4,
        cooldown_ms = 5000,
    },
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
-- rift admin groups
-- rift admin servers
-- rift admin start lobby-1  -- Resets an exhausted recovery budget after a fix.
