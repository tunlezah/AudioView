//! Enrichment end to end, against a mock catalogue.
//!
//! **No test here touches the real internet.** A suite that reaches Apple
//! fails on a train, fails in a locked-down runner, and quietly starts
//! testing Apple's uptime instead of our gates. The server below is a
//! hand-rolled tokio listener rather than a mocking crate: it has to serve
//! synthesised images at arbitrary sizes, count requests, refuse connections
//! outright and return a 429 with a chosen `Retry-After`, and that is less
//! code to write directly than to configure.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use artd::enrich::catalogue::Endpoints;
use artd::enrich::{album_key, Enricher, Report, Upgrade, Verdict};
use artd::hub::Hub;
use artd::machine::{Counters, Input};
use artd::runtime::{Clock, Core, MonotonicClock, RecordingAmp};
use lpframe_config::{ByteSize, Config, Dur, Strictness};
use lpframe_proto::{ArtworkSource, State};
use spmeta::{CoreField, MetaEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

// --- synthetic covers -----------------------------------------------------

/// A cover with a recognisable layout, rendered at any size. Scaling it is
/// the "same album, bigger file" case the perceptual gate exists to accept.
fn cover(size: u32, shift: i16) -> Vec<u8> {
    let mut px = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 / size as f32, y as f32 / size as f32);
            let base: [i16; 3] = if (fx + fy) > 0.9 && (fx + fy) < 1.1 {
                [240, 230, 60]
            } else if fx < 0.5 && fy < 0.5 {
                [200, 40, 40]
            } else if fx >= 0.5 && fy < 0.5 {
                [30, 60, 180]
            } else if fx < 0.5 {
                [20, 20, 20]
            } else {
                [230, 230, 220]
            };
            px.extend([
                (base[0] + shift).clamp(0, 255) as u8,
                (base[1] + shift / 2).clamp(0, 255) as u8,
                (base[2] - shift).clamp(0, 255) as u8,
                255,
            ]);
        }
    }
    png(&px, size)
}

/// A different record entirely: horizontal bands in a cool palette.
fn other_cover(size: u32) -> Vec<u8> {
    let px: Vec<u8> = (0..size * size)
        .flat_map(|i| {
            let y = i / size;
            let v = if (y * 8 / size) % 2 == 0 { 40u8 } else { 110 };
            [v / 2, v, (255 - v).min(200), 255]
        })
        .collect();
    png(&px, size)
}

fn png(px: &[u8], size: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_raw(size, size, px.to_vec()).expect("raw image");
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("encoding png");
    out.into_inner()
}

// --- the mock catalogue ---------------------------------------------------

#[derive(Default)]
struct Script {
    /// `(artistName, collectionName)` rows the search returns.
    itunes: Vec<(String, String)>,
    /// `(id, title, artist)` release groups MusicBrainz returns.
    musicbrainz: Vec<(String, String, String)>,
    /// Image body per requested square size; a missing size 404s.
    images: HashMap<u32, Vec<u8>>,
    /// Body served for any Cover Art Archive front request.
    caa_image: Option<Vec<u8>>,
    /// Searches to answer with 503 before answering properly.
    fail_searches: usize,
    /// Searches to answer with 429 before answering properly.
    rate_limit_searches: usize,
    retry_after: u64,
    /// `http://host:port`, filled in once the listener has a port. The search
    /// response has to hand out image URLs that point back at this server.
    base: String,
}

struct Mock {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Mock {
    async fn start(mut script: Script) -> Mock {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        script.base = format!("http://{addr}");
        let script = Arc::new(Mutex::new(script));
        let requests = Arc::new(Mutex::new(Vec::new()));

        let s = script.clone();
        let r = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (s, r) = (s.clone(), r.clone());
                tokio::spawn(async move { serve(stream, s, r).await });
            }
        });
        Mock { addr, requests }
    }

    fn endpoints(&self) -> Endpoints {
        Endpoints {
            itunes_search: format!("http://{}/search", self.addr),
            musicbrainz: format!("http://{}/ws/2", self.addr),
            cover_art_archive: format!("http://{}", self.addr),
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn searches(&self) -> usize {
        self.requests()
            .iter()
            .filter(|p| p.starts_with("/search") || p.starts_with("/ws/2"))
            .count()
    }
}

async fn serve(
    mut stream: tokio::net::TcpStream,
    script: Arc<Mutex<Script>>,
    requests: Arc<Mutex<Vec<String>>>,
) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    // Requests are all GETs with no body, so the header terminator is the end.
    loop {
        let Ok(n) = stream.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
    requests.lock().unwrap().push(path.clone());

    let (status, content_type, body) = route(&path, &script);
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Connection: close\r\n{}\r\n",
        body.len(),
        if status.starts_with("429") {
            format!(
                "Retry-After: {}\r\n",
                script.lock().unwrap().retry_after.max(1)
            )
        } else {
            String::new()
        }
    );
    let _ = stream.write_all(header.as_bytes()).await;
    let _ = stream.write_all(&body).await;
    let _ = stream.shutdown().await;
}

fn route(path: &str, script: &Arc<Mutex<Script>>) -> (&'static str, &'static str, Vec<u8>) {
    let mut s = script.lock().unwrap();

    if path.starts_with("/search") || path.starts_with("/ws/2") {
        if s.rate_limit_searches > 0 {
            s.rate_limit_searches -= 1;
            return ("429 Too Many Requests", "text/plain", b"slow down".to_vec());
        }
        if s.fail_searches > 0 {
            s.fail_searches -= 1;
            return ("503 Service Unavailable", "text/plain", b"later".to_vec());
        }
    }

    if path.starts_with("/search") {
        let results: Vec<String> = s
            .itunes
            .iter()
            .map(|(artist, album)| {
                format!(
                    r#"{{"artistName":{},"collectionName":{},"artworkUrl100":"{}/image/100x100bb.png"}}"#,
                    json_string(artist),
                    json_string(album),
                    s.base
                )
            })
            .collect();
        let body = format!(
            r#"{{"resultCount":{},"results":[{}]}}"#,
            results.len(),
            results.join(",")
        );
        return ("200 OK", "application/json", body.into_bytes());
    }

    if path.starts_with("/ws/2/release-group") {
        let groups: Vec<String> = s
            .musicbrainz
            .iter()
            .map(|(id, title, artist)| {
                format!(
                    r#"{{"id":{},"title":{},"artist-credit":[{{"name":{}}}]}}"#,
                    json_string(id),
                    json_string(title),
                    json_string(artist)
                )
            })
            .collect();
        let body = format!(r#"{{"release-groups":[{}]}}"#, groups.join(","));
        return ("200 OK", "application/json", body.into_bytes());
    }

    if path.starts_with("/release-group/") {
        return match s.caa_image.clone() {
            Some(b) => ("200 OK", "image/png", b),
            None => ("404 Not Found", "text/plain", b"no cover".to_vec()),
        };
    }

    if let Some(size) = image_size(path) {
        if let Some(body) = s.images.get(&size).cloned() {
            return ("200 OK", "image/png", body);
        }
        return ("404 Not Found", "text/plain", b"no such size".to_vec());
    }

    ("404 Not Found", "text/plain", b"no route".to_vec())
}

/// Pull the square size out of Apple's `.../<n>x<n>bb.png` segment.
fn image_size(path: &str) -> Option<u32> {
    let file = path.rsplit('/').next()?;
    let dims = file.split('.').next()?.strip_suffix("bb")?;
    let (w, _) = dims.split_once('x')?;
    w.parse().ok()
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).expect("string always serialises")
}

// --- the rig --------------------------------------------------------------

/// A `Core` with an enricher pointed at the mock, plus the report channel the
/// daemon's select loop would normally own.
struct Rig {
    core: Core,
    reports: mpsc::Receiver<Report>,
    hub: Hub,
    dir: PathBuf,
    revision: u64,
}

impl Rig {
    async fn new(name: &str, endpoints: Endpoints, tweak: impl FnOnce(&mut Config)) -> Rig {
        let dir = std::env::temp_dir().join(format!("artd-enrich-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut cfg = Config::default();
        cfg.ipc.art_dir = dir.join("art");
        cfg.cache.dir = dir.join("cache");
        cfg.cache.max_bytes = ByteSize(64 << 20);
        cfg.cache.negative_ttl = Dur::from_secs(3600);
        // Generous, so a test never waits on the limiter unless it means to.
        cfg.enrichment.rate_limit_per_min = 600;
        cfg.web.enabled = false;
        tweak(&mut cfg);

        let hub = Hub::new(64);
        let (tx, reports) = mpsc::channel(16);
        let mut core = Core::new(
            cfg.clone(),
            hub.clone(),
            Arc::new(MonotonicClock::default()) as Arc<dyn Clock>,
            Box::new(RecordingAmp::default()),
        )
        .unwrap();
        let enricher = Enricher::new(&cfg, endpoints, tx).unwrap().inspect(|e| {
            // Three real backoffs is six seconds of test suite for a code
            // path that is about ordering, not about duration.
            e.set_retry_backoff(Duration::from_millis(5));
        });
        core.set_enricher(enricher);

        Rig {
            core,
            reports,
            hub,
            dir,
            revision: 0,
        }
    }

    /// Play a track: a metadata bundle, then its AirPlay artwork.
    fn play(&mut self, artist: &str, album: &str, title: &str, art: &[u8]) {
        for ev in [
            MetaEvent::BundleStart,
            MetaEvent::Core(CoreField::Artist(artist.into())),
            MetaEvent::Core(CoreField::Album(album.into())),
            MetaEvent::Core(CoreField::Title(title.into())),
            MetaEvent::BundleEnd,
            MetaEvent::Picture(art.to_vec()),
        ] {
            self.core.handle(Input::Meta(ev));
        }
        self.revision = self.artwork().map_or(0, |a| a.revision);
    }

    /// Apply every report that arrives, then wait for quiet.
    async fn settle(&mut self) -> Vec<Verdict> {
        let mut seen = Vec::new();
        // The first report has to cross a real socket; later ones do not.
        let mut window = Duration::from_secs(10);
        while let Ok(Some(report)) = tokio::time::timeout(window, self.reports.recv()).await {
            seen.push(report.verdict.clone());
            self.core.handle_enrichment(report);
            window = Duration::from_millis(400);
        }
        seen
    }

    /// Wait for quiet without expecting anything, for the tests that assert
    /// nothing happened.
    async fn settle_quiet(&mut self) -> Vec<Verdict> {
        let mut seen = Vec::new();
        while let Ok(Some(report)) =
            tokio::time::timeout(Duration::from_millis(600), self.reports.recv()).await
        {
            seen.push(report.verdict.clone());
            self.core.handle_enrichment(report);
        }
        seen
    }

    fn state(&self) -> State {
        self.core.machine().state().clone()
    }

    fn artwork(&self) -> Option<lpframe_proto::Artwork> {
        self.core.machine().state().artwork.clone()
    }

    fn counters(&self) -> Counters {
        self.core.machine().counters()
    }

    fn notes(&self) -> Vec<String> {
        self.hub.notes().into_iter().map(|n| n.text).collect()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn itunes_script(artist: &str, album: &str, images: &[(u32, Vec<u8>)]) -> Script {
    Script {
        itunes: vec![(artist.into(), album.into())],
        images: images.iter().cloned().collect(),
        ..Default::default()
    }
}

// --- tests ----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_larger_itunes_cover_replaces_the_airplay_art_as_an_upgrade() {
    let mock = Mock::start(itunes_script(
        "Massive Attack",
        "Mezzanine",
        &[(3000, cover(1500, 0))],
    ))
    .await;
    let mut rig = Rig::new("upgrade", mock.endpoints(), |_| {}).await;

    rig.play("Massive Attack", "Mezzanine", "Angel", &cover(500, 0));
    let before = rig.artwork().expect("AirPlay art must be up immediately");
    assert_eq!(before.source, ArtworkSource::Airplay);
    assert!(!before.is_upgrade);
    assert_eq!(before.width, Some(500));

    rig.settle().await;

    let after = rig.artwork().expect("artwork disappeared");
    assert_eq!(after.source, ArtworkSource::Itunes);
    assert!(after.is_upgrade, "the renderer needs the short crossfade");
    assert!(
        after.revision > before.revision,
        "the revision did not bump: {} -> {}",
        before.revision,
        after.revision
    );
    assert_eq!(after.width, Some(1500));
    assert!(after.path.exists(), "the renderer reads this by path");
    assert_eq!(
        std::fs::read(&after.path).unwrap().len() as u64,
        after.bytes
    );

    let c = rig.counters().enrichment;
    assert_eq!((c.attempted, c.upgrades), (1, 1), "{c:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_airplay_art_is_displayed_before_any_request_is_made() {
    // Design principle 1, asserted directly: the image is on the wall while
    // the catalogue has not been asked anything yet.
    let mock = Mock::start(itunes_script(
        "Massive Attack",
        "Mezzanine",
        &[(3000, cover(1500, 0))],
    ))
    .await;
    let mut rig = Rig::new("immediate", mock.endpoints(), |_| {}).await;

    rig.play("Massive Attack", "Mezzanine", "Angel", &cover(500, 0));
    let art = rig.artwork().expect("no artwork");
    assert_eq!(art.revision, 1);
    assert_eq!(art.source, ArtworkSource::Airplay);

    rig.settle().await;
    assert!(mock.searches() >= 1, "the search never happened");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_album_is_rejected_by_the_text_gate_and_the_scores_are_recorded() {
    let mock = Mock::start(itunes_script(
        "The Beatles",
        "Revolver",
        &[(3000, cover(1500, 0))],
    ))
    .await;
    let mut rig = Rig::new("text-reject", mock.endpoints(), |_| {}).await;

    rig.play("The Beatles", "Abbey Road", "Come Together", &cover(500, 0));
    rig.settle().await;

    let art = rig.artwork().unwrap();
    assert_eq!(
        art.source,
        ArtworkSource::Airplay,
        "the wrong album swapped in"
    );
    assert_eq!(
        art.revision, 1,
        "the revision moved for a rejected candidate"
    );

    let c = rig.counters().enrichment;
    assert_eq!((c.attempted, c.text_rejections, c.upgrades), (1, 1, 0));

    // §7.3: the page must be able to say why, with numbers.
    let note = rig
        .notes()
        .into_iter()
        .find(|n| n.contains("text gate"))
        .expect("no text-gate note");
    assert!(note.contains("Revolver"), "{note}");
    assert!(
        note.contains("artist ") && note.contains("album "),
        "{note}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_marginal_upgrade_is_rejected_by_the_size_gate() {
    // 600 over 500 is not worth a crossfade (DESIGN §5.4 ④).
    let mock = Mock::start(itunes_script(
        "Massive Attack",
        "Mezzanine",
        &[(3000, cover(600, 0))],
    ))
    .await;
    let mut rig = Rig::new("size-reject", mock.endpoints(), |_| {}).await;

    rig.play("Massive Attack", "Mezzanine", "Angel", &cover(500, 0));
    rig.settle().await;

    assert_eq!(rig.artwork().unwrap().source, ArtworkSource::Airplay);
    let c = rig.counters().enrichment;
    assert_eq!((c.size_rejections, c.upgrades), (1, 0), "{c:?}");
    assert!(
        rig.notes().iter().any(|n| n.contains("size gate")),
        "{:?}",
        rig.notes()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unrelated_picture_is_rejected_by_the_perceptual_gate() {
    // The text is close enough to pass but not near-exact, so the bypass does
    // not apply and the picture has to agree.
    let mock = Mock::start(itunes_script(
        "Massive Attack",
        "Mezzanine Live",
        &[(3000, other_cover(1500))],
    ))
    .await;
    let mut rig = Rig::new("perceptual-reject", mock.endpoints(), |_| {}).await;

    rig.play("Massive Attack", "Mezzanine", "Angel", &cover(500, 0));
    rig.settle().await;

    assert_eq!(rig.artwork().unwrap().source, ArtworkSource::Airplay);
    let c = rig.counters().enrichment;
    assert_eq!((c.perceptual_rejections, c.upgrades), (1, 0), "{c:?}");
    let note = rig
        .notes()
        .into_iter()
        .find(|n| n.contains("perceptual gate"))
        .expect("no perceptual-gate note");
    assert!(
        note.contains("hamming") && note.contains("similarity"),
        "{note}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn near_exact_text_lets_a_regional_cover_variant_through() {
    // A different cover for an unambiguously identified album is the case the
    // bypass exists for (DESIGN §5.4 ⑤).
    let mock = Mock::start(itunes_script(
        "Massive Attack",
        "Mezzanine",
        &[(3000, other_cover(1500))],
    ))
    .await;
    let mut rig = Rig::new("bypass", mock.endpoints(), |_| {}).await;

    rig.play("Massive Attack", "Mezzanine", "Angel", &cover(500, 0));
    rig.settle().await;

    let art = rig.artwork().unwrap();
    assert_eq!(
        art.source,
        ArtworkSource::Itunes,
        "the bypass did not apply"
    );
    assert!(art.is_upgrade);
}

#[tokio::test(flavor = "multi_thread")]
async fn strict_mode_removes_the_bypass() {
    let mock = Mock::start(itunes_script(
        "Massive Attack",
        "Mezzanine",
        &[(3000, other_cover(1500))],
    ))
    .await;
    let mut rig = Rig::new("strict", mock.endpoints(), |cfg| {
        cfg.enrichment.strictness = Strictness::Strict;
    })
    .await;

    rig.play("Massive Attack", "Mezzanine", "Angel", &cover(500, 0));
    rig.settle().await;

    assert_eq!(rig.artwork().unwrap().source, ArtworkSource::Airplay);
    assert_eq!(rig.counters().enrichment.perceptual_rejections, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn text_only_strictness_never_looks_at_the_picture() {
    let mock = Mock::start(itunes_script(
        "Massive Attack",
        "Mezzanine Live",
        &[(3000, other_cover(1500))],
    ))
    .await;
    let mut rig = Rig::new("text-only", mock.endpoints(), |cfg| {
        cfg.enrichment.strictness = Strictness::TextOnly;
    })
    .await;

    rig.play("Massive Attack", "Mezzanine", "Angel", &cover(500, 0));
    rig.settle().await;

    let c = rig.counters().enrichment;
    assert_eq!((c.perceptual_rejections, c.upgrades), (0, 1), "{c:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn apple_size_fallbacks_are_walked_down_on_404() {
    // Only 1000 exists, so 3000 and 1500 must 404 and be stepped past.
    let mock = Mock::start(itunes_script(
        "Massive Attack",
        "Mezzanine",
        &[(1000, cover(1000, 0))],
    ))
    .await;
    let mut rig = Rig::new("fallback", mock.endpoints(), |_| {}).await;

    rig.play("Massive Attack", "Mezzanine", "Angel", &cover(500, 0));
    rig.settle().await;

    assert_eq!(rig.artwork().unwrap().width, Some(1000));
    let images: Vec<String> = mock
        .requests()
        .into_iter()
        .filter(|p| p.contains("bb.png"))
        .collect();
    assert_eq!(
        images,
        [
            "/image/3000x3000bb.png",
            "/image/1500x1500bb.png",
            "/image/1000x1000bb.png"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_next_track_of_the_same_album_is_served_from_the_cache() {
    let mock = Mock::start(itunes_script(
        "Massive Attack",
        "Mezzanine",
        &[(3000, cover(1500, 0))],
    ))
    .await;
    let mut rig = Rig::new("cache-hit", mock.endpoints(), |_| {}).await;

    rig.play("Massive Attack", "Mezzanine", "Angel", &cover(500, 0));
    rig.settle().await;
    assert_eq!(rig.counters().enrichment.upgrades, 1);
    let searches = mock.searches();

    // Next track, same record, same AirPlay art.
    rig.play("Massive Attack", "Mezzanine", "Teardrop", &cover(500, 0));
    rig.settle().await;

    let art = rig.artwork().unwrap();
    assert_eq!(art.source, ArtworkSource::Cache);
    assert!(art.is_upgrade);
    assert_eq!(art.width, Some(1500));
    assert_eq!(
        mock.searches(),
        searches,
        "the cache hit still went to the network"
    );
    assert_eq!(rig.counters().enrichment.cache_hits, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_album_no_catalogue_has_is_only_asked_about_once() {
    // The negative cache: coming back to an unknown record must not spend
    // another request finding out the same thing.
    let mock = Mock::start(Script::default()).await;
    let mut rig = Rig::new("negative", mock.endpoints(), |_| {}).await;

    rig.play("Some Band", "Unreleased Demos", "One", &cover(500, 0));
    rig.settle().await;
    assert_eq!(mock.searches(), 1);
    assert_eq!(rig.counters().enrichment.attempted, 1);

    // The next track carries re-encoded art, so this is a genuinely new
    // artwork revision rather than a repeat the deduplicator would swallow.
    rig.play("Some Band", "Unreleased Demos", "Two", &cover(500, 3));
    rig.settle().await;

    assert_eq!(mock.searches(), 1, "the negative cache was ignored");
    let c = rig.counters().enrichment;
    assert_eq!(c.negative_cache_hits, 1, "{c:?}");
    assert_eq!(rig.artwork().unwrap().source, ArtworkSource::Airplay);
}

#[tokio::test(flavor = "multi_thread")]
async fn identical_artwork_for_the_same_album_is_not_reconsidered_at_all() {
    // Cheaper than a cache lookup: with nothing new on screen there is
    // nothing to decide, so no job is started and no report is produced.
    let mock = Mock::start(Script::default()).await;
    let mut rig = Rig::new("no-rework", mock.endpoints(), |_| {}).await;

    let art = cover(500, 0);
    rig.play("Some Band", "Unreleased Demos", "One", &art);
    rig.settle().await;
    assert_eq!(rig.counters().enrichment.attempted, 1);

    rig.play("Some Band", "Unreleased Demos", "Two", &art);
    assert!(
        rig.settle_quiet().await.is_empty(),
        "the same art was re-judged"
    );
    assert_eq!(rig.counters().enrichment.attempted, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resent_bundle_does_not_issue_a_second_search() {
    let mock = Mock::start(itunes_script(
        "Massive Attack",
        "Mezzanine",
        &[(3000, cover(1500, 0))],
    ))
    .await;
    let mut rig = Rig::new("single-flight", mock.endpoints(), |_| {}).await;

    // Senders resend the identical bundle routinely, and progress items
    // arrive between them.
    let art = cover(500, 0);
    for _ in 0..4 {
        rig.play("Massive Attack", "Mezzanine", "Angel", &art);
        rig.core
            .handle(Input::Meta(MetaEvent::Progress(spmeta::dmap::Progress {
                start: 1,
                current: 44_101,
                end: 441_001,
            })));
    }
    rig.settle().await;

    assert_eq!(
        mock.searches(),
        1,
        "duplicate searches: {:?}",
        mock.requests()
    );
    assert_eq!(rig.counters().enrichment.upgrades, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_catalogue_leaves_the_airplay_art_untouched() {
    // Offline is the normal state of a device on flaky Wi-Fi, not an error.
    let dead = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    let endpoints = Endpoints {
        itunes_search: format!("http://{dead}/search"),
        musicbrainz: format!("http://{dead}/ws/2"),
        cover_art_archive: format!("http://{dead}"),
    };
    let mut rig = Rig::new("offline", endpoints, |_| {}).await;

    rig.play("Massive Attack", "Mezzanine", "Angel", &cover(500, 0));
    let before = rig.artwork().unwrap();
    rig.settle().await;

    let after = rig.artwork().unwrap();
    assert_eq!(after.revision, before.revision, "the display changed");
    assert_eq!(after.source, ArtworkSource::Airplay);
    assert_eq!(after.sha256, before.sha256);
    assert_eq!(rig.state().playback, lpframe_proto::Playback::Idle);

    let c = rig.counters().enrichment;
    assert_eq!(c.network_errors, 1, "{c:?}");
    assert_eq!(c.upgrades, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transient_failure_is_retried_and_then_succeeds() {
    let mut script = itunes_script("Massive Attack", "Mezzanine", &[(3000, cover(1500, 0))]);
    script.fail_searches = 2;
    let mock = Mock::start(script).await;
    let mut rig = Rig::new("retry", mock.endpoints(), |_| {}).await;

    let started = std::time::Instant::now();
    rig.play("Massive Attack", "Mezzanine", "Angel", &cover(500, 0));
    rig.settle().await;

    assert_eq!(mock.searches(), 3, "the backoff did not retry three times");
    assert_eq!(rig.artwork().unwrap().source, ArtworkSource::Itunes);
    // The configured backoff really is in force: the production default
    // would have spent six seconds here.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn three_failures_give_up_rather_than_hammering_the_api() {
    let mut script = itunes_script("Massive Attack", "Mezzanine", &[(3000, cover(1500, 0))]);
    script.fail_searches = 99;
    let mock = Mock::start(script).await;
    let mut rig = Rig::new("give-up", mock.endpoints(), |_| {}).await;

    rig.play("Massive Attack", "Mezzanine", "Angel", &cover(500, 0));
    rig.settle().await;

    assert_eq!(mock.searches(), 3, "more than three attempts per track");
    assert_eq!(rig.artwork().unwrap().source, ArtworkSource::Airplay);
    assert_eq!(rig.counters().enrichment.network_errors, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_429_is_counted_and_its_retry_after_is_waited_out() {
    let mut script = itunes_script("Massive Attack", "Mezzanine", &[(3000, cover(1500, 0))]);
    script.rate_limit_searches = 1;
    script.retry_after = 1;
    let mock = Mock::start(script).await;
    let mut rig = Rig::new("rate-limit", mock.endpoints(), |_| {}).await;

    let started = std::time::Instant::now();
    rig.play("Massive Attack", "Mezzanine", "Angel", &cover(500, 0));
    rig.settle().await;

    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "Retry-After was ignored: finished in {:?}",
        started.elapsed()
    );
    assert_eq!(rig.artwork().unwrap().source, ArtworkSource::Itunes);
    assert_eq!(rig.counters().enrichment.rate_limited, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn musicbrainz_is_tried_when_itunes_has_the_wrong_album() {
    let mut script = itunes_script("The Beatles", "Revolver", &[(3000, cover(1500, 0))]);
    script.musicbrainz = vec![(
        "b1a9c0e9-d987-4042-ae91-78d6a3267d69".into(),
        "Abbey Road".into(),
        "The Beatles".into(),
    )];
    script.caa_image = Some(cover(1200, 0));
    let mock = Mock::start(script).await;
    let mut rig = Rig::new("musicbrainz", mock.endpoints(), |cfg| {
        cfg.enrichment.sources = vec!["itunes".into(), "musicbrainz".into()];
        cfg.enrichment.contact = "lpframe@example.test".into();
    })
    .await;

    rig.play("The Beatles", "Abbey Road", "Come Together", &cover(500, 0));
    rig.settle().await;

    let art = rig.artwork().unwrap();
    assert_eq!(art.source, ArtworkSource::CoverArtArchive);
    assert!(art.is_upgrade);
    assert_eq!(art.width, Some(1200));
    assert!(
        mock.requests().iter().any(|p| p.contains("front-1200")),
        "{:?}",
        mock.requests()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn enrichment_disabled_issues_no_requests_at_all() {
    // Privacy is user-visible: "off" must mean no artist or album name leaves
    // the device, not "off except for the request already in flight".
    let mock = Mock::start(itunes_script(
        "Massive Attack",
        "Mezzanine",
        &[(3000, cover(1500, 0))],
    ))
    .await;
    let mut rig = Rig::new("disabled", mock.endpoints(), |cfg| {
        cfg.enrichment.enabled = false;
    })
    .await;

    rig.play("Massive Attack", "Mezzanine", "Angel", &cover(500, 0));
    assert!(rig.settle_quiet().await.is_empty());

    assert!(
        mock.requests().is_empty(),
        "requests were made with enrichment off: {:?}",
        mock.requests()
    );
    assert_eq!(rig.artwork().unwrap().source, ArtworkSource::Airplay);
    assert_eq!(rig.counters().enrichment, Default::default());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_result_for_an_album_that_has_ended_is_discarded() {
    // The stale-swap failure: a report arrives after the listener has moved
    // on, and applying it would put the previous record on the wall.
    let mock = Mock::start(itunes_script(
        "Massive Attack",
        "Mezzanine",
        &[(3000, cover(1500, 0))],
    ))
    .await;
    let mut rig = Rig::new("stale", mock.endpoints(), |_| {}).await;

    rig.play("Portishead", "Dummy", "Roads", &cover(500, 0));
    let before = rig.artwork().unwrap();

    rig.core.handle_enrichment(Report {
        key: album_key("Massive Attack", "Mezzanine"),
        label: "Massive Attack — Mezzanine".into(),
        verdict: Verdict::Upgraded(Box::new(Upgrade {
            bytes: cover(1500, 0),
            dimensions: (1500, 1500),
            source: ArtworkSource::Itunes,
        })),
        rate_limited: false,
    });

    let after = rig.artwork().unwrap();
    assert_eq!(
        after.sha256, before.sha256,
        "another album's art swapped in"
    );
    assert_eq!(after.revision, before.revision);
    assert_eq!(
        rig.counters().enrichment,
        Default::default(),
        "a discarded report was counted"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_track_without_an_album_is_never_searched_for() {
    // A one-sided query returns somebody else's record.
    let mock = Mock::start(itunes_script(
        "Massive Attack",
        "Mezzanine",
        &[(3000, cover(1500, 0))],
    ))
    .await;
    let mut rig = Rig::new("no-album", mock.endpoints(), |_| {}).await;

    for ev in [
        MetaEvent::BundleStart,
        MetaEvent::Core(CoreField::Artist("Massive Attack".into())),
        MetaEvent::Core(CoreField::Title("Angel".into())),
        MetaEvent::BundleEnd,
        MetaEvent::Picture(cover(500, 0)),
    ] {
        rig.core.handle(Input::Meta(ev));
    }
    assert!(rig.settle_quiet().await.is_empty());
    assert!(mock.requests().is_empty(), "{:?}", mock.requests());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_applied_upgrade_leaves_the_cache_populated() {
    // Persistence across a reopen is covered by the cache's own tests; what
    // matters here is that the pipeline actually wrote through to it.
    let mock = Mock::start(itunes_script(
        "Massive Attack",
        "Mezzanine",
        &[(3000, cover(1500, 0))],
    ))
    .await;
    let mut rig = Rig::new("cache-write", mock.endpoints(), |_| {}).await;

    rig.play("Massive Attack", "Mezzanine", "Angel", &cover(500, 0));
    rig.settle().await;

    let enricher = rig.core.enricher().expect("no enricher");
    assert_eq!(enricher.cache_entries(), 1);
    assert!(enricher.cache_bytes() > 0);
}
