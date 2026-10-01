-- Host bootstrap. Capture primitives before configuration can replace globals.
local type, pairs, ipairs, error, tostring = type, pairs, ipairs, error, tostring
local api, runtime = rift, ...
local config = { listeners = {}, backends = {}, managed_servers = {}, routes = {}, limits = {} }
local handlers, commands, finished = {}, {}, false
local events = {
    route = "on_route", http = "on_http", message = "on_message",
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
    permissions = { "has", "set" },
    http = { "request" },
}) do
    local proxy = {}
    for _, method in ipairs(methods) do
        proxy[method] = function(...)
            local service = runtime[namespace]
            if not service then
                error("rift." .. namespace .. " is only available inside API v2 extension callbacks")
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

local function extensions()
    if config.extensions == nil then config.extensions = { api_version = 1 } end
    if type(config.extensions) ~= "table" then error("extensions must be a table") end
    if config.extensions.api_version == nil then config.extensions.api_version = 1 end
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
    for event, list in pairs(handlers) do
        local target = (event == "route" or event == "http" or event == "message") and config or extensions()
        local key = events[event]
        target[key] = compose(target[key], list, event)
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
