-- Run: rift check examples/nodes.lua
-- Game servers on more than one machine, managed from this proxy. Each extra
-- host runs `rift node`; this proxy stays the controller and decides what runs
-- where. See docs/nodes.md.
--
-- On node1 (10.0.0.11), as an unprivileged user:
--   RIFT_NODE_TOKEN=... rift node --controller 10.0.0.2:7443 --name node1 \
--     --root /srv/rift --ca /etc/rift/messaging-ca.pem \
--     --web http://10.0.0.2:8080 --allow-program java
local config = require("rift.config")
config.listeners = { public = "0.0.0.0:25565" }
config.routes = { public = "lobby" }
config.network = { hubs = { "lobby" } }
config.authentication = { online_mode = true }
config.forwarding = { mode = "velocity", secret_env = "RIFT_FORWARDING_SECRET" }

-- Agents connect to the message bus over QUIC with TLS. Each node gets its own
-- principal; its token is read from RIFT_NODE_<NAME>_TOKEN unless token_env is set.
config.messaging = {
    listen = "0.0.0.0:7443",
    certificate = "certs/messaging.pem",
    private_key = "certs/messaging-key.pem",
}
-- Agents download template folders from the web API with their node token.
-- Keep it on a private network or behind TLS.
config.web = { listen = "10.0.0.2:8080", token = "replace-with-a-long-random-secret" }

config.nodes.node1 = { address = "10.0.0.11", memory_mb = 32768, max_instances = 40, labels = { "games" } }
config.nodes.node2 = { address = "10.0.0.12", memory_mb = 16384, labels = { "games" } }

-- Template folders used on nodes must live in template_storage (local/templates).
config.templates.lobby = {
    software = { type = "paper", version = "latest" },
    configs = { "local/templates/Global/server", "local/templates/Lobby/default" },
    accept_eula = true, -- You accept https://aka.ms/MinecraftEULA.
}
config.templates.bedwars = {
    software = { type = "paper", version = "1.21.8" },
    configs = { "local/templates/Global/server", "local/templates/BedWars/default" },
    map = "local/templates/BedWars/map",
    accept_eula = true,
}

-- Groups without `nodes` run on this host, as before.
config.service_groups.lobby = {
    template = "lobby",
    storage = "disposable",
    port_range = { 25600, 25609 },
    scaling = { min_instances = 1, max_instances = 4, spare_instances = 1, capacity_per_instance = 50 },
}
-- Port ranges are per node: node1 and node2 can both use 30000.
config.service_groups.bedwars = {
    template = "bedwars",
    storage = "disposable",
    port_range = { 30000, 30099 },
    memory_mb = 2048,
    nodes = { labels = { "games" } },
    placement = "least_memory",
    scaling = { min_instances = 2, max_instances = 40, spare_instances = 1, capacity_per_instance = 16 },
}

-- A named server on a node. Its directory is relative to the agent's --root;
-- prepare it there (or through the dashboard's Files page) before starting.
config.backends.survival = "10.0.0.11:25570"
config.managed_servers.survival = {
    node = "node1",
    directory = "survival",
    command = { "java", "-Xmx4G", "-jar", "server.jar", "nogui" },
    start_on_connect = true,
}
