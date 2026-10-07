//! Backend-agnostic description of a single MIDI port.
//!
//! Both the ALSA sequencer backend and the native PipeWire backend
//! translate whatever they discover into [`PortInfo`], so the rest of the
//! daemon (rule matching, the engine, logging) never has to know which
//! backend a port came from beyond the [`Backend`] tag itself.

use std::fmt;

/// Which subsystem a port was discovered through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    Alsa,
    PipeWire,
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Backend::Alsa => write!(f, "alsa"),
            Backend::PipeWire => write!(f, "pipewire"),
        }
    }
}

/// Opaque, backend-specific identity for a single port.
///
/// Deliberately not `Copy`: PipeWire ids are plain `u32`s that get reused
/// over the life of a session, so callers should not hold onto a `PortId`
/// past the `PortRemoved` event for it.
///
/// `PipeWire` carries both the owning node's id and the port's own id:
/// creating a `pw_link` requires all four of
/// `link.output.{node,port}`/`link.input.{node,port}`, not just the port ids.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PortId {
    Alsa { client: i32, port: i32 },
    PipeWire { node_id: u32, port_id: u32 },
}

impl PortId {
    pub fn backend(&self) -> Backend {
        match self {
            PortId::Alsa { .. } => Backend::Alsa,
            PortId::PipeWire { .. } => Backend::PipeWire,
        }
    }
}

impl fmt::Display for PortId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PortId::Alsa { client, port } => write!(f, "alsa:{client}:{port}"),
            PortId::PipeWire { node_id, port_id } => write!(f, "pipewire:{node_id}:{port_id}"),
        }
    }
}

/// Which direction(s) of data a port supports, in ALSA-seq terms:
/// a port that `can_be_source` can be the *sender* half of a subscription,
/// a port that `can_be_sink` can be the *receiver* half. Most hardware
/// MIDI ports (and most PipeWire MIDI ports) support both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PortDirection {
    pub can_be_source: bool,
    pub can_be_sink: bool,
}

/// A single discovered MIDI port, backend-agnostic.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PortInfo {
    pub id: PortId,
    pub client_name: String,
    pub port_name: String,
    pub direction: PortDirection,
}

impl PortInfo {
    pub fn backend(&self) -> Backend {
        self.id.backend()
    }

    /// The `"client:port"` form that rule regexes match against, matching
    /// the convention used by `aconnect`, QjackCtl, Carla, etc.
    pub fn full_name(&self) -> String {
        format!("{}:{}", self.client_name, self.port_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_name_is_client_colon_port() {
        let p = PortInfo {
            id: PortId::Alsa {
                client: 128,
                port: 0,
            },
            client_name: "Arturia KeyLab mkII".into(),
            port_name: "MIDI 1".into(),
            direction: PortDirection {
                can_be_source: true,
                can_be_sink: false,
            },
        };
        assert_eq!(p.full_name(), "Arturia KeyLab mkII:MIDI 1");
    }

    #[test]
    fn port_id_backend_tag_matches_variant() {
        assert_eq!(PortId::Alsa { client: 0, port: 0 }.backend(), Backend::Alsa);
        assert_eq!(
            PortId::PipeWire {
                node_id: 0,
                port_id: 0
            }
            .backend(),
            Backend::PipeWire
        );
    }
}
