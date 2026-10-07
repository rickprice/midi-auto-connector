//! Backend abstraction shared by the ALSA and PipeWire implementations.
//!
//! Each backend owns a dedicated OS thread that talks to the underlying
//! C library (neither `alsa`'s `Seq` nor PipeWire's objects are meant to be
//! driven concurrently from multiple threads). The engine never touches
//! backend internals directly: it only ever sees [`BackendEvent`]s coming
//! in over a channel, and issues commands through a [`BackendHandle`].

pub mod alsa_backend;
pub mod pipewire_backend;

use crate::port::{PortId, PortInfo};

/// Something happened to the port graph of a backend.
#[derive(Debug, Clone)]
pub enum BackendEvent {
    PortAdded(PortInfo),
    PortRemoved(PortId),
}

/// A handle the engine uses to ask a running backend to connect or
/// disconnect two of its ports.
///
/// Implementations submit the request to their owning thread and return as
/// soon as it's queued; they do not wait for the underlying subscribe/link
/// call to complete. Failures are logged by the backend itself, since the
/// engine has no useful recovery action beyond "try again next time the
/// port set changes".
pub trait BackendHandle: Send {
    /// Human-readable name for logging, e.g. `"alsa"` or `"pipewire"`.
    fn name(&self) -> &'static str;

    fn connect(&self, source: &PortId, dest: &PortId);
    fn disconnect(&self, source: &PortId, dest: &PortId);
}
