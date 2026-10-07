//! The engine owns the daemon's state: every known port, every connection
//! it has made, and the rules driving both. It is backend-agnostic: it
//! only ever sees [`BackendEvent`]s and talks back through [`BackendHandle`]s,
//! so it can be exercised in tests without any real ALSA/PipeWire backend.

use std::collections::HashMap;
use std::time::Duration;

use crossbeam_channel::Receiver;
use tracing::{debug, warn};

use crate::backend::{BackendEvent, BackendHandle};
use crate::config::{Config, Rule};
use crate::lua_hooks::{self, HookContext, HookEvent};
use crate::matcher::compute_desired_connections;
use crate::port::{Backend, PortId, PortInfo};

/// Everything the engine needs to remember about a connection it made, so
/// it can still describe it to an `on_disconnect` hook after the port(s)
/// involved have already been removed from [`Engine::ports`].
struct ActiveConnection {
    rule_name: String,
    source_client: String,
    source_port: String,
    dest_client: String,
    dest_port: String,
}

pub struct Engine {
    rules: Vec<Rule>,
    rules_by_name: HashMap<String, Rule>,
    lua_timeout: Duration,
    handles: HashMap<Backend, Box<dyn BackendHandle>>,
    ports: HashMap<PortId, PortInfo>,
    active: HashMap<(PortId, PortId), ActiveConnection>,
    events_rx: Receiver<BackendEvent>,
}

impl Engine {
    pub fn new(
        config: &Config,
        handles: HashMap<Backend, Box<dyn BackendHandle>>,
        events_rx: Receiver<BackendEvent>,
    ) -> Self {
        let rules_by_name = config
            .rules
            .iter()
            .map(|r| (r.name.clone(), r.clone()))
            .collect();

        Engine {
            rules: config.rules.clone(),
            rules_by_name,
            lua_timeout: Duration::from_millis(config.lua.timeout_ms),
            handles,
            ports: HashMap::new(),
            active: HashMap::new(),
            events_rx,
        }
    }

    /// Runs until the event channel closes (all backends gone) or a
    /// message arrives on `shutdown_rx`.
    pub fn run(&mut self, shutdown_rx: &Receiver<()>) {
        loop {
            crossbeam_channel::select! {
                recv(self.events_rx) -> msg => match msg {
                    Ok(event) => self.handle_event(event),
                    Err(_) => return,
                },
                recv(shutdown_rx) -> _ => return,
            }
        }
    }

    pub fn handle_event(&mut self, event: BackendEvent) {
        match event {
            BackendEvent::PortAdded(port) => self.handle_port_added(port),
            BackendEvent::PortRemoved(id) => self.handle_port_removed(id),
        }
    }

    fn handle_port_added(&mut self, port: PortInfo) {
        self.ports.insert(port.id.clone(), port);
        self.reconcile();
    }

    fn handle_port_removed(&mut self, id: PortId) {
        self.ports.remove(&id);

        let affected: Vec<(PortId, PortId)> = self
            .active
            .keys()
            .filter(|(source, dest)| *source == id || *dest == id)
            .cloned()
            .collect();

        for (source, dest) in affected {
            self.teardown(source, dest);
        }
    }

    /// Connects every currently-desired pair that isn't already connected.
    ///
    /// Rules never change at runtime, so a port being *added* can only ever
    /// create new matches, never invalidate existing ones; removal is
    /// handled directly in [`Self::handle_port_removed`]. So this only
    /// needs to add, never remove.
    fn reconcile(&mut self) {
        let all_ports: Vec<PortInfo> = self.ports.values().cloned().collect();
        let desired = compute_desired_connections(&all_ports, &self.rules);

        for conn in desired {
            let key = (conn.source.clone(), conn.dest.clone());
            if self.active.contains_key(&key) {
                continue;
            }
            let Some((source_info, dest_info)) =
                self.ports.get(&conn.source).zip(self.ports.get(&conn.dest))
            else {
                continue;
            };

            let active = ActiveConnection {
                rule_name: conn.rule_name,
                source_client: source_info.client_name.clone(),
                source_port: source_info.port_name.clone(),
                dest_client: dest_info.client_name.clone(),
                dest_port: dest_info.port_name.clone(),
            };

            if let Some(handle) = self.handles.get(&conn.source.backend()) {
                debug!(backend = handle.name(), rule = %active.rule_name, source = %conn.source, dest = %conn.dest, "connecting");
                handle.connect(&conn.source, &conn.dest);
            }
            self.run_hook(&active, HookEvent::Connect);
            self.active.insert(key, active);
        }
    }

    fn teardown(&mut self, source: PortId, dest: PortId) {
        let Some(active) = self.active.remove(&(source.clone(), dest.clone())) else {
            return;
        };

        if let Some(handle) = self.handles.get(&source.backend()) {
            debug!(backend = handle.name(), rule = %active.rule_name, source = %source, dest = %dest, "disconnecting");
            handle.disconnect(&source, &dest);
        }
        self.run_hook(&active, HookEvent::Disconnect);
    }

    fn run_hook(&self, active: &ActiveConnection, event: HookEvent) {
        let Some(rule) = self.rules_by_name.get(&active.rule_name) else {
            return;
        };
        let path = match event {
            HookEvent::Connect => &rule.on_connect,
            HookEvent::Disconnect => &rule.on_disconnect,
        };
        let Some(path) = path else { return };

        let backend_name =
            path_backend_hint(&active.rule_name, &self.rules_by_name).unwrap_or("unknown");

        let ctx = HookContext {
            rule_name: &active.rule_name,
            backend: backend_name,
            source_client: &active.source_client,
            source_port: &active.source_port,
            dest_client: &active.dest_client,
            dest_port: &active.dest_port,
        };

        if let Err(err) = lua_hooks::run_hook(path, event, &ctx, self.lua_timeout) {
            warn!(rule = %active.rule_name, path = %path.display(), error = %err, "lua hook failed");
        }
    }
}

fn path_backend_hint<'a>(rule_name: &str, rules: &'a HashMap<String, Rule>) -> Option<&'a str> {
    rules.get(rule_name).map(|r| match r.backend {
        crate::config::RuleBackend::Alsa => "alsa",
        crate::config::RuleBackend::PipeWire => "pipewire",
        crate::config::RuleBackend::Any => "any",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackendsConfig, LuaConfig, RuleBackend};
    use crate::port::PortDirection;
    use regex::Regex;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum RecordedCall {
        Connect(PortId, PortId),
        Disconnect(PortId, PortId),
    }

    #[derive(Clone)]
    struct RecordingBackend {
        name: &'static str,
        calls: Arc<Mutex<Vec<RecordedCall>>>,
    }

    impl RecordingBackend {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> Vec<RecordedCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl BackendHandle for RecordingBackend {
        fn name(&self) -> &'static str {
            self.name
        }

        fn connect(&self, source: &PortId, dest: &PortId) {
            self.calls
                .lock()
                .unwrap()
                .push(RecordedCall::Connect(source.clone(), dest.clone()));
        }

        fn disconnect(&self, source: &PortId, dest: &PortId) {
            self.calls
                .lock()
                .unwrap()
                .push(RecordedCall::Disconnect(source.clone(), dest.clone()));
        }
    }

    fn rule(name: &str, left: &str, right: &str) -> Rule {
        Rule {
            name: name.to_string(),
            backend: RuleBackend::Alsa,
            left: Regex::new(left).unwrap(),
            right: Regex::new(right).unwrap(),
            on_connect: None,
            on_disconnect: None,
        }
    }

    fn port(
        client: i32,
        port: i32,
        client_name: &str,
        port_name: &str,
        src: bool,
        sink: bool,
    ) -> PortInfo {
        PortInfo {
            id: PortId::Alsa { client, port },
            client_name: client_name.to_string(),
            port_name: port_name.to_string(),
            direction: PortDirection {
                can_be_source: src,
                can_be_sink: sink,
            },
        }
    }

    fn config(rules: Vec<Rule>) -> Config {
        Config {
            backends: BackendsConfig {
                alsa: true,
                pipewire: true,
            },
            lua: LuaConfig { timeout_ms: 1000 },
            rules,
        }
    }

    fn engine_with(
        rules: Vec<Rule>,
        backend: RecordingBackend,
    ) -> (Engine, crossbeam_channel::Sender<BackendEvent>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut handles: HashMap<Backend, Box<dyn BackendHandle>> = HashMap::new();
        handles.insert(Backend::Alsa, Box::new(backend));
        (Engine::new(&config(rules), handles, rx), tx)
    }

    #[test]
    fn connects_when_both_sides_of_a_rule_are_present() {
        let backend = RecordingBackend::new("alsa");
        let (mut engine, _tx) = engine_with(vec![rule("r", "^Out", "^In")], backend.clone());

        let source = port(1, 0, "Out", "p", true, false);
        let dest = port(2, 0, "In", "p", false, true);

        engine.handle_event(BackendEvent::PortAdded(source.clone()));
        assert!(
            backend.calls().is_empty(),
            "no connection until both sides exist"
        );

        engine.handle_event(BackendEvent::PortAdded(dest.clone()));
        assert_eq!(
            backend.calls(),
            vec![RecordedCall::Connect(source.id.clone(), dest.id.clone())]
        );
    }

    #[test]
    fn does_not_reconnect_a_pair_it_already_connected() {
        let backend = RecordingBackend::new("alsa");
        let (mut engine, _tx) = engine_with(vec![rule("r", "^Out", "^In")], backend.clone());

        let source = port(1, 0, "Out", "p", true, false);
        let dest = port(2, 0, "In", "p", false, true);
        engine.handle_event(BackendEvent::PortAdded(source.clone()));
        engine.handle_event(BackendEvent::PortAdded(dest.clone()));
        // A second, unrelated port arriving should not cause a re-scan to
        // re-issue the connect call for the pair we already handled.
        engine.handle_event(BackendEvent::PortAdded(port(
            3,
            0,
            "Unrelated",
            "p",
            true,
            false,
        )));

        assert_eq!(backend.calls().len(), 1);
    }

    #[test]
    fn disconnects_and_cleans_up_when_a_port_disappears() {
        let backend = RecordingBackend::new("alsa");
        let (mut engine, _tx) = engine_with(vec![rule("r", "^Out", "^In")], backend.clone());

        let source = port(1, 0, "Out", "p", true, false);
        let dest = port(2, 0, "In", "p", false, true);
        engine.handle_event(BackendEvent::PortAdded(source.clone()));
        engine.handle_event(BackendEvent::PortAdded(dest.clone()));
        engine.handle_event(BackendEvent::PortRemoved(source.id.clone()));

        assert_eq!(
            backend.calls(),
            vec![
                RecordedCall::Connect(source.id.clone(), dest.id.clone()),
                RecordedCall::Disconnect(source.id.clone(), dest.id.clone()),
            ]
        );
        assert!(engine.active.is_empty());
        assert!(!engine.ports.contains_key(&source.id));
    }

    #[test]
    fn removing_an_untracked_port_is_a_harmless_no_op() {
        let backend = RecordingBackend::new("alsa");
        let (mut engine, _tx) = engine_with(vec![rule("r", "^Out", "^In")], backend.clone());

        engine.handle_event(BackendEvent::PortRemoved(PortId::Alsa {
            client: 99,
            port: 0,
        }));
        assert!(backend.calls().is_empty());
    }

    #[test]
    fn fans_out_one_source_to_multiple_matching_sinks() {
        let backend = RecordingBackend::new("alsa");
        let (mut engine, _tx) = engine_with(vec![rule("r", "^Out", "^In.*")], backend.clone());

        engine.handle_event(BackendEvent::PortAdded(port(1, 0, "Out", "p", true, false)));
        engine.handle_event(BackendEvent::PortAdded(port(2, 0, "InA", "p", false, true)));
        engine.handle_event(BackendEvent::PortAdded(port(3, 0, "InB", "p", false, true)));

        assert_eq!(backend.calls().len(), 2);
    }

    #[test]
    fn run_returns_when_shutdown_channel_fires() {
        let backend = RecordingBackend::new("alsa");
        let (mut engine, _events_tx) = engine_with(vec![rule("r", "^Out", "^In")], backend);
        let (shutdown_tx, shutdown_rx) = crossbeam_channel::unbounded();

        shutdown_tx.send(()).unwrap();
        engine.run(&shutdown_rx);
        // If we get here, `run` returned promptly as expected.
    }

    #[test]
    fn run_returns_when_event_channel_closes() {
        let backend = RecordingBackend::new("alsa");
        let (mut engine, events_tx) = engine_with(vec![rule("r", "^Out", "^In")], backend);
        let (_shutdown_tx, shutdown_rx) = crossbeam_channel::unbounded::<()>();

        drop(events_tx);
        engine.run(&shutdown_rx);
    }

    #[test]
    fn connect_and_disconnect_hooks_run_with_correct_context() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("log.txt");
        let connect_script = dir.path().join("on_connect.lua");
        let disconnect_script = dir.path().join("on_disconnect.lua");

        std::fs::write(
            &connect_script,
            format!(
                r#"
                assert(ctx.event == "connect")
                assert(ctx.source.client == "Out")
                assert(ctx.dest.client == "In")
                local f = io.open("{}", "a")
                f:write("connect:" .. ctx.rule .. "\n")
                f:close()
                "#,
                log_path.display()
            ),
        )
        .unwrap();
        std::fs::write(
            &disconnect_script,
            format!(
                r#"
                assert(ctx.event == "disconnect")
                local f = io.open("{}", "a")
                f:write("disconnect:" .. ctx.rule .. "\n")
                f:close()
                "#,
                log_path.display()
            ),
        )
        .unwrap();

        let mut r = rule("r", "^Out", "^In");
        r.on_connect = Some(connect_script);
        r.on_disconnect = Some(disconnect_script);

        let backend = RecordingBackend::new("alsa");
        let (mut engine, _tx) = engine_with(vec![r], backend);

        let source = port(1, 0, "Out", "p", true, false);
        let dest = port(2, 0, "In", "p", false, true);
        engine.handle_event(BackendEvent::PortAdded(source.clone()));
        engine.handle_event(BackendEvent::PortAdded(dest.clone()));
        engine.handle_event(BackendEvent::PortRemoved(source.id.clone()));

        let log = std::fs::read_to_string(&log_path).unwrap();
        assert_eq!(log, "connect:r\ndisconnect:r\n");
    }

    #[test]
    fn a_failing_hook_does_not_crash_the_engine() {
        let dir = tempfile::tempdir().unwrap();
        let bad_script = dir.path().join("bad.lua");
        std::fs::write(&bad_script, "error('boom')").unwrap();

        let mut r = rule("r", "^Out", "^In");
        r.on_connect = Some(bad_script);

        let backend = RecordingBackend::new("alsa");
        let (mut engine, _tx) = engine_with(vec![r], backend.clone());

        engine.handle_event(BackendEvent::PortAdded(port(1, 0, "Out", "p", true, false)));
        engine.handle_event(BackendEvent::PortAdded(port(2, 0, "In", "p", false, true)));

        // The connect call still went through even though its hook failed.
        assert_eq!(backend.calls().len(), 1);
    }
}
