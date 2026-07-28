//! Artwork enrichment: replacing AirPlay's ~500px art with a catalogue's
//! original (DESIGN §5.4).
//!
//! The shape of this module follows design principle 1 exactly: **the AirPlay
//! art is the source of truth and is already on the wall before anything here
//! runs.** Nothing in this file can delay it, and every path out of here
//! either produces a strictly better image or leaves the display alone. A
//! device with no network, a blocked API, or an album nobody has heard of
//! behaves identically minus sharpness.
//!
//! The work happens on a bounded pool of two tasks so it can never starve the
//! pipe reader, results come back to [`crate::runtime::Core`] over a channel,
//! and `Core` re-checks that the album is still playing before swapping —
//! because the failure mode of getting that wrong is the wrong record on the
//! wall, which is worse than no upgrade at all.

pub mod cache;
pub mod catalogue;
pub mod gate;
pub mod limit;
pub mod text;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use lpframe_config::{Config, Strictness};
use lpframe_proto::ArtworkSource;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, Semaphore};

use cache::Cache;
use catalogue::{Candidate, Catalogue, CatalogueError, Endpoints};
use gate::Visual;
use limit::{Bucket, SingleFlight};
use text::Scores;

/// Concurrent enrichment jobs (DESIGN §5.4).
const WORKERS: usize = 2;

/// Attempts per track before giving up until the next track (DESIGN §5.4).
const MAX_ATTEMPTS: u32 = 3;

/// First retry delay; doubled per attempt.
const RETRY_BACKOFF: Duration = Duration::from_secs(2);

/// How often the cache is swept after the startup sweep.
const SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// The cache key for an album: `sha256(norm(artist) ‖ norm(album))`.
///
/// Normalised text rather than raw, so "Sigur Rós" and "Sigur Ros" — which
/// different senders genuinely produce for the same record — share one entry
/// and one negative entry.
pub fn album_key(artist: &str, album: &str) -> String {
    let mut h = Sha256::new();
    h.update(text::normalise(artist).as_bytes());
    h.update([0]);
    h.update(text::normalise(album).as_bytes());
    format!("{:x}", h.finalize())
}

/// What `Core` knows when it asks for an album to be enriched.
#[derive(Debug, Clone)]
pub struct Request {
    pub artist: String,
    pub album: String,
    /// The AirPlay image currently on the wall, on tmpfs.
    pub current_path: PathBuf,
    /// Its revision. Part of the deduplication key rather than its sha256,
    /// because the same bytes genuinely do reappear — the next track off the
    /// same record carries identical art — and that reappearance is a fresh
    /// opportunity to apply the cached upgrade, not a duplicate.
    pub current_revision: u64,
    pub current_dimensions: Option<(u32, u32)>,
}

/// An image good enough to swap in.
#[derive(Debug, Clone)]
pub struct Upgrade {
    pub bytes: Vec<u8>,
    pub dimensions: (u32, u32),
    pub source: ArtworkSource,
}

/// How one attempt ended, carrying the numbers behind the decision.
///
/// Every rejection keeps its scores: DESIGN §7.3 requires the diagnostics
/// page to show *why* a candidate was refused, and without the numbers that
/// is guesswork in somebody else's living room.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// The cache had this album. `None` when what it had is no sharper than
    /// what is already displayed.
    Cached(Option<Box<Upgrade>>),
    /// Fetched and passed every gate that applies.
    Upgraded(Box<Upgrade>),
    /// Missed recently; no request was made.
    NegativeCacheHit,
    TextRejected {
        best: Scores,
        candidate: String,
    },
    SizeRejected {
        current: u32,
        candidate: u32,
    },
    PerceptualRejected {
        visual: Visual,
        scores: Scores,
    },
    /// No catalogue offered a match. A negative entry was written.
    NoMatch,
    /// Offline, refused, timed out. Not an error state — it is the normal
    /// state of a device on flaky Wi-Fi (DESIGN §5.4).
    Unreachable(String),
    /// A candidate arrived but could not be used.
    Undecodable(String),
}

/// One completed attempt, delivered back to `Core`.
#[derive(Debug, Clone)]
pub struct Report {
    /// The album this is about. `Core` refuses to apply a report whose key is
    /// not the album currently playing.
    pub key: String,
    pub label: String,
    pub verdict: Verdict,
    /// Whether the rate limiter made this attempt wait.
    pub rate_limited: bool,
}

/// Which catalogues to ask, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Itunes,
    MusicBrainz,
}

pub struct Enricher {
    inner: Arc<Inner>,
}

struct Inner {
    strictness: Strictness,
    sources: Vec<Source>,
    catalogue: Catalogue,
    cache: Cache,
    itunes: Bucket,
    musicbrainz: Bucket,
    single_flight: SingleFlight,
    workers: Semaphore,
    /// Milliseconds before the first retry. Atomic because the sweeper task
    /// already holds a clone of this `Arc` by the time a test wants to
    /// shorten it, so there is no `&mut` to be had.
    retry_backoff_ms: AtomicU64,
    /// The album currently playing. A job whose key no longer matches has
    /// been overtaken by a track change and abandons rather than finishing
    /// work whose result would be thrown away anyway.
    current_key: Mutex<Option<String>>,
    /// The last `(key, artwork revision)` considered, so the progress updates
    /// that arrive every second do not each re-enter the pipeline.
    last_considered: Mutex<Option<(String, u64)>>,
    tx: mpsc::Sender<Report>,
}

impl Enricher {
    /// Build an enricher, or `None` if the device is configured not to.
    ///
    /// Returning `None` rather than a disabled instance is deliberate: with
    /// no object there is no code path that could issue a request, which is
    /// the property the privacy test asserts. Enrichment sends the artist and
    /// album to third parties, and "off" has to mean off.
    pub fn new(
        cfg: &Config,
        endpoints: Endpoints,
        tx: mpsc::Sender<Report>,
    ) -> Result<Option<Enricher>> {
        if !cfg.enrichment.enabled || cfg.enrichment.strictness == Strictness::Off {
            tracing::info!("artwork enrichment is disabled; no catalogue requests will be made");
            return Ok(None);
        }

        let sources: Vec<Source> = cfg
            .enrichment
            .sources
            .iter()
            .filter_map(|s| match s.as_str() {
                "itunes" => Some(Source::Itunes),
                "musicbrainz" => Some(Source::MusicBrainz),
                // Config validation already rejects unknown sources; this is
                // only reachable if that check and this list disagree.
                _ => None,
            })
            .collect();
        if sources.is_empty() {
            tracing::info!("artwork enrichment has no sources configured; disabled");
            return Ok(None);
        }

        let cache = Cache::open(
            &cfg.cache.dir,
            cfg.cache.max_bytes.0,
            (cfg.cache.negative_ttl.as_millis() / 1000) as i64,
        )?;

        let inner = Arc::new(Inner {
            strictness: cfg.enrichment.strictness,
            sources,
            catalogue: Catalogue::new(
                endpoints,
                &cfg.enrichment.itunes_country,
                cfg.enrichment.max_dimension,
                &cfg.enrichment.contact,
            ),
            cache,
            itunes: Bucket::per_minute(cfg.enrichment.rate_limit_per_min),
            // MusicBrainz publishes one request per second and enforces it.
            musicbrainz: Bucket::per_second(1.0),
            single_flight: SingleFlight::default(),
            workers: Semaphore::new(WORKERS),
            retry_backoff_ms: AtomicU64::new(RETRY_BACKOFF.as_millis() as u64),
            current_key: Mutex::new(None),
            last_considered: Mutex::new(None),
            tx,
        });

        // The startup sweep is the one that matters: it is where a power
        // cut's half-written files and orphaned rows go.
        let sweeper = inner.clone();
        tokio::spawn(async move {
            loop {
                match sweeper.cache.sweep() {
                    Ok(s) if !s.is_empty() => tracing::info!(
                        "cache sweep: evicted {}, expired {} negatives, removed {} orphan files",
                        s.evicted,
                        s.expired_negatives,
                        s.orphan_files
                    ),
                    Ok(_) => {}
                    Err(e) => tracing::warn!("cache sweep failed: {e}"),
                }
                tokio::time::sleep(SWEEP_INTERVAL).await;
            }
        });

        Ok(Some(Enricher { inner }))
    }

    /// Shorten the retry backoff. Tests only — three real backoffs is six
    /// seconds of test suite for a code path that is about ordering.
    pub fn set_retry_backoff(&self, d: Duration) {
        self.inner
            .retry_backoff_ms
            .store(d.as_millis() as u64, Ordering::Relaxed);
    }

    pub fn cache_bytes(&self) -> u64 {
        self.inner.cache.total_bytes()
    }

    pub fn cache_entries(&self) -> u64 {
        self.inner.cache.entries()
    }

    /// Consider enriching this album. Returns immediately; never blocks.
    ///
    /// Called on every state change that carries both a track and AirPlay
    /// art, which is several times per track — so most of what happens here
    /// is deciding *not* to start a job.
    pub fn consider(&self, request: Request) {
        if request.artist.trim().is_empty() || request.album.trim().is_empty() {
            // Without both fields there is nothing to search for, and a
            // one-sided query returns somebody else's record.
            return;
        }
        let key = album_key(&request.artist, &request.album);

        // A track change abandons whatever is in flight — but only when the
        // *album* changed. The next track off the same record wants the same
        // image, and cancelling would throw away a fetch already half done.
        {
            let mut current = self
                .inner
                .current_key
                .lock()
                .expect("enrich mutex poisoned");
            if current.as_deref() != Some(key.as_str()) {
                *current = Some(key.clone());
            }
        }

        {
            let mut last = self
                .inner
                .last_considered
                .lock()
                .expect("enrich mutex poisoned");
            let pair = (key.clone(), request.current_revision);
            if last.as_ref() == Some(&pair) {
                return;
            }
            *last = Some(pair);
        }

        // Claim the key here rather than inside the task: a burst of resent
        // bundles must not spawn a burst of tasks that then all discover they
        // are duplicates.
        let Some(flight) = self.inner.single_flight.enter(&key) else {
            tracing::debug!("enrichment for this album is already in flight");
            return;
        };

        let inner = self.inner.clone();
        let label = format!("{} — {}", request.artist, request.album);
        tokio::spawn(async move {
            let _flight = flight;
            // Waiting for a worker slot happens inside the task, so a full
            // pool delays enrichment rather than blocking the caller.
            let Ok(_permit) = inner.workers.acquire().await else {
                return;
            };
            if !inner.still_current(&key) {
                return;
            }

            let mut rate_limited = false;
            let verdict = inner.run(&key, &request, &mut rate_limited).await;
            if !inner.still_current(&key) {
                tracing::debug!("discarding an enrichment result for an album that has ended");
                return;
            }
            let _ = inner
                .tx
                .send(Report {
                    key,
                    label,
                    verdict,
                    rate_limited,
                })
                .await;
        });
    }
}

impl Inner {
    fn still_current(&self, key: &str) -> bool {
        self.current_key
            .lock()
            .expect("enrich mutex poisoned")
            .as_deref()
            == Some(key)
    }

    async fn run(&self, key: &str, request: &Request, rate_limited: &mut bool) -> Verdict {
        let current_min_side = request.current_dimensions.map_or(0, |(w, h)| w.min(h));

        if let Some((entry, bytes)) = self.cache.get(key) {
            let Some(dims) = entry.width.zip(entry.height) else {
                // An entry stored before we could read its header: treat it
                // as unusable rather than swapping in an unknown size.
                return Verdict::Cached(None);
            };
            // Everything in the cache passed the gates when it was stored, so
            // the design displays it unconditionally. The one check kept is
            // that it really is sharper than what is up: the same album can
            // arrive with larger AirPlay art from a different sender, and
            // swapping down is a visible downgrade.
            if dims.0.min(dims.1) <= current_min_side {
                return Verdict::Cached(None);
            }
            return Verdict::Cached(Some(Box::new(Upgrade {
                bytes,
                dimensions: dims,
                source: ArtworkSource::Cache,
            })));
        }

        if self.cache.is_negative(key) {
            return Verdict::NegativeCacheHit;
        }

        let mut best_text: Option<(Scores, String)> = None;
        for source in &self.sources {
            match self
                .try_source(*source, key, request, current_min_side, rate_limited)
                .await
            {
                Ok(verdict) => return verdict,
                Err(SourceOutcome::NoTextMatch(scores, name)) => {
                    if best_text
                        .as_ref()
                        .is_none_or(|(b, _)| scores.album > b.album)
                    {
                        best_text = Some((scores, name));
                    }
                }
                Err(SourceOutcome::Nothing) => {}
                Err(SourceOutcome::Failed(v)) => return v,
            }
        }

        // A confident wrong answer is worse than none, so a text-gate failure
        // is remembered: asking again in four minutes gives the same answer
        // and spends the same request budget.
        let _ = self.cache.note_miss(key);
        match best_text {
            Some((best, candidate)) => Verdict::TextRejected { best, candidate },
            None => Verdict::NoMatch,
        }
    }

    async fn try_source(
        &self,
        source: Source,
        key: &str,
        request: &Request,
        current_min_side: u32,
        rate_limited: &mut bool,
    ) -> Result<Verdict, SourceOutcome> {
        let bucket = match source {
            Source::Itunes => &self.itunes,
            Source::MusicBrainz => &self.musicbrainz,
        };

        let mut delay = Duration::from_millis(self.retry_backoff_ms.load(Ordering::Relaxed));
        let mut candidates = None;
        for attempt in 1..=MAX_ATTEMPTS {
            if !self.still_current(key) {
                return Err(SourceOutcome::Nothing);
            }
            if !bucket.acquire().await.is_zero() {
                *rate_limited = true;
            }

            let result = match source {
                Source::Itunes => self.catalogue.itunes(&request.artist, &request.album).await,
                Source::MusicBrainz => {
                    self.catalogue
                        .musicbrainz(&request.artist, &request.album)
                        .await
                }
            };
            match result {
                Ok(found) => {
                    candidates = Some(found);
                    break;
                }
                Err(CatalogueError::NotFound) => return Err(SourceOutcome::Nothing),
                Err(CatalogueError::RateLimited(after)) => {
                    // Pause the bucket, not just this caller: the server is
                    // talking about our address, not about one request.
                    *rate_limited = true;
                    bucket.pause(after);
                }
                Err(e) if e.is_transient() && attempt < MAX_ATTEMPTS => {
                    tracing::debug!("enrichment attempt {attempt} failed: {e}");
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                Err(e) => {
                    return Err(SourceOutcome::Failed(if e.is_transient() {
                        Verdict::Unreachable(e.to_string())
                    } else {
                        Verdict::Undecodable(e.to_string())
                    }))
                }
            }
        }

        let Some(candidates) = candidates else {
            return Err(SourceOutcome::Failed(Verdict::Unreachable(format!(
                "no answer after {MAX_ATTEMPTS} attempts"
            ))));
        };

        // Score every candidate and take the best rather than the first that
        // passes: iTunes routinely returns the single, the deluxe edition and
        // the original in one response, and the original is the one we want.
        let mut best: Option<(Scores, Candidate)> = None;
        for c in candidates {
            let scores = Scores::compare(&request.artist, &request.album, &c.artist, &c.album);
            if best
                .as_ref()
                .is_none_or(|(b, _)| scores.artist + scores.album > b.artist + b.album)
            {
                best = Some((scores, c));
            }
        }
        let Some((scores, candidate)) = best else {
            return Err(SourceOutcome::Nothing);
        };
        if !scores.passes() {
            return Err(SourceOutcome::NoTextMatch(
                scores,
                format!("{} — {}", candidate.artist, candidate.album),
            ));
        }

        let bytes = match self.catalogue.fetch_image(&candidate.art_urls).await {
            Ok(b) => b,
            Err(CatalogueError::NotFound) => return Err(SourceOutcome::Nothing),
            Err(e) => return Err(SourceOutcome::Failed(Verdict::Unreachable(e.to_string()))),
        };

        Ok(self
            .judge(
                key,
                request,
                current_min_side,
                scores,
                candidate.source,
                bytes,
            )
            .await)
    }

    /// The size and perceptual gates, off the reactor.
    ///
    /// Decoding a 3000² JPEG and reducing it is tens of milliseconds of solid
    /// CPU. On the reactor that is tens of milliseconds the pipe reader is not
    /// running, which is exactly what the bounded pool exists to prevent.
    async fn judge(
        &self,
        key: &str,
        request: &Request,
        current_min_side: u32,
        scores: Scores,
        source: ArtworkSource,
        bytes: Vec<u8>,
    ) -> Verdict {
        let strictness = self.strictness;
        let current_path = request.current_path.clone();

        let judged = tokio::task::spawn_blocking(move || {
            let candidate = match gate::fingerprint(&bytes) {
                Ok(f) => f,
                Err(e) => return Err(Verdict::Undecodable(e.to_string())),
            };

            // Fall back to decoding the displayed image when the header sniff
            // gave us nothing; a size gate against an unknown size would
            // otherwise let anything through.
            let current_side = if current_min_side > 0 {
                current_min_side
            } else {
                std::fs::read(&current_path)
                    .ok()
                    .and_then(|b| gate::fingerprint(&b).ok())
                    .map_or(0, |f| f.min_side())
            };
            if !gate::size_gate(current_side, candidate.min_side()) {
                return Err(Verdict::SizeRejected {
                    current: current_side,
                    candidate: candidate.min_side(),
                });
            }

            if strictness == Strictness::TextOnly {
                return Ok((candidate, bytes));
            }
            // The bypass exists because legitimate regional cover variants are
            // exactly the albums where a high-resolution upgrade matters, and
            // a colour histogram cannot tell one from a different record.
            // `strict` removes it (DESIGN §5.4 ⑤).
            if scores.near_exact() && strictness != Strictness::Strict {
                return Ok((candidate, bytes));
            }

            let Some(current) = std::fs::read(&current_path)
                .ok()
                .and_then(|b| gate::fingerprint(&b).ok())
            else {
                // We cannot see what is on the wall, so we cannot authorise a
                // swap. Keeping the AirPlay art is always the safe answer.
                return Err(Verdict::Undecodable(
                    "the displayed artwork could not be re-read".into(),
                ));
            };
            let visual = gate::compare(&current, &candidate);
            if !visual.passes() {
                return Err(Verdict::PerceptualRejected { visual, scores });
            }
            Ok((candidate, bytes))
        })
        .await;

        let (fingerprint, bytes) = match judged {
            Ok(Ok(v)) => v,
            Ok(Err(verdict)) => return verdict,
            Err(e) => return Verdict::Undecodable(format!("gate task failed: {e}")),
        };

        let dimensions = (fingerprint.width, fingerprint.height);
        if let Err(e) = self.cache.put(key, &bytes, Some(dimensions), source) {
            // A cache we cannot write to costs a re-fetch next time, not the
            // upgrade we are holding.
            tracing::warn!("could not cache enriched artwork: {e}");
        }
        Verdict::Upgraded(Box::new(Upgrade {
            bytes,
            dimensions,
            source,
        }))
    }
}

/// Why one source did not produce an image, when that is not yet final.
enum SourceOutcome {
    /// Nothing offered, or nothing usable. Try the next source.
    Nothing,
    /// Candidates arrived but none matched the text gate. Try the next
    /// source, and remember the scores: if every source fails this way the
    /// diagnostics page should say so rather than reporting "no match".
    NoTextMatch(Scores, String),
    /// Terminal for this track.
    Failed(Verdict),
}

impl Verdict {
    /// A one-line explanation, with the numbers, for the diagnostics page.
    pub fn describe(&self) -> String {
        match self {
            Verdict::Cached(Some(u)) => {
                format!("cache hit, {}×{}", u.dimensions.0, u.dimensions.1)
            }
            Verdict::Cached(None) => "cache hit, no sharper than the AirPlay art".into(),
            Verdict::Upgraded(u) => format!(
                "upgraded to {}×{} from {}",
                u.dimensions.0,
                u.dimensions.1,
                source_name(u.source)
            ),
            Verdict::NegativeCacheHit => "recently missed; skipped the network".into(),
            Verdict::TextRejected { best, candidate } => format!(
                "text gate rejected {candidate:?} — {best} (need artist {:.2} album {:.2})",
                text::ARTIST_THRESHOLD,
                text::ALBUM_THRESHOLD
            ),
            Verdict::SizeRejected { current, candidate } => format!(
                "size gate rejected {candidate}px against {current}px (need {:.1}×)",
                gate::SIZE_RATIO
            ),
            Verdict::PerceptualRejected { visual, scores } => format!(
                "perceptual gate rejected {visual} with {scores} \
                 (need hamming <= {} similarity >= {:.2})",
                gate::HAMMING_MAX,
                gate::SIMILARITY_MIN
            ),
            Verdict::NoMatch => "no catalogue had this album".into(),
            Verdict::Unreachable(e) => format!("catalogue unreachable: {e}"),
            Verdict::Undecodable(e) => format!("candidate unusable: {e}"),
        }
    }

    /// The image to display, if this verdict produced one.
    pub fn upgrade(&self) -> Option<&Upgrade> {
        match self {
            Verdict::Cached(Some(u)) | Verdict::Upgraded(u) => Some(u),
            _ => None,
        }
    }
}

fn source_name(s: ArtworkSource) -> &'static str {
    match s {
        ArtworkSource::Airplay => "airplay",
        ArtworkSource::Itunes => "itunes",
        ArtworkSource::CoverArtArchive => "coverartarchive",
        ArtworkSource::Cache => "cache",
        ArtworkSource::Placeholder => "placeholder",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_album_key_ignores_the_differences_senders_disagree_about() {
        // One cache entry per record, however it was spelled.
        assert_eq!(
            album_key("Sigur Rós", "Ágætis byrjun"),
            album_key("SIGUR ROS", "agaetis  byrjun")
        );
        assert_eq!(
            album_key("Nirvana", "Nevermind"),
            album_key("Nirvana", "Nevermind (Deluxe Edition)")
        );
        assert_ne!(
            album_key("Queen", "Greatest Hits"),
            album_key("Tom Petty", "Greatest Hits")
        );
        assert_ne!(
            album_key("The Beatles", "Abbey Road"),
            album_key("The Beatles", "Revolver")
        );
    }

    #[test]
    fn every_rejection_explains_itself_with_numbers() {
        let scores = Scores {
            artist: 0.62,
            album: 0.99,
        };
        let d = Verdict::TextRejected {
            best: scores,
            candidate: "Fleetwood Mac — Greatest Hits".into(),
        }
        .describe();
        assert!(d.contains("0.62") && d.contains("0.99"), "{d}");

        let d = Verdict::SizeRejected {
            current: 500,
            candidate: 600,
        }
        .describe();
        assert!(d.contains("600") && d.contains("500"), "{d}");

        let d = Verdict::PerceptualRejected {
            visual: Visual {
                hamming: 31,
                similarity: 0.41,
            },
            scores,
        }
        .describe();
        assert!(d.contains("31") && d.contains("0.41"), "{d}");
    }

    #[tokio::test]
    async fn enrichment_disabled_yields_no_enricher_at_all() {
        // The privacy guarantee, at the strongest point it can be made: with
        // `enabled = false` there is no object that could reach the network.
        let (tx, _rx) = mpsc::channel(4);

        let mut cfg = Config::default();
        cfg.enrichment.enabled = false;
        assert!(Enricher::new(&cfg, Endpoints::default(), tx.clone())
            .unwrap()
            .is_none());

        let mut cfg = Config::default();
        cfg.enrichment.strictness = Strictness::Off;
        assert!(Enricher::new(&cfg, Endpoints::default(), tx.clone())
            .unwrap()
            .is_none());

        let mut cfg = Config::default();
        cfg.enrichment.sources.clear();
        assert!(Enricher::new(&cfg, Endpoints::default(), tx)
            .unwrap()
            .is_none());
    }
}
