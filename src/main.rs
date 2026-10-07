mod backend;
mod config;
mod engine;
mod lua_hooks;
mod matcher;
mod port;

use std::collections::HashMap;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tracing::{error, info};

use backend::alsa_backend::AlsaBackend;
use backend::pipewire_backend::PipeWireBackend;
use backend::{BackendEvent, BackendHandle};
use config::Config;
use engine::Engine;
use port::{Backend, PortInfo};

#[derive(Parser)]
#[command(
    name = "midi-auto-connector",
    version,
    about = "Auto-connects ALSA and PipeWire MIDI ports by regex rule."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon. This is the default when no subcommand is given.
    Run {
        /// Path to the config file. Defaults to
        /// `$XDG_CONFIG_HOME/midi-auto-connector/config.toml`.
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Validate a config file and exit without starting any backend.
    CheckConfig {
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Print every currently visible MIDI port, for writing rule regexes.
    ListPorts,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "midi_auto_connector=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run { config: None }) {
        Command::Run { config } => run(resolve_config_path(config)),
        Command::CheckConfig { config } => check_config(resolve_config_path(config)),
        Command::ListPorts => list_ports(),
    }
}

fn resolve_config_path(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(default_config_path)
}

fn default_config_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "midi-auto-connector")
        .map(|dirs| dirs.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("/etc/midi-auto-connector/config.toml"))
}

fn check_config(path: PathBuf) {
    match Config::load(&path) {
        Ok(cfg) => println!("{} is valid: {} rule(s)", path.display(), cfg.rules.len()),
        Err(err) => {
            eprintln!("{} is invalid: {err}", path.display());
            std::process::exit(1);
        }
    }
}

fn run(config_path: PathBuf) {
    let config = match Config::load(&config_path) {
        Ok(cfg) => cfg,
        Err(err) => {
            error!(path = %config_path.display(), error = %err, "failed to load config");
            std::process::exit(1);
        }
    };

    let (events_tx, events_rx) = crossbeam_channel::unbounded();
    let mut handles: HashMap<Backend, Box<dyn BackendHandle>> = HashMap::new();
    let mut pipewire_backend: Option<PipeWireBackend> = None;

    if config.backends.alsa {
        match AlsaBackend::start(events_tx.clone()) {
            Ok(backend) => {
                handles.insert(Backend::Alsa, Box::new(backend));
            }
            Err(err) => {
                error!(error = %err, "failed to start ALSA backend; ALSA rules will not run")
            }
        }
    }
    if config.backends.pipewire {
        match PipeWireBackend::start(events_tx.clone()) {
            Ok(backend) => {
                handles.insert(Backend::PipeWire, Box::new(backend.handle()));
                pipewire_backend = Some(backend);
            }
            Err(err) => {
                error!(error = %err, "failed to start PipeWire backend; PipeWire rules will not run")
            }
        }
    }
    drop(events_tx);

    if handles.is_empty() {
        error!("no backend could be started; exiting");
        std::process::exit(1);
    }

    let (shutdown_tx, shutdown_rx) = crossbeam_channel::unbounded();
    spawn_signal_handler(shutdown_tx);

    let mut engine = Engine::new(&config, handles, events_rx);
    info!(rules = config.rules.len(), "midi-auto-connector running");
    engine.run(&shutdown_rx);

    // Any connections made along the way persist on their own: ALSA
    // subscriptions live at the kernel level independent of the client
    // that requested them, and PipeWire links are created with
    // `object.linger = true`. So shutting down here only needs to stop our
    // own threads, not undo anything.
    if let Some(mut pw) = pipewire_backend {
        pw.shutdown();
    }
    info!("midi-auto-connector shut down");
}

fn spawn_signal_handler(shutdown_tx: crossbeam_channel::Sender<()>) {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = match Signals::new([SIGINT, SIGTERM]) {
        Ok(signals) => signals,
        Err(err) => {
            error!(error = %err, "failed to install signal handlers; Ctrl-C/SIGTERM will not shut down cleanly");
            return;
        }
    };

    thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            if signals.forever().next().is_some() {
                info!("received shutdown signal");
                let _ = shutdown_tx.send(());
            }
        })
        .expect("failed to spawn signal-handling thread");
}

fn list_ports() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut started_any = false;

    match AlsaBackend::start(tx.clone()) {
        Ok(backend) => {
            started_any = true;
            std::mem::forget(backend);
        }
        Err(err) => eprintln!("warning: could not start ALSA backend: {err}"),
    }
    match PipeWireBackend::start(tx.clone()) {
        Ok(backend) => {
            started_any = true;
            std::mem::forget(backend);
        }
        Err(err) => eprintln!("warning: could not start PipeWire backend: {err}"),
    }

    if !started_any {
        eprintln!("error: neither backend could be started");
        std::process::exit(1);
    }

    drop(tx);
    thread::sleep(Duration::from_millis(400));

    let mut ports: Vec<PortInfo> = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let BackendEvent::PortAdded(port) = event {
            ports.push(port);
        }
    }
    ports.sort_by_key(|p| p.full_name());

    if ports.is_empty() {
        println!("No MIDI ports found.");
        return;
    }
    for p in &ports {
        println!(
            "[{}] {:<60} source={:<5} sink={:<5}",
            p.backend(),
            p.full_name(),
            p.direction.can_be_source,
            p.direction.can_be_sink
        );
    }
}
