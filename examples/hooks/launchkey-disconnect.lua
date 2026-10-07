-- Example on_disconnect hook. See launchkey-connect.lua for the `ctx`
-- table reference.

print(string.format(
    "[%s] disconnected %s:%s -> %s:%s",
    ctx.rule,
    ctx.source.client, ctx.source.port,
    ctx.dest.client, ctx.dest.port
))
