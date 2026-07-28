//! `lprender` — the LP Frame fullscreen artwork renderer.
//!
//! Two sources, the same render path behind both: `artd` over its Unix
//! socket, which is the device, and `--slideshow`, which is a directory of
//! images with no daemon and no Pi. The slideshow stays useful forever as a
//! smoke test (DESIGN §6.4).

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use lpframe_config::Config;
use lprender::app::App;
use lprender::source::{ArtdSource, Slideshow, Source};

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum BackendKind {
    /// KMS/DRM on the device.
    Drm,
    /// A window, for development on a laptop.
    Sdl2,
    /// Surfaceless EGL rendering to PNG files. Used by CI and for debugging.
    Headless,
}

#[derive(Parser)]
#[command(name = "lprender", version, about = "LP Frame artwork renderer")]
struct Cli {
    #[arg(long, default_value = lpframe_config::DEFAULT_BASE_PATH)]
    config: PathBuf,
    #[arg(long, default_value = lpframe_config::DEFAULT_LOCAL_PATH)]
    config_local: PathBuf,

    #[arg(long, value_enum, default_value_t = BackendKind::Drm)]
    backend: BackendKind,

    /// Cycle the images in this directory instead of following artd.
    #[arg(long)]
    slideshow: Option<PathBuf>,
    /// Seconds between slideshow images.
    #[arg(long, default_value_t = 5.0)]
    interval: f64,

    /// Panel size for the headless and SDL2 backends.
    #[arg(long, default_value = "720x720")]
    size: String,

    /// Headless: write each frame here as a PNG.
    #[arg(long)]
    dump: Option<PathBuf>,
    /// Headless: stop after this many frames.
    #[arg(long, default_value_t = 60)]
    frames: u32,
    /// Headless: milliseconds of simulated time per frame.
    #[arg(long, default_value_t = 16)]
    frame_ms: u64,

    /// Report what the display stack would select, then exit.
    #[arg(long)]
    probe: bool,
}

fn parse_size(s: &str) -> Result<(u32, u32)> {
    s.split_once('x')
        .and_then(|(a, b)| Some((a.trim().parse().ok()?, b.trim().parse().ok()?)))
        .filter(|(w, h): &(u32, u32)| *w > 0 && *h > 0)
        .ok_or_else(|| anyhow::anyhow!("--size {s:?} is not WIDTHxHEIGHT"))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let loaded = Config::load(&cli.config, &cli.config_local)?;
    let cfg = loaded.config;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| cfg.logging.level.clone().into()),
        )
        .with_target(false)
        .init();

    if cli.probe {
        return probe(&cfg);
    }

    let mut source: Box<dyn Source> = match &cli.slideshow {
        Some(dir) => {
            let s = Slideshow::new(dir, (cli.interval * 1000.0) as u64)?;
            tracing::info!("slideshow: {} image(s) from {}", s.len(), dir.display());
            Box::new(s)
        }
        // Not an error if artd is not up: the two services are ordered but
        // not bound together, so the renderer waits on a black screen and
        // connects when the daemon appears (DESIGN §8.1).
        None => {
            tracing::info!("following artd at {}", cfg.ipc.socket.display());
            Box::new(ArtdSource::new(&cfg.ipc.socket))
        }
    };

    match cli.backend {
        BackendKind::Headless => run_headless(&cli, cfg, source.as_mut()),
        BackendKind::Sdl2 => run_sdl2(&cli, cfg, source.as_mut()),
        BackendKind::Drm => run_drm(&cli, cfg, source.as_mut()),
    }
}

/// Report the display stack's choices without rendering anything.
///
/// The first thing to run on a new panel: it says whether the mode you asked
/// for actually exists before anything tries to set it.
fn probe(cfg: &Config) -> Result<()> {
    #[cfg(feature = "backend-drm")]
    {
        use lprender::backend::drm as kms;
        let card = kms::Card::open(&cfg.display.card)?;
        println!("card       {}", card.path().display());
        let connector = card.connector(&cfg.display.connector)?;
        println!("connector  {}", kms::connector_name(&connector));
        println!("modes:");
        for m in connector.modes() {
            let (w, h) = m.size();
            println!("  {w}x{h}@{}", m.vrefresh());
        }
        let mode = kms::choose_mode(&connector, &cfg.display.mode)?;
        println!("selected   {}", kms::describe(&connector, &mode));
        Ok(())
    }
    #[cfg(not(feature = "backend-drm"))]
    {
        let _ = cfg;
        anyhow::bail!("--probe needs the backend-drm feature")
    }
}

fn run_headless(cli: &Cli, cfg: Config, source: &mut dyn Source) -> Result<()> {
    let (w, h) = parse_size(&cli.size)?;
    let mut hl = lprender::backend::headless::Headless::new(w, h)
        .context("creating a headless EGL context")?;
    let mut app = App::new(cfg, (w, h));

    if let Some(dir) = &cli.dump {
        std::fs::create_dir_all(dir)?;
    }

    let mut drawn = 0u32;
    for i in 0..cli.frames {
        // Simulated time, so a dump is reproducible rather than depending on
        // how fast the machine happens to be.
        let now = i as u64 * cli.frame_ms;
        app.update_at(hl.renderer(), source, now);
        // Simulated time outruns the decode thread; wait for it rather than
        // dumping black frames that misrepresent what the device would show.
        app.settle(hl.renderer(), 2000, now);
        app.draw(hl.renderer(), now);
        drawn += 1;

        if let Some(dir) = &cli.dump {
            let px = hl.read();
            image::RgbaImage::from_raw(w, h, px)
                .ok_or_else(|| anyhow::anyhow!("frame buffer size mismatch"))?
                .save(dir.join(format!("frame-{i:04}.png")))?;
        }
    }

    match &cli.dump {
        Some(dir) => println!("wrote {drawn} frame(s) to {}", dir.display()),
        None => println!("rendered {drawn} frame(s)"),
    }
    Ok(())
}

#[cfg(feature = "backend-sdl2")]
fn run_sdl2(cli: &Cli, cfg: Config, source: &mut dyn Source) -> Result<()> {
    let (w, h) = parse_size(&cli.size)?;
    let mut win = lprender::backend::sdl::Window::new(w, h, "LP Frame")?;
    let mut app = App::new(cfg, (w, h));

    while win.pump() {
        let drew = app.step(win.renderer(), source);
        if drew {
            win.present();
        }
        // Sleep until something can actually change, rather than spinning.
        // The same discipline as the device: a static image costs nothing.
        let now = app.now_ms();
        let sleep = app
            .next_wakeup_ms(source)
            .map(|t| t.saturating_sub(now).min(250))
            .unwrap_or(50);
        std::thread::sleep(std::time::Duration::from_millis(sleep.max(1)));
    }
    Ok(())
}

#[cfg(not(feature = "backend-sdl2"))]
fn run_sdl2(_cli: &Cli, _cfg: Config, _source: &mut dyn Source) -> Result<()> {
    anyhow::bail!("this binary was built without the backend-sdl2 feature")
}

#[cfg(feature = "backend-drm")]
fn run_drm(_cli: &Cli, cfg: Config, _source: &mut dyn Source) -> Result<()> {
    use lprender::backend::drm as kms;
    // Device discovery and mode selection are implemented and reachable via
    // --probe; the GBM surface, atomic commit and page-flip loop are the
    // remaining piece of milestone 3. Fail with something actionable rather
    // than a black screen.
    let card = kms::Card::open(&cfg.display.card)?;
    let connector = card.connector(&cfg.display.connector)?;
    let mode = kms::choose_mode(&connector, &cfg.display.mode)?;
    let crtc = kms::choose_crtc(&card, &connector)?;
    anyhow::bail!(
        "display stack resolved ({}, crtc {crtc:?}) but the DRM present path \
         is not wired up yet. Use --backend sdl2 or --backend headless; run \
         --probe to inspect the panel.",
        kms::describe(&connector, &mode)
    )
}

#[cfg(not(feature = "backend-drm"))]
fn run_drm(_cli: &Cli, _cfg: Config, _source: &mut dyn Source) -> Result<()> {
    anyhow::bail!("this binary was built without the backend-drm feature")
}
