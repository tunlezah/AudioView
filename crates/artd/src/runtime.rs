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
use crate::hub::Hub;
use crate::ipc::Command;
use crate::machine::{Effect, Input, Machine};

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
        })
    }

    pub fn machine(&self) -> &Machine {
        &self.machine
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
                        artwork_changed |= self.machine.set_artwork(
                            stored.sha256,
                            stored.path,
                            stored.bytes,
                            stored.dimensions,
                            ArtworkSource::Airplay,
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
