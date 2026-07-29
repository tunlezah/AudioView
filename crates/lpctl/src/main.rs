//! `lpctl` — command-line client for `artd`.
//!
//! `lpctl watch` is the milestone-2 deliverable: play something from a phone
//! and watch the state machine track it, with no renderer or Pi involved.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use lpframe_proto::{ClientMessage, DisplayPower, ServerMessage, State};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Parser)]
#[command(name = "lpctl", version, about = "Talk to artd")]
struct Cli {
    /// The artd socket.
    #[arg(long, default_value = lpframe_proto::DEFAULT_SOCKET)]
    socket: PathBuf,
    /// Writable overrides, whose directory also holds the web password file.
    #[arg(long, default_value = lpframe_config::DEFAULT_LOCAL_PATH)]
    config_local: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print state changes as they happen.
    Watch {
        /// Print raw NDJSON instead of a summary.
        #[arg(long)]
        raw: bool,
    },
    /// Print the current state once and exit.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Round-trip a ping.
    Ping,
    /// Request a display power state.
    Display { value: String },
    /// Display an image immediately (artd must be running with --debug).
    Inject { path: PathBuf },
    /// Reprint the generated web interface password.
    WebPassword,
}

/// The one place on the device the web password exists in plaintext.
///
/// Only the Argon2id hash is in the configuration, so this file is the only
/// way to recover a password nobody wrote down. Losing it means deleting
/// `web.password_hash` from the local override and restarting `artd`, which
/// generates a new one.
const PASSWORD_FILE: &str = "web-password.txt";

fn print_web_password(config_local: &std::path::Path) -> Result<()> {
    let path = config_local
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join(PASSWORD_FILE);
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading {}. It exists only once authentication has generated a password; \
             if artd could not write it, the password is in the startup log instead.",
            path.display()
        )
    })?;
    println!("{}", text.trim_end());
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Answered from the filesystem, before anything tries to reach a daemon:
    // needing the password most likely means the web interface is the thing
    // that is not working.
    if matches!(cli.command, Command::WebPassword) {
        return print_web_password(&cli.config_local);
    }

    let stream = UnixStream::connect(&cli.socket)
        .await
        .with_context(|| format!("connecting to {}", cli.socket.display()))?;
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    let hello = ClientMessage::Hello {
        client: "lpctl".into(),
        proto: lpframe_proto::PROTOCOL_VERSION,
    };
    write_half
        .write_all(format!("{}\n", serde_json::to_string(&hello)?).as_bytes())
        .await?;

    match cli.command {
        Command::Watch { raw } => {
            let mut last: Option<State> = None;
            while let Some(line) = lines.next_line().await? {
                if raw {
                    println!("{line}");
                    continue;
                }
                if let Ok(ServerMessage::State { seq, state, .. }) = serde_json::from_str(&line) {
                    print_summary(seq, &state, last.as_ref());
                    last = Some(*state);
                }
            }
        }
        Command::Status { json } => {
            send(&mut write_half, &ClientMessage::GetState).await?;
            while let Some(line) = lines.next_line().await? {
                if let Ok(ServerMessage::State { seq, state, .. }) = serde_json::from_str(&line) {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&state)?);
                    } else {
                        print_summary(seq, &state, None);
                    }
                    return Ok(());
                }
            }
        }
        Command::Ping => {
            send(&mut write_half, &ClientMessage::Ping).await?;
            while let Some(line) = lines.next_line().await? {
                if matches!(
                    serde_json::from_str::<ServerMessage>(&line),
                    Ok(ServerMessage::Pong { .. })
                ) {
                    println!("pong");
                    return Ok(());
                }
            }
            anyhow::bail!("no pong before the connection closed");
        }
        Command::Display { value } => {
            let value = match value.as_str() {
                "on" => DisplayPower::On,
                "ambient" => DisplayPower::Ambient,
                "off" => DisplayPower::Off,
                other => anyhow::bail!("unknown display state {other:?}; use on, ambient or off"),
            };
            send(&mut write_half, &ClientMessage::SetDisplay { value }).await?;
        }
        Command::Inject { path } => {
            let path = std::fs::canonicalize(&path)
                .with_context(|| format!("resolving {}", path.display()))?;
            send(&mut write_half, &ClientMessage::InjectArtwork { path }).await?;
        }
        // Handled before the socket was opened.
        Command::WebPassword => unreachable!(),
    }
    Ok(())
}

async fn send(w: &mut tokio::net::unix::OwnedWriteHalf, msg: &ClientMessage) -> Result<()> {
    w.write_all(format!("{}\n", serde_json::to_string(msg)?).as_bytes())
        .await?;
    Ok(())
}

fn print_summary(seq: u64, state: &State, previous: Option<&State>) {
    let t = state.track.as_ref();
    let now = t
        .map(|t| {
            format!(
                "{} — {}",
                t.artist.as_deref().unwrap_or("?"),
                t.title.as_deref().unwrap_or("?")
            )
        })
        .unwrap_or_else(|| "—".into());

    let art = state
        .artwork
        .as_ref()
        .map(|a| {
            let dims = match (a.width, a.height) {
                (Some(w), Some(h)) => format!("{w}×{h}"),
                _ => "?".into(),
            };
            format!("art r{} {dims} {}", a.revision, a.source_label())
        })
        .unwrap_or_else(|| "no art".into());

    // Mark the fields that actually changed, so a long watch is readable.
    let mark = |changed: bool| if changed { "*" } else { " " };
    let (track_changed, art_changed) = match previous {
        Some(p) => (
            p.track.as_ref().map(|t| &t.id) != t.map(|t| &t.id),
            p.artwork.as_ref().map(|a| a.revision) != state.artwork.as_ref().map(|a| a.revision),
        ),
        None => (false, false),
    };

    println!(
        "[{seq:>4}] {:<8} {}{:<44} {}{:<28} amp:{} display:{}",
        state.playback.as_str(),
        mark(track_changed),
        now,
        mark(art_changed),
        art,
        if state.power.amp { "on " } else { "off" },
        state.power.display.as_str(),
    );
}

trait SourceLabel {
    fn source_label(&self) -> &'static str;
}

impl SourceLabel for lpframe_proto::Artwork {
    fn source_label(&self) -> &'static str {
        use lpframe_proto::ArtworkSource as S;
        match self.source {
            S::Airplay => "airplay",
            S::Itunes => "itunes",
            S::CoverArtArchive => "caa",
            S::Cache => "cache",
            S::Placeholder => "placeholder",
        }
    }
}
