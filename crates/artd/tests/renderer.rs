//! The whole device, minus the phone and the panel.
//!
//! A real `artd` process reading a real FIFO, a recorded AirPlay session
//! replayed into it, and the real `lprender` render path rendering the result
//! through surfaceless EGL. This is the milestone-4 claim — play something
//! and the art appears — reduced to something CI can assert without a Pi, a
//! display, or a phone.
//!
//! It lives in `artd`'s test directory rather than `lprender`'s for a dull
//! reason: Cargo only exports `CARGO_BIN_EXE_artd` to the package that owns
//! the binary, and starting the genuine daemon is the entire point. The
//! renderer is used as a library, exactly as `lprender`'s own `main` uses it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use lpframe_config::Config;
use lprender::app::App;
use lprender::backend::headless::Headless;
use lprender::source::{ArtdSource, Source};

/// Small, because nothing here is a golden image — this asserts colours, not
/// layout, and the layout tests already cover the panel matrix.
const PANEL: u32 = 64;

/// Sampled well away from the fake cover's diagonal seam and its white corner
/// block, so bilinear filtering cannot blend two regions into the answer.
const SAMPLE: (f32, f32) = (0.75, 0.75);

/// The centre-of-lower-right colour of each fixture track's cover, derived
/// from `lpcapture`'s synthetic hues (30, 90, 170). Asserting the actual
/// pixel is what makes this an end-to-end test rather than a liveness check.
const TRACK_COLOURS: [[u8; 3]; 3] = [[225, 210, 94], [165, 118, 154], [85, 166, 234]];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf()
}

/// A working directory the caller owns across daemon restarts.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lpframe-renderer-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Daemon {
    child: Child,
    pipe: PathBuf,
}

impl Daemon {
    fn socket(dir: &Path) -> PathBuf {
        dir.join("artd.sock")
    }

    /// Start a daemon against `dir`. Does not create or clean `dir`: the
    /// restart test needs the same paths across two processes.
    fn start(dir: &Path) -> Daemon {
        let pipe = dir.join("metadata");
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

[power.display]
# Long enough that the session ending cannot fade the panel to black before
# the assertions have run.
blank_after = "60s"
"#,
                pipe.display(),
                Daemon::socket(dir).display(),
                dir.join("art").display(),
            ),
        )
        .unwrap();

        let child = Command::new(env!("CARGO_BIN_EXE_artd"))
            .arg("--config")
            .arg(&config)
            .arg("--config-local")
            .arg(dir.join("does-not-exist.toml"))
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawning artd");

        let d = Daemon { child, pipe };
        d.wait_for_socket(&Daemon::socket(dir));
        d
    }

    fn wait_for_socket(&self, socket: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if socket.exists() && std::os::unix::net::UnixStream::connect(socket).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("artd never bound {}", socket.display());
    }

    /// Replay the recorded album into the FIFO, paced so that the three
    /// tracks arrive seconds apart.
    ///
    /// The pacing is load-bearing. Blasted in at once, `artd` would publish
    /// all three artwork revisions before the first crossfade had finished,
    /// and the renderer would legitimately skip straight to the last one —
    /// correct snapshot behaviour, but it would prove nothing about tracks
    /// changing.
    fn replay_album(&self, over: Duration) -> std::thread::JoinHandle<()> {
        let bytes = lpcapture::unpack(
            &std::fs::read(repo_root().join("fixtures/sessions/album.pipe")).unwrap(),
            &repo_root().join("fixtures/art"),
        )
        .unwrap();
        let pipe = self.pipe.clone();

        std::thread::spawn(move || {
            // Small writes, so the daemon sees boundaries mid-item.
            let chunks: Vec<&[u8]> = bytes.chunks(997).collect();
            let gap = over / chunks.len().max(1) as u32;
            let mut f = loop {
                match std::fs::OpenOptions::new().write(true).open(&pipe) {
                    Ok(f) => break f,
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            };
            for chunk in chunks {
                if f.write_all(chunk).is_err() {
                    return;
                }
                let _ = f.flush();
                std::thread::sleep(gap);
            }
        })
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A headless renderer, or `None` if this machine has no EGL.
fn headless() -> Option<Headless> {
    match Headless::new(PANEL, PANEL) {
        Ok(hl) => Some(hl),
        Err(e) => {
            // Loud, not silent: a CI runner that quietly stopped exercising
            // the end-to-end path would be a hole nobody noticed.
            eprintln!("SKIPPING GPU TEST: no EGL context available: {e:#}");
            None
        }
    }
}

fn sample(hl: &Headless) -> [u8; 3] {
    let px = hl.read();
    let (w, h) = hl.size();
    let x = (w as f32 * SAMPLE.0) as u32;
    let y = (h as f32 * SAMPLE.1) as u32;
    let i = ((y * w + x) * 4) as usize;
    [px[i], px[i + 1], px[i + 2]]
}

fn close_to(got: [u8; 3], want: [u8; 3], tol: i32) -> bool {
    (0..3).all(|i| (got[i] as i32 - want[i] as i32).abs() <= tol)
}

fn is_black(px: [u8; 3]) -> bool {
    px.iter().all(|c| *c < 8)
}

/// Wait for `f`, driving nothing.
fn eventually(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("never observed: {what}");
}

#[test]
fn artwork_published_by_artd_reaches_the_framebuffer_as_tracks_change() {
    let dir = scratch("follows");
    let daemon = Daemon::start(&dir);
    let mut source = ArtdSource::new(&Daemon::socket(&dir));
    let Some(mut hl) = headless() else {
        return;
    };
    let mut app = App::new(Config::default(), (PANEL, PANEL));

    let feeder = daemon.replay_album(Duration::from_secs(5));

    // Every distinct colour the panel settled on, and every artwork revision
    // the daemon published, in order.
    let mut settled: Vec<[u8; 3]> = Vec::new();
    let mut revisions: Vec<u64> = Vec::new();
    let mut shown: Vec<u64> = Vec::new();

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let now = app.now_ms();
        app.update_at(hl.renderer(), &mut source, now);
        hl.bind();
        app.draw(hl.renderer(), now);

        if let Some(rev) = source
            .state()
            .and_then(|s| s.artwork)
            .map(|a| a.revision)
            .filter(|rev| revisions.last() != Some(rev))
        {
            revisions.push(rev);
        }

        // Only sample once the crossfade and the fade-in have finished, so a
        // recorded colour is a colour the viewer would actually have seen.
        if let Some(id) = app.scene.current() {
            if !app.scene.frame(now).animating {
                let px = sample(&hl);
                if shown.last() != Some(&id) {
                    shown.push(id);
                    settled.push(px);
                }
            }
        }

        if settled.len() == TRACK_COLOURS.len() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = feeder.join();

    assert_eq!(
        revisions,
        vec![1, 2, 3],
        "artd did not publish one artwork revision per track"
    );
    assert!(
        shown.windows(2).all(|w| w[1] > w[0]),
        "the renderer went backwards through images: {shown:?}"
    );
    assert!(
        !settled.is_empty() && settled.iter().all(|px| !is_black(*px)),
        "the panel was black with artwork available: {settled:?}"
    );
    assert_ne!(
        settled.first(),
        settled.last(),
        "a track change produced an identical frame: {settled:?}"
    );

    // Every settled frame must be one of the three covers, and the last one
    // must be the last track's — proof the pixels came from the fixture and
    // not from anywhere else.
    for px in &settled {
        assert!(
            TRACK_COLOURS.iter().any(|want| close_to(*px, *want, 10)),
            "{px:?} is not any of the fixture's covers"
        );
    }
    assert!(
        close_to(*settled.last().unwrap(), TRACK_COLOURS[2], 10),
        "the panel did not end on Teardrop's cover: {:?}",
        settled.last()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_source_started_before_artd_exists_connects_when_it_appears() {
    // The ordinary boot: systemd starts the two independently and the
    // renderer usually wins (DESIGN §8.1).
    let dir = scratch("early");
    let socket = Daemon::socket(&dir);
    let source = ArtdSource::new(&socket);

    assert!(
        !source.connected(),
        "connected to a socket that is not there"
    );
    assert_eq!(
        source.power(),
        lpframe_proto::DisplayPower::Off,
        "the panel should stay dark until artd says otherwise"
    );

    let _daemon = Daemon::start(&dir);
    eventually("a connection to artd", Duration::from_secs(10), || {
        source.connected()
    });
    // The first message is a full snapshot, so the source is correct without
    // having to ask for anything.
    eventually("the first snapshot", Duration::from_secs(10), || {
        source.state().is_some()
    });

    drop(source);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_source_reconnects_after_artd_is_killed_and_restarted() {
    let dir = scratch("restart");
    let socket = Daemon::socket(&dir);
    let daemon = Daemon::start(&dir);
    let source = ArtdSource::new(&socket);

    eventually("the first connection", Duration::from_secs(10), || {
        source.connected()
    });

    // SIGKILL, so there is no orderly close: the renderer only learns about
    // it from the socket dying under it.
    drop(daemon);
    eventually("the connection dropping", Duration::from_secs(10), || {
        !source.connected()
    });
    // The last snapshot is kept rather than discarded, so an artd restart
    // does not blank the panel.
    assert!(
        source.state().is_some(),
        "state was thrown away when artd died"
    );

    let _daemon = Daemon::start(&dir);
    // Generous: the backoff is capped at 5s, so a reconnect can take that
    // long plus however long artd needs to bind.
    eventually("reconnection", Duration::from_secs(20), || {
        source.connected()
    });

    drop(source);
    let _ = std::fs::remove_dir_all(&dir);
}
