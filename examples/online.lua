-- Authenticated Java 1.21.11 network with two Paper servers.
-- Start: RIFT_FORWARDING_SECRET='<your existing Velocity secret>' rift --config examples/online.lua
-- Keep backend ports private. On BOTH servers:
--   server.properties: online-mode=false
--   spigot.yml: settings.bungeecord=false
--   config/paper-global.yml: proxies.velocity.enabled=true,
--     proxies.velocity.online-mode=true, proxies.velocity.secret=<matching secret>
-- Restart Paper after editing. Existing UUID-based player data and skins persist.
return {
    listeners = { public = "0.0.0.0:25565" },
    backends = {
        lobby = "127.0.0.1:25566",
        survival = "127.0.0.1:25567",
    },
    routes = { public = "lobby" },
    authentication = { online_mode = true, timeout_ms = 10000 },
    forwarding = { mode = "velocity", secret_env = "RIFT_FORWARDING_SECRET" },
    network = {
        initial = { "lobby" },
        hubs = { "lobby" },
    },
    fallbacks = { survival = { "lobby" } },
    login_rate_limit = { per_ip_per_second = 5, per_ip_burst = 10 },
    limits = { connect_timeout_ms = 5000 },
}
