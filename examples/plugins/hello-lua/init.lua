-- hello-lua: the Lua example plugin, behaving exactly like hello-rs, hello-c
-- and hello-go (see examples/plugins/README.md). Each handler body would
-- work unchanged in rift.lua.
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
        return { message = "Hello, " .. ctx.name .. "!" }
    end,
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
