-- Run: rift check examples/cloud.lua
-- A CloudNet-style network: Rift downloads the server software, assembles each
-- instance from template folders and runs it. Install Java 21 or newer first.
--
--   local/templates/Global/server   shared files for every server (optional)
--   local/templates/Lobby/default   lobby files, editable on the dashboard's Files page
--   local/versioncache/             downloaded server jars
--   local/services/{name}           persistent instances (survival)
--   temp/services/{name}            disposable instances (lobby, games)
local config = require("rift.config")
config.listeners = { public = "0.0.0.0:25565" }
config.backends = {}
config.routes = { public = "lobby" }
config.network = { hubs = { "lobby" } }

-- Rift authenticates players and configures Velocity forwarding on new
-- Paper instances with this secret.
config.authentication = { online_mode = true }
config.forwarding = { mode = "velocity", secret_env = "RIFT_FORWARDING_SECRET" }

config.templates.lobby = {
    software = { type = "paper", version = "latest" },
    configs = { "local/templates/Global/server", "local/templates/Lobby/default" },
    accept_eula = true, -- You accept https://aka.ms/MinecraftEULA.
}
config.templates.survival = {
    software = { type = "paper", version = "1.21.8" },
    configs = { "local/templates/Global/server" },
    accept_eula = true,
}

config.service_groups.lobby = {
    template = "lobby",
    storage = "disposable",
    port_range = { 25600, 25649 },
    memory_mb = 1024,
    scaling = { min_instances = 1, max_instances = 4, spare_instances = 1, capacity_per_instance = 50 },
}
config.service_groups.survival = {
    template = "survival",
    storage = "persistent",
    port_range = { 25650, 25659 },
    memory_mb = 4096,
    start_on_connect = true,
    idle_timeout_ms = 10 * 60 * 1000,
}

config.web = { listen = "127.0.0.1:8080" } -- Dashboard: groups, templates, files.
config.admin = {
    listen = "127.0.0.1:9091",
    token_env = "RIFT_ADMIN_TOKEN",
    permissions = { "status", "servers", "drain", "transfer", "reload", "shutdown" },
}

-- Plugins can follow capacity on the message bus and spin up servers.
config.messaging = { subscriptions = { { subject = "rift.events.instances" } } }
rift.on("message", function(message)
    -- {"event":"created","name":"lobby-2","group":"lobby","port":25601,...}
    rift.publish("network.capacity", message.payload)
end)
rift.on("http", function(request)
    if request.path == "/ext/survival/new" and request.method == "POST" then
        return { status = 202, body = rift.servers.create("survival") }
    end
end)

-- Set RIFT_FORWARDING_SECRET and RIFT_ADMIN_TOKEN, start Rift, then:
-- rift admin templates            # Software and download cache state.
-- rift admin download survival    # Optional: fetch the jar before first use.
-- rift admin create survival      # survival-1 in local/services/survival-1.
