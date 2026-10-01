-- Run: rift check examples/managed.lua
-- Provision examples/servers/{lobby,survival}/paper.jar and configure Paper first.
-- See docs/managed-servers.md. Directories are relative to this configuration.
rift.config.listeners = { public = "0.0.0.0:25565" }
rift.config.backends = {
    lobby = "127.0.0.1:25566",
    survival = "127.0.0.1:25567",
}
rift.config.routes = { public = "lobby" }
rift.config.network = { hubs = { "lobby" } }
rift.config.fallbacks = { survival = { "lobby" } }

-- Configure the same Velocity secret on both Paper backends.
rift.config.authentication = { online_mode = true }
rift.config.forwarding = { mode = "velocity", secret_env = "RIFT_FORWARDING_SECRET" }

rift.config.managed_servers.lobby = {
    directory = "servers/lobby",
    command = { "java", "-Xms512M", "-Xmx1G", "-jar", "paper.jar", "nogui" },
    autostart = true,
}
rift.config.managed_servers.survival = {
    directory = "servers/survival",
    command = { "java", "-Xms512M", "-Xmx2G", "-jar", "paper.jar", "nogui" },
    start_on_connect = true,
    idle_timeout_ms = 5 * 60 * 1000,
    start_timeout_ms = 120000,
    stop_timeout_ms = 30000,
    restart_delay_ms = 5000,
}

-- Set RIFT_ADMIN_TOKEN in both Rift's and your administration shell's environment.
rift.config.admin = {
    listen = "127.0.0.1:9091",
    token_env = "RIFT_ADMIN_TOKEN",
    permissions = { "status", "servers", "drain", "transfer", "reload", "shutdown" },
}
-- Optional local dashboard with managed-server start/stop controls.
rift.config.web = { listen = "127.0.0.1:8080" }
