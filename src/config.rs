//! Config file schema: top-level settings plus a list of connection rules.
//!
//! Each `[[rule]]` row is independent: its own `backend`, `output`/`input`
//! regexes, and optional Lua hooks. A config normally has many rows, e.g.
//! one per instrument/DAW pairing.

use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;
use thiserror::Error;

use crate::port::{Backend, PortKind};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("rule {index} ({name:?}) has an invalid `output` regex: {source}")]
    OutputRegex {
        index: usize,
        name: String,
        #[source]
        source: regex::Error,
    },
    #[error("rule {index} ({name:?}) has an invalid `input` regex: {source}")]
    InputRegex {
        index: usize,
        name: String,
        #[source]
        source: regex::Error,
    },
    #[error("no [[rule]] rows defined in config")]
    NoRules,
    #[error("rule {index} has an empty `name`")]
    EmptyName { index: usize },
    #[error("duplicate rule name {name:?} (rows {first} and {second})")]
    DuplicateName {
        name: String,
        first: usize,
        second: usize,
    },
}

/// Which graph(s) of ports a rule should be evaluated against.
///
/// PipeWire bridges ALSA-sequencer MIDI clients into its own node graph, so
/// the same physical device can show up as both an ALSA-seq port and a
/// native PipeWire port at the same time. Pinning a rule to one backend
/// avoids matching (and trying to connect) both representations at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleBackend {
    Alsa,
    PipeWire,
    Any,
}

impl RuleBackend {
    /// Which concrete backends this rule applies to.
    pub fn applies_to(self, backend: Backend) -> bool {
        match self {
            RuleBackend::Alsa => backend == Backend::Alsa,
            RuleBackend::PipeWire => backend == Backend::PipeWire,
            RuleBackend::Any => true,
        }
    }
}

/// Which kind of port a rule's `output`/`input` regexes are evaluated
/// against. Defaults to `midi` so existing configs without a `kind` field
/// keep behaving exactly as before.
///
/// Audio ports only ever exist on the PipeWire backend (the ALSA
/// sequencer API this daemon's ALSA backend uses has no concept of
/// audio), so `kind = "audio"` combined with `backend = "alsa"` is valid
/// but will never match anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleKind {
    #[default]
    Midi,
    Audio,
}

impl RuleKind {
    /// Whether a port of the given kind is in scope for this rule.
    pub fn matches(self, kind: PortKind) -> bool {
        match self {
            RuleKind::Midi => kind == PortKind::Midi,
            RuleKind::Audio => kind == PortKind::Audio,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    backends: BackendsConfig,
    #[serde(default)]
    lua: LuaConfig,
    #[serde(default)]
    rule: Vec<RawRule>,
}

/// Which backends the daemon starts up at all. Independent of the
/// per-rule `backend` field: a backend must be enabled here *and*
/// matched by a rule's `backend` for a connection to be made on it.
#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(default)]
pub struct BackendsConfig {
    pub alsa: bool,
    pub pipewire: bool,
}

impl Default for BackendsConfig {
    fn default() -> Self {
        Self {
            alsa: true,
            pipewire: true,
        }
    }
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(default)]
pub struct LuaConfig {
    pub timeout_ms: u64,
}

impl Default for LuaConfig {
    fn default() -> Self {
        Self { timeout_ms: 500 }
    }
}

#[derive(Debug, Deserialize, Clone)]
struct RawRule {
    name: String,
    backend: RuleBackend,
    #[serde(default)]
    kind: RuleKind,
    output: String,
    input: String,
    #[serde(default)]
    on_connect: Option<PathBuf>,
    #[serde(default)]
    on_disconnect: Option<PathBuf>,
}

/// A single, validated output/input connection rule.
///
/// `output` always matches the source (sending) port and `input` always
/// matches the destination (receiving) port -- that mapping never flips,
/// unlike a visual "left"/"right" framing.
#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    pub backend: RuleBackend,
    pub kind: RuleKind,
    pub output: Regex,
    pub input: Regex,
    pub on_connect: Option<PathBuf>,
    pub on_disconnect: Option<PathBuf>,
}

/// Fully validated configuration, ready to drive the engine.
#[derive(Debug, Clone)]
pub struct Config {
    pub backends: BackendsConfig,
    pub lua: LuaConfig,
    pub rules: Vec<Rule>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text, path)
    }

    pub fn parse(text: &str, path: &Path) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: RawConfig) -> Result<Self, ConfigError> {
        if raw.rule.is_empty() {
            return Err(ConfigError::NoRules);
        }

        let mut rules = Vec::with_capacity(raw.rule.len());
        let mut seen_names: Vec<String> = Vec::with_capacity(raw.rule.len());

        for (index, raw_rule) in raw.rule.into_iter().enumerate() {
            if raw_rule.name.trim().is_empty() {
                return Err(ConfigError::EmptyName { index });
            }
            if let Some(first) = seen_names.iter().position(|n| n == &raw_rule.name) {
                return Err(ConfigError::DuplicateName {
                    name: raw_rule.name,
                    first,
                    second: index,
                });
            }
            let output =
                Regex::new(&raw_rule.output).map_err(|source| ConfigError::OutputRegex {
                    index,
                    name: raw_rule.name.clone(),
                    source,
                })?;
            let input = Regex::new(&raw_rule.input).map_err(|source| ConfigError::InputRegex {
                index,
                name: raw_rule.name.clone(),
                source,
            })?;
            seen_names.push(raw_rule.name.clone());
            rules.push(Rule {
                name: raw_rule.name,
                backend: raw_rule.backend,
                kind: raw_rule.kind,
                output,
                input,
                on_connect: raw_rule.on_connect,
                on_disconnect: raw_rule.on_disconnect,
            });
        }

        Ok(Config {
            backends: raw.backends,
            lua: raw.lua,
            rules,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::parse(text, Path::new("test.toml"))
    }

    #[test]
    fn parses_multiple_rule_rows_with_defaults() {
        let cfg = parse(
            r#"
            [[rule]]
            name = "keyboard-to-synth"
            backend = "alsa"
            output = "^Arturia.*"
            input = "^FluidSynth.*"

            [[rule]]
            name = "controller-to-daw"
            backend = "pipewire"
            output = "^Launchkey.*"
            input = "^Ableton.*"
            "#,
        )
        .expect("valid config should parse");

        assert_eq!(cfg.rules.len(), 2);
        assert_eq!(cfg.rules[0].name, "keyboard-to-synth");
        assert_eq!(cfg.rules[0].backend, RuleBackend::Alsa);
        assert_eq!(cfg.rules[0].kind, RuleKind::Midi);
        assert_eq!(cfg.rules[1].backend, RuleBackend::PipeWire);
        assert!(cfg.backends.alsa);
        assert!(cfg.backends.pipewire);
        assert_eq!(cfg.lua.timeout_ms, 500);
    }

    #[test]
    fn rule_with_any_backend_and_hooks() {
        let cfg = parse(
            r#"
            [[rule]]
            name = "any-rule"
            backend = "any"
            output = "output.*"
            input = "input.*"
            on_connect = "/etc/midi-auto-connector/connect.lua"
            on_disconnect = "/etc/midi-auto-connector/disconnect.lua"
            "#,
        )
        .expect("valid config should parse");

        assert_eq!(cfg.rules[0].backend, RuleBackend::Any);
        assert_eq!(
            cfg.rules[0].on_connect,
            Some(PathBuf::from("/etc/midi-auto-connector/connect.lua"))
        );
    }

    #[test]
    fn rejects_empty_config() {
        let err = parse("").unwrap_err();
        assert!(matches!(err, ConfigError::NoRules));
    }

    #[test]
    fn rejects_bad_output_regex() {
        let err = parse(
            r#"
            [[rule]]
            name = "bad"
            backend = "any"
            output = "("
            input = ".*"
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::OutputRegex { index: 0, .. }));
    }

    #[test]
    fn rejects_bad_input_regex() {
        let err = parse(
            r#"
            [[rule]]
            name = "bad"
            backend = "any"
            output = ".*"
            input = "("
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::InputRegex { index: 0, .. }));
    }

    #[test]
    fn rejects_empty_rule_name() {
        let err = parse(
            r#"
            [[rule]]
            name = ""
            backend = "any"
            output = ".*"
            input = ".*"
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::EmptyName { index: 0 }));
    }

    #[test]
    fn rejects_duplicate_rule_names() {
        let err = parse(
            r#"
            [[rule]]
            name = "dup"
            backend = "any"
            output = ".*"
            input = ".*"

            [[rule]]
            name = "dup"
            backend = "any"
            output = ".*"
            input = ".*"
            "#,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::DuplicateName {
                first: 0,
                second: 1,
                ..
            }
        ));
    }

    #[test]
    fn rejects_missing_backend_field() {
        let err = parse(
            r#"
            [[rule]]
            name = "no-backend"
            output = ".*"
            input = ".*"
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn rule_backend_applies_to_matches_concrete_backend() {
        assert!(RuleBackend::Alsa.applies_to(Backend::Alsa));
        assert!(!RuleBackend::Alsa.applies_to(Backend::PipeWire));
        assert!(RuleBackend::PipeWire.applies_to(Backend::PipeWire));
        assert!(!RuleBackend::PipeWire.applies_to(Backend::Alsa));
        assert!(RuleBackend::Any.applies_to(Backend::Alsa));
        assert!(RuleBackend::Any.applies_to(Backend::PipeWire));
    }

    #[test]
    fn rule_kind_matches_only_the_same_kind() {
        assert!(RuleKind::Midi.matches(PortKind::Midi));
        assert!(!RuleKind::Midi.matches(PortKind::Audio));
        assert!(RuleKind::Audio.matches(PortKind::Audio));
        assert!(!RuleKind::Audio.matches(PortKind::Midi));
    }

    #[test]
    fn rule_kind_defaults_to_midi_when_omitted() {
        let cfg = parse(
            r#"
            [[rule]]
            name = "r"
            backend = "any"
            output = ".*"
            input = ".*"
            "#,
        )
        .expect("valid config should parse");
        assert_eq!(cfg.rules[0].kind, RuleKind::Midi);
    }

    #[test]
    fn rule_kind_audio_parses() {
        let cfg = parse(
            r#"
            [[rule]]
            name = "r"
            backend = "pipewire"
            kind = "audio"
            output = ".*"
            input = ".*"
            "#,
        )
        .expect("valid config should parse");
        assert_eq!(cfg.rules[0].kind, RuleKind::Audio);
    }

    #[test]
    fn custom_backends_and_lua_timeout_override_defaults() {
        let cfg = parse(
            r#"
            [backends]
            alsa = true
            pipewire = false

            [lua]
            timeout_ms = 1000

            [[rule]]
            name = "r"
            backend = "alsa"
            output = ".*"
            input = ".*"
            "#,
        )
        .expect("valid config should parse");

        assert!(cfg.backends.alsa);
        assert!(!cfg.backends.pipewire);
        assert_eq!(cfg.lua.timeout_ms, 1000);
    }
}
