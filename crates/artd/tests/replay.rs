//! Replay every fixture through the state machine with a fake clock and
//! compare the transcript against a golden file.
//!
//! This is the milestone-2 equivalent of `spmeta`'s golden event logs: the
//! reviewable artefact is `fixtures/sessions/NAME.states.json`, and a diff
//! there shows exactly how a behaviour change moves the device.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use artd::hub::Hub;
use artd::machine::{Effect, Input, Machine};
use artd::runtime::{Clock, Core, RecordingAmp, TestClock};
use lpframe_config::{Config, Dur};
use lpframe_proto::{DisplayPower, Playback};
use serde::{Deserialize, Serialize};
use spmeta::MetaEvent;

/// Simulated gap between consecutive pipe items. Real sessions are slower,
/// but the interesting timing is relative to the timeouts, not to reality.
const STEP_MS: u64 = 100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Line {
    at_ms: u64,
    input: String,
    playback: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    track: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    artwork: Option<String>,
    amp: bool,
    display: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    notes: Vec<String>,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

fn fixture_paths() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(repo_root().join("fixtures/sessions"))
        .expect("fixtures/sessions")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "pipe"))
        .collect();
    out.sort();
    assert!(!out.is_empty());
    out
}

fn load_events(path: &Path) -> Vec<MetaEvent> {
    let raw = lpcapture::unpack(
        &std::fs::read(path).unwrap(),
        &repo_root().join("fixtures/art"),
    )
    .unwrap();
    let mut d = spmeta::Decoder::new();
    d.feed(&raw);
    d.drain().into_iter().filter_map(|r| r.ok()).collect()
}

/// Deterministic config, so a golden file does not move when a default does.
fn test_config(art_dir: PathBuf) -> Config {
    let mut cfg = Config::default();
    cfg.ipc.art_dir = art_dir;
    cfg.timeouts.stall = Dur::from_secs(15);
    cfg.timeouts.session = Dur::from_secs(60);
    cfg.power.amp.off_delay = Dur::from_secs(600);
    cfg.power.display.blank_after = Dur::from_secs(300);
    cfg.web.enabled = false;
    cfg
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("artd-replay-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Run one fixture and return its transcript.
fn transcript(fixture: &Path) -> Vec<Line> {
    let events = load_events(fixture);
    let name = fixture.file_stem().unwrap().to_string_lossy().to_string();
    let art_dir = scratch(&name);

    let clock = Arc::new(TestClock::default());
    let hub = Hub::new(64);
    let amp = RecordingAmp::default();
    let mut core = Core::new(
        test_config(art_dir.clone()),
        hub.clone(),
        clock.clone() as Arc<dyn Clock>,
        Box::new(amp),
    )
    .unwrap();

    let mut lines = Vec::new();
    // Notes are newest-first and consecutive ticks share an input label, so
    // slice by count rather than filtering by label — otherwise every tick
    // re-reports the previous tick's notes.
    let mut record = |hub: &Hub, at: u64, input: &str, before_seq: u64, notes_before: usize| {
        let (seq, state) = hub.snapshot();
        if seq == before_seq {
            return; // nothing published, nothing to record
        }
        let all = hub.notes();
        let added = all.len().saturating_sub(notes_before);
        let notes: Vec<String> = all[..added].iter().rev().map(|n| n.text.clone()).collect();
        lines.push(Line {
            at_ms: at,
            input: input.to_string(),
            playback: state.playback.as_str().to_string(),
            track: state.track.as_ref().map(|t| {
                format!(
                    "{} — {}",
                    t.artist.as_deref().unwrap_or("?"),
                    t.title.as_deref().unwrap_or("?")
                )
            }),
            artwork: state
                .artwork
                .as_ref()
                .map(|a| format!("r{} {}b {}", a.revision, a.bytes, &a.sha256[..12])),
            amp: state.power.amp,
            display: state.power.display.as_str().to_string(),
            notes,
        });
    };

    let mut at = 0u64;
    for ev in events {
        at += STEP_MS;
        clock.set(at);
        let before = hub.snapshot().0;
        let notes_before = hub.notes().len();
        let label = ev.label().to_string();
        core.handle(Input::Meta(ev));
        record(&hub, at, &label, before, notes_before);
    }

    // Then let the timers run out: stall, session timeout, ambient, blank,
    // and finally the amp off delay.
    for _ in 0..40 {
        let Some(deadline) = core.next_deadline_ms() else {
            break;
        };
        at = deadline.max(at + 1);
        clock.set(at);
        let before = hub.snapshot().0;
        let notes_before = hub.notes().len();
        core.handle(Input::Tick);
        record(&hub, at, "tick", before, notes_before);
    }

    let _ = std::fs::remove_dir_all(&art_dir);
    lines
}

fn golden_path(fixture: &Path) -> PathBuf {
    fixture.with_extension("states.json")
}

fn render(lines: &[Line]) -> String {
    let mut s = serde_json::to_string_pretty(lines).unwrap();
    s.push('\n');
    s
}

#[test]
fn state_transcripts_match_their_golden_files() {
    let regenerate = std::env::var_os("UPDATE_GOLDEN").is_some();
    let mut stale = Vec::new();

    for fixture in fixture_paths() {
        let rendered = render(&transcript(&fixture));
        let path = golden_path(&fixture);
        if regenerate {
            std::fs::write(&path, &rendered).unwrap();
            continue;
        }
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        if existing != rendered {
            stale.push(path.display().to_string());
        }
    }

    assert!(
        stale.is_empty(),
        "state transcripts are stale; rerun with UPDATE_GOLDEN=1 and review the diff:\n  {}",
        stale.join("\n  ")
    );
}

// --- targeted behaviour tests --------------------------------------------
//
// The golden files catch *change*; these pin the specific properties the
// design argues for, so a regression names itself.

fn machine() -> Machine {
    Machine::new(test_config(scratch("unit")), 0)
}

fn meta(m: &mut Machine, ev: MetaEvent, at: u64) -> artd::machine::Outcome {
    m.apply(Input::Meta(ev), at)
}

#[test]
fn per_track_play_cycles_do_not_leave_the_session() {
    // The core §5.1.1 property: pbeg/pend fire per track, so a four-track
    // album must stay Active between tracks and never reach Idle.
    let mut m = machine();
    meta(&mut m, MetaEvent::ActiveBegin, 0);
    let mut t = 0;
    for _ in 0..4 {
        t += 1000;
        meta(&mut m, MetaEvent::PlayBegin, t);
        meta(&mut m, MetaEvent::FirstFrame, t + 10);
        assert_eq!(m.state().playback, Playback::Playing);
        t += 2000;
        meta(&mut m, MetaEvent::PlayEnd, t);
        assert_eq!(
            m.state().playback,
            Playback::Active,
            "pend must return to Active, not Idle"
        );
        assert!(m.state().power.amp, "amp must not drop between tracks");
        assert_eq!(m.state().power.display, DisplayPower::On);
    }
    meta(&mut m, MetaEvent::ActiveEnd, t + 100);
    assert_eq!(m.state().playback, Playback::Idle);
}

#[test]
fn the_amp_switches_once_per_listening_session() {
    let mut m = machine();
    let mut amp_changes = 0;
    let mut t = 0;

    let apply = |m: &mut Machine, ev: MetaEvent, t: u64, count: &mut i32| {
        for e in m.apply(Input::Meta(ev), t).effects {
            if matches!(e, Effect::SetAmp(_)) {
                *count += 1;
            }
        }
    };

    apply(&mut m, MetaEvent::ActiveBegin, 0, &mut amp_changes);
    for _ in 0..5 {
        t += 1000;
        apply(&mut m, MetaEvent::PlayBegin, t, &mut amp_changes);
        apply(&mut m, MetaEvent::FirstFrame, t + 10, &mut amp_changes);
        apply(&mut m, MetaEvent::PlayEnd, t + 2000, &mut amp_changes);
        t += 2000;
    }
    assert_eq!(amp_changes, 1, "amp relay chattered between tracks");
}

#[test]
fn a_sender_that_omits_abeg_still_opens_and_closes_a_session() {
    let mut m = machine();
    meta(&mut m, MetaEvent::PlayBegin, 0);
    assert!(m.state().playback.is_session(), "pbeg must imply a session");
    meta(&mut m, MetaEvent::FirstFrame, 10);
    meta(&mut m, MetaEvent::PlayEnd, 1000);
    assert_eq!(m.state().playback, Playback::Active);

    // No aend ever arrives; the session timeout has to close it.
    m.apply(Input::Tick, 1000 + 60_000);
    assert_eq!(m.state().playback, Playback::Idle);
    assert_eq!(m.counters().session_timeouts, 1);
}

#[test]
fn a_stalled_stream_pauses_then_times_out() {
    let mut m = machine();
    meta(&mut m, MetaEvent::ActiveBegin, 0);
    meta(&mut m, MetaEvent::PlayBegin, 100);
    meta(&mut m, MetaEvent::FirstFrame, 200);

    m.apply(Input::Tick, 200 + 15_000);
    assert_eq!(m.state().playback, Playback::Paused);
    assert_eq!(m.counters().stall_timeouts, 1);

    m.apply(Input::Tick, 200 + 60_000);
    assert_eq!(m.state().playback, Playback::Idle);
}

#[test]
fn pipe_eof_forces_idle_immediately() {
    // shairport-sync dying must not leave the amp on and the screen lit.
    let mut m = machine();
    meta(&mut m, MetaEvent::ActiveBegin, 0);
    meta(&mut m, MetaEvent::PlayBegin, 100);
    meta(&mut m, MetaEvent::FirstFrame, 200);
    assert!(m.state().power.amp);

    let out = m.apply(Input::PipeEof, 300);
    assert_eq!(m.state().playback, Playback::Idle);
    assert_eq!(m.counters().pipe_eofs, 1);
    assert!(out.notes.iter().any(|n| n.contains("writer closed")));

    // The amp holds for its configured delay, then drops.
    assert!(m.state().power.amp, "amp should hold through the off delay");
    m.apply(Input::Tick, 300 + 600_000);
    assert!(!m.state().power.amp);
}

#[test]
fn the_display_blank_timer_starts_at_idle_not_at_play_end() {
    let mut m = machine();
    meta(&mut m, MetaEvent::ActiveBegin, 0);
    meta(&mut m, MetaEvent::PlayBegin, 100);
    meta(&mut m, MetaEvent::FirstFrame, 200);
    meta(&mut m, MetaEvent::PlayEnd, 1_000);

    // Six minutes after pend but still in a session: the display stays on,
    // because between-tracks gaps must not start the countdown.
    m.apply(Input::Meta(MetaEvent::PlayBegin), 300_000);
    assert_eq!(m.state().power.display, DisplayPower::On);

    meta(&mut m, MetaEvent::PlayEnd, 301_000);
    meta(&mut m, MetaEvent::ActiveEnd, 302_000);
    assert_eq!(m.state().power.display, DisplayPower::On, "just went idle");
    m.apply(Input::Tick, 302_000 + 300_000);
    assert_eq!(m.state().power.display, DisplayPower::Off);
}

#[test]
fn a_bundle_is_committed_atomically() {
    let mut m = machine();
    meta(&mut m, MetaEvent::ActiveBegin, 0);
    meta(&mut m, MetaEvent::BundleStart, 10);
    meta(
        &mut m,
        MetaEvent::Core(spmeta::CoreField::Artist("Massive Attack".into())),
        20,
    );
    meta(
        &mut m,
        MetaEvent::Core(spmeta::CoreField::Title("Teardrop".into())),
        30,
    );
    // Mid-bundle, nothing is published yet.
    assert!(m.state().track.is_none(), "partial bundle leaked");

    meta(&mut m, MetaEvent::BundleEnd, 40);
    let t = m.state().track.as_ref().unwrap();
    assert_eq!(t.artist.as_deref(), Some("Massive Attack"));
    assert_eq!(t.title.as_deref(), Some("Teardrop"));
    assert_eq!(m.counters().tracks, 1);
}

#[test]
fn a_resent_identical_bundle_is_not_a_new_track() {
    let mut m = machine();
    let send = |m: &mut Machine, at: u64| {
        m.apply(Input::Meta(MetaEvent::BundleStart), at);
        m.apply(
            Input::Meta(MetaEvent::Core(spmeta::CoreField::PersistentId(0x1234))),
            at + 1,
        );
        m.apply(
            Input::Meta(MetaEvent::Core(spmeta::CoreField::Title("Same".into()))),
            at + 2,
        );
        m.apply(Input::Meta(MetaEvent::BundleEnd), at + 3);
    };
    send(&mut m, 0);
    send(&mut m, 100);
    send(&mut m, 200);
    assert_eq!(
        m.counters().tracks,
        1,
        "resent bundles counted as new tracks"
    );
}

#[test]
fn identical_artwork_does_not_bump_the_revision() {
    // The revision is the renderer's crossfade trigger; senders resend the
    // same PICT routinely and that must not cause a visible transition.
    let mut m = machine();
    let set = |m: &mut Machine, sha: &str| {
        m.set_artwork(
            sha.into(),
            "/tmp/x.jpg".into(),
            100,
            Some((500, 500)),
            lpframe_proto::ArtworkSource::Airplay,
            false,
        )
    };
    assert!(set(&mut m, "aaaa"));
    assert_eq!(m.state().artwork.as_ref().unwrap().revision, 1);
    assert!(!set(&mut m, "aaaa"), "duplicate art bumped the revision");
    assert_eq!(m.state().artwork.as_ref().unwrap().revision, 1);
    assert!(set(&mut m, "bbbb"));
    assert_eq!(m.state().artwork.as_ref().unwrap().revision, 2);
    assert_eq!(m.counters().artwork_duplicates, 1);
}

#[test]
fn an_enrichment_swap_is_marked_as_an_upgrade_and_airplay_art_is_not() {
    // The renderer picks its crossfade length off this flag alone: 250ms for
    // the same picture getting sharper, 600ms for a new record (§5.3).
    let mut m = machine();
    m.set_artwork(
        "aaaa".into(),
        "/tmp/a.jpg".into(),
        100,
        Some((500, 500)),
        lpframe_proto::ArtworkSource::Airplay,
        false,
    );
    assert!(!m.state().artwork.as_ref().unwrap().is_upgrade);

    m.set_artwork(
        "bbbb".into(),
        "/tmp/b.jpg".into(),
        900,
        Some((3000, 3000)),
        lpframe_proto::ArtworkSource::Itunes,
        true,
    );
    let art = m.state().artwork.as_ref().unwrap();
    assert!(art.is_upgrade);
    assert_eq!(art.revision, 2);
    assert_eq!(art.source, lpframe_proto::ArtworkSource::Itunes);

    // The next track's AirPlay art clears the flag again.
    m.set_artwork(
        "cccc".into(),
        "/tmp/c.jpg".into(),
        100,
        Some((500, 500)),
        lpframe_proto::ArtworkSource::Airplay,
        false,
    );
    assert!(!m.state().artwork.as_ref().unwrap().is_upgrade);
}

#[test]
fn every_enrichment_outcome_lands_in_its_own_counter() {
    use artd::machine::EnrichmentOutcome as O;
    let mut m = machine();
    for outcome in [
        O::CacheHit,
        O::NegativeCacheHit,
        O::TextRejected,
        O::SizeRejected,
        O::PerceptualRejected,
        O::Upgraded,
        O::NetworkError,
        O::NoMatch,
    ] {
        m.record_enrichment(outcome, false);
    }
    m.record_enrichment(O::Upgraded, true);

    let c = m.counters().enrichment;
    assert_eq!(c.attempted, 9);
    assert_eq!(c.cache_hits, 1);
    assert_eq!(c.negative_cache_hits, 1);
    assert_eq!(c.text_rejections, 1);
    assert_eq!(c.size_rejections, 1);
    assert_eq!(c.perceptual_rejections, 1);
    assert_eq!(c.upgrades, 2);
    assert_eq!(c.network_errors, 1);
    assert_eq!(c.rate_limited, 1);
}

#[test]
fn core_fields_outside_a_bundle_are_dropped() {
    // Without mdst/mden framing there is no way to know whether a field
    // belongs to the current track or the next one.
    let mut m = machine();
    meta(
        &mut m,
        MetaEvent::Core(spmeta::CoreField::Title("Orphan".into())),
        0,
    );
    assert!(m.state().track.is_none());
}

#[test]
fn progress_recovers_a_session_that_was_paused_by_the_stall_timeout() {
    let mut m = machine();
    meta(&mut m, MetaEvent::ActiveBegin, 0);
    meta(&mut m, MetaEvent::PlayBegin, 100);
    meta(&mut m, MetaEvent::FirstFrame, 200);
    m.apply(Input::Tick, 200 + 15_000);
    assert_eq!(m.state().playback, Playback::Paused);

    meta(
        &mut m,
        MetaEvent::Progress(spmeta::dmap::Progress {
            start: 1,
            current: 44_101,
            end: 441_001,
        }),
        200 + 16_000,
    );
    assert_eq!(m.state().playback, Playback::Playing);
}

#[test]
fn next_deadline_never_schedules_a_wakeup_with_nothing_to_do() {
    // A wakeup that changes nothing is wasted power on a device that should
    // sit at zero work while idle.
    let mut m = machine();
    // Fresh boot, never been on: nothing pending.
    assert_eq!(m.next_deadline_ms(), None);

    meta(&mut m, MetaEvent::ActiveBegin, 0);
    let d = m.next_deadline_ms().expect("session timeout is pending");
    assert_eq!(d, 60_000);
}

#[test]
fn a_settled_idle_device_has_no_pending_deadline() {
    // A deadline left in the past makes the main loop compute a zero-length
    // sleep and spin at full CPU for as long as the device stays idle.
    let mut m = machine();
    meta(&mut m, MetaEvent::ActiveBegin, 0);
    meta(&mut m, MetaEvent::PlayBegin, 100);
    meta(&mut m, MetaEvent::FirstFrame, 200);
    meta(&mut m, MetaEvent::ActiveEnd, 1_000);

    // Run every timer to completion the way the daemon would.
    let mut at = 1_000u64;
    for _ in 0..16 {
        let Some(deadline) = m.next_deadline_ms() else {
            break;
        };
        assert!(
            deadline > at,
            "deadline {deadline} is not in the future of {at}: the main loop would spin"
        );
        at = deadline;
        m.apply(Input::Tick, at);
    }

    assert_eq!(m.next_deadline_ms(), None, "a timer never retired");
    assert!(!m.state().power.amp);
    assert_eq!(m.state().power.display, DisplayPower::Off);
}

// --- power management (milestone 6) --------------------------------------

#[test]
fn ambient_precedes_blanking_when_it_is_configured() {
    // Off by default, so this is the path most likely to rot unnoticed.
    let mut cfg = test_config(scratch("ambient"));
    cfg.power.display.ambient_after = lpframe_config::MaybeDuration::secs(30);
    cfg.power.display.blank_after = Dur::from_secs(300);
    let mut m = Machine::new(cfg, 0);

    meta(&mut m, MetaEvent::ActiveBegin, 0);
    meta(&mut m, MetaEvent::PlayBegin, 100);
    meta(&mut m, MetaEvent::FirstFrame, 200);
    assert_eq!(m.state().power.display, DisplayPower::On);

    meta(&mut m, MetaEvent::ActiveEnd, 1_000);
    assert_eq!(m.state().power.display, DisplayPower::On, "just went idle");

    m.apply(Input::Tick, 1_000 + 30_000);
    assert_eq!(
        m.state().power.display,
        DisplayPower::Ambient,
        "ambient never engaged"
    );

    m.apply(Input::Tick, 1_000 + 300_000);
    assert_eq!(m.state().power.display, DisplayPower::Off);

    // And playing again wakes it straight back to full brightness.
    meta(&mut m, MetaEvent::ActiveBegin, 400_000);
    assert_eq!(m.state().power.display, DisplayPower::On);
}

#[test]
fn the_amp_backend_is_driven_off_on_shutdown() {
    // The orderly half of the guarantee. The disorderly half is the external
    // pull-down resistor, which no test can stand in for.
    use artd::runtime::RecordingAmp;
    let amp = RecordingAmp::default();
    let log = amp.0.clone();
    let clock = Arc::new(TestClock::default());
    let hub = Hub::new(16);
    let mut core = Core::new(
        test_config(scratch("shutdown")),
        hub,
        clock.clone() as Arc<dyn Clock>,
        Box::new(amp),
    )
    .unwrap();

    core.handle(Input::Meta(MetaEvent::ActiveBegin));
    assert_eq!(
        log.lock().unwrap().last().map(|e| e.1),
        Some(true),
        "the amp never came on"
    );

    core.shutdown();
    assert_eq!(
        log.lock().unwrap().last().map(|e| e.1),
        Some(false),
        "shutdown left the amplifier powered"
    );
}
