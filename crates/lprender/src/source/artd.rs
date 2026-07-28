//! A [`Source`] that follows `artd` over its Unix socket.
//!
//! The socket is drained on a background thread and the latest snapshot is
//! handed across a mutex, because the render loop must never block: a
//! daemon stalled on an HTTPS request or a slow card write cannot be allowed
//! to cost a frame (DESIGN §3.1).
//!
//! Reconnection is an ordinary state, not an error path. The two units are
//! ordered but deliberately not bound to each other (DESIGN §8.1), so
//! `lprender` routinely starts before `artd` exists and must sit on a black
//! screen until it appears. Because the protocol only ever publishes full
//! snapshots, the first message after any reconnect makes the renderer
//! correct — there is nothing to replay and nothing to catch up on.
//!
//! One thing this does *not* do yet: a track whose `PICT` was zero-length
//! publishes no artwork at all, and the last image stays on screen rather
//! than being replaced by a placeholder. The placeholder is configured but
//! unimplemented, and blanking would be worse than holding.

use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use lpframe_proto::{ClientMessage, DisplayPower, ServerMessage, State, PROTOCOL_VERSION};

use crate::source::{Source, Wanted};

/// First retry delay. Short, so a renderer that wins the race against the
/// daemon by a few milliseconds picks it up immediately.
const RETRY_MIN: Duration = Duration::from_millis(100);
/// Ceiling on the retry delay. A restarting `artd` is back within seconds;
/// a missing one may be missing all day, and neither should busy-wait.
const RETRY_MAX: Duration = Duration::from_secs(5);
/// Granularity of the retry sleep, so shutting down does not have to wait
/// out a full backoff interval.
const RETRY_SLICE: Duration = Duration::from_millis(25);
/// How soon the render loop is asked to look again for a new snapshot.
const POLL_INTERVAL_MS: u64 = 100;

#[derive(Default)]
struct Latest {
    state: Option<State>,
    connected: bool,
}

struct Shared {
    latest: Mutex<Latest>,
    /// A second handle on the live socket, purely so [`Drop`] can shut it
    /// down and break the reader out of a blocking read.
    stream: Mutex<Option<UnixStream>>,
    stop: AtomicBool,
}

/// Follows `artd`'s published state and turns each new artwork revision into
/// an image for the render loop.
pub struct ArtdSource {
    shared: Arc<Shared>,
    reader: Option<JoinHandle<()>>,
    /// The revision most recently acted on, including one whose file had
    /// already been unlinked — so a vanished image is reported once rather
    /// than on every poll.
    handled: Option<u64>,
    /// Identity handed to the scene. Not the revision: `artd` restarting
    /// resets its counter to 1, and a renderer already showing revision 1
    /// would then ignore the new image.
    next_id: u64,
}

impl ArtdSource {
    /// Start following `socket`.
    ///
    /// Infallible on purpose. The daemon not being up is the expected state
    /// during boot, so connecting is the background thread's job and the
    /// caller gets a source that reports a dark panel until the first
    /// snapshot arrives.
    pub fn new(socket: &Path) -> ArtdSource {
        let shared = Arc::new(Shared {
            latest: Mutex::new(Latest::default()),
            stream: Mutex::new(None),
            stop: AtomicBool::new(false),
        });
        let reader = std::thread::Builder::new()
            .name("lprender-artd".into())
            .spawn({
                let shared = Arc::clone(&shared);
                let socket = socket.to_path_buf();
                move || reader(&socket, &shared)
            })
            .expect("spawning the artd reader thread");

        ArtdSource {
            shared,
            reader: Some(reader),
            handled: None,
            next_id: 0,
        }
    }

    /// The most recent snapshot, if one has arrived.
    pub fn state(&self) -> Option<State> {
        self.lock().state.clone()
    }

    /// Whether the socket is attached right now.
    pub fn connected(&self) -> bool {
        self.lock().connected
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Latest> {
        self.shared.latest.lock().expect("artd state poisoned")
    }
}

impl Source for ArtdSource {
    fn poll(&mut self, _now_ms: u64) -> Option<Wanted> {
        let (revision, path, is_upgrade) = {
            let latest = self.lock();
            let art = latest.state.as_ref()?.artwork.as_ref()?;
            if self.handled == Some(art.revision) {
                return None;
            }
            (art.revision, art.path.clone(), art.is_upgrade)
        };
        self.handled = Some(revision);

        // `artd` retains the last few revisions and unlinks the rest after a
        // grace period. Missing that window is survivable — keep whatever is
        // on screen rather than crossfading to a decode error.
        if !path.exists() {
            tracing::warn!(
                "artwork revision {revision} is already gone ({}); keeping the current image",
                path.display()
            );
            return None;
        }

        self.next_id += 1;
        Some(Wanted {
            id: self.next_id,
            path,
            is_upgrade,
        })
    }

    fn power(&self) -> DisplayPower {
        // Before the first snapshot the panel stays dark. Guessing `On` would
        // light an empty screen for however long the daemon takes to arrive.
        self.lock()
            .state
            .as_ref()
            .map_or(DisplayPower::Off, |s| s.power.display)
    }

    fn is_live(&self) -> bool {
        true
    }

    fn next_wakeup_ms(&self, now_ms: u64) -> Option<u64> {
        // Snapshots land on the reader thread, so nothing in the render loop
        // is woken by them. Until the DRM loop carries the socket in its
        // epoll set, a short poll is how a new revision gets noticed; it is
        // one uncontended mutex acquisition and no GPU work.
        Some(now_ms + POLL_INTERVAL_MS)
    }
}

impl Drop for ArtdSource {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        // The reader is parked in a blocking read; shutting the socket down
        // is what returns it. Without this, dropping a source would hang
        // until `artd` happened to say something.
        if let Some(stream) = self
            .shared
            .stream
            .lock()
            .expect("artd stream poisoned")
            .as_ref()
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Connect, read until the connection dies, back off, repeat.
fn reader(socket: &Path, shared: &Arc<Shared>) {
    let mut retry = RETRY_MIN;
    let mut reported_absent = false;

    while !shared.stop.load(Ordering::Relaxed) {
        match UnixStream::connect(socket) {
            Ok(stream) => {
                retry = RETRY_MIN;
                reported_absent = false;
                tracing::info!("connected to artd at {}", socket.display());

                let why = session(stream, shared);

                *shared.stream.lock().expect("artd stream poisoned") = None;
                shared.latest.lock().expect("artd state poisoned").connected = false;
                if !shared.stop.load(Ordering::Relaxed) {
                    tracing::info!("artd connection lost ({why}); reconnecting");
                }
            }
            Err(e) => {
                // Once, not once per attempt: at the ceiling this would be a
                // log line every five seconds for as long as artd is absent,
                // which on a fresh boot is most of the interesting window.
                if !reported_absent {
                    tracing::info!("waiting for artd at {} ({e})", socket.display());
                    reported_absent = true;
                }
            }
        }

        sleep_unless_stopped(retry, shared);
        retry = (retry * 2).min(RETRY_MAX);
    }
}

/// Drain one connection. Returns why it ended, for the log line.
fn session(stream: UnixStream, shared: &Arc<Shared>) -> String {
    let hello = ClientMessage::Hello {
        client: "lprender".into(),
        proto: PROTOCOL_VERSION,
    };
    let line = format!(
        "{}\n",
        serde_json::to_string(&hello).expect("ClientMessage is always serialisable")
    );
    if let Err(e) = (&stream).write_all(line.as_bytes()) {
        return format!("could not send hello: {e}");
    }

    match stream.try_clone() {
        Ok(handle) => *shared.stream.lock().expect("artd stream poisoned") = Some(handle),
        Err(e) => return format!("could not duplicate the socket: {e}"),
    }
    shared.latest.lock().expect("artd state poisoned").connected = true;

    for line in BufReader::new(stream).lines() {
        if shared.stop.load(Ordering::Relaxed) {
            return "shutting down".into();
        }
        let line = match line {
            Ok(line) => line,
            Err(e) => return format!("read failed: {e}"),
        };

        match serde_json::from_str::<ServerMessage>(&line) {
            Ok(ServerMessage::State { state, .. }) => {
                shared.latest.lock().expect("artd state poisoned").state = Some(*state);
            }
            Ok(ServerMessage::Hello {
                server,
                version,
                proto,
                ..
            }) => tracing::info!("{server} {version} speaks protocol {proto}"),
            // Pings are ours to send and the log channel belongs to the
            // diagnostics page, not to the renderer.
            Ok(ServerMessage::Pong { .. } | ServerMessage::Log { .. }) => {}
            // Ignoring what we do not understand is a design property rather
            // than politeness: it is how a newer daemon adds a message type
            // without a flag day (DESIGN §5.3). Debug, because on a mixed
            // pair this would otherwise warn on every snapshot.
            Err(e) => tracing::debug!("ignoring an unrecognised message: {e}"),
        }
    }
    "the daemon closed the connection".into()
}

fn sleep_unless_stopped(total: Duration, shared: &Shared) {
    let mut left = total;
    while left > Duration::ZERO && !shared.stop.load(Ordering::Relaxed) {
        let slice = left.min(RETRY_SLICE);
        std::thread::sleep(slice);
        left -= slice;
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::time::Instant;

    use lpframe_proto::{Artwork, ArtworkSource, Playback, Power};

    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lprender-artd-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn snapshot(revision: u64, path: &Path, is_upgrade: bool) -> String {
        let state = State {
            playback: Playback::Playing,
            artwork: Some(Artwork {
                revision,
                path: path.to_path_buf(),
                sha256: "0".repeat(64),
                bytes: 0,
                width: Some(8),
                height: Some(8),
                source: ArtworkSource::Airplay,
                is_upgrade,
            }),
            power: Power {
                amp: true,
                display: DisplayPower::On,
            },
            ..Default::default()
        };
        ServerMessage::state(revision, "now".into(), state).to_line()
    }

    /// A stand-in for `artd`, driven a line at a time.
    ///
    /// Sending is explicit rather than scripted up front because the source
    /// only ever exposes the *latest* snapshot: two lines written back to
    /// back would legitimately coalesce, and a test that assumed otherwise
    /// would be asserting the opposite of the protocol's design.
    struct FakeDaemon {
        conn: Arc<Mutex<Option<UnixStream>>>,
        accept: Option<JoinHandle<()>>,
    }

    impl FakeDaemon {
        fn bind(socket: &Path) -> FakeDaemon {
            let listener = UnixListener::bind(socket).expect("binding the fake daemon");
            let conn = Arc::new(Mutex::new(None));
            let accept = std::thread::spawn({
                let conn = Arc::clone(&conn);
                move || {
                    if let Ok((stream, _)) = listener.accept() {
                        *conn.lock().expect("fake daemon poisoned") = Some(stream);
                    }
                }
            });
            FakeDaemon {
                conn,
                accept: Some(accept),
            }
        }

        fn send(&self, line: &str) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(stream) = self.conn.lock().expect("fake daemon poisoned").as_mut() {
                    stream
                        .write_all(line.as_bytes())
                        .expect("writing to client");
                    return;
                }
                assert!(Instant::now() < deadline, "no client ever connected");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for FakeDaemon {
        fn drop(&mut self) {
            *self.conn.lock().expect("fake daemon poisoned") = None;
            if let Some(accept) = self.accept.take() {
                let _ = accept.join();
            }
        }
    }

    /// Poll until `f` yields something, or give up.
    fn eventually<T>(mut f: impl FnMut() -> Option<T>) -> Option<T> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(v) = f() {
                return Some(v);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    #[test]
    fn a_source_with_no_daemon_shows_nothing_and_keeps_the_panel_dark() {
        let dir = tmpdir("absent");
        let mut source = ArtdSource::new(&dir.join("nowhere.sock"));
        assert_eq!(source.poll(0), None);
        assert_eq!(source.power(), DisplayPower::Off);
        assert!(!source.connected());
        drop(source);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_snapshot_becomes_the_image_the_renderer_asks_for() {
        let dir = tmpdir("snapshot");
        let art = dir.join("0001-abc.png");
        std::fs::write(&art, b"pretend this is a png").unwrap();
        let socket = dir.join("artd.sock");

        let daemon = FakeDaemon::bind(&socket);
        let mut source = ArtdSource::new(&socket);
        daemon.send(&ServerMessage::hello().to_line());
        daemon.send(&snapshot(1, &art, true));

        let wanted = eventually(|| source.poll(0)).expect("no image arrived");
        assert_eq!(wanted.path, art);
        assert!(wanted.is_upgrade, "is_upgrade did not survive the socket");
        assert_eq!(source.power(), DisplayPower::On);
        assert!(source.connected());

        // The same revision is not a change, however often it is republished.
        daemon.send(&snapshot(1, &art, true));
        for _ in 0..5 {
            assert_eq!(source.poll(10), None, "re-requested an unchanged image");
            std::thread::sleep(Duration::from_millis(5));
        }

        drop(source);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_revision_is_a_new_image_even_at_the_same_path() {
        let dir = tmpdir("revision");
        let art = dir.join("cover.png");
        std::fs::write(&art, b"pretend this is a png").unwrap();
        let socket = dir.join("artd.sock");

        let daemon = FakeDaemon::bind(&socket);
        let mut source = ArtdSource::new(&socket);

        daemon.send(&snapshot(1, &art, false));
        let first = eventually(|| source.poll(0)).expect("no first image");
        daemon.send(&snapshot(2, &art, false));
        let second = eventually(|| source.poll(0)).expect("no second image");

        assert_eq!(first.path, second.path);
        assert_ne!(second.id, first.id, "the scene would ignore a repeated id");

        drop(source);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_artwork_file_that_has_been_collected_is_skipped_rather_than_shown() {
        let dir = tmpdir("vanished");
        let socket = dir.join("artd.sock");
        let gone = dir.join("0009-gone.png");

        let daemon = FakeDaemon::bind(&socket);
        let mut source = ArtdSource::new(&socket);
        daemon.send(&snapshot(9, &gone, false));

        // The snapshot itself still lands: only the image is skipped, so
        // power and the rest of the state stay live.
        eventually(|| source.state()).expect("no snapshot arrived");
        for _ in 0..5 {
            assert_eq!(source.poll(0), None, "asked to decode a missing file");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(source.power(), DisplayPower::On);

        drop(source);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unrecognised_messages_do_not_break_the_connection() {
        let dir = tmpdir("forward-compat");
        let art = dir.join("cover.png");
        std::fs::write(&art, b"pretend this is a png").unwrap();
        let socket = dir.join("artd.sock");

        let daemon = FakeDaemon::bind(&socket);
        let mut source = ArtdSource::new(&socket);
        // A message type from some later milestone, and a line that is not
        // JSON at all.
        daemon.send("{\"v\":1,\"type\":\"teleport\",\"where\":\"away\"}\n");
        daemon.send("not json\n");
        daemon.send(&snapshot(1, &art, false));

        let wanted = eventually(|| source.poll(0)).expect("the connection did not survive");
        assert_eq!(wanted.path, art);

        drop(source);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_source_asks_to_be_woken_soon_enough_to_notice_a_new_track() {
        let dir = tmpdir("wakeup");
        let source = ArtdSource::new(&dir.join("nowhere.sock"));
        let wake = source.next_wakeup_ms(1_000).expect("no wakeup requested");
        assert!(
            wake > 1_000 && wake - 1_000 <= 250,
            "a new revision would take {}ms to appear",
            wake - 1_000
        );
        drop(source);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
