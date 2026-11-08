-- Host bootstrap. Capture primitives before configuration can replace globals.
local type, pairs, ipairs, error, tostring, next = type, pairs, ipairs, error, tostring, next
local api, runtime = rift, ...
local config = { listeners = {}, backends = {}, managed_servers = {}, templates = {}, service_groups = {}, nodes = {}, routes = {}, limits = {} }
local handlers, commands, finished = {}, {}, false
local jobs, subscriptions, subjects = {}, {}, {}
local events = {
    route = "on_route", http = "on_http", message = "on_message", status = "on_status",
    login = "login", initial_server = "initial_server", join = "join",
    disconnect = "disconnect", before_transfer = "before_transfer",
    after_transfer = "after_transfer",
}

api.config = config
api.enabled = false
-- Keep captured function references usable when Rust attaches the runtime broker.
api.publish = function(...) return runtime.publish(...) end

-- Captured tables/functions keep working once the host attaches v2 services.
-- Configuration evaluation (including rift check) never performs these effects.
for namespace, methods in pairs({
    store = { "get", "set", "delete", "increment" },
    permissions = { "has", "set", "add_group", "remove_group", "groups" },
    http = { "request" },
    players = { "list", "get", "names", "message", "action_bar", "title", "tab_list", "boss_bar", "kick", "transfer", "broadcast" },
    servers = { "create", "remove", "start", "stop", "download" },
}) do
    local proxy = {}
    local scope = namespace == "players" and "API v2 extension callbacks, on_message and on_http"
        or namespace == "servers" and "callbacks such as on_message, on_http and extensions"
        or "API v2 extension callbacks"
    for _, method in ipairs(methods) do
        proxy[method] = function(...)
            local service = runtime[namespace]
            if not service then
                error("rift." .. namespace .. " is only available inside " .. scope)
            end
            return service[method](...)
        end
    end
    api[namespace] = proxy
end

local function configuring()
    if finished then error("configuration registration is only available during initialization") end
    if api.config ~= config then error("assign fields on rift.config instead of replacing it") end
end

function api.setup(values)
    configuring()
    if type(values) ~= "table" then error("rift.setup expects a table") end
    for key, value in pairs(values) do config[key] = value end
end

function api.on(event, callback)
    configuring()
    if not events[event] then error("unknown Rift event: " .. tostring(event)) end
    if type(callback) ~= "function" then error("rift.on expects a function") end
    local list = handlers[event] or {}
    list[#list + 1] = callback
    handlers[event] = list
end

function api.command(name, definition)
    configuring()
    if type(name) ~= "string" or type(definition) ~= "table" then
        error("rift.command expects a name and { permission = ..., run = function(ctx) ... end }")
    end
    if commands[name] then error("duplicate Rift command: " .. name) end
    commands[name] = definition
end

-- rift.every(name, every_ms, function(ctx) ... end): an extensions.jobs
-- entry (fixed delay, first run after one interval). Implies api_version = 2.
function api.every(name, every_ms, callback)
    configuring()
    if type(name) ~= "string" or type(callback) ~= "function" then
        error("rift.every expects a job name, an interval in milliseconds and a function")
    end
    if jobs[name] then error("duplicate rift.every job: " .. name) end
    jobs[name] = { every_ms = every_ms, run = callback }
end

-- rift.subscribe(subject, [queue,] function(message) ... end): a
-- messaging.subscriptions entry whose messages go to this function instead
-- of on_message.
function api.subscribe(subject, queue, callback)
    configuring()
    if callback == nil then callback, queue = queue, nil end
    if type(subject) ~= "string" or (queue ~= nil and type(queue) ~= "string")
        or type(callback) ~= "function" then
        error("rift.subscribe expects a subject, an optional queue group and a function")
    end
    if subscriptions[subject] then error("duplicate rift.subscribe subject: " .. subject) end
    subscriptions[subject] = callback
    subjects[#subjects + 1] = { subject = subject, queue = queue }
end

local function extensions(version)
    if config.extensions == nil then config.extensions = { api_version = version or 1 } end
    if type(config.extensions) ~= "table" then error("extensions must be a table") end
    if config.extensions.api_version == nil then config.extensions.api_version = version or 1 end
    return config.extensions
end

local function compose(previous, list, event)
    if previous ~= nil and type(previous) ~= "function" then
        error("configured " .. events[event] .. " must be a function")
    end
    return function(ctx)
        if previous then
            local result = previous(ctx)
            if event ~= "message" and result ~= nil then return result end
        end
        for _, callback in ipairs(list) do
            local result = callback(ctx)
            if event ~= "message" and result ~= nil then return result end
        end
    end
end

return function(returned)
    configuring()
    if returned ~= nil then
        if type(returned) ~= "table" then error("configuration: expected a table or a script using rift.config") end
        api.setup(returned)
    end
    -- rift.config.plugins is another name for rift.config.extensions.
    if config.plugins ~= nil then
        if config.extensions ~= nil then error("set rift.config.plugins or rift.config.extensions, not both") end
        config.extensions, config.plugins = config.plugins, nil
    end
    if next(jobs) ~= nil then
        local ext = extensions(2)
        if ext.jobs == nil then ext.jobs = {} end
        if type(ext.jobs) ~= "table" then error("extensions.jobs must be a table") end
        for name, job in pairs(jobs) do
            if ext.jobs[name] ~= nil then error("duplicate rift.every job: " .. name) end
            ext.jobs[name] = job
        end
    end
    if #subjects > 0 then
        if config.messaging == nil then config.messaging = {} end
        if type(config.messaging) ~= "table" then error("messaging must be a table") end
        local list = config.messaging.subscriptions
        if list == nil then list = {} config.messaging.subscriptions = list end
        if type(list) ~= "table" then error("messaging.subscriptions must be a list") end
        for _, entry in ipairs(subjects) do list[#list + 1] = entry end
    end
    for event, list in pairs(handlers) do
        local target = (event == "route" or event == "http" or event == "message" or event == "status")
            and config or extensions()
        local key = events[event]
        target[key] = compose(target[key], list, event)
    end
    if #subjects > 0 then
        local previous = config.on_message
        if previous ~= nil and type(previous) ~= "function" then
            error("configured on_message must be a function")
        end
        config.on_message = function(message)
            local callback = subscriptions[message.subscription]
            if callback then return callback(message) end
            if previous then return previous(message) end
        end
    end
    for name, definition in pairs(commands) do
        local ext = extensions()
        if ext.commands == nil then ext.commands = {} end
        if type(ext.commands) ~= "table" then error("extensions.commands must be a table") end
        if ext.commands[name] ~= nil then error("duplicate Rift command: " .. name) end
        ext.commands[name] = definition
    end
    finished = true
    return config
end
