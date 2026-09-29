-- Run: rift --config examples/network.lua
-- Validate edits: rift --check examples/network.lua
-- Reload with SIGHUP (Unix) or Ctrl-Break (Windows).
-- Listener names/addresses and the metrics address require a restart.
rift.config.listeners = {
    public = "0.0.0.0:25565",
}
rift.config.backends = {
    survival = "127.0.0.1:25566",
    lobby = "127.0.0.1:25567",
}
rift.config.routes = {
    public = {
        ["play.example.com"] = "survival",
        ["lobby.example.com"] = "lobby",
        ["*"] = "survival",
    },
}
rift.config.fallbacks = {
    survival = { "lobby" },
}
rift.config.limits = {
    max_connections = 1024,
    connect_timeout_ms = 5000,
    buffer_size = 32 * 1024,
}
rift.config.rate_limit = {
    per_ip_per_second = 20,
    per_ip_burst = 40,
    global_per_second = 200,
    global_burst = 400,
    max_ips = 65536,
}
rift.config.health_check = {
    interval_ms = 5000,
    timeout_ms = 1000,
    unhealthy_threshold = 2,
    healthy_threshold = 1,
}
rift.config.status_cache = {
    ttl_ms = 1000,
    max_entries = 1024,
    max_response_bytes = 65536,
}
rift.config.metrics = "127.0.0.1:9090"
rift.config.shutdown_timeout_ms = 30000
