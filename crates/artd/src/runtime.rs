//! The async core: owns the state machine, drains the pipe, executes effects.
//!
//! Everything that needs the current time goes through [`Clock`], so tests
//! can replay a whole session in microseconds with the timers still firing at
//! the right relative moments.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use lpframe_config::Config;
use lpframe_proto::ArtworkSource;

use crate::artwork::ArtStore;
use crate::enrich::{album_key, Enricher, Report, Request, Verdict};
use crate::hub::Hub;
use crate::ipc::Command;
use crate::machine::{Effect, EnrichmentOutcome, Input, Machine};

/// Monotonic milliseconds. Never wall-clock: an NTP step shortly after boot
/// must not fire a ten-minute amp timer early.
pub trait Clock: Send + Sync + 'static {
    fn now_ms(&self) -> u64;
}

pub struct MonotonicClock {
    start: Instant,
}

impl Default for MonotonicClock {
    fn default() -> Self {
        MonotonicClock {
            start: Instant::now(),
        }
    }
}

impl Clock for MonotonicClock {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

/// A clock tests drive by hand.
#[derive(Default)]
pub struct TestClock(AtomicU64);

impl TestClock {
    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
    pub fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Where amp transitions go.
///
/// Milestone 2 ships only the logging backend: the state machine computes
/// amp and display *intent* and publishes it, but nothing touches GPIO yet.
/// The libgpiod backend arrives with milestone 6.
pub trait AmpBackend: Send + 'static {
    fn set(&mut self, on: bool) -> Result<()>;
}

#[derive(Default)]
pub struct LoggingAmp;

impl AmpBackend for LoggingAmp {
    fn set(&mut self, on: bool) -> Result<()> {
        tracing::info!(
            "amp trigger -> {} (no GPIO backend yet)",
            if on { "on" } else { "off" }
        );
        Ok(())
    }
}

/// Records transitions instead of performing them.
#[derive(Default, Clone)]
pub struct RecordingAmp(pub Arc<std::sync::Mutex<Vec<(u64, bool)>>>);

pub struct Core {
    machine: Machine,
    store: ArtStore,
    hub: Hub,
    clock: Arc<dyn Clock>,
    amp: Box<dyn AmpBackend>,
    /// Absent when enrichment is off, which is how "off means off" is
    /// enforced: there is no object here that could reach the network.
    enricher: Option<Enricher>,
}

impl Core {
    pub fn new(
        cfg: Config,
        hub: Hub,
        clock: Arc<dyn Clock>,
        amp: Box<dyn AmpBackend>,
    ) -> Result<Core> {
        let store = ArtStore::new(&cfg.ipc.art_dir, cfg.ipc.art_retain)?;
        let machine = Machine::new(cfg, clock.now_ms());
        Ok(Core {
            machine,
            store,
            hub,
            clock,
            amp,
            enricher: None,
        })
    }

    /// Attach the enrichment pipeline.
    ///
    /// Separate from [`Core::new`] because it needs a tokio runtime to spawn
    /// into, and the state-machine replay tests deliberately run without one.
    pub fn set_enricher(&mut self, enricher: Option<Enricher>) {
        self.enricher = enricher;
    }

    pub fn machine(&self) -> &Machine {
        &self.machine
    }

    pub fn enricher(&self) -> Option<&Enricher> {
        self.enricher.as_ref()
    }

    /// Feed one input and perform whatever it implies.
    pub fn handle(&mut self, input: Input) {
        let now = self.clock.now_ms();
        let label = input.label();
        let outcome = self.machine.apply(input, now);

        let mut artwork_changed = false;
        for effect in &outcome.effects {
            match effect {
                Effect::StoreArtwork(bytes) => match self.store.store(bytes) {
                    Ok(stored) => {
                        // Unconditional and immediate. Enrichment is decoration
                        // and may never delay this (DESIGN principle 1).
                        artwork_changed |= self.machine.set_artwork(
                            stored.sha256,
                            stored.path,
                            stored.bytes,
                            stored.dimensions,
                            ArtworkSource::Airplay,
                            false,
                        );
                    }
                    // Losing artwork must never take the daemon down; the
                    // previous image stays up and the next track retries.
                    Err(e) => tracing::warn!("could not store artwork: {e}"),
                },
                Effect::ClearArtwork => {
                    self.store.clear();
                    artwork_changed = true;
                }
                Effect::SetAmp(on) => {
                    if let Err(e) = self.amp.set(*on) {
                        tracing::error!("amp backend failed: {e}");
                    }
                }
                Effect::SetDisplay(_) => {
                    // Published as intent; the renderer performs the
                    // transition. Nothing to do here.
                }
            }
        }

        for note in &outcome.notes {
            tracing::info!("{note}");
            self.hub.push_note(&label, note.clone());
        }

        if outcome.changed || artwork_changed {
            self.hub.set_counters(self.machine.counters());
            self.hub.publish(self.machine.state().clone());
        }

        self.consider_enrichment();
    }

    /// Offer the current track to the enrichment pipeline.
    ///
    /// Called after every input rather than only on a track change: the
    /// artwork arrives separately from the metadata bundle, so which of the
    /// two lands last varies by sender. `Enricher::consider` is the thing
    /// that decides whether there is anything new to do.
    fn consider_enrichment(&self) {
        let Some(enricher) = &self.enricher else {
            return;
        };
        let state = self.machine.state();
        let (Some(track), Some(art)) = (&state.track, &state.artwork) else {
            return;
        };
        // Only AirPlay art is a candidate for upgrading. Re-offering an
        // already-enriched image would compare it against itself.
        if art.source != ArtworkSource::Airplay {
            return;
        }
        let (Some(artist), Some(album)) = (track.artist.as_ref(), track.album.as_ref()) else {
            return;
        };

        enricher.consider(Request {
            artist: artist.clone(),
            album: album.clone(),
            current_path: art.path.clone(),
            current_revision: art.revision,
            current_dimensions: art.width.zip(art.height),
        });
    }

    /// Apply one enrichment result.
    ///
    /// The staleness check is the important part. A report can arrive after
    /// the track has moved on, and swapping then would put the previous
    /// album's cover over the current one — a failure a viewer notices
    /// immediately and cannot explain.
    pub fn handle_enrichment(&mut self, report: Report) {
        let current = self.machine.state().track.as_ref().and_then(|t| {
            let (artist, album) = (t.artist.as_deref()?, t.album.as_deref()?);
            Some(album_key(artist, album))
        });
        if current.as_deref() != Some(report.key.as_str()) {
            tracing::debug!("discarding a stale enrichment result for {}", report.label);
            return;
        }

        let outcome = match &report.verdict {
            Verdict::Cached(_) => EnrichmentOutcome::CacheHit,
            Verdict::Upgraded(_) => EnrichmentOutcome::Upgraded,
            Verdict::NegativeCacheHit => EnrichmentOutcome::NegativeCacheHit,
            Verdict::TextRejected { .. } => EnrichmentOutcome::TextRejected,
            Verdict::SizeRejected { .. } => EnrichmentOutcome::SizeRejected,
            Verdict::PerceptualRejected { .. } => EnrichmentOutcome::PerceptualRejected,
            Verdict::Unreachable(_) => EnrichmentOutcome::NetworkError,
            Verdict::NoMatch | Verdict::Undecodable(_) => EnrichmentOutcome::NoMatch,
        };
        self.machine.record_enrichment(outcome, report.rate_limited);

        let detail = report.verdict.describe();
        match &report.verdict {
            // Offline is the normal state of a device on flaky Wi-Fi, not an
            // error, and it must be silent above debug level (DESIGN §5.4).
            Verdict::Unreachable(_) => tracing::debug!("enrichment: {}: {detail}", report.label),
            _ => tracing::info!("enrichment: {}: {detail}", report.label),
        }
        // Notes are what the diagnostics page shows, and every rejection
        // carries its scores so a wrong decision can be argued with (§7.3).
        self.hub
            .push_note("enrichment", format!("{}: {detail}", report.label));

        let mut changed = false;
        if let Some(upgrade) = report.verdict.upgrade() {
            match self.store.store(&upgrade.bytes) {
                Ok(stored) => {
                    changed = self.machine.set_artwork(
                        stored.sha256,
                        stored.path,
                        stored.bytes,
                        stored.dimensions.or(Some(upgrade.dimensions)),
                        upgrade.source,
                        true,
                    );
                }
                Err(e) => tracing::warn!("could not stage enriched artwork: {e}"),
            }
        }

        self.hub.set_counters(self.machine.counters());
        if changed {
            self.hub.publish(self.machine.state().clone());
        }
    }

    pub fn handle_command(&mut self, cmd: Command) {
        match cmd {
            Command::SetDisplay(value) => {
                tracing::info!("manual display override: {}", value.as_str());
                // Overrides land with the settings UI (milestone 7); for now
                // this is visible in the log rather than silently ignored.
            }
            Command::InjectArtwork(path) => match std::fs::read(&path) {
                Ok(bytes) => self.handle(Input::Meta(spmeta::MetaEvent::Picture(bytes))),
                Err(e) => tracing::warn!("could not read {}: {e}", path.display()),
            },
        }
    }

    /// How long until a timer could next change something.
    pub fn next_deadline_ms(&self) -> Option<u64> {
        self.machine.next_deadline_ms()
    }

    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }
}

impl AmpBackend for RecordingAmp {
    fn set(&mut self, on: bool) -> Result<()> {
        self.0.lock().expect("amp log poisoned").push((0, on));
        Ok(())
    }
}
