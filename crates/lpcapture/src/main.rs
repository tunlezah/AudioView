//! `lpcapture` — record, replay and manage shairport-sync metadata fixtures.
//!
//! Recording real sessions is the point: several code semantics stay
//! unverified until we have captures from a real phone (DESIGN §4.3), and
//! replaying them is how `artd`'s state machine gets tested without hardware.

use lpcapture::{fifo, fixture, synth};

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "lpcapture", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Record the live metadata pipe to a file, optionally passing it on.
    Tee(TeeArgs),
    /// Write a fixture into a pipe (or stdout) for replay.
    Replay(ReplayArgs),
    /// Externalise artwork payloads so a capture is small enough for git.
    Pack(PackArgs),
    /// Reverse `pack`, rehydrating artwork from `fixtures/art/`.
    Unpack(PackArgs),
    /// Regenerate or verify the golden event files beside each fixture.
    Golden(GoldenArgs),
    /// Write the synthetic fixture set.
    Synth(SynthArgs),
    /// Print a fixture's decoded events.
    Dump(DumpArgs),
}

#[derive(clap::Args)]
struct TeeArgs {
    /// The shairport-sync metadata pipe to read.
    #[arg(long, default_value = "/tmp/shairport-sync-metadata")]
    input: PathBuf,
    /// Where to write the raw capture.
    #[arg(long, short)]
    output: PathBuf,
    /// Downstream FIFO to forward bytes to, so artd keeps working live.
    #[arg(long)]
    fanout: Option<PathBuf>,
    /// Also write a `.timing` sidecar recording inter-read delays.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    timing: bool,
    /// Stop after this many seconds with no data.
    #[arg(long)]
    idle_timeout: Option<u64>,
}

#[derive(clap::Args)]
struct ReplayArgs {
    fixture: PathBuf,
    /// Destination FIFO or file. Defaults to stdout.
    #[arg(long)]
    to: Option<PathBuf>,
    /// Replay using the recorded timing sidecar, if present.
    #[arg(long)]
    realtime: bool,
    /// Divide recorded delays by this factor.
    #[arg(long, default_value_t = 1.0)]
    speed: f64,
    /// Split the stream into chunks of this many bytes, to exercise a
    /// reader's handling of arbitrary boundaries.
    #[arg(long)]
    chunk: Option<usize>,
}

#[derive(clap::Args)]
struct PackArgs {
    input: PathBuf,
    #[arg(long, short)]
    output: Option<PathBuf>,
    /// Defaults to an `art/` directory beside the fixture's parent.
    #[arg(long)]
    art_dir: Option<PathBuf>,
}

#[derive(clap::Args)]
struct GoldenArgs {
    /// Fixtures to process. Defaults to everything in `fixtures/sessions`.
    fixtures: Vec<PathBuf>,
    /// Verify instead of rewriting; exits non-zero on a mismatch.
    #[arg(long)]
    check: bool,
}

#[derive(clap::Args)]
struct SynthArgs {
    #[arg(long, default_value = "fixtures/sessions")]
    out_dir: PathBuf,
}

#[derive(clap::Args)]
struct DumpArgs {
    fixture: PathBuf,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Tee(a) => tee(a),
        Command::Replay(a) => replay(a),
        Command::Pack(a) => pack(a),
        Command::Unpack(a) => unpack(a),
        Command::Golden(a) => golden(a),
        Command::Synth(a) => synth_cmd(a),
        Command::Dump(a) => dump(a),
    }
}

fn tee(a: TeeArgs) -> Result<()> {
    let mut sink = fifo::CaptureSink::create(&a.output, a.timing)?;
    let mut fanout = match &a.fanout {
        Some(p) => Some(fifo::Fanout::create(p)?),
        None => None,
    };

    eprintln!("lpcapture: reading {}", a.input.display());
    if let Some(p) = &a.fanout {
        eprintln!("lpcapture: forwarding to {}", p.display());
    }
    eprintln!("lpcapture: writing {} (ctrl-c to stop)", a.output.display());

    let reopens = fifo::read_loop(&a.input, a.idle_timeout, |chunk| {
        sink.write(chunk)?;
        if let Some(f) = fanout.as_mut() {
            f.write(chunk)?;
        }
        Ok(())
    })?;

    let bytes = sink.finish()?;
    eprintln!("lpcapture: captured {bytes} bytes across {reopens} writer session(s)");
    Ok(())
}

fn replay(a: ReplayArgs) -> Result<()> {
    let bytes = fixture::load(&a.fixture)?;
    let timing = fifo::Timing::load(&a.fixture.with_extension("timing"))?;

    let mut out: Box<dyn Write> = match &a.to {
        Some(p) => Box::new(fifo::open_for_write(p)?),
        None => Box::new(std::io::stdout().lock()),
    };

    if a.realtime && timing.is_none() {
        eprintln!("lpcapture: no timing sidecar; replaying at full speed");
    }

    let chunks: Vec<(usize, usize, u64)> = match (&timing, a.realtime) {
        (Some(t), true) => t.chunks(bytes.len()),
        _ => {
            let size = a.chunk.unwrap_or(bytes.len()).max(1);
            (0..bytes.len())
                .step_by(size)
                .map(|off| (off, (off + size).min(bytes.len()), 0))
                .collect()
        }
    };

    for (start, end, delay_ms) in chunks {
        if delay_ms > 0 && a.speed > 0.0 {
            let scaled = (delay_ms as f64 / a.speed) as u64;
            std::thread::sleep(std::time::Duration::from_millis(scaled));
        }
        out.write_all(&bytes[start..end])?;
        out.flush()?;
    }
    Ok(())
}

fn art_dir_for(explicit: &Option<PathBuf>, fixture: &Path) -> PathBuf {
    explicit
        .clone()
        .unwrap_or_else(|| fixture::default_art_dir(fixture))
}

fn pack(a: PackArgs) -> Result<()> {
    let raw = std::fs::read(&a.input).with_context(|| format!("reading {}", a.input.display()))?;
    let art_dir = art_dir_for(&a.art_dir, &a.input);
    let (packed, digests) = fixture::pack(&raw, &art_dir)?;

    // Refuse to publish a fixture that does not survive the round trip: a
    // silently lossy fixture would weaken every test built on it.
    let restored = fixture::unpack(&packed, &art_dir)?;
    if restored != fixture::canonicalise(&raw)? {
        bail!(
            "pack/unpack did not round-trip; refusing to write {}",
            a.input.display()
        );
    }

    let out = a.output.unwrap_or(a.input);
    std::fs::write(&out, &packed)?;
    eprintln!(
        "lpcapture: {} bytes -> {} bytes, {} artwork file(s) in {}",
        raw.len(),
        packed.len(),
        digests.len(),
        art_dir.display()
    );
    Ok(())
}

fn unpack(a: PackArgs) -> Result<()> {
    let packed =
        std::fs::read(&a.input).with_context(|| format!("reading {}", a.input.display()))?;
    let art_dir = art_dir_for(&a.art_dir, &a.input);
    let raw = fixture::unpack(&packed, &art_dir)?;
    match a.output {
        Some(p) => std::fs::write(p, &raw)?,
        None => std::io::stdout().lock().write_all(&raw)?,
    }
    Ok(())
}

fn golden(a: GoldenArgs) -> Result<()> {
    let fixtures = if a.fixtures.is_empty() {
        discover("fixtures/sessions")?
    } else {
        a.fixtures
    };
    if fixtures.is_empty() {
        bail!("no fixtures found");
    }

    let mut failed = Vec::new();
    for f in &fixtures {
        let bytes = fixture::load(f)?;
        let rendered = fixture::render_golden(&fixture::events(&bytes))?;
        let path = fixture::golden_path(f);
        if a.check {
            let existing = std::fs::read_to_string(&path).unwrap_or_default();
            if existing != rendered {
                failed.push(path);
            }
        } else {
            std::fs::write(&path, &rendered)?;
            println!("wrote {}", path.display());
        }
    }

    if !failed.is_empty() {
        for p in &failed {
            eprintln!("golden mismatch: {}", p.display());
        }
        bail!(
            "{} golden file(s) out of date; run `lpcapture golden` to update",
            failed.len()
        );
    }
    Ok(())
}

fn synth_cmd(a: SynthArgs) -> Result<()> {
    std::fs::create_dir_all(&a.out_dir)?;
    let art_dir = fixture::default_art_dir(&a.out_dir.join("placeholder"));

    for session in synth::all() {
        let path = a.out_dir.join(format!("{}.pipe", session.name));
        let (packed, _) = fixture::pack(&session.bytes, &art_dir)?;
        std::fs::write(&path, &packed)?;

        let golden = fixture::render_golden(&fixture::events(&session.bytes))?;
        std::fs::write(fixture::golden_path(&path), golden)?;

        println!(
            "{:<20} {:>6} bytes  {}",
            session.name,
            packed.len(),
            session.description
        );
    }
    Ok(())
}

fn dump(a: DumpArgs) -> Result<()> {
    let bytes = fixture::load(&a.fixture)?;
    for ev in fixture::events(&bytes) {
        match ev.detail {
            Some(d) => println!("{:<18} {}", ev.event, d),
            None => println!("{}", ev.event),
        }
    }
    Ok(())
}

fn discover(dir: &str) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(out);
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "pipe") {
            out.push(p);
        }
    }
    out.sort();
    Ok(out)
}
