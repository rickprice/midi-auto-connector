-- Example on_connect hook.
--
-- Every hook script gets a global `ctx` table:
--   ctx.event    -- "connect" or "disconnect"
--   ctx.rule     -- the rule name that produced this connection
--   ctx.backend  -- "alsa" or "pipewire"
--   ctx.source.client / ctx.source.port
--   ctx.dest.client   / ctx.dest.port
--
-- Scripts run with a bounded wall-clock timeout (see [lua] in the
-- config); an infinite loop here only ever kills this one hook
-- invocation, not the daemon.

print(string.format(
    "[%s] connected %s:%s -> %s:%s",
    ctx.rule,
    ctx.source.client, ctx.source.port,
    ctx.dest.client, ctx.dest.port
))
