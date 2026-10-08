//! The engine owns the daemon's state: every known port, every connection
//! it has made, and the rules driving both. It is backend-agnostic: it
//! only ever sees [`BackendEvent`]s and talks back through [`BackendHandle`]s,
//! so it can be exercised in tests without any real ALSA/PipeWire backend.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use crossbeam_channel::Receiver;
use tracing::{debug, info, warn};

use crate::backend::{BackendEvent, BackendHandle};
use crate::config::{BackendsConfig, Config, Rule};
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
    config_path: PathBuf,
    backends_config: BackendsConfig,
    disconnect_on_shutdown: bool,
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
        config_path: PathBuf,
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
            config_path,
            backends_config: config.backends,
            disconnect_on_shutdown: config.disconnect_on_shutdown,
            rules: config.rules.clone(),
            rules_by_name,
            lua_timeout: Duration::from_millis(config.lua.timeout_ms),
            handles,
            ports: HashMap::new(),
            active: HashMap::new(),
            events_rx,
        }
    }

    pub fn disconnect_on_shutdown(&self) -> bool {
        self.disconnect_on_shutdown
    }

    /// Runs until the event channel closes (all backends gone) or a
    /// message arrives on `shutdown_rx`. `reload_rx` fires once per
    /// detected change to the config file on disk.
    pub fn run(&mut self, shutdown_rx: &Receiver<()>, reload_rx: &Receiver<()>) {
        loop {
            crossbeam_channel::select! {
                recv(self.events_rx) -> msg => match msg {
                    Ok(event) => self.handle_event(event),
                    Err(_) => return,
                },
                recv(reload_rx) -> msg => {
                    if msg.is_ok() {
                        self.reload();
                    }
                },
                recv(shutdown_rx) -> _ => return,
            }
        }
    }

    /// Re-reads the config file this engine was started with and applies
    /// whatever changed: rules are swapped in wholesale, any active
    /// connection no longer matched by the new rules is torn down (running
    /// its `on_disconnect` hook as usual), and [`Self::reconcile`] picks up
    /// anything newly matched against the ports already known.
    ///
    /// `[backends]` can't be changed this way -- starting/stopping a
    /// backend mid-run isn't supported, so that section is ignored on
    /// reload (with a warning) and a full restart is required instead.
    fn reload(&mut self) {
        let new_config = match Config::load(&self.config_path) {
            Ok(cfg) => cfg,
            Err(err) => {
                warn!(path = %self.config_path.display(), error = %err, "failed to reload config; keeping existing rules");
                return;
            }
        };

        if new_config.backends != self.backends_config {
            warn!("[backends] changed in config but can't be hot-reloaded; restart the daemon for this to take effect");
        }

        let all_ports: Vec<PortInfo> = self.ports.values().cloned().collect();
        let still_desired: HashSet<(PortId, PortId)> =
            compute_desired_connections(&all_ports, &new_config.rules)
                .into_iter()
                .map(|c| (c.source, c.dest))
                .collect();
        let stale: Vec<(PortId, PortId)> = self
            .active
            .keys()
            .filter(|key| !still_desired.contains(*key))
            .cloned()
            .collect();

        self.rules_by_name = new_config
            .rules
            .iter()
            .map(|r| (r.name.clone(), r.clone()))
            .collect();
        self.rules = new_config.rules.clone();
        self.lua_timeout = Duration::from_millis(new_config.lua.timeout_ms);
        self.disconnect_on_shutdown = new_config.disconnect_on_shutdown;

        for (source, dest) in stale {
            self.teardown(source, dest);
        }
        self.reconcile();

        info!(rules = self.rules.len(), "config reloaded");
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

    /// Disconnects every connection currently tracked as active, running
    /// each one's `on_disconnect` hook along the way, same as if each
    /// port involved had just disappeared. Used on a clean shutdown when
    /// `disconnect_on_shutdown` is enabled.
    pub fn disconnect_all(&mut self) {
        let keys: Vec<(PortId, PortId)> = self.active.keys().cloned().collect();
        for (source, dest) in keys {
            self.teardown(source, dest);
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
    use crate::config::{BackendsConfig, LuaConfig, RuleBackend, RuleKind};
    use crate::port::{PortDirection, PortKind};
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

    fn rule(name: &str, output: &str, input: &str) -> Rule {
        Rule {
            name: name.to_string(),
            backend: RuleBackend::Alsa,
            kind: RuleKind::Midi,
            output: Regex::new(output).unwrap(),
            input: Regex::new(input).unwrap(),
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
            kind: PortKind::Midi,
        }
    }

    fn config(rules: Vec<Rule>) -> Config {
        Config {
            backends: BackendsConfig {
                alsa: true,
                pipewire: true,
            },
            lua: LuaConfig { timeout_ms: 1000 },
            disconnect_on_shutdown: true,
            rules,
        }
    }

    fn engine_with(
        rules: Vec<Rule>,
        backend: RecordingBackend,
    ) -> (Engine, crossbeam_channel::Sender<BackendEvent>) {
        engine_with_path(PathBuf::from("test.toml"), rules, backend)
    }

    fn engine_with_path(
        config_path: PathBuf,
        rules: Vec<Rule>,
        backend: RecordingBackend,
    ) -> (Engine, crossbeam_channel::Sender<BackendEvent>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut handles: HashMap<Backend, Box<dyn BackendHandle>> = HashMap::new();
        handles.insert(Backend::Alsa, Box::new(backend));
        (Engine::new(config_path, &config(rules), handles, rx), tx)
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
    fn disconnect_all_tears_down_every_active_connection() {
        let backend = RecordingBackend::new("alsa");
        let (mut engine, _tx) = engine_with(
            vec![rule("a", "^OutA", "^InA"), rule("b", "^OutB", "^InB")],
            backend.clone(),
        );

        let source_a = port(1, 0, "OutA", "p", true, false);
        let dest_a = port(2, 0, "InA", "p", false, true);
        let source_b = port(3, 0, "OutB", "p", true, false);
        let dest_b = port(4, 0, "InB", "p", false, true);
        engine.handle_event(BackendEvent::PortAdded(source_a.clone()));
        engine.handle_event(BackendEvent::PortAdded(dest_a.clone()));
        engine.handle_event(BackendEvent::PortAdded(source_b.clone()));
        engine.handle_event(BackendEvent::PortAdded(dest_b.clone()));
        assert_eq!(engine.active.len(), 2);

        engine.disconnect_all();

        assert!(engine.active.is_empty());
        assert_eq!(
            backend
                .calls()
                .iter()
                .filter(|c| matches!(c, RecordedCall::Disconnect(..)))
                .count(),
            2
        );
        assert!(backend.calls().contains(&RecordedCall::Disconnect(
            source_a.id.clone(),
            dest_a.id.clone()
        )));
        assert!(backend.calls().contains(&RecordedCall::Disconnect(
            source_b.id.clone(),
            dest_b.id.clone()
        )));
    }

    #[test]
    fn disconnect_all_on_an_idle_engine_is_a_harmless_no_op() {
        let backend = RecordingBackend::new("alsa");
        let (mut engine, _tx) = engine_with(vec![rule("r", "^Out", "^In")], backend.clone());

        engine.disconnect_all();
        assert!(backend.calls().is_empty());
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
        let (_reload_tx, reload_rx) = crossbeam_channel::unbounded();

        shutdown_tx.send(()).unwrap();
        engine.run(&shutdown_rx, &reload_rx);
        // If we get here, `run` returned promptly as expected.
    }

    #[test]
    fn run_returns_when_event_channel_closes() {
        let backend = RecordingBackend::new("alsa");
        let (mut engine, events_tx) = engine_with(vec![rule("r", "^Out", "^In")], backend);
        let (_shutdown_tx, shutdown_rx) = crossbeam_channel::unbounded::<()>();
        let (_reload_tx, reload_rx) = crossbeam_channel::unbounded();

        drop(events_tx);
        engine.run(&shutdown_rx, &reload_rx);
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

    #[test]
    fn reload_tears_down_connections_no_longer_matched_by_the_new_rules() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
            [[rule]]
            name = "r2"
            backend = "alsa"
            output = "^Nothing"
            input = "^Matches"
            "#,
        )
        .unwrap();

        let backend = RecordingBackend::new("alsa");
        let (mut engine, _tx) =
            engine_with_path(config_path, vec![rule("r", "^Out", "^In")], backend.clone());

        let source = port(1, 0, "Out", "p", true, false);
        let dest = port(2, 0, "In", "p", false, true);
        engine.handle_event(BackendEvent::PortAdded(source.clone()));
        engine.handle_event(BackendEvent::PortAdded(dest.clone()));
        assert_eq!(backend.calls().len(), 1, "connected under the old rule");

        engine.reload();

        assert!(engine.active.is_empty());
        assert_eq!(
            backend.calls(),
            vec![
                RecordedCall::Connect(source.id.clone(), dest.id.clone()),
                RecordedCall::Disconnect(source.id, dest.id),
            ]
        );
    }

    #[test]
    fn reload_picks_up_newly_matching_connections() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
            [[rule]]
            name = "r"
            backend = "alsa"
            output = "^Out"
            input = "^In"
            "#,
        )
        .unwrap();

        let backend = RecordingBackend::new("alsa");
        // Starts with a rule that matches nothing yet present.
        let (mut engine, _tx) = engine_with_path(
            config_path,
            vec![rule("r", "^Nothing", "^Matches")],
            backend.clone(),
        );

        let source = port(1, 0, "Out", "p", true, false);
        let dest = port(2, 0, "In", "p", false, true);
        engine.handle_event(BackendEvent::PortAdded(source.clone()));
        engine.handle_event(BackendEvent::PortAdded(dest.clone()));
        assert!(backend.calls().is_empty(), "old rule doesn't match either port");

        engine.reload();

        assert_eq!(
            backend.calls(),
            vec![RecordedCall::Connect(source.id, dest.id)]
        );
    }

    #[test]
    fn reload_keeps_old_rules_when_new_config_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "this is not valid toml[[[").unwrap();

        let backend = RecordingBackend::new("alsa");
        let (mut engine, _tx) =
            engine_with_path(config_path, vec![rule("r", "^Out", "^In")], backend.clone());

        let source = port(1, 0, "Out", "p", true, false);
        let dest = port(2, 0, "In", "p", false, true);
        engine.handle_event(BackendEvent::PortAdded(source.clone()));
        engine.handle_event(BackendEvent::PortAdded(dest.clone()));
        assert_eq!(backend.calls().len(), 1);

        engine.reload();

        // The bad file was rejected, so the existing connection (made
        // under the still-active old rule) stays up.
        assert_eq!(engine.active.len(), 1);
        assert_eq!(backend.calls().len(), 1);
    }
}
