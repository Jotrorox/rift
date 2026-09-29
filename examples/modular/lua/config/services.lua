local config = require("rift.config")

config.web = { listen = "127.0.0.1:8080" }
config.messaging = {
    subscriptions = { { subject = "plugins.echo", queue = "echo-workers" } },
}
