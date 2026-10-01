-- Run: rift check examples/templates.lua
-- Install Java and put your jars, configs and map under examples/assets/.
-- Paths below are local and relative to this configuration file.
-- Rift does not download assets or accept the Minecraft EULA for you.
-- If you accept the EULA, supply your eula.txt in assets/configs/ or each instance.
-- Configure assets/configs/ for the authentication and forwarding settings below.
local config = require("rift.config")
config.listeners = { public = "0.0.0.0:25565" }
config.backends = {}
config.routes = { public = "survival" }
config.templates.paper = {
    server_jar = "assets/paper.jar", -- Copied to each instance as server.jar.
    plugins = { "assets/plugins/example.jar" }, -- Minecraft server plugin jars.
    configs = "assets/configs", -- Contents copied into the instance working root.
    map = "assets/maps/start-world", -- Contents copied into world/.
}
config.service_groups.survival = {
    template = "paper",
    storage = "persistent", -- Keep world/files on remove; reuse without reseeding.
    directory = "servers/{name}",
    command = { "java", "-Xms512M", "-Xmx1G", "-jar", "server.jar", "nogui" },
    port_range = { 25600, 25649 },
    autostart = false,
    start_on_connect = true,
    idle_timeout_ms = 5 * 60 * 1000,
}
config.service_groups.games = {
    template = "paper",
    storage = "disposable", -- Remove permanently deletes this generated directory.
    directory = "servers/{name}",
    command = { "java", "-Xms512M", "-Xmx1G", "-jar", "server.jar", "nogui" },
    port_range = { 25650, 25699 },
    autostart = false,
    start_on_connect = true,
    idle_timeout_ms = 5 * 60 * 1000,
}
config.authentication = { online_mode = true }
config.forwarding = { mode = "velocity", secret_env = "RIFT_FORWARDING_SECRET" }
config.admin = {
    listen = "127.0.0.1:9091",
    token_env = "RIFT_ADMIN_TOKEN",
    permissions = { "status", "servers", "drain", "transfer", "reload", "shutdown" },
}
config.web = { listen = "127.0.0.1:8080" }
-- Set RIFT_ADMIN_TOKEN and RIFT_FORWARDING_SECRET, then start Rift.
-- rift admin create survival
-- rift admin create games
-- rift admin groups
-- rift admin remove games-1     # Stops it and deletes its world and server files.
-- rift admin remove survival-1  # Stops it and retains its world and server files.
-- Stopping either instance preserves its files. Registration is runtime only;
-- after a proxy restart, `rift admin create survival` reuses survival-1 first.
