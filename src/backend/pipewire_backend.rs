//! Native PipeWire backend.
//!
//! PipeWire objects are not `Send`/`Sync`, so everything that touches them
//! (the main loop, context, core, registry, and the listener closures)
//! lives on one dedicated thread. The engine talks to that thread through
//! two channels: a [`crossbeam_channel`] carrying [`BackendEvent`]s out,
//! and a [`pipewire::channel`] carrying [`PwCommand`]s in (a plain
//! `mpsc`-style channel can't be used for the inbound side, since the
//! PipeWire thread is blocked inside its own main loop and can only be
//! woken through a loop-attached source).
//!
//! Links are created with `object.linger = true`, so they survive even if
//! our local proxy for them is dropped; the PipeWire server keeps the link
//! alive until one of its ports disappears or something explicitly
//! destroys it. To support explicit disconnection (as opposed to waiting
//! for a port to vanish), we track every `Link` global's endpoints from
//! the registry and `destroy_global` it on request.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::thread;

use crossbeam_channel::Sender as EventSender;
use pipewire::context::ContextRc;
use pipewire::core::CoreRc;
use pipewire::keys;
use pipewire::main_loop::MainLoopRc;
use pipewire::properties::properties;
use pipewire::registry::{GlobalObject, RegistryRc};
use pipewire::spa;
use pipewire::types::ObjectType;
use tracing::{debug, warn};

use crate::backend::{BackendEvent, BackendHandle};
use crate::port::{PortDirection, PortId, PortInfo, PortKind};

enum PwCommand {
    Connect { source: PortId, dest: PortId },
    Disconnect { source: PortId, dest: PortId },
    Quit,
}

pub struct PipeWireBackend {
    cmd_tx: pipewire::channel::Sender<PwCommand>,
    join: Option<thread::JoinHandle<()>>,
}

impl PipeWireBackend {
    /// Connects to the PipeWire session and starts watching its registry.
    /// Blocks until the PipeWire thread has either finished connecting or
    /// failed to.
    pub fn start(events_tx: EventSender<BackendEvent>) -> Result<Self, String> {
        let (cmd_tx, cmd_rx) = pipewire::channel::channel::<PwCommand>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

        let join = thread::Builder::new()
            .name("pipewire".into())
            .spawn(move || pw_thread(events_tx, cmd_rx, ready_tx))
            .expect("failed to spawn PipeWire thread");

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(PipeWireBackend {
                cmd_tx,
                join: Some(join),
            }),
            Ok(Err(message)) => Err(message),
            Err(_) => Err("PipeWire thread exited before it finished starting up".to_string()),
        }
    }

    /// Asks the PipeWire main loop to quit and waits for its thread to
    /// exit. Best-effort: if the channel send fails the thread is assumed
    /// to already be gone.
    pub fn shutdown(&mut self) {
        let _ = self.cmd_tx.send(PwCommand::Quit);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

    /// A cheap, cloneable [`BackendHandle`] for this backend.
    ///
    /// Separate from `PipeWireBackend` itself so the caller can keep the
    /// backend around (to call [`shutdown`](Self::shutdown) later) while
    /// also handing a `Box<dyn BackendHandle>` to the engine.
    pub fn handle(&self) -> PipeWireHandle {
        PipeWireHandle {
            cmd_tx: self.cmd_tx.clone(),
        }
    }
}

/// The engine-facing half of [`PipeWireBackend`]: just enough to submit
/// connect/disconnect requests.
#[derive(Clone)]
pub struct PipeWireHandle {
    cmd_tx: pipewire::channel::Sender<PwCommand>,
}

impl BackendHandle for PipeWireHandle {
    fn name(&self) -> &'static str {
        "pipewire"
    }

    fn connect(&self, source: &PortId, dest: &PortId) {
        let _ = self.cmd_tx.send(PwCommand::Connect {
            source: source.clone(),
            dest: dest.clone(),
        });
    }

    fn disconnect(&self, source: &PortId, dest: &PortId) {
        let _ = self.cmd_tx.send(PwCommand::Disconnect {
            source: source.clone(),
            dest: dest.clone(),
        });
    }
}

/// Per-connection endpoints of a `Link` global, keyed by its registry id,
/// so an explicit disconnect request can find the right global to destroy.
type LinkMap = Rc<RefCell<HashMap<u32, (PortId, PortId)>>>;
/// Maps a `Port` global's id to the [`PortId`] we reported it as, so a
/// `global_remove` (which only gives us an id) can be turned back into a
/// [`BackendEvent::PortRemoved`].
type PortMap = Rc<RefCell<HashMap<u32, PortId>>>;
/// Maps a `Node` global's id to its display name, so a `Port` global
/// (which only carries its parent node's id) can be given a useful
/// `client_name`.
type NodeMap = Rc<RefCell<HashMap<u32, String>>>;

fn pw_thread(
    events_tx: EventSender<BackendEvent>,
    cmd_rx: pipewire::channel::Receiver<PwCommand>,
    ready_tx: std::sync::mpsc::Sender<Result<(), String>>,
) {
    pipewire::init();

    macro_rules! try_or_report {
        ($expr:expr, $what:literal) => {
            match $expr {
                Ok(value) => value,
                Err(err) => {
                    let _ = ready_tx.send(Err(format!("{}: {}", $what, err)));
                    return;
                }
            }
        };
    }

    let mainloop = try_or_report!(MainLoopRc::new(None), "failed to create PipeWire main loop");
    let context = try_or_report!(
        ContextRc::new(&mainloop, None),
        "failed to create PipeWire context"
    );
    let core = try_or_report!(
        context.connect_rc(None),
        "failed to connect to PipeWire core"
    );
    let registry = try_or_report!(core.get_registry_rc(), "failed to get PipeWire registry");

    let nodes: NodeMap = Rc::new(RefCell::new(HashMap::new()));
    let ports: PortMap = Rc::new(RefCell::new(HashMap::new()));
    let links: LinkMap = Rc::new(RefCell::new(HashMap::new()));

    let _registry_listener = registry
        .add_listener_local()
        .global({
            let nodes = nodes.clone();
            let ports = ports.clone();
            let links = links.clone();
            let events_tx = events_tx.clone();
            move |global| on_global(global, &nodes, &ports, &links, &events_tx)
        })
        .global_remove({
            let ports = ports.clone();
            let links = links.clone();
            let events_tx = events_tx.clone();
            move |id| on_global_remove(id, &ports, &links, &events_tx)
        })
        .register();

    let _cmd_receiver = cmd_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        let core = core.clone();
        let registry = registry.clone();
        let links = links.clone();
        move |cmd| on_command(cmd, &mainloop, &core, &registry, &links)
    });

    if ready_tx.send(Ok(())).is_err() {
        return;
    }

    mainloop.run();
}

fn on_global(
    global: &GlobalObject<&spa::utils::dict::DictRef>,
    nodes: &NodeMap,
    ports: &PortMap,
    links: &LinkMap,
    events_tx: &EventSender<BackendEvent>,
) {
    match &global.type_ {
        ObjectType::Node => on_node_global(global, nodes),
        ObjectType::Port => on_port_global(global, nodes, ports, events_tx),
        ObjectType::Link => on_link_global(global, links),
        _ => {}
    }
}

fn on_node_global(global: &GlobalObject<&spa::utils::dict::DictRef>, nodes: &NodeMap) {
    let Some(props) = global.props else { return };
    let name = props
        .get(*keys::NODE_NAME)
        .or_else(|| props.get(*keys::NODE_DESCRIPTION))
        .unwrap_or_default()
        .to_string();
    nodes.borrow_mut().insert(global.id, name);
}

fn on_port_global(
    global: &GlobalObject<&spa::utils::dict::DictRef>,
    nodes: &NodeMap,
    ports: &PortMap,
    events_tx: &EventSender<BackendEvent>,
) {
    let Some(props) = global.props else { return };

    let Some(kind) = props.get(*keys::FORMAT_DSP).and_then(|v| {
        let v = v.to_ascii_lowercase();
        if v.contains("midi") {
            Some(PortKind::Midi)
        } else if v.contains("audio") {
            Some(PortKind::Audio)
        } else {
            None
        }
    }) else {
        return;
    };

    let Some(node_id) = props
        .get(*keys::NODE_ID)
        .and_then(|v| v.parse::<u32>().ok())
    else {
        return;
    };

    let (can_be_source, can_be_sink) = match props.get(*keys::PORT_DIRECTION) {
        Some("out") => (true, false),
        Some("in") => (false, true),
        _ => return,
    };

    let port_name = props.get(*keys::PORT_NAME).unwrap_or_default().to_string();
    let client_name = nodes
        .borrow()
        .get(&node_id)
        .cloned()
        .unwrap_or_else(|| format!("node-{node_id}"));

    let port_id = PortId::PipeWire {
        node_id,
        port_id: global.id,
    };
    ports.borrow_mut().insert(global.id, port_id.clone());

    let info = PortInfo {
        id: port_id,
        client_name,
        port_name,
        direction: PortDirection {
            can_be_source,
            can_be_sink,
        },
        kind,
    };
    let _ = events_tx.send(BackendEvent::PortAdded(info));
}

fn on_link_global(global: &GlobalObject<&spa::utils::dict::DictRef>, links: &LinkMap) {
    let Some(props) = global.props else { return };
    let parse = |key: &str| props.get(key).and_then(|v| v.parse::<u32>().ok());

    if let (Some(out_node), Some(out_port), Some(in_node), Some(in_port)) = (
        parse(*keys::LINK_OUTPUT_NODE),
        parse(*keys::LINK_OUTPUT_PORT),
        parse(*keys::LINK_INPUT_NODE),
        parse(*keys::LINK_INPUT_PORT),
    ) {
        let source = PortId::PipeWire {
            node_id: out_node,
            port_id: out_port,
        };
        let dest = PortId::PipeWire {
            node_id: in_node,
            port_id: in_port,
        };
        links.borrow_mut().insert(global.id, (source, dest));
    }
}

fn on_global_remove(
    id: u32,
    ports: &PortMap,
    links: &LinkMap,
    events_tx: &EventSender<BackendEvent>,
) {
    if let Some(port_id) = ports.borrow_mut().remove(&id) {
        let _ = events_tx.send(BackendEvent::PortRemoved(port_id));
    }
    links.borrow_mut().remove(&id);
    // Node globals are intentionally left untouched here: node ids are not
    // reused while the daemon runs, and a stale name in `nodes` is harmless.
}

fn on_command(
    cmd: PwCommand,
    mainloop: &MainLoopRc,
    core: &CoreRc,
    registry: &RegistryRc,
    links: &LinkMap,
) {
    match cmd {
        PwCommand::Connect { source, dest } => connect(&source, &dest, core),
        PwCommand::Disconnect { source, dest } => disconnect(&source, &dest, registry, links),
        PwCommand::Quit => mainloop.quit(),
    }
}

fn connect(source: &PortId, dest: &PortId, core: &CoreRc) {
    let (
        PortId::PipeWire {
            node_id: out_node,
            port_id: out_port,
        },
        PortId::PipeWire {
            node_id: in_node,
            port_id: in_port,
        },
    ) = (source, dest)
    else {
        warn!(
            ?source,
            ?dest,
            "PipeWire connect called with non-PipeWire port id(s)"
        );
        return;
    };

    let props = properties! {
        *keys::LINK_OUTPUT_NODE => out_node.to_string(),
        *keys::LINK_OUTPUT_PORT => out_port.to_string(),
        *keys::LINK_INPUT_NODE => in_node.to_string(),
        *keys::LINK_INPUT_PORT => in_port.to_string(),
        *keys::OBJECT_LINGER => "true",
    };

    match core.create_object::<pipewire::link::Link>("link-factory", &props) {
        Ok(_link) => debug!(?source, ?dest, "created PipeWire link"),
        Err(err) => warn!(error = %err, ?source, ?dest, "failed to create PipeWire link"),
    }
}

fn disconnect(source: &PortId, dest: &PortId, registry: &RegistryRc, links: &LinkMap) {
    let found = links
        .borrow()
        .iter()
        .find(|(_, endpoints)| **endpoints == (source.clone(), dest.clone()))
        .map(|(id, _)| *id);

    let Some(link_id) = found else {
        debug!(
            ?source,
            ?dest,
            "no tracked PipeWire link for this pair; nothing to destroy"
        );
        return;
    };

    if let Err(err) = registry.destroy_global(link_id).into_result() {
        warn!(error = %err, ?source, ?dest, "failed to destroy PipeWire link");
    } else {
        links.borrow_mut().remove(&link_id);
        debug!(?source, ?dest, "destroyed PipeWire link");
    }
}
