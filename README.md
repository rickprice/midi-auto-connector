# midi-auto-connector

A daemon that watches the ALSA sequencer and/or the native PipeWire graph
for MIDI (and, on PipeWire, audio) ports appearing and disappearing, and
automatically connects the ones that match your regex rules —
reconnecting your keyboard to your synth, or your mixer's channels to its
mix bus, every time you plug it in or start it up, without touching
`aconnect`/`pw-link`/QjackCtl by hand.

- **Config-driven**: any number of `[[rule]]` rows, each with its own
  `backend`, `kind` (midi or audio), `output`/`input` regex pair, and
  optional Lua hooks.
- **Event-driven, not polling**: each backend blocks on its own kernel/IPC
  event stream and reacts immediately; there's no periodic rescan.
- **ALSA sequencer and native PipeWire**, independently enable-able, with
  rules scoped to one backend or evaluated against both.
- **Lua hooks** on connect and disconnect, with a bounded per-script
  timeout so a bad script can't hang the daemon.

## Why a `backend` field per rule?

PipeWire bridges ALSA-sequencer MIDI clients into its own node graph, so
the same physical device can show up twice: once as an ALSA-seq port, and
once as a native PipeWire `Midi/Bridge` node. If you run both backends,
a regex with no backend scoping could match — and try to connect — both
representations of the same device at once. Every rule therefore declares
`backend = "alsa"`, `"pipewire"`, or `"any"` (evaluate independently
against each backend's own ports, never pairing one of each together).

## Building

### Rust toolchain

Needs a reasonably recent stable Rust (edition 2024, so 1.85+).

### Native dependencies

Building links against `libasound` (ALSA) and `libpipewire-0.3`, and uses
`clang`/`libclang` at build time (for PipeWire's FFI bindings).

**NixOS / Nix users**: a `flake.nix` dev shell is included —

```sh
nix develop
cargo build --release
```

**Debian/Ubuntu**:

```sh
sudo apt install libasound2-dev libpipewire-0.3-dev clang pkg-config
cargo build --release
```

**Fedora**:

```sh
sudo dnf install alsa-lib-devel pipewire-devel clang pkgconf-pkg-config
cargo build --release
```

**Arch**:

```sh
sudo pacman -S alsa-lib pipewire clang pkgconf
cargo build --release
```

Lua is vendored (via `mlua`'s `vendored` feature) and compiled in, so there
is no Lua dev package to install.

### Tests

```sh
cargo test
```

The test suite covers config parsing, rule matching, the connect/disconnect
engine, and Lua hook execution entirely with fakes/mocks — none of it
touches a real ALSA or PipeWire session, so `cargo test` works in CI or a
headless container with no audio hardware.

## Running

```sh
midi-auto-connector run --config /path/to/config.toml
```

With no `--config`, it defaults to
`$XDG_CONFIG_HOME/midi-auto-connector/config.toml` (typically
`~/.config/midi-auto-connector/config.toml`).

If no home directory can be resolved (e.g. `$HOME` is unset, as can happen
under a system service account or a stripped-down container), it falls back
to `/etc/midi-auto-connector/config.toml` instead. Nothing creates this file
or directory automatically — you need to create it yourself if you're
relying on this fallback.

Other subcommands:

```sh
# Validate a config file without starting any backend.
midi-auto-connector check-config --config /path/to/config.toml

# Show every connection this config would make against the ports visible
# right now, without making any of them. Starts only the backend(s) the
# config enables, same as `run` would -- a fast way to sanity-check a
# rule set (e.g. after editing a regex) before actually running it.
midi-auto-connector dry-run --config /path/to/config.toml

# Print every MIDI/audio port visible right now, with its "client:port"
# name -- exactly what your regexes match against. Handy for writing rules.
midi-auto-connector list-ports

# Narrow it down with --backend (alsa|pipewire), --kind (midi|audio),
# --output (source-capable ports only), and/or --input (sink-capable
# ports only). They combine as AND, so e.g. --backend pipewire --kind
# audio --output lists only the PipeWire audio ports a rule's `output`
# regex could match.
midi-auto-connector list-ports --backend pipewire --kind audio --output
```

Logging is via `tracing`; set `RUST_LOG=midi_auto_connector=debug` for
per-connection detail.

### As a systemd user service

```sh
cp systemd/midi-auto-connector.service ~/.config/systemd/user/
cp examples/config.toml ~/.config/midi-auto-connector/config.toml   # then edit it
systemctl --user enable --now midi-auto-connector.service
```

## Config format

See [`examples/config.toml`](examples/config.toml) for a complete example,
or [`examples/non-mixer-xt.toml`](examples/non-mixer-xt.toml) for a
capture-group-pairing setup that fans a multi-channel mixing session into
a single mix bus. The shape:

```toml
[backends]
alsa = true       # start the ALSA sequencer backend at all (default: true)
pipewire = true   # start the native PipeWire backend at all (default: true)

[lua]
timeout_ms = 500  # per-hook-invocation wall-clock budget (default: 500)

disconnect_on_shutdown = true  # tear down every active connection on exit (default: true)

[[rule]]
name = "keylab-to-fluidsynth"   # must be unique
backend = "alsa"                # "alsa" | "pipewire" | "any"
kind = "midi"                   # "midi" (default) | "audio"
output = "^Arturia KeyLab.*"    # regex matched against a source port's "client:port"
input = "^FluidSynth.*"         # regex matched against a destination port's "client:port"
on_connect = "/path/to/connect.lua"       # optional
on_disconnect = "/path/to/disconnect.lua" # optional
```

`output` always matches the sending port and `input` always matches the
receiving port -- that mapping never flips.

`disconnect_on_shutdown` controls what happens to every connection the
daemon made by the time it exits (via Ctrl-C/SIGTERM, or because every
backend disappeared). Defaults to `true`: every active connection is
torn down on the way out, running each rule's `on_disconnect` hook along
the way, same as if the ports involved had just disappeared. Set it to
`false` to instead leave connections in place after the daemon exits --
ALSA subscriptions live at the kernel level independent of the client
that requested them, and PipeWire links are created with
`object.linger = true`, so they're able to outlive the daemon if you
want them to.

`kind` defaults to `"midi"` if omitted, so existing configs keep working
unchanged. `output`/`input` only ever match ports of that same kind --
a `midi` rule can't accidentally wire an audio port to a MIDI one, even
if a name happens to match both. Audio ports only exist on the PipeWire
backend: the ALSA sequencer API this daemon's ALSA backend uses has no
concept of audio, so `kind = "audio"` combined with `backend = "alsa"`
is valid but will never match anything.

You can have as many `[[rule]]` rows as you like; each is matched
independently. A rule fans out: if `output` matches 1 port and `input`
matches 3, all 3 connections are made.

### Pairing by capture group

If `output` and `input` each contain exactly one regex capture group, a
source/dest pair is only connected when the two captured substrings are
equal, instead of the usual fan-out-to-everything behavior:

```toml
[[rule]]
name = "mixer-channels"
backend = "alsa"
output = '^Mixer:out-(\d+)$'
input = '^Mixer:in-(\d+)$'
```

(Use single-quoted TOML literal strings for regexes with backslashes --
`\d` isn't a valid escape inside a double-quoted TOML string.)

This connects `out-1` to `in-1`, `out-2` to `in-2`, and so on for any
number of channels, but never `out-1` to `in-2`. The comparison is a plain
string match on whatever the group captures, so it works for non-numeric
labels too (e.g. capturing `_FL`/`_FR` suffixes). Multiple ports that
happen to capture the same text (say, `out-1` ports on several different
devices) all fan into every `input` match with that same captured text --
pairing is "same key connects", not "exactly one-to-one". With zero or
more than one capture group on either side, this falls back to the
regular full fan-out.

Run `midi-auto-connector list-ports` to see the exact `client:port` strings
on your system before writing a rule's regexes.

### Lua hooks

Each hook script gets a global `ctx` table:

```lua
ctx.event          -- "connect" | "disconnect"
ctx.rule           -- the rule name that produced this connection
ctx.backend        -- "alsa" | "pipewire" | "any"
ctx.source.client   ctx.source.port
ctx.dest.client     ctx.dest.port
```

See [`examples/hooks/`](examples/hooks) for working examples. A script that
runs past `[lua].timeout_ms` is aborted and logged as a warning — this only
affects that one hook invocation, not the connection itself or the daemon.

## How connections persist

Connections made by the daemon don't *inherently* depend on it staying
alive:

- ALSA sequencer subscriptions exist at the kernel level, independent of
  the client that requested them.
- PipeWire links are created with `object.linger = true`, so they survive
  even after the daemon's own connection to the PipeWire server closes.

Whether that's what actually happens on exit is controlled by
`disconnect_on_shutdown` (see [Config format](#config-format) above) --
the default (`true`) tears every active connection down on a clean exit,
rather than leaving them in place. Set it to `false` to get the
lower-level persistence described above instead: restarting (or briefly
stopping) the daemon then does not tear down anything it already
connected, and on the next startup it just reconciles against whatever's
already there.

## License

BSD 3-Clause. See [LICENSE](LICENSE).
