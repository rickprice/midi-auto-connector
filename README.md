# midi-auto-connector

A daemon that watches the ALSA sequencer and/or the native PipeWire graph
for MIDI ports appearing and disappearing, and automatically connects the
ones that match your regex rules — reconnecting your keyboard to your synth
every time you plug it in, without touching `aconnect`/`pw-link`/QjackCtl
by hand.

- **Config-driven**: any number of `[[rule]]` rows, each with its own
  `backend`, `left`/`right` regex pair, and optional Lua hooks.
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

Other subcommands:

```sh
# Validate a config file without starting any backend.
midi-auto-connector check-config --config /path/to/config.toml

# Print every MIDI port visible right now, with its "client:port" name --
# exactly what your regexes match against. Handy for writing rules.
midi-auto-connector list-ports
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

See [`examples/config.toml`](examples/config.toml) for a complete example.
The shape:

```toml
[backends]
alsa = true       # start the ALSA sequencer backend at all (default: true)
pipewire = true   # start the native PipeWire backend at all (default: true)

[lua]
timeout_ms = 500  # per-hook-invocation wall-clock budget (default: 500)

[[rule]]
name = "keylab-to-fluidsynth"   # must be unique
backend = "alsa"                # "alsa" | "pipewire" | "any"
left = "^Arturia KeyLab.*"      # regex matched against a source port's "client:port"
right = "^FluidSynth.*"         # regex matched against a destination port's "client:port"
on_connect = "/path/to/connect.lua"       # optional
on_disconnect = "/path/to/disconnect.lua" # optional
```

You can have as many `[[rule]]` rows as you like; each is matched
independently. A rule fans out: if `left` matches 1 port and `right`
matches 3, all 3 connections are made.

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

Connections made by the daemon don't depend on it staying alive:

- ALSA sequencer subscriptions exist at the kernel level, independent of
  the client that requested them.
- PipeWire links are created with `object.linger = true`, so they survive
  even after the daemon's own connection to the PipeWire server closes.

So restarting (or briefly stopping) the daemon does not tear down anything
it already connected; on the next startup it just reconciles against
whatever's already there.

## License

BSD 3-Clause. See [LICENSE](LICENSE).
