//! ALSA sequencer backend.
//!
//! Two sequencer clients are opened:
//! - A "control" client, used for the initial port enumeration and for
//!   issuing `subscribe_port`/`unsubscribe_port` calls on request.
//! - A "monitor" client, owned entirely by a dedicated thread, which
//!   subscribes to the kernel's system announce port and translates
//!   `PortStart`/`PortExit` events into [`BackendEvent`]s.
//!
//! The monitor is started *before* the initial enumeration runs, so any
//! port created in that short window is merely reported twice (harmless;
//! the engine dedupes by [`PortId`]) rather than missed entirely.

use std::ffi::CString;
use std::sync::{Arc, Mutex};
use std::thread;

use alsa::seq::{Addr, ClientIter, EventType, PortCap, PortIter, PortSubscribe, PortType, Seq};
use crossbeam_channel::Sender;
use tracing::{debug, warn};

use crate::backend::{BackendEvent, BackendHandle};
use crate::port::{PortDirection, PortId, PortInfo};

/// ALSA reserves client id 0 for the kernel's own "System" client (the
/// timer and announce ports); it never represents a real MIDI device.
const ALSA_SYSTEM_CLIENT: i32 = 0;

pub struct AlsaBackend {
    seq: Arc<Mutex<Seq>>,
}

impl AlsaBackend {
    /// Opens the ALSA sequencer and starts monitoring it. Sends a
    /// [`BackendEvent::PortAdded`] for every MIDI-capable port that exists
    /// at startup, then keeps streaming add/remove events as they happen.
    pub fn start(events_tx: Sender<BackendEvent>) -> Result<Self, alsa::Error> {
        let control = Seq::open(None, None, false)?;
        control.set_client_name(&client_name("midi-auto-connector"))?;

        let monitor = Seq::open(None, None, false)?;
        monitor.set_client_name(&client_name("midi-auto-connector-monitor"))?;
        subscribe_to_announcements(&monitor)?;

        let monitor_tx = events_tx.clone();
        thread::Builder::new()
            .name("alsa-monitor".into())
            .spawn(move || monitor_loop(monitor, monitor_tx))
            .expect("failed to spawn ALSA monitor thread");

        for port in enumerate_ports(&control) {
            let _ = events_tx.send(BackendEvent::PortAdded(port));
        }

        Ok(AlsaBackend {
            seq: Arc::new(Mutex::new(control)),
        })
    }
}

impl BackendHandle for AlsaBackend {
    fn name(&self) -> &'static str {
        "alsa"
    }

    fn connect(&self, source: &PortId, dest: &PortId) {
        let (Some(sender), Some(dest_addr)) = (to_addr(source), to_addr(dest)) else {
            warn!(
                ?source,
                ?dest,
                "AlsaBackend::connect called with non-ALSA port id(s)"
            );
            return;
        };

        let sub = match PortSubscribe::empty() {
            Ok(sub) => sub,
            Err(err) => {
                warn!(error = %err, "failed to allocate ALSA port subscription");
                return;
            }
        };
        sub.set_sender(sender);
        sub.set_dest(dest_addr);

        let seq = self.seq.lock().expect("alsa control mutex poisoned");
        match seq.subscribe_port(&sub) {
            Ok(()) => debug!(?source, ?dest, "connected ALSA ports"),
            Err(err) => warn!(error = %err, ?source, ?dest, "failed to subscribe ALSA ports"),
        }
    }

    fn disconnect(&self, source: &PortId, dest: &PortId) {
        let (Some(sender), Some(dest_addr)) = (to_addr(source), to_addr(dest)) else {
            warn!(
                ?source,
                ?dest,
                "AlsaBackend::disconnect called with non-ALSA port id(s)"
            );
            return;
        };

        let seq = self.seq.lock().expect("alsa control mutex poisoned");
        match seq.unsubscribe_port(sender, dest_addr) {
            Ok(()) => debug!(?source, ?dest, "disconnected ALSA ports"),
            Err(err) => warn!(error = %err, ?source, ?dest, "failed to unsubscribe ALSA ports"),
        }
    }
}

fn to_addr(id: &PortId) -> Option<Addr> {
    match *id {
        PortId::Alsa { client, port } => Some(Addr { client, port }),
        PortId::PipeWire { .. } => None,
    }
}

fn client_name(name: &str) -> CString {
    CString::new(name).expect("backend name must not contain a NUL byte")
}

/// Create a local port and subscribe it to the kernel's system announce
/// port, so we receive `PortStart`/`PortExit`/etc. events for the whole
/// system, not just our own clients.
fn subscribe_to_announcements(seq: &Seq) -> Result<(), alsa::Error> {
    let port = seq.create_simple_port(
        &client_name("announce-listener"),
        PortCap::WRITE | PortCap::SUBS_WRITE,
        PortType::MIDI_GENERIC | PortType::APPLICATION,
    )?;

    let sub = PortSubscribe::empty()?;
    sub.set_sender(Addr::system_announce());
    sub.set_dest(Addr {
        client: seq.client_id()?,
        port,
    });
    seq.subscribe_port(&sub)
}

fn monitor_loop(monitor: Seq, events_tx: Sender<BackendEvent>) {
    loop {
        let mut input = monitor.input();
        let event = match input.event_input() {
            Ok(event) => event,
            Err(err) => {
                warn!(error = %err, "ALSA monitor event_input failed; ALSA monitoring has stopped");
                return;
            }
        };
        let event_type = event.get_type();
        let addr = event.get_data::<Addr>();
        drop(event);
        drop(input);

        let Some(addr) = addr else { continue };
        if addr.client == ALSA_SYSTEM_CLIENT {
            continue;
        }

        match event_type {
            EventType::PortStart => {
                if let Some(port) = lookup_port(&monitor, addr) {
                    let _ = events_tx.send(BackendEvent::PortAdded(port));
                }
            }
            EventType::PortExit => {
                let _ = events_tx.send(BackendEvent::PortRemoved(PortId::Alsa {
                    client: addr.client,
                    port: addr.port,
                }));
            }
            _ => {}
        }
    }
}

fn enumerate_ports(seq: &Seq) -> Vec<PortInfo> {
    let mut out = Vec::new();
    for client in ClientIter::new(seq) {
        let client_id = client.get_client();
        if client_id == ALSA_SYSTEM_CLIENT {
            continue;
        }
        let client_name = client.get_name().unwrap_or("").to_string();
        for port in PortIter::new(seq, client_id) {
            if let Some(info) = port_info_to_port(&client_name, &port) {
                out.push(info);
            }
        }
    }
    out
}

fn lookup_port(seq: &Seq, addr: Addr) -> Option<PortInfo> {
    let client_name = seq
        .get_any_client_info(addr.client)
        .ok()
        .and_then(|c| c.get_name().ok().map(str::to_string))
        .unwrap_or_default();
    let info = seq.get_any_port_info(addr).ok()?;
    port_info_to_port(&client_name, &info)
}

fn port_info_to_port(client_name: &str, port: &alsa::seq::PortInfo) -> Option<PortInfo> {
    let cap = port.get_capability();
    let can_be_source = cap.contains(PortCap::READ) && cap.contains(PortCap::SUBS_READ);
    let can_be_sink = cap.contains(PortCap::WRITE) && cap.contains(PortCap::SUBS_WRITE);
    if !can_be_source && !can_be_sink {
        return None;
    }

    let addr = port.addr();
    Some(PortInfo {
        id: PortId::Alsa {
            client: addr.client,
            port: addr.port,
        },
        client_name: client_name.to_string(),
        port_name: port.get_name().unwrap_or("").to_string(),
        direction: PortDirection {
            can_be_source,
            can_be_sink,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_addr_rejects_pipewire_ids() {
        assert_eq!(
            to_addr(&PortId::Alsa { client: 1, port: 2 }),
            Some(Addr { client: 1, port: 2 })
        );
        assert_eq!(
            to_addr(&PortId::PipeWire {
                node_id: 1,
                port_id: 2
            }),
            None
        );
    }
}
