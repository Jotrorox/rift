-- vanish-lua: /vanish hides you from players who do not hold the `see`
-- node. Rift does the hiding (tab lists, counts, completion, BungeeCord);
-- this only sets the `rift.visible-to` meta key, so the state is durable,
-- replicated to backends (where a Paper plugin calls hidePlayer) and
-- survives switches, reconnects and restarts.
--   /vanish            toggle everywhere
--   /vanish here       toggle on this server only
--   /vanish <minutes>  vanish everywhere for a while
local _, config = ...
local see = (config and config.see) or "rift.vanish.see"
local KEY = "rift.visible-to"

-- The node is in exactly these contexts: global, or this server.
local function scope(ctx, here)
    return here and { contexts = { server = ctx.server } } or nil
end

local function hidden(ctx, here)
    local global = rift.permissions.meta(ctx.uuid, {}).meta[KEY] ~= nil
    if not here then
        return global
    end
    return not global and rift.permissions.meta(ctx.uuid, { server = ctx.server }).meta[KEY] ~= nil
end

rift.command("vanish", {
    permission = "rift.vanish.use",
    run = function(ctx)
        local here = ctx.args == "here"
        local minutes = tonumber(ctx.args)
        if hidden(ctx, here) and not minutes then
            rift.permissions.set_meta(ctx.uuid, KEY, nil, scope(ctx, here))
            return { message = "You are visible again." }
        end
        local options = scope(ctx, here) or {}
        if minutes then
            options.expires_in = math.floor(minutes * 60)
        end
        rift.permissions.set_meta(ctx.uuid, KEY, see, options)
        local where = here and (" on " .. ctx.server) or ""
        local until_ = minutes and (" for " .. minutes .. " minutes") or ""
        return { message = "You are vanished" .. where .. until_ .. "." }
    end,
})
