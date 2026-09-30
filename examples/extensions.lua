-- Authenticated network extensions v2: durable state, jobs and live permissions.
-- Use the Paper/Velocity setup documented in examples/online.lua.
-- RIFT_FORWARDING_SECRET='<shared secret>' rift --config examples/extensions.lua
-- Validate without credentials or backend servers: rift --check examples/extensions.lua
-- Replace the example UUID below with a real lowercase UUID without hyphens.
local staff_uuid = "0123456789abcdef0123456789abcdef"
-- Set true after starting an HTTP permission service on 127.0.0.1:8080.
-- GET /permissions/<uuid> should return HTTP 200 with exactly "allow" or "deny".
local refresh_staff_permissions = false

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
    api_version = 2,
    -- Relative to this Lua file; use a writable location for production.
    -- Omit storage for process-local state that survives callbacks and reloads.
    storage = { path = "extensions.state" },
    permissions = {
        ["*"] = { ["network.queue"] = true },
        [staff_uuid] = { ["network.staff"] = true },
    },
    -- Includes players joining or transferring; all entry paths reserve a
    -- slot. Set this to 1 to try the queue with two authenticated players.
    queues = { survival = 100 },
    jobs = {
        heartbeat = {
            every_ms = 60000,
            run = function(ctx)
                local ticks = rift.store.increment("example", "heartbeats", 1)
                rift.publish("example.jobs." .. ctx.job, tostring(ticks))
            end,
        },
    },

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
        rift.store.increment("visits", ctx.uuid, 1)
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

-- External work runs in a bounded background job. Permission updates apply to
-- connected players' next command/transfer check without reconnecting.
if refresh_staff_permissions then
    rift.config.extensions.integrations = {
        directory = { url = "http://127.0.0.1:8080/permissions/", timeout_ms = 1000 },
    }
    rift.config.extensions.jobs.staff_permissions = {
        every_ms = 30000,
        run = function()
            rift.permissions.set(staff_uuid, "network.staff", false)
            local response = rift.http.request("directory", { path = staff_uuid })
            if response.status == 200 and response.body == "allow" then
                rift.permissions.set(staff_uuid, "network.staff", true)
            end
        end,
    }
end
