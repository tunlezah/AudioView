//! Fixture pipeline tests: the committed fixtures must be exactly what the
//! generator produces, must round-trip through pack/unpack, and must decode
//! to their golden event logs.
//!
//! These run against the real `fixtures/` tree, so a fixture edited by hand
//! or a golden file left stale fails CI.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/lpcapture.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

fn fixtures_dir() -> PathBuf {
    repo_root().join("fixtures/sessions")
}

fn art_dir() -> PathBuf {
    repo_root().join("fixtures/art")
}

fn fixture_paths() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(fixtures_dir())
        .expect("fixtures/sessions is missing; run `cargo run -p lpcapture -- synth`")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "pipe"))
        .collect();
    out.sort();
    assert!(!out.is_empty(), "no fixtures found");
    out
}

/// Every fixture must survive unpack → pack → unpack unchanged. A lossy
/// fixture would silently weaken every test built on it.
#[test]
fn fixtures_round_trip_through_pack() {
    let art = art_dir();
    for path in fixture_paths() {
        let packed = std::fs::read(&path).unwrap();
        let raw = lpcapture_unpack(&packed, &art);
        let (repacked, _) = lpcapture_pack(&raw, &art);
        let raw2 = lpcapture_unpack(&repacked, &art);
        assert_eq!(
            raw,
            raw2,
            "{} did not round-trip through pack/unpack",
            path.display()
        );
    }
}

/// The committed bytes must match what `synth` produces, so a fixture cannot
/// drift from its generator.
#[test]
fn committed_fixtures_match_the_generator() {
    let art = art_dir();
    for session in lpcapture::synth_sessions() {
        let path = fixtures_dir().join(format!("{}.pipe", session.name));
        let committed = std::fs::read(&path).unwrap_or_else(|e| panic!("{} : {e}", path.display()));
        let (expected, _) = lpcapture_pack(&session.bytes, &art);
        assert_eq!(
            committed,
            expected,
            "{} differs from the generator; run `cargo run -p lpcapture -- synth`",
            path.display()
        );
    }
}

/// Golden event logs must match the decoder's current output.
#[test]
fn golden_event_logs_are_current() {
    let art = art_dir();
    for path in fixture_paths() {
        let packed = std::fs::read(&path).unwrap();
        let raw = lpcapture_unpack(&packed, &art);
        let rendered = lpcapture::render_events(&raw);
        let golden_path = path.with_extension("events.json");
        let golden = std::fs::read_to_string(&golden_path)
            .unwrap_or_else(|e| panic!("{} : {e}", golden_path.display()));
        assert_eq!(
            golden,
            rendered,
            "{} is stale; run `cargo run -p lpcapture -- golden`",
            golden_path.display()
        );
    }
}

/// Decoding must not depend on how the byte stream is chunked. This is the
/// same property `spmeta` tests at the unit level, asserted end to end
/// against real fixture content.
#[test]
fn fixtures_decode_identically_at_any_chunk_size() {
    let art = art_dir();
    for path in fixture_paths() {
        let raw = lpcapture_unpack(&std::fs::read(&path).unwrap(), &art);
        let whole = lpcapture::render_events(&raw);
        for chunk in [1usize, 3, 17, 256, 4096] {
            let mut d = spmeta::Decoder::new();
            let mut events = Vec::new();
            for part in raw.chunks(chunk) {
                d.feed(part);
                while let Some(ev) = d.next_event() {
                    events.push(ev);
                }
            }
            let piecewise = lpcapture::render_event_results(&events);
            assert_eq!(
                whole,
                piecewise,
                "{} decoded differently at chunk size {chunk}",
                path.display()
            );
        }
    }
}

/// The truncated fixture must actually be truncated — this caught `pack`
/// silently discarding the trailing partial item.
#[test]
fn the_abrupt_disconnect_fixture_ends_mid_item() {
    let raw = lpcapture_unpack(
        &std::fs::read(fixtures_dir().join("abrupt-disconnect.pipe")).unwrap(),
        &art_dir(),
    );
    let tail = String::from_utf8_lossy(&raw[raw.len() - 40..]).to_string();
    assert!(
        tail.contains("<item>") && !tail.trim_end().ends_with("</item>"),
        "fixture no longer ends mid-item; tail was {tail:?}"
    );

    // And the parser must treat that tail as "waiting", not as damage.
    let mut d = spmeta::Decoder::new();
    d.feed(&raw);
    let results = d.drain();
    assert!(
        results.iter().all(|r| r.is_ok()),
        "a truncated tail must not produce a parse error"
    );
}

/// The CLI is the interface the build guide documents; smoke-test that the
/// verification path actually fails when a golden file is wrong.
#[test]
fn golden_check_detects_a_stale_file() {
    let tmp = repo_root().join("target/test-golden");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("sessions")).unwrap();

    let bin = env!("CARGO_BIN_EXE_lpcapture");
    let ok = Command::new(bin)
        .args(["synth", "--out-dir"])
        .arg(tmp.join("sessions"))
        .current_dir(repo_root())
        .status()
        .unwrap();
    assert!(ok.success());

    let golden = tmp.join("sessions/album.events.json");
    std::fs::write(&golden, "[]\n").unwrap();

    let status = Command::new(bin)
        .args(["golden", "--check"])
        .arg(tmp.join("sessions/album.pipe"))
        .current_dir(repo_root())
        .status()
        .unwrap();
    assert!(!status.success(), "golden --check accepted a stale file");

    let _ = std::fs::remove_dir_all(&tmp);
}

// --- helpers --------------------------------------------------------------

fn lpcapture_pack(raw: &[u8], art: &Path) -> (Vec<u8>, std::collections::BTreeSet<String>) {
    lpcapture::pack(raw, art).expect("pack")
}

fn lpcapture_unpack(packed: &[u8], art: &Path) -> Vec<u8> {
    lpcapture::unpack(packed, art).expect("unpack")
}
