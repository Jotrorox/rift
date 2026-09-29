local rift = require("rift")

rift.on("message", function(message)
    if message.subject == "plugins.echo" and message.reply then
        rift.publish(message.reply, message.payload)
    end
end)
