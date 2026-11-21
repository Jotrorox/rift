-- The conformance guest for the persistent Lua engine. Its behaviour follows
-- the configuration keys documented in src/plugins/conformance.rs.
local name, config = ...
local subject = "conformance." .. name

local function report(text)
    rift.publish(subject, text)
end

local function tag(of)
    return of.tag ~= nil and tostring(of.tag) or "-"
end

report("init " .. tag(config))

local slow = {}
for _, hook in ipairs(config.slow or {}) do
    slow[hook] = true
end

-- Every handled hook reports, then waits, fails or traps as configured.
local function behave(hook)
    report(hook)
    if slow[hook] then
        local started = rift.clock()
        while rift.clock() - started < 0.3 do end
    end
    local failure = (config.fail or {})[hook]
    if failure == "handler" then
        error("database down")
    elseif failure == "fatal" then
        -- Exhausting the manifest's memory limit leaves the state unusable.
        local huge = string.rep("x", 256 * 1024 * 1024)
        return #huge
    end
end

local function decide(hook)
    local action = (config.decide or {})[hook]
    if action == nil or action == "continue" then
        return nil
    end
    return action
end

for _, hook in ipairs({ "login", "initial_server", "before_transfer" }) do
    rift.on(hook, function()
        behave(hook)
        return decide(hook)
    end)
end

for _, hook in ipairs({ "after_transfer", "disconnect" }) do
    rift.on(hook, function()
        behave(hook)
    end)
end

-- With `rich` (a chat component table), `join` sends it through every
-- player-facing text function and reports the results.
local function show(ok, result)
    if ok then
        return tostring(result)
    end
    return "error(" .. tostring(result) .. ")"
end

rift.on("join", function(ctx)
    behave("join")
    local rich = config.rich
    if rich ~= nil then
        local players = rift.players
        local message = show(pcall(players.message, ctx.uuid, rich))
        local action_bar = show(pcall(players.action_bar, ctx.uuid, rich))
        local broadcast = show(pcall(players.broadcast, rich))
        local kick = show(pcall(players.kick, ctx.uuid, rich))
        report("rich " .. message .. " " .. action_bar .. " " .. broadcast .. " " .. kick)
    end
    if config.menu then
        local chest = rift.players.menu(ctx.uuid, {
            id = "pick",
            rows = 1,
            title = "Pick",
            slots = {
                { slot = 0, item = "diamond", name = "Buy", id = "buy", payload = "gold", clicks = "all" },
            },
            state = 1,
        })
        local anvil = rift.players.menu(ctx.uuid, '{"id":"nick","type":"anvil","text":"Steve","typing":true}')
        local ok, err = pcall(rift.players.menu, ctx.uuid, { id = "bad", keys = 1 })
        local invalid = not ok and string.find(err, "keys must be an object", 1, true) ~= nil
        report("menu shown " .. tostring(chest) .. " " .. tostring(anvil) .. " " .. tostring(invalid))
    end
end)

-- With `menu`, every menu event reports its fields, and the guest patches
-- and closes its menus in answer.
if config.menu then
    local function field(value)
        if value == nil then
            return "-"
        end
        return tostring(value)
    end
    rift.on("menu", function(ctx)
        report(table.concat({
            "menu", ctx.menu, ctx.kind, field(ctx.action), field(ctx.slot), field(ctx.click),
            field(ctx.key), field(ctx.text), field(ctx.reason), field(ctx.payload), field(ctx.state),
        }, " "))
        if ctx.kind == "click" and ctx.action == "buy" then
            local updated = rift.players.update_menu(ctx.uuid, "pick", { title = "Bought", state = 2 })
            report("update " .. tostring(updated))
        elseif ctx.kind == "text" then
            report("close " .. tostring(rift.players.close_menu(ctx.uuid, "nick")))
        elseif ctx.kind == "close" and ctx.menu == "pick" then
            report("close any " .. tostring(rift.players.close_menu(ctx.uuid)))
        end
    end)
end

rift.on("route", function(connection)
    behave("route")
    local reject = config.route_reject
    if reject and connection.peer_ip == reject.ip then
        return { reject = true, reason = reject.reason }
    end
end)

rift.on("status", function()
    behave("status")
    if config.status_motd then
        return { motd = config.status_motd }
    end
end)

rift.on("http", function(request)
    behave("http")
    local http = config.http
    if http and request.path == http.path then
        return { status = 200, content_type = "text/plain", body = http.body }
    end
end)

rift.on("reload", function(new)
    config = new
    report("reload " .. tag(new))
end)

rift.on("shutdown", function(reason)
    report("shutdown " .. reason)
end)

for _, command in ipairs(config.commands or {}) do
    local suggest = command.suggest
    if suggest == "none" then
        suggest = nil
    elseif suggest == "dynamic" then
        suggest = function()
            return { "alpha", "beta", "gamma" }
        end
    end
    rift.command(command.name, {
        permission = command.permission,
        suggest = suggest,
        run = function(ctx)
            behave("command")
            if (config.decide or {}).command ~= nil then
                return decide("command")
            end
            return { message = "hi " .. ctx.args }
        end,
    })
end

-- With `chat`, the guest handles the chat hook: it reports every attempt and
-- answers `decide.chat` for text that contains `chat.block`.
if config.chat then
    rift.on("chat", function(ctx)
        report("chat " .. ctx.kind .. " " .. (ctx.command or "-") .. " " .. ctx.text)
        local block = config.chat.block
        if block and string.find(ctx.text, block, 1, true) then
            behave("chat")
            return decide("chat")
        end
    end)
end

for _, grant in ipairs(config.grant or {}) do
    rift.permissions.set(grant[1], grant[2], true)
end

if config.increment then
    rift.store.increment("counters", "hits", config.increment)
end

if config.timer_ms then
    rift.timers.every(config.timer_ms, function(handle)
        rift.timers.cancel(handle)
        report("timer " .. (rift.store.get("counters", "hits") or ""))
    end)
end

if config.subscribe then
    rift.subscribe(config.subscribe, function(message)
        report("message " .. message.payload)
    end)
end
