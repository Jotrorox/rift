-- Local Lua plugins work without a QUIC listener. See docs/messaging.md for TLS
-- and per-plugin credentials when accepting external server plugins.
return {
    listeners = { public = "0.0.0.0:25565" },
    backends = { lobby = "127.0.0.1:25566" },
    routes = { public = "lobby" },
    messaging = {
        subscription_capacity = 1024,
        subscriptions = {
            { subject = "plugins.echo", queue = "echo-workers" },
        },
        -- Optional QUIC listener (UDP), disabled until these fields are supplied:
        -- listen = "127.0.0.1:4222",
        -- certificate = "messaging-cert.pem",
        -- private_key = "messaging-key.pem",
        -- principals = {
        --     { name = "paper", token_env = "RIFT_PAPER_TOKEN",
        --       publish = { "plugins.>", "_INBOX.>" },
        --       subscribe = { "plugins.>", "rift.events.>" } },
        --     { name = "operator", token_env = "RIFT_OPERATOR_TOKEN",
        --       publish = { "rift.control.*" }, subscribe = { "rift.events.>" },
        --       control = true },
        -- },
        streams = {
            jobs = {
                subjects = { "plugins.jobs.>" },
                max_messages = 100000,
                max_bytes = 64 * 1024 * 1024,
                -- storage_path = "data/jobs", -- Omit for memory-only retention.
            },
        },
    },
    on_message = function(message)
        if message.subject == "plugins.echo" and message.reply then
            rift.publish(message.reply, message.payload)
        end
    end,
}
