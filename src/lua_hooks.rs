//! Runs user-supplied Lua scripts on connect/disconnect.
//!
//! Each script gets a global `ctx` table describing the event:
//!
//! ```lua
//! -- ctx.event    : "connect" | "disconnect"
//! -- ctx.rule     : the rule name that produced this connection
//! -- ctx.backend  : "alsa" | "pipewire"
//! -- ctx.source.client / ctx.source.port
//! -- ctx.dest.client   / ctx.dest.port
//! ```
//!
//! Scripts are bounded by a wall-clock timeout, enforced via a Lua debug
//! hook that fires every [`INSTRUCTIONS_PER_CHECK`] VM instructions. A
//! runaway or malicious script can only ever stall the one connect/disconnect
//! event that triggered it, not the daemon's event loop (hooks run
//! synchronously but off the backend threads; see `engine.rs`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mlua::{HookTriggers, Lua, VmState};
use thiserror::Error;

/// How many VM instructions elapse between timeout checks. Low enough to
/// catch a tight infinite loop quickly, high enough that the check itself
/// is not a measurable overhead for normal scripts.
const INSTRUCTIONS_PER_CHECK: u32 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    Connect,
    Disconnect,
}

impl HookEvent {
    fn as_str(self) -> &'static str {
        match self {
            HookEvent::Connect => "connect",
            HookEvent::Disconnect => "disconnect",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HookContext<'a> {
    pub rule_name: &'a str,
    pub backend: &'a str,
    pub source_client: &'a str,
    pub source_port: &'a str,
    pub dest_client: &'a str,
    pub dest_port: &'a str,
}

#[derive(Debug, Error)]
pub enum HookError {
    #[error("failed to read lua script {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("lua error running {path}: {source}")]
    Lua {
        path: PathBuf,
        #[source]
        source: mlua::Error,
    },
}

/// Run a single Lua hook script with a bounded wall-clock budget.
pub fn run_hook(
    path: &Path,
    event: HookEvent,
    ctx: &HookContext<'_>,
    timeout: Duration,
) -> Result<(), HookError> {
    let script = std::fs::read_to_string(path).map_err(|source| HookError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    run_hook_source(&script, path, event, ctx, timeout)
}

/// Core implementation, taking the script source directly so unit tests
/// don't need to touch the filesystem.
fn run_hook_source(
    script: &str,
    path: &Path,
    event: HookEvent,
    ctx: &HookContext<'_>,
    timeout: Duration,
) -> Result<(), HookError> {
    let to_hook_error = |source: mlua::Error| HookError::Lua {
        path: path.to_path_buf(),
        source,
    };

    let lua = Lua::new();

    let deadline = Instant::now() + timeout;
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(INSTRUCTIONS_PER_CHECK),
        move |_, _| {
            if Instant::now() >= deadline {
                Err(mlua::Error::runtime("lua hook exceeded its timeout"))
            } else {
                Ok(VmState::Continue)
            }
        },
    )
    .map_err(to_hook_error)?;

    install_ctx(&lua, event, ctx).map_err(to_hook_error)?;

    lua.load(script)
        .set_name(path.to_string_lossy())
        .exec()
        .map_err(to_hook_error)
}

fn install_ctx(lua: &Lua, event: HookEvent, ctx: &HookContext<'_>) -> mlua::Result<()> {
    let ctx_table = lua.create_table()?;
    ctx_table.set("event", event.as_str())?;
    ctx_table.set("rule", ctx.rule_name)?;
    ctx_table.set("backend", ctx.backend)?;

    let source = lua.create_table()?;
    source.set("client", ctx.source_client)?;
    source.set("port", ctx.source_port)?;
    ctx_table.set("source", source)?;

    let dest = lua.create_table()?;
    dest.set("client", ctx.dest_client)?;
    dest.set("port", ctx.dest_port)?;
    ctx_table.set("dest", dest)?;

    lua.globals().set("ctx", ctx_table)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>() -> HookContext<'a> {
        HookContext {
            rule_name: "my-rule",
            backend: "alsa",
            source_client: "Arturia KeyLab",
            source_port: "MIDI 1",
            dest_client: "FluidSynth",
            dest_port: "MIDI In",
        }
    }

    fn run(script: &str, event: HookEvent, timeout: Duration) -> Result<(), HookError> {
        run_hook_source(script, Path::new("test.lua"), event, &ctx(), timeout)
    }

    #[test]
    fn exposes_all_ctx_fields_to_the_script() {
        let script = r#"
            assert(ctx.event == "connect")
            assert(ctx.rule == "my-rule")
            assert(ctx.backend == "alsa")
            assert(ctx.source.client == "Arturia KeyLab")
            assert(ctx.source.port == "MIDI 1")
            assert(ctx.dest.client == "FluidSynth")
            assert(ctx.dest.port == "MIDI In")
        "#;
        run(script, HookEvent::Connect, Duration::from_secs(1)).expect("script should succeed");
    }

    #[test]
    fn disconnect_event_is_passed_through() {
        let script = r#"assert(ctx.event == "disconnect")"#;
        run(script, HookEvent::Disconnect, Duration::from_secs(1)).expect("script should succeed");
    }

    #[test]
    fn propagates_lua_runtime_errors() {
        let err = run("error('boom')", HookEvent::Connect, Duration::from_secs(1)).unwrap_err();
        assert!(matches!(err, HookError::Lua { .. }));
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn propagates_lua_syntax_errors() {
        let err = run(
            "this is not lua (((",
            HookEvent::Connect,
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(matches!(err, HookError::Lua { .. }));
    }

    #[test]
    fn a_runaway_script_is_killed_by_the_timeout() {
        let err = run(
            "while true do end",
            HookEvent::Connect,
            Duration::from_millis(20),
        )
        .unwrap_err();
        assert!(matches!(err, HookError::Lua { .. }));
        assert!(err.to_string().contains("timeout"));
    }

    #[test]
    fn missing_file_reports_a_read_error() {
        let err = run_hook(
            Path::new("/nonexistent/does-not-exist.lua"),
            HookEvent::Connect,
            &ctx(),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(matches!(err, HookError::Read { .. }));
    }

    #[test]
    fn a_well_behaved_script_finishes_well_within_a_generous_timeout() {
        let script = "local sum = 0\nfor i = 1, 1000 do sum = sum + i end";
        run(script, HookEvent::Connect, Duration::from_secs(5)).expect("script should succeed");
    }
}
