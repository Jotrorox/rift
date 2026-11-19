-- A cluster of three proxies, eu-1, eu-2 and eu-3, that all accept players.
-- Every peer runs this same file; see docs/clustering.md.
--
-- Each host keeps this file and its own cluster.toml (eu-1.toml, eu-2.toml or
-- eu-3.toml here, copied as cluster.toml beside rift.lua) in /var/lib/rift,
-- which rift-cluster.service uses. Check it on a peer with:
--   rift check /var/lib/rift/rift.lua            (cluster.toml beside it)
--   rift check --strict /var/lib/rift/rift.lua   (also asks the running cluster)
--
-- Set up once, on eu-1, then on any member:
--   rift cluster init --peer eu-1
--   rift admin cluster add eu-2 10.0.0.2:7444 && rift admin cluster promote eu-2
--   rift admin cluster add eu-3 10.0.0.3:7444 && rift admin cluster promote eu-3
-- eu-2 and eu-3 start without a rift.lua and receive this file from the cluster.
-- Later edits are deployments: change it on one peer (or in the dashboard) and
-- reload; every peer writes the new revision to its rift.lua and reloads.

-- rift.peer is this peer's name. It is nil while `rift cluster init` reads
-- this file and outside a cluster, hence the fallback.
local hosts = {
    ["eu-1"] = "10.0.0.1",
    ["eu-2"] = "10.0.0.2",
    ["eu-3"] = "10.0.0.3",
}
local host = hosts[rift.peer] or hosts["eu-1"]

-- Shared by every peer. Changes take effect on each peer when it restarts.
rift.config.cluster = {
    peer_timeout_ms = 10000,
    partition_grace_ms = 60000,
    max_players = 5000,
}

-- Peer sections: listeners, routes, limits, health checks, the server list,
-- the tab list and the web listener may differ per peer through rift.peer.
rift.config.listeners = { public = "0.0.0.0:25565" }
rift.config.routes = { public = "lobby" }
rift.config.metrics = host .. ":9090"
-- Each peer serves the dashboard and the HTTP API on its own address, over
-- HTTPS with the messaging certificate (each host's names that host).
rift.config.web = { listen = host .. ":8443", token_env = "RIFT_WEB_TOKEN", tls = true }
-- The admin endpoint is loopback only, so every host can use the same port.
rift.config.admin = {
    listen = "127.0.0.1:9091",
    token_env = "RIFT_ADMIN_TOKEN",
    permissions = { "status", "servers", "maintenance", "drain", "transfer", "reload", "shutdown", "cluster" },
}

-- Everything below is shared: it must come out the same on every peer, or
-- the deployment is refused.
rift.config.authentication = { online_mode = true }
-- RIFT_FORWARDING_SECRET must be the same on every peer; the peers check.
rift.config.forwarding = { mode = "velocity", secret_env = "RIFT_FORWARDING_SECRET" }
rift.config.network = { hubs = { "lobby" } }

-- Bus clients and the servers' bus logins reach any peer. The messaging
-- section is shared, so advertise a name that resolves to every peer; each
-- host's certificate at this path names that name and the host itself.
rift.config.messaging = {
    listen = "0.0.0.0:7443",
    advertise = "bus.eu.internal:7443",
    certificate = "/etc/rift/messaging.pem",
    private_key = "/etc/rift/messaging-key.pem",
}

rift.config.templates.lobby = {
    software = { type = "paper", version = "latest" },
    configs = { "local/templates/Global/server", "local/templates/Lobby/default" },
    accept_eula = true, -- You accept https://aka.ms/MinecraftEULA.
}

-- Every peer is also a node named after itself. [labels] region = "eu" in
-- each cluster.toml gives its node the labels "region" and "eu".
rift.config.service_groups.lobby = {
    template = "lobby",
    storage = "disposable",
    port_range = { 25600, 25609 },
    memory_mb = 1024,
    nodes = { labels = { "eu" } },
    placement = "least_memory",
    scaling = { min_instances = 3, max_instances = 9, spare_instances = 1, capacity_per_instance = 50 },
}

-- A named server on one peer's host, below that peer's /var/lib/rift.
rift.config.backends.survival = "10.0.0.1:25570"
rift.config.managed_servers.survival = {
    node = "eu-1",
    directory = "survival",
    command = { "java", "-Xmx4G", "-jar", "server.jar", "nogui" },
    start_on_connect = true,
}
