//! The metadata state machine (DESIGN §5.2).
//!
//! Deliberately pure: no IO, no clock of its own, no async. Time arrives as a
//! millisecond parameter and side effects leave as [`Effect`] values. That is
//! what lets a whole AirPlay session replay through it deterministically in a
//! test, timers included.

use std::collections::BTreeMap;

use lpframe_config::{AmpTrigger, Config};
use lpframe_proto::{
    Artwork, ArtworkSource, DisplayPower, Playback, Power, Progress, State, Track, Volume,
};
use sha2::{Digest, Sha256};
use spmeta::{CoreField, MetaEvent};

/// Everything the machine can be told.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    Meta(MetaEvent),
    /// The pipe's writer closed: shairport-sync is gone. Unambiguous, and
    /// the reason we do not hold a spare write descriptor open (DESIGN §5.1).
    PipeEof,
    /// A parse error. Counted, but does not disturb state.
    ParseError,
    /// Nothing happened; evaluate timers.
    Tick,
}

impl Input {
    pub fn label(&self) -> String {
        match self {
            Input::Meta(e) => e.label().to_string(),
            Input::PipeEof => "pipe_eof".into(),
            Input::ParseError => "parse_error".into(),
            Input::Tick => "tick".into(),
        }
    }
}

/// Work the runtime must perform. The machine never does IO itself.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Persist these bytes and call [`Machine::set_artwork`] with the result.
    StoreArtwork(Vec<u8>),
    /// The sender says this track has no art.
    ClearArtwork,
    SetAmp(bool),
    SetDisplay(DisplayPower),
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Outcome {
    /// The published state changed and must be broadcast.
    pub changed: bool,
    pub effects: Vec<Effect>,
    /// Notable transitions, for the diagnostics page and the log.
    pub notes: Vec<String>,
}

impl Outcome {
    fn note(&mut self, s: impl Into<String>) {
        self.notes.push(s.into());
    }
}

/// Counters for the diagnostics page.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    pub tracks: u64,
    pub sessions: u64,
    pub artwork_updates: u64,
    pub artwork_duplicates: u64,
    pub parse_errors: u64,
    pub pipe_eofs: u64,
    pub stall_timeouts: u64,
    pub session_timeouts: u64,
}

pub struct Machine {
    cfg: Config,
    state: State,
    counters: Counters,

    /// `core` fields accumulated between `mdst` and `mden`. A bundle is
    /// committed atomically at `mden`, so the renderer never sees a track
    /// with the new artist and the old title.
    pending: Option<BTreeMap<&'static str, CoreField>>,
    /// Whether a picture block is open (`pcst` seen, `pcen` not yet).
    picture_open: bool,

    last_activity_ms: u64,
    /// When the machine last left a session, for the idle-driven power timers.
    idle_since_ms: Option<u64>,
    amp_off_at_ms: Option<u64>,
    display_ambient_at_ms: Option<u64>,
    display_off_at_ms: Option<u64>,
}

impl Machine {
    pub fn new(cfg: Config, now_ms: u64) -> Self {
        let mut m = Machine {
            cfg,
            state: State::default(),
            counters: Counters::default(),
            pending: None,
            picture_open: false,
            last_activity_ms: now_ms,
            idle_since_ms: Some(now_ms),
            amp_off_at_ms: None,
            display_ambient_at_ms: None,
            display_off_at_ms: None,
        };
        // Boot state is idle with everything off; no delay applies because we
        // were never on.
        m.state.power = Power {
            amp: false,
            display: DisplayPower::Off,
        };
        m
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn counters(&self) -> Counters {
        self.counters
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Record artwork the runtime has stored. Returns true if this is a new
    /// image; identical bytes resent by the sender do not bump the revision,
    /// because the revision is the renderer's crossfade trigger.
    pub fn set_artwork(
        &mut self,
        sha256: String,
        path: std::path::PathBuf,
        bytes: u64,
        dimensions: Option<(u32, u32)>,
        source: ArtworkSource,
    ) -> bool {
        if let Some(existing) = &self.state.artwork {
            if existing.sha256 == sha256 {
                self.counters.artwork_duplicates += 1;
                return false;
            }
        }
        let revision = self.state.artwork.as_ref().map_or(0, |a| a.revision) + 1;
        self.state.artwork = Some(Artwork {
            revision,
            path,
            sha256,
            bytes,
            width: dimensions.map(|d| d.0),
            height: dimensions.map(|d| d.1),
            source,
            is_upgrade: false,
        });
        self.counters.artwork_updates += 1;
        true
    }

    /// The next moment a timer could change something, so the runtime can
    /// sleep exactly rather than polling.
    pub fn next_deadline_ms(&self) -> Option<u64> {
        let mut out: Option<u64> = None;
        let mut consider = |t: Option<u64>| {
            if let Some(t) = t {
                out = Some(out.map_or(t, |cur: u64| cur.min(t)));
            }
        };
        consider(self.amp_off_at_ms);
        consider(self.display_ambient_at_ms);
        consider(self.display_off_at_ms);
        if self.state.playback == Playback::Playing {
            consider(Some(
                self.last_activity_ms + self.cfg.timeouts.stall.as_millis(),
            ));
        }
        if self.state.playback.is_session() {
            consider(Some(
                self.last_activity_ms + self.cfg.timeouts.session.as_millis(),
            ));
        }
        out
    }

    pub fn apply(&mut self, input: Input, now_ms: u64) -> Outcome {
        let before = self.state.clone();
        let mut out = Outcome::default();

        match &input {
            Input::Tick => {}
            Input::ParseError => {
                self.counters.parse_errors += 1;
            }
            // A pipe EOF is not "activity" — it must not postpone the session
            // timeout it is about to make irrelevant anyway.
            Input::PipeEof => {}
            Input::Meta(_) => self.last_activity_ms = now_ms,
        }

        match input {
            Input::Meta(ev) => self.on_meta(ev, now_ms, &mut out),
            Input::PipeEof => {
                self.counters.pipe_eofs += 1;
                if self.state.playback.is_session() {
                    out.note("pipe writer closed; forcing idle");
                }
                self.enter_idle(now_ms, &mut out);
            }
            Input::ParseError | Input::Tick => {}
        }

        self.evaluate_timers(now_ms, &mut out);
        self.apply_power(now_ms, &mut out);

        out.changed = self.state != before;
        out
    }

    // --- metadata ---------------------------------------------------------

    fn on_meta(&mut self, ev: MetaEvent, now_ms: u64, out: &mut Outcome) {
        use MetaEvent as E;
        match ev {
            E::ActiveBegin => self.enter_active(now_ms, out),
            E::ActiveEnd => self.enter_idle(now_ms, out),

            E::PlayBegin => {
                // Senders that predate AirPlay 2 never emit abeg, so pbeg has
                // to imply the session.
                if !self.state.playback.is_session() {
                    self.enter_active(now_ms, out);
                }
                self.set_playback(Playback::Starting, out);
            }
            E::FirstFrame => {
                if self.state.playback.is_session() {
                    self.set_playback(Playback::Playing, out);
                }
            }
            E::PlayEnd => {
                if self.state.playback.is_session() {
                    // Back to Active, not Idle: gaps between tracks live here,
                    // and the display blank timer must not start (§5.1.1).
                    self.set_playback(Playback::Active, out);
                    self.state.progress = None;
                }
            }
            E::PlayFlush => {
                if self.state.playback == Playback::Playing {
                    self.set_playback(Playback::Paused, out);
                }
            }
            E::PlayResume => {
                if self.state.playback.is_session() {
                    self.set_playback(Playback::Playing, out);
                }
            }
            E::Stall => {
                if self.state.playback == Playback::Playing {
                    self.set_playback(Playback::Paused, out);
                    out.note("metadata stalled");
                }
            }

            E::BundleStart => self.pending = Some(BTreeMap::new()),
            E::BundleEnd => self.commit_bundle(out),
            E::Core(field) => {
                if let Some(p) = self.pending.as_mut() {
                    p.insert(core_key(&field), field);
                }
                // A core item outside a bundle is dropped: without mdst/mden
                // framing there is no way to know it belongs to the current
                // track rather than the next one.
            }

            E::PictureStart => self.picture_open = true,
            E::PictureEnd => self.picture_open = false,
            E::Picture(bytes) => out.effects.push(Effect::StoreArtwork(bytes)),
            E::PictureCleared => {
                if self.state.artwork.is_some() {
                    self.state.artwork = None;
                    out.effects.push(Effect::ClearArtwork);
                }
            }

            E::Progress(p) => {
                let progress = Progress {
                    position_ms: p.position_ms(),
                    duration_ms: p.duration_ms(),
                };
                // Progress advancing is evidence audio is flowing, which is
                // how a paused-by-timeout session recovers without a prsm.
                if self.state.playback == Playback::Paused {
                    self.set_playback(Playback::Playing, out);
                }
                self.state.progress = Some(progress);
            }
            E::Volume(v) => {
                self.state.volume = Some(Volume {
                    airplay_db: v.airplay_db,
                    muted: v.is_muted(),
                })
            }

            E::ClientName(s) => self.state.session.client_name = Some(s),
            E::UserAgent(s) => self.state.session.user_agent = Some(s),
            E::ClientIp(s) => self.state.session.client_ip = Some(s),
            E::ClientConnected(s) => {
                if !s.is_empty() {
                    self.state.session.client_ip = Some(s);
                }
            }

            E::ServerName(_) | E::ServerIp(_) | E::ClientDisconnected(_) | E::Unknown { .. } => {}
        }
    }

    fn commit_bundle(&mut self, out: &mut Outcome) {
        let Some(fields) = self.pending.take() else {
            return;
        };
        if fields.is_empty() {
            return;
        }

        let mut track = Track::default();
        let mut persistent_id = None;
        for field in fields.values() {
            match field {
                CoreField::Title(s) => track.title = non_empty(s),
                CoreField::Artist(s) => track.artist = non_empty(s),
                CoreField::Album(s) => track.album = non_empty(s),
                CoreField::AlbumArtist(s) => track.album_artist = non_empty(s),
                CoreField::Genre(s) => track.genre = non_empty(s),
                CoreField::Composer(s) => track.composer = non_empty(s),
                CoreField::DurationMs(v) => track.duration_ms = Some(*v),
                CoreField::TrackNumber(v) => track.track_number = Some(*v),
                CoreField::PersistentId(v) => persistent_id = Some(*v),
                _ => {}
            }
        }
        track.id = track_identity(persistent_id, &track);

        let is_new = self.state.track.as_ref().map(|t| &t.id) != Some(&track.id);
        if is_new {
            self.counters.tracks += 1;
            out.note(format!(
                "track: {} — {}",
                track.artist.as_deref().unwrap_or("?"),
                track.title.as_deref().unwrap_or("?")
            ));
        }
        self.state.track = Some(track);
    }

    // --- transitions ------------------------------------------------------

    fn set_playback(&mut self, next: Playback, out: &mut Outcome) {
        if self.state.playback == next {
            return;
        }
        out.note(format!(
            "{} -> {}",
            self.state.playback.as_str(),
            next.as_str()
        ));
        self.state.playback = next;
    }

    fn enter_active(&mut self, now_ms: u64, out: &mut Outcome) {
        if !self.state.playback.is_session() {
            self.counters.sessions += 1;
            self.idle_since_ms = None;
            // Leaving idle cancels every pending power-down.
            self.amp_off_at_ms = None;
            self.display_ambient_at_ms = None;
            self.display_off_at_ms = None;
        }
        self.last_activity_ms = now_ms;
        self.set_playback(Playback::Active, out);
        self.state.session.active = true;
    }

    fn enter_idle(&mut self, now_ms: u64, out: &mut Outcome) {
        if !self.state.playback.is_session() {
            return;
        }
        self.set_playback(Playback::Idle, out);
        self.state.session.active = false;
        self.state.progress = None;
        self.pending = None;
        self.picture_open = false;
        self.idle_since_ms = Some(now_ms);

        let d = &self.cfg.power.display;
        self.amp_off_at_ms = Some(now_ms + self.cfg.power.amp.off_delay.as_millis());
        self.display_ambient_at_ms = d.ambient_after.as_millis().map(|ms| now_ms + ms);
        self.display_off_at_ms = Some(now_ms + d.blank_after.as_millis());
    }

    fn evaluate_timers(&mut self, now_ms: u64, out: &mut Outcome) {
        let since_activity = now_ms.saturating_sub(self.last_activity_ms);

        // Session timeout first: it subsumes the stall timeout.
        if self.state.playback.is_session()
            && since_activity >= self.cfg.timeouts.session.as_millis()
        {
            self.counters.session_timeouts += 1;
            out.note("session timed out");
            self.enter_idle(now_ms, out);
        } else if self.state.playback == Playback::Playing
            && since_activity >= self.cfg.timeouts.stall.as_millis()
        {
            self.counters.stall_timeouts += 1;
            out.note("stalled; assuming paused");
            self.set_playback(Playback::Paused, out);
        }
    }

    fn apply_power(&mut self, now_ms: u64, out: &mut Outcome) {
        let in_session = self.state.playback.is_session();

        let want_amp = if !self.cfg.power.amp.enabled {
            false
        } else if in_session {
            match self.cfg.power.amp.on_event {
                // abeg: one transition per listening session.
                AmpTrigger::SessionBegin => true,
                // pbeg: fires per track. Available, not recommended.
                AmpTrigger::PlayBegin => {
                    matches!(self.state.playback, Playback::Starting | Playback::Playing)
                        || self.state.power.amp
                }
            }
        } else {
            // Idle: hold until the off delay expires.
            self.amp_off_at_ms.is_some_and(|t| now_ms < t) && self.state.power.amp
        };

        let want_display = if in_session {
            DisplayPower::On
        } else if self.display_off_at_ms.is_some_and(|t| now_ms >= t) {
            DisplayPower::Off
        } else if self.display_ambient_at_ms.is_some_and(|t| now_ms >= t) {
            DisplayPower::Ambient
        } else {
            // Idle but still inside the blank delay: hold what we have.
            self.state.power.display
        };

        if want_amp != self.state.power.amp {
            self.state.power.amp = want_amp;
            out.effects.push(Effect::SetAmp(want_amp));
            out.note(format!("amp {}", if want_amp { "on" } else { "off" }));
        }
        if want_display != self.state.power.display {
            self.state.power.display = want_display;
            out.effects.push(Effect::SetDisplay(want_display));
            out.note(format!("display {}", want_display.as_str()));
        }

        // Retire timers that have fired.
        //
        // Not housekeeping: `next_deadline_ms` is what the main loop sleeps
        // on, so a deadline left in the past means it computes a zero-length
        // sleep and spins at full CPU for as long as the device stays idle —
        // the exact opposite of what an idle appliance should cost.
        if self.amp_off_at_ms.is_some_and(|t| now_ms >= t) {
            self.amp_off_at_ms = None;
        }
        if self.display_ambient_at_ms.is_some_and(|t| now_ms >= t) {
            self.display_ambient_at_ms = None;
        }
        if self.display_off_at_ms.is_some_and(|t| now_ms >= t) {
            self.display_off_at_ms = None;
        }
    }
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn core_key(f: &CoreField) -> &'static str {
    use CoreField as F;
    match f {
        F::Title(_) => "title",
        F::Artist(_) => "artist",
        F::Album(_) => "album",
        F::AlbumArtist(_) => "album_artist",
        F::Genre(_) => "genre",
        F::Composer(_) => "composer",
        F::Comment(_) => "comment",
        F::Description(_) => "description",
        F::SortName(_) => "sort_name",
        F::Url(_) => "url",
        F::DurationMs(_) => "duration_ms",
        F::TrackNumber(_) => "track_number",
        F::PersistentId(_) => "persistent_id",
        F::Other { .. } => "other",
    }
}

/// Stable identity for a track.
///
/// The sender's persistent ID when there is one; otherwise a hash of the
/// normalised text fields. Identity is what distinguishes a genuinely new
/// track from the same bundle resent, which senders do routinely.
fn track_identity(persistent_id: Option<u64>, t: &Track) -> String {
    if let Some(id) = persistent_id {
        if id != 0 {
            return format!("mper:{id:016x}");
        }
    }
    let mut h = Sha256::new();
    for part in [&t.artist, &t.album, &t.title] {
        h.update(part.as_deref().unwrap_or("").to_lowercase().as_bytes());
        h.update([0]);
    }
    format!("h:{:x}", h.finalize())[..18].to_string()
}
