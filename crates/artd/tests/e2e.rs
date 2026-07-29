//! End-to-end: a real `artd` process, a real FIFO, a real Unix socket.
//!
//! The replay tests cover the state machine; this covers the wiring the
//! renderer will actually depend on — snapshot-on-connect, live updates,
//! artwork files appearing on disk, and surviving a writer restart.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use lpframe_proto::{ClientMessage, Playback, ServerMessage, State};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf()
}

struct Daemon {
    child: Child,
    dir: PathBuf,
    socket: PathBuf,
    pipe: PathBuf,
    art_dir: PathBuf,
}

impl Daemon {
    fn start(name: &str) -> Daemon {
        let dir = std::env::temp_dir().join(format!("artd-e2e-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let socket = dir.join("artd.sock");
        let pipe = dir.join("metadata");
        let art_dir = dir.join("art");
        let config = dir.join("config.toml");

        std::fs::write(
            &config,
            format!(
                r#"
[device]
metadata_pipe = "{}"

[ipc]
socket = "{}"
art_dir = "{}"

[web]
enabled = false

[timeouts]
stall = "2s"
session = "4s"

[power.amp]
off_delay = "3s"

[power.display]
blank_after = "2s"
"#,
                pipe.display(),
                socket.display(),
                art_dir.display()
            ),
        )
        .unwrap();

        let child = Command::new(env!("CARGO_BIN_EXE_artd"))
            .arg("--config")
            .arg(&config)
            .arg("--config-local")
            .arg(dir.join("does-not-exist.toml"))
            .arg("--debug")
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawning artd");

        let d = Daemon {
            child,
            dir,
            socket,
            pipe,
            art_dir,
        };
        d.wait_for_socket();
        d
    }

    fn wait_for_socket(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.socket.exists() && std::os::unix::net::UnixStream::connect(&self.socket).is_ok()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("artd never bound {}", self.socket.display());
    }

    /// Write bytes into the FIFO as shairport-sync would, then close —
    /// which is exactly the EOF that tells artd the writer is gone.
    fn write_pipe(&self, bytes: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match std::fs::OpenOptions::new().write(true).open(&self.pipe) {
                Ok(mut f) => {
                    f.write_all(bytes).unwrap();
                    f.flush().unwrap();
                    return;
                }
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Err(e) => panic!("opening {}: {e}", self.pipe.display()),
            }
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Client {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    write: tokio::net::unix::OwnedWriteHalf,
}

impl Client {
    async fn connect(socket: &Path) -> Client {
        let stream = UnixStream::connect(socket).await.expect("connect");
        let (r, write) = stream.into_split();
        Client {
            lines: BufReader::new(r).lines(),
            write,
        }
    }

    async fn send(&mut self, msg: &ClientMessage) {
        self.write
            .write_all(format!("{}\n", serde_json::to_string(msg).unwrap()).as_bytes())
            .await
            .unwrap();
    }

    async fn next(&mut self) -> ServerMessage {
        let line = tokio::time::timeout(Duration::from_secs(5), self.lines.next_line())
            .await
            .expect("timed out waiting for a message")
            .unwrap()
            .expect("connection closed");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad message {line:?}: {e}"))
    }

    /// Wait for a state satisfying `pred`.
    async fn wait_for(&mut self, what: &str, mut pred: impl FnMut(&State) -> bool) -> State {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if let ServerMessage::State { state, .. } = self.next().await {
                if pred(&state) {
                    return *state;
                }
            }
        }
        panic!("never observed: {what}");
    }
}

fn album_fixture() -> Vec<u8> {
    lpcapture::unpack(
        &std::fs::read(repo_root().join("fixtures/sessions/album.pipe")).unwrap(),
        &repo_root().join("fixtures/art"),
    )
    .unwrap()
}

#[tokio::test]
async fn a_new_client_gets_hello_then_a_full_snapshot() {
    let d = Daemon::start("hello");
    let mut c = Client::connect(&d.socket).await;

    match c.next().await {
        ServerMessage::Hello { v, proto, .. } => {
            assert_eq!(v, lpframe_proto::ENVELOPE_VERSION);
            assert_eq!(proto, lpframe_proto::PROTOCOL_VERSION);
        }
        other => panic!("expected hello, got {other:?}"),
    }
    match c.next().await {
        ServerMessage::State { state, .. } => assert_eq!(state.playback, Playback::Idle),
        other => panic!("expected state, got {other:?}"),
    }

    c.send(&ClientMessage::Ping).await;
    assert!(matches!(c.next().await, ServerMessage::Pong { .. }));
}

#[tokio::test]
async fn a_session_replayed_through_the_pipe_reaches_the_socket() {
    let d = Daemon::start("session");
    let mut c = Client::connect(&d.socket).await;

    let pipe = d.pipe.clone();
    let bytes = album_fixture();
    std::thread::spawn(move || {
        // Small writes, so the daemon sees boundaries mid-item.
        let mut f = loop {
            match std::fs::OpenOptions::new().write(true).open(&pipe) {
                Ok(f) => break f,
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        };
        for chunk in bytes.chunks(997) {
            f.write_all(chunk).unwrap();
            f.flush().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
    });

    let playing = c
        .wait_for("playing with a track and artwork", |s| {
            s.playback == Playback::Playing && s.track.is_some() && s.artwork.is_some()
        })
        .await;

    let track = playing.track.unwrap();
    assert_eq!(track.artist.as_deref(), Some("Massive Attack"));
    assert_eq!(track.album.as_deref(), Some("Mezzanine"));
    assert!(playing.power.amp, "amp intent should be on while playing");

    // The renderer reads artwork by path, so the file must genuinely exist,
    // be complete, and live under the configured directory.
    let art = playing.artwork.unwrap();
    assert!(art.path.starts_with(&d.art_dir), "{:?}", art.path);
    let bytes = std::fs::read(&art.path).expect("artwork file missing");
    assert_eq!(bytes.len() as u64, art.bytes);
    assert_eq!(art.width, Some(128));
    assert_eq!(art.height, Some(128));

    // Later tracks bump the revision rather than reusing it.
    let last = c
        .wait_for("the third track", |s| {
            s.track.as_ref().and_then(|t| t.title.as_deref()) == Some("Teardrop")
        })
        .await;
    assert!(last.artwork.unwrap().revision >= 2);
}

#[tokio::test]
async fn the_writer_closing_forces_idle() {
    let d = Daemon::start("eof");
    let mut c = Client::connect(&d.socket).await;

    // A partial session: active, playing, and then the writer vanishes.
    let mut bytes = Vec::new();
    for (kind, code) in [
        (spmeta::codes::kind::SSNC, spmeta::codes::ssnc::ABEG),
        (spmeta::codes::kind::SSNC, spmeta::codes::ssnc::PBEG),
        (spmeta::codes::kind::SSNC, spmeta::codes::ssnc::PFFR),
    ] {
        bytes.extend(spmeta::encode_item(kind, code, b""));
    }
    d.write_pipe(&bytes); // closing the file is the EOF

    c.wait_for("playing", |s| s.playback == Playback::Playing)
        .await;
    let idle = c
        .wait_for("idle after the writer closed", |s| {
            s.playback == Playback::Idle
        })
        .await;

    // The amp holds through its off delay rather than cutting mid-note.
    assert!(idle.power.amp, "amp dropped immediately on EOF");
}

#[tokio::test]
async fn a_late_client_is_immediately_correct() {
    // The whole point of snapshot-on-every-change: no replay, no catch-up.
    let d = Daemon::start("late");

    let mut bytes = Vec::new();
    bytes.extend(spmeta::encode_item(
        spmeta::codes::kind::SSNC,
        spmeta::codes::ssnc::ABEG,
        b"",
    ));
    bytes.extend(spmeta::encode_item(
        spmeta::codes::kind::SSNC,
        spmeta::codes::ssnc::MDST,
        b"",
    ));
    bytes.extend(spmeta::encode_item(
        spmeta::codes::kind::CORE,
        spmeta::codes::dmap::MINM,
        b"Late Joiner",
    ));
    bytes.extend(spmeta::encode_item(
        spmeta::codes::kind::SSNC,
        spmeta::codes::ssnc::MDEN,
        b"",
    ));

    let mut first = Client::connect(&d.socket).await;
    d.write_pipe(&bytes);
    first
        .wait_for("the track", |s| {
            s.track.as_ref().and_then(|t| t.title.as_deref()) == Some("Late Joiner")
        })
        .await;

    // Connect only now; the first snapshot must already carry the track.
    let mut late = Client::connect(&d.socket).await;
    assert!(matches!(late.next().await, ServerMessage::Hello { .. }));
    match late.next().await {
        ServerMessage::State { state, .. } => {
            assert_eq!(
                state.track.and_then(|t| t.title).as_deref(),
                Some("Late Joiner"),
                "a late client did not receive current state"
            );
        }
        other => panic!("expected state, got {other:?}"),
    }
}

#[tokio::test]
async fn a_malformed_client_message_does_not_break_the_connection() {
    let d = Daemon::start("malformed");
    let mut c = Client::connect(&d.socket).await;
    assert!(matches!(c.next().await, ServerMessage::Hello { .. }));
    assert!(matches!(c.next().await, ServerMessage::State { .. }));

    c.write.write_all(b"{ not json at all\n").await.unwrap();
    c.write.write_all(b"\n").await.unwrap();
    c.send(&ClientMessage::Unknown).await;

    // Still alive and still answering.
    c.send(&ClientMessage::Ping).await;
    assert!(matches!(c.next().await, ServerMessage::Pong { .. }));
}

#[test]
fn check_config_validates_without_starting_anything() {
    let out = Command::new(env!("CARGO_BIN_EXE_artd"))
        .arg("--config")
        .arg(repo_root().join("provisioning/config.toml"))
        .arg("--config-local")
        .arg("/nonexistent/lpframe.toml")
        .arg("--check-config")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the shipped config failed validation: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("valid"));
}

fn web_scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("artd-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_non_loopback_web_bind_with_auth_off_refuses_to_start() {
    let dir = web_scratch("bind");
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        "[web]\nbind = \"0.0.0.0:8730\"\nauth = false\n[ipc]\nsocket = \"/nonexistent/no.sock\"\n",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_artd"))
        .arg("--config")
        .arg(&config)
        .arg("--config-local")
        .arg(dir.join("none.toml"))
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "artd exposed an unauthenticated page"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("web.auth = false") || stderr.contains("web.auth"),
        "unexpected error: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_escape_hatch_permits_the_bind_and_says_so_loudly() {
    let dir = web_scratch("escape");
    let config = dir.join("config.toml");
    // Loopback would be permitted regardless; `--check-config` exits before
    // anything is bound, so this asserts the policy without holding a port.
    std::fs::write(
        &config,
        "[web]\nbind = \"0.0.0.0:8730\"\nauth = false\ninsecure_no_auth = true\n",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_artd"))
        .arg("--config")
        .arg(&config)
        .arg("--config-local")
        .arg(dir.join("none.toml"))
        .arg("--check-config")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the escape hatch did not permit the bind: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The first-start password: generated, written 0600, hashed into the local
/// override, never stored in plaintext anywhere but that one file, and
/// reprintable with `lpctl web-password`.
#[test]
fn a_first_start_generates_a_password_and_stores_only_its_hash() {
    let dir = web_scratch("password");
    let config = dir.join("config.toml");
    let local = dir.join("config.local.toml");
    std::fs::write(
        &config,
        format!(
            // Port 0 so this never collides with anything else on the machine.
            "[device]\nmetadata_pipe = \"{}\"\n[ipc]\nsocket = \"{}\"\nart_dir = \"{}\"\n\
             [web]\nbind = \"127.0.0.1:0\"\nauth = true\nmdns = false\n\
             [enrichment]\nenabled = false\n",
            dir.join("metadata").display(),
            dir.join("artd.sock").display(),
            dir.join("art").display(),
        ),
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_artd"))
        .arg("--config")
        .arg(&config)
        .arg("--config-local")
        .arg(&local)
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let password_file = dir.join("web-password.txt");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !local.exists() {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();

    let password = std::fs::read_to_string(&password_file)
        .expect("no password file")
        .trim()
        .to_string();
    assert_eq!(password.len(), 23, "{password:?}");

    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&password_file)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the password file is readable by others");

    let stored = std::fs::read_to_string(&local).unwrap();
    assert!(stored.contains("$argon2id$"), "{stored}");
    assert!(
        !stored.contains(&password),
        "the plaintext reached the config file"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
