-- Host bootstrap. Capture primitives before configuration can replace globals.
local type, pairs, ipairs, next, error, tostring = type, pairs, ipairs, next, error, tostring
local api, runtime = rift, ...
local config = { listeners = {}, backends = {}, managed_servers = {}, templates = {}, service_groups = {}, nodes = {}, routes = {}, limits = {} }
local handlers, commands, finished = {}, {}, false
local permission_handlers, context_providers = {}, {}
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
    permissions = { "check", "compiled", "meta", "user", "group", "groups", "track" },
    http = { "request" },
    players = { "list", "get", "names", "message", "action_bar", "title", "tab_list", "boss_bar", "kick", "transfer", "broadcast" },
    servers = { "create", "remove", "start", "stop", "download" },
}) do
    local proxy = {}
    local scope = namespace == "players" and "API v2 extension callbacks, on_message and on_http"
        or namespace == "servers" and "callbacks such as on_message, on_http and extensions"
        or namespace == "permissions" and "API v3 extension callbacks"
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

-- The API v2 permission functions were removed with extensions API v3.
for _, method in ipairs({ "has", "set", "add_group", "remove_group" }) do
    api.permissions[method] = function()
        error("rift.permissions." .. method .. " was removed; use rift.permissions.check, "
            .. "user(uuid):set and user(uuid):add_parent with extensions.api_version = 3 (docs/permissions.md)")
    end
end

-- Registrations made while the configuration is evaluated, like rift.on.
function api.permissions.on_change(callback)
    configuring()
    if type(callback) ~= "function" then error("rift.permissions.on_change expects a function") end
    permission_handlers[#permission_handlers + 1] = callback
end

api.permissions.contexts = {
    provide = function(key, callback)
        configuring()
        if type(key) ~= "string" or type(callback) ~= "function" then
            error("rift.permissions.contexts.provide expects a context key and a function")
        end
        if context_providers[key] then error("duplicate context provider: " .. key) end
        context_providers[key] = callback
    end,
}

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
        local target = (event == "route" or event == "http" or event == "message" or event == "status")
            and config or extensions()
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
    if #permission_handlers > 0 then
        local ext = extensions()
        local previous = ext.on_permission_change
        if previous ~= nil and type(previous) ~= "function" then
            error("extensions.on_permission_change must be a function")
        end
        ext.on_permission_change = function(event)
            if previous then previous(event) end
            for _, callback in ipairs(permission_handlers) do callback(event) end
        end
    end
    if next(context_providers) ~= nil then
        local ext = extensions()
        if ext.context_providers == nil then ext.context_providers = {} end
        if type(ext.context_providers) ~= "table" then error("extensions.context_providers must be a table") end
        for key, callback in pairs(context_providers) do
            if ext.context_providers[key] ~= nil then error("duplicate context provider: " .. key) end
            ext.context_providers[key] = callback
        end
    end
    finished = true
    return config
end
