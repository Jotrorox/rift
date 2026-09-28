-- Run: rift --config examples/admin.lua
-- Admin: http://127.0.0.1:8080  Status: http://127.0.0.1:9090
-- Save & apply in the admin editor preserves this entire file, including hooks.
return {
    listeners = { public = "0.0.0.0:25565" },
    backends = {
        survival = "127.0.0.1:25566",
        lobby = "127.0.0.1:25567",
    },
    routes = {
        public = {
            ["survival.example.com"] = "survival",
            ["*"] = "lobby",
        },
    },
    fallbacks = { survival = { "lobby" } },
    health_check = { interval_ms = 5000, timeout_ms = 1000 },
    limits = { max_connections = 1024 },

    -- Set web=false to disable this server. ui/api are independent switches.
    -- A non-loopback bind requires a token of at least 16 printable characters.
    web = {
        listen = "127.0.0.1:8080",
        ui = true,
        api = true,
        -- token = "replace-with-a-long-random-secret",
    },
    -- A separate read-only server, with no configuration or extension routes.
    -- Set status=false to disable it, or metrics=false to disable its scraper.
    status = {
        listen = "127.0.0.1:9090",
        ui = true,
        metrics = true,
    },
    -- Optional separate Prometheus-only server; usually status.metrics is enough.
    metrics = false,

    -- All methods under /ext/* share web.token authentication when configured.
    -- Return nil for unhandled paths. Each request gets a fresh sandboxed VM.
    on_http = function(request)
        if request.method == "GET" and request.path == "/ext/network" then
            local lines = { "Rift " .. request.context.version }
            for _, backend in ipairs(request.context.backends) do
                table.insert(lines, backend.name .. " -> " .. backend.address
                    .. (backend.healthy and " (available)" or " (unhealthy)"))
            end
            return { content_type = "text/plain", body = table.concat(lines, "\n") }
        end
        if request.method == "GET" and request.path == "/ext/maintenance" then
            return {
                content_type = "text/html; charset=utf-8",
                body = "<!doctype html><html lang='en'><title>Network maintenance</title>"
                    .. "<h1>Network operations</h1><p>Deployments: weekdays at 06:00 UTC.</p></html>",
            }
        end
        if request.method == "POST" and request.path == "/ext/echo" then
            return { body = request.body }
        end
    end,
}
