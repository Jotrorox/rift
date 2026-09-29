local config = require("rift.config")

config.listeners.public = "0.0.0.0:25565"
config.backends.lobby = "127.0.0.1:25566"
config.routes.public = "lobby"
config.limits.max_connections = 1024

-- Enable authentication and forwarding together for a public Paper network:
-- config.authentication = { online_mode = true }
-- config.forwarding = { mode = "velocity", secret_env = "RIFT_FORWARDING_SECRET" }
