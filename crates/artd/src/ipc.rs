//! The Unix socket server: NDJSON, one message per line.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lpframe_proto::{ClientMessage, ServerMessage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::hub::Hub;

/// Requests from clients that the core has to act on.
///
/// Also the channel the web interface uses: a settings change that can be
/// applied without a restart arrives here as [`Command::Reload`], so there is
/// one place where the running configuration is replaced rather than two.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    SetDisplay(lpframe_proto::DisplayPower),
    SetAmp(bool),
    InjectArtwork(PathBuf),
    Reload(Box<lpframe_config::Config>),
}

pub struct Server {
    listener: UnixListener,
    path: PathBuf,
}

impl Server {
    pub fn bind(path: &Path) -> Result<Server> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        // A stale socket from an unclean exit would make bind fail; removing
        // it is safe because two artd instances are not a supported setup and
        // systemd will not start a second one.
        match std::fs::metadata(path) {
            Ok(m) if is_socket(&m) => {
                let _ = std::fs::remove_file(path);
            }
            Ok(_) => anyhow::bail!("{} exists and is not a socket", path.display()),
            Err(_) => {}
        }

        let listener =
            UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
        set_socket_permissions(path)?;
        Ok(Server {
            listener,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accept connections until cancelled.
    pub async fn run(
        self,
        hub: Hub,
        commands: tokio::sync::mpsc::Sender<Command>,
        debug: bool,
    ) -> Result<()> {
        loop {
            let (stream, _) = match self.listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("accept failed: {e}");
                    continue;
                }
            };
            let hub = hub.clone();
            let commands = commands.clone();
            tokio::spawn(async move {
                if let Err(e) = serve(stream, hub, commands, debug).await {
                    tracing::debug!("client disconnected: {e}");
                }
            });
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn is_socket(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;
    m.file_type().is_socket()
}

fn set_socket_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // 0660: the renderer runs as the same user/group. Not world-writable,
    // because anything that can write here can blank the display.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
        .with_context(|| format!("setting permissions on {}", path.display()))
}

async fn serve(
    stream: UnixStream,
    hub: Hub,
    commands: tokio::sync::mpsc::Sender<Command>,
    debug: bool,
) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    // Subscribe before sending the snapshot, so a change landing in between
    // is queued rather than missed.
    let mut updates = hub.subscribe();

    write_half
        .write_all(ServerMessage::hello().to_line().as_bytes())
        .await?;
    write_half
        .write_all(hub.current_message().to_line().as_bytes())
        .await?;

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { return Ok(()) };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<ClientMessage>(&line) {
                    Ok(msg) => {
                        if let Some(reply) =
                            handle(msg, &hub, &commands, debug).await
                        {
                            write_half.write_all(reply.to_line().as_bytes()).await?;
                        }
                    }
                    // Malformed input from a client is that client's problem;
                    // it must not take down the connection or the daemon.
                    Err(e) => tracing::debug!("ignoring unparseable client message: {e}"),
                }
            }
            update = updates.recv() => {
                match update {
                    Ok(msg) => write_half.write_all(msg.to_line().as_bytes()).await?,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        // Every message is a full snapshot, so a lagging
                        // client recovers simply by receiving the next one.
                        tracing::warn!("client lagged {n} snapshot(s)");
                        write_half
                            .write_all(hub.current_message().to_line().as_bytes())
                            .await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        }
    }
}

async fn handle(
    msg: ClientMessage,
    hub: &Hub,
    commands: &tokio::sync::mpsc::Sender<Command>,
    debug: bool,
) -> Option<ServerMessage> {
    match msg {
        ClientMessage::Ping => Some(ServerMessage::Pong {
            v: lpframe_proto::ENVELOPE_VERSION,
        }),
        ClientMessage::GetState => Some(hub.current_message()),
        ClientMessage::Hello { client, proto } => {
            tracing::info!("client {client:?} connected, proto {proto}");
            None
        }
        ClientMessage::SetDisplay { value } => {
            let _ = commands.send(Command::SetDisplay(value)).await;
            None
        }
        ClientMessage::InjectArtwork { path } => {
            if debug {
                let _ = commands.send(Command::InjectArtwork(path)).await;
            } else {
                tracing::warn!("inject_artwork refused: not a debug build");
            }
            None
        }
        // Forward compatibility: an unrecognised message is ignored, never
        // fatal, so a newer client can talk to an older daemon.
        ClientMessage::Unknown => None,
    }
}
