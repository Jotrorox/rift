-- Authenticated network extensions v1: FIFO survival queue and staff routing.
-- Use the Paper/Velocity setup documented in examples/online.lua.
-- RIFT_FORWARDING_SECRET='<shared secret>' rift --config examples/extensions.lua
-- Validate without credentials or backend servers: rift --check examples/extensions.lua
-- Replace the example UUID below with a real lowercase UUID without hyphens.
local staff_uuid = "0123456789abcdef0123456789abcdef"

local function may_enter(ctx, server)
    return server ~= "staff" or ctx.has_permission("network.staff")
end

rift.config.listeners = { public = "0.0.0.0:25565" }
rift.config.backends = {
    lobby = "127.0.0.1:25566",
    survival = "127.0.0.1:25567",
    staff = "127.0.0.1:25568",
}
rift.config.routes = { public = "lobby" }
rift.config.authentication = { online_mode = true, timeout_ms = 10000 }
rift.config.forwarding = { mode = "velocity", secret_env = "RIFT_FORWARDING_SECRET" }
rift.config.network = { hubs = { "lobby" } }
rift.config.fallbacks = { survival = { "lobby" }, staff = { "lobby" } }

rift.config.extensions = {
    api_version = 1,
    permissions = {
        ["*"] = { ["network.queue"] = true },
        [staff_uuid] = { ["network.staff"] = true },
    },
    -- Includes players joining or transferring; all entry paths reserve a
    -- slot. Set this to 1 to try the queue with two authenticated players.
    queues = { survival = 100 },

    login = function(ctx)
        if ctx.name == "ExampleBlockedAccount" then
            return { deny = "This account cannot join this network." }
        end
    end,
    initial_server = function(ctx)
        if ctx.hostname == "staff.example.com" then
            if not may_enter(ctx, "staff") then
                return { deny = "Staff access is required." }
            end
            return { server = "staff" }
        end
        return { server = "lobby" }
    end,
    before_transfer = function(ctx)
        -- Applies to /server, /staff, queues, administrator transfers and
        -- outage recovery. Command permissions alone do not protect routes.
        if not may_enter(ctx, ctx.target) then
            return { deny = "Staff access is required." }
        end
    end,
    commands = {
        queue = {
            permission = "network.queue",
            run = function(ctx)
                if ctx.args == "leave" then return { leave_queue = true } end
                if ctx.args == "" or ctx.args == "survival" then
                    return { queue = "survival" }
                end
                return { message = "Usage: /queue [survival|leave]" }
            end,
        },
        staff = {
            permission = "network.staff",
            run = function(ctx) return { server = "staff" } end,
        },
    },
    -- Optional observations through the existing in-process message broker.
    -- Subscribe with a messaging consumer to capture these events.
    join = function(ctx)
        rift.publish("example.players.join", ctx.uuid .. " " .. ctx.server)
    end,
    after_transfer = function(ctx)
        rift.publish("example.players.transfer",
            ctx.uuid .. " " .. ctx.target .. " " .. tostring(ctx.success))
    end,
    disconnect = function(ctx)
        rift.publish("example.players.disconnect", ctx.uuid)
    end,
}
