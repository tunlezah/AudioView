//! End-to-end test of the live capture path against a real FIFO.
//!
//! This is the part that cannot be tested by replaying bytes: FIFO creation,
//! a writer attaching and detaching, and the reopen-on-EOF behaviour that
//! `artd` will depend on to notice shairport-sync going away (DESIGN §5.1).

use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lpcapture-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn creates_the_fifo_if_it_is_missing() {
    let dir = scratch("create");
    let path = dir.join("meta");
    lpcapture::fifo::ensure_fifo(&path).unwrap();
    let m = std::fs::metadata(&path).unwrap();
    assert!(std::os::unix::fs::FileTypeExt::is_fifo(&m.file_type()));

    // Idempotent.
    lpcapture::fifo::ensure_fifo(&path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn refuses_to_clobber_a_regular_file() {
    let dir = scratch("clobber");
    let path = dir.join("not-a-fifo");
    std::fs::write(&path, b"important").unwrap();

    let err = lpcapture::fifo::ensure_fifo(&path).unwrap_err();
    assert!(err.to_string().contains("not a FIFO"), "{err}");
    assert_eq!(std::fs::read(&path).unwrap(), b"important");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two writers in succession must both be captured, and the reopen between
/// them must be visible in the session count.
#[test]
fn captures_across_a_writer_restart() {
    let dir = scratch("restart");
    let path = dir.join("meta");
    lpcapture::fifo::ensure_fifo(&path).unwrap();

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let reader_path = path.clone();
    let reader = std::thread::spawn(move || {
        lpcapture::fifo::read_loop(&reader_path, Some(3), move |chunk| {
            tx.send(chunk.to_vec()).unwrap();
            Ok(())
        })
    });

    // Let the reader attach before the first writer opens.
    std::thread::sleep(Duration::from_millis(200));

    for msg in [b"first-writer".as_slice(), b"second-writer".as_slice()] {
        let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.write_all(msg).unwrap();
        f.flush().unwrap();
        drop(f); // EOF for the reader
        std::thread::sleep(Duration::from_millis(300));
    }

    let sessions = reader.join().unwrap().unwrap();

    let mut got = Vec::new();
    while let Ok(chunk) = rx.try_recv() {
        got.extend(chunk);
    }
    let text = String::from_utf8_lossy(&got);
    assert!(text.contains("first-writer"), "got {text:?}");
    assert!(text.contains("second-writer"), "got {text:?}");
    assert_eq!(sessions, 2, "expected two distinct writer sessions");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The fanout must not block or fail when nothing is reading it — the
/// capture is the deliverable, forwarding is a convenience.
#[test]
fn fanout_tolerates_no_reader() {
    let dir = scratch("fanout");
    let path = dir.join("fanout");
    let mut f = lpcapture::fifo::Fanout::create(&path).unwrap();
    // Would block forever on a blocking open; must return promptly instead.
    for _ in 0..3 {
        f.write(b"data-with-nobody-listening").unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A real replay into a real FIFO, decoded by a real parser at the far end:
/// the loop the build guide tells you to run before trusting a capture.
#[test]
fn replayed_fixture_decodes_at_the_far_end() {
    let dir = scratch("replay");
    let path = dir.join("meta");
    lpcapture::fifo::ensure_fifo(&path).unwrap();

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf();
    let fixture = root.join("fixtures/sessions/album.pipe");
    let raw = lpcapture::unpack(
        &std::fs::read(&fixture).unwrap(),
        &root.join("fixtures/art"),
    )
    .unwrap();
    let expected = lpcapture::render_events(&raw);

    let reader_path = path.clone();
    let reader = std::thread::spawn(move || {
        let mut decoder = spmeta::Decoder::new();
        let mut events = Vec::new();
        lpcapture::fifo::read_loop(&reader_path, Some(2), |chunk| {
            decoder.feed(chunk);
            while let Some(ev) = decoder.next_event() {
                events.push(ev);
            }
            Ok(())
        })
        .unwrap();
        events
    });

    std::thread::sleep(Duration::from_millis(200));
    {
        let mut w = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        // Small writes, so the far end sees boundaries mid-item.
        for chunk in raw.chunks(1000) {
            w.write_all(chunk).unwrap();
            w.flush().unwrap();
        }
    }

    let events = reader.join().unwrap();
    assert_eq!(lpcapture::render_event_results(&events), expected);

    let _ = std::fs::remove_dir_all(&dir);
}
