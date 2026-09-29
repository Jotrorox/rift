-- Run: rift --config examples/two-server.lua
-- Configure both backends with online-mode=false and enforce-secure-profile=false.
-- Block direct public access to backend ports: this proxy uses offline identities.
rift.config.listeners = { public = "0.0.0.0:25565" }
rift.config.backends = {
    lobby = "127.0.0.1:25566",
    survival = "127.0.0.1:25567",
}
rift.config.routes = { public = "lobby" }
rift.config.network = {
    -- Ordered initial login destinations for the default direct route.
    -- Hostname routes and explicit on_route selections keep their own targets.
    initial = { "lobby" },
    -- Ordered destinations for /hub and recovery from a backend failure.
    hubs = { "lobby" },
    access = {
        -- Omitted backends are public. Names match without case; deny wins.
        -- survival = { allow = { "Alice", "Bob" }, deny = { "Bob" } },
        -- survival = { allow = {} }, -- nobody may enter
    },
}
rift.config.fallbacks = { survival = { "lobby" } }
rift.config.limits = { connect_timeout_ms = 5000 }
