-- rift.menus.on(id, handlers): the events of one menu, sorted by kind and,
-- for clicks, by button id (docs/menus.md). Plain Lua on top of
-- rift.on("menu", fn), the same in rift.lua and persistent Lua plugins.
local type, pairs, error, tostring = type, pairs, error, tostring
local rift = ...
local on = rift.on
local kinds = { click = true, text = true, close = true }

rift.menus = {
    on = function(id, handlers)
        if type(id) ~= "string" or type(handlers) ~= "table" then
            error("rift.menus.on expects a menu id and a table of handlers", 2)
        end
        for kind, handler in pairs(handlers) do
            if not kinds[kind] then
                error("rift.menus.on: unknown handler " .. tostring(kind) .. " (expected click, text or close)", 2)
            end
            if kind == "click" and type(handler) == "table" then
                for action, callback in pairs(handler) do
                    if type(action) ~= "string" or type(callback) ~= "function" then
                        error("rift.menus.on: click = { ... } maps button ids to functions", 2)
                    end
                end
            elseif type(handler) ~= "function" then
                error("rift.menus.on: " .. kind .. " must be a function", 2)
            end
        end
        on("menu", function(ctx)
            if ctx.menu ~= id then return end
            local handler = handlers[ctx.kind]
            if type(handler) == "table" then handler = handler[ctx.action] end
            if handler then handler(ctx) end
        end)
    end,
}
