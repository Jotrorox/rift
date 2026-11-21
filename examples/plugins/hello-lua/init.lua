-- hello-lua: the Lua example plugin, behaving exactly like hello-rs, hello-c
-- and hello-go (see examples/plugins/README.md). Each handler body except
-- the hello command's, which sends a message of the plugin's own, would work
-- unchanged in rift.lua.
local _, config = ...
local deny = config.deny

rift.on("login", function(ctx)
    if ctx.name == deny then
        return { deny = "You are not welcome here." }
    end
end)

rift.command("hello", {
    permission = "hello.use",
    suggest = "players",
    run = function(ctx)
        -- The text lives in messages.toml, translated per player.
        return { message = rift.text.message("greeting", { name = ctx.name }) }
    end,
})

-- The server selector: a chest of three rows framed in gray glass, with a
-- button per server (action `join`, the server's name as payload) and one
-- that closes the menu.
local function server(item, name, target)
    return { item = item, name = { mini = name }, lore = { "Click to join" }, id = "join", payload = target }
end

local selector = {
    id = "servers",
    title = { mini = "<dark_gray>Choose a server" },
    layout = { "#########", "# S C M #", "####X####" },
    keys = {
        ["#"] = { item = "gray_stained_glass_pane", name = " " },
        S = server("grass_block", "<green>Survival", "survival"),
        C = server("bricks", "<gold>Creative", "creative"),
        M = server("bow", "<red>Minigames", "minigames"),
        X = { item = "barrier", name = { mini = "<red>Close" }, close = true },
    },
}

rift.command("servers", {
    permission = "", -- every player
    run = function(ctx)
        rift.try(rift.players.menu, ctx.uuid, selector)
    end,
})

rift.menus.on("servers", {
    click = {
        join = function(ctx)
            rift.try(rift.players.transfer, ctx.uuid, ctx.payload)
        end,
    },
})

rift.on("join", function(ctx)
    -- Host errors are ignored.
    rift.try(rift.players.message, ctx.uuid, "Welcome, " .. ctx.name .. "!")
    rift.try(rift.store.increment, "hello", "joins", 1)
end)

rift.every("tick", 1000, function()
    rift.log("debug", "tick")
end)

rift.subscribe("hello.ping", function(message)
    if message.reply then
        rift.try(rift.publish, message.reply, message.payload)
    end
end)
