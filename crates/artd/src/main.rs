//! `artd` — the LP Frame metadata, artwork and power daemon.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use artd::hub::Hub;
use artd::ipc::{Command, Server};
use artd::machine::Input;
use artd::runtime::{AmpBackend, Core, LoggingAmp, MonotonicClock};
use clap::Parser;
use lpframe_config::Config;

#[derive(Parser)]
#[command(name = "artd", version, about = "LP Frame metadata and artwork daemon")]
struct Cli {
    /// Package-owned base configuration.
    #[arg(long, default_value = lpframe_config::DEFAULT_BASE_PATH)]
    config: PathBuf,
    /// Writable overrides, owned by the web interface.
    #[arg(long, default_value = lpframe_config::DEFAULT_LOCAL_PATH)]
    config_local: PathBuf,
    /// Read this instead of the configured metadata pipe.
    #[arg(long)]
    pipe: Option<PathBuf>,
    /// Enable debug-only commands such as artwork injection.
    #[arg(long)]
    debug: bool,
    /// Validate the configuration and exit.
    #[arg(long)]
    check_config: bool,
}

/// Open the amplifier trigger, falling back to logging transitions.
///
/// A GPIO line we cannot take is not worth failing the service for: the
/// device still plays music and still shows artwork, and the alternative is a
/// silent speaker because a pin was busy.
fn open_amp(cfg: &lpframe_config::Amp) -> Box<dyn AmpBackend> {
    if !cfg.enabled {
        tracing::info!("amp trigger disabled by configuration");
        return Box::new(LoggingAmp);
    }
    match artd::gpio::GpioAmp::open(cfg) {
        Ok(amp) => {
            tracing::info!("amp trigger ready on {}", amp.describe());
            Box::new(amp)
        }
        Err(e) => {
            tracing::warn!("amp trigger unavailable, logging transitions only: {e:#}");
            Box::new(LoggingAmp)
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let loaded = Config::load(&cli.config, &cli.config_local)?;
    let mut cfg = loaded.config;
    if let Some(pipe) = cli.pipe {
        cfg.device.metadata_pipe = pipe;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| cfg.logging.level.clone().into()),
        )
        .with_target(false)
        .init();

    if cli.check_config {
        println!("configuration is valid");
        if loaded.overridden.is_empty() {
            println!("no local overrides");
        } else {
            println!("local overrides:");
            for key in &loaded.overridden {
                println!("  {key}");
            }
        }
        return Ok(());
    }

    // Fail before opening anything if the web interface is misconfigured,
    // rather than bringing a device up with half its surface live.
    let web_addr = if cfg.web.enabled {
        Some(artd::web::check_bind(&cfg.web.bind, false)?)
    } else {
        None
    };

    let hub = Hub::new(cfg.logging.event_buffer);
    let clock = Arc::new(MonotonicClock::default());
    let amp = open_amp(&cfg.power.amp);
    let mut core = Core::new(cfg.clone(), hub.clone(), clock, amp)?;

    // The sender is kept here as well as handed to the enricher. With
    // enrichment disabled nothing else holds one, and a closed channel makes
    // `recv()` return immediately — which would turn the select loop below
    // into a spin at full CPU on a device that is meant to cost nothing idle.
    let (enrich_tx, mut enrich_rx) = tokio::sync::mpsc::channel(16);
    let _enrich_tx = enrich_tx.clone();
    // A cache directory we cannot open is not worth failing the service for:
    // the device still plays music and still shows AirPlay art.
    match artd::enrich::Enricher::new(&cfg, Default::default(), enrich_tx) {
        Ok(enricher) => core.set_enricher(enricher),
        Err(e) => tracing::warn!("artwork enrichment is unavailable: {e}"),
    }

    // Publish an initial snapshot so a client that connects before any
    // metadata arrives gets a state rather than silence.
    hub.publish(core.machine().state().clone());

    let (pipe_tx, mut pipe_rx) = tokio::sync::mpsc::channel::<Input>(256);
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<Command>(16);

    let pipe_path = cfg.device.metadata_pipe.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = artd::pipe::run(&pipe_path, pipe_tx) {
            tracing::error!("metadata pipe reader stopped: {e}");
        }
    });

    let server = Server::bind(&cfg.ipc.socket)
        .with_context(|| format!("binding {}", cfg.ipc.socket.display()))?;
    tracing::info!("ipc socket at {}", server.path().display());
    {
        let hub = hub.clone();
        let cmd_tx = cmd_tx.clone();
        let debug = cli.debug;
        tokio::spawn(async move {
            if let Err(e) = server.run(hub, cmd_tx, debug).await {
                tracing::error!("ipc server stopped: {e}");
            }
        });
    }

    if let Some(addr) = web_addr {
        let hub = hub.clone();
        tokio::spawn(async move {
            if let Err(e) = artd::web::serve(hub, addr).await {
                tracing::error!("web interface stopped: {e}");
            }
        });
    }

    tracing::info!("artd ready");

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing SIGTERM handler")?;

    loop {
        // Sleep exactly until the next timer could change something, rather
        // than polling: an idle device should wake only when there is work.
        let sleep = match core.next_deadline_ms() {
            Some(deadline) => Duration::from_millis(deadline.saturating_sub(core.now_ms())),
            None => Duration::from_secs(3600),
        };

        tokio::select! {
            input = pipe_rx.recv() => match input {
                Some(input) => core.handle(input),
                None => {
                    tracing::error!("pipe reader channel closed");
                    break;
                }
            },
            cmd = cmd_rx.recv() => {
                if let Some(cmd) = cmd {
                    core.handle_command(cmd);
                }
            }
            report = enrich_rx.recv() => {
                if let Some(report) = report {
                    core.handle_enrichment(report);
                }
            }
            _ = tokio::time::sleep(sleep) => core.handle(Input::Tick),
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("interrupted; shutting down");
                break;
            }
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM; shutting down");
                break;
            }
        }
    }

    core.shutdown();
    Ok(())
}
