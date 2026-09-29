local rift = require("rift")
local greeting = require("greeting")

rift.on("http", function(request)
    if request.method == "GET" and request.path == "/ext/hello" then
        return { body = greeting.text() }
    end
end)

-- Authenticated commands use the same module. Enable online authentication in
-- lua/config/network.lua before enabling this registration and permission grant:
-- rift.config.extensions = {
--     api_version = 1,
--     permissions = { ["*"] = { ["greeting.hello"] = true } },
-- }
-- rift.command("hello", {
--     permission = "greeting.hello",
--     run = function(ctx) return { message = greeting.text() .. " " .. ctx.name } end,
-- })
