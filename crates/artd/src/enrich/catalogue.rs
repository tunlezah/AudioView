//! Talking to iTunes Search, MusicBrainz and the Cover Art Archive.
//!
//! Every base URL is injectable, because no test in this repository is
//! allowed to touch the real internet: a suite that reaches Apple fails on a
//! train, fails in a locked-down CI runner, and silently starts testing
//! Apple's uptime instead of our code.

use std::time::Duration;

use lpframe_proto::ArtworkSource;
use serde::Deserialize;

/// Sizes Apple is asked for, in order, after the configured maximum
/// (DESIGN §5.4 ③). Apple serves the largest available when the request
/// exceeds it, but a URL for a size a particular record does not have still
/// 404s, so the ladder is real rather than defensive.
const APPLE_FALLBACK_SIZES: [u32; 3] = [1500, 1000, 600];

/// Cover Art Archive's front-cover thumbnail size. 1200 is the largest
/// thumbnail the archive generates; the unsized `front` is the submitter's
/// original and is occasionally 30 MB of TIFF-grade JPEG.
const CAA_FRONT: &str = "front-1200";

/// Where the catalogues live.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub itunes_search: String,
    pub musicbrainz: String,
    pub cover_art_archive: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            itunes_search: "https://itunes.apple.com/search".into(),
            musicbrainz: "https://musicbrainz.org/ws/2".into(),
            cover_art_archive: "https://coverartarchive.org".into(),
        }
    }
}

/// One album a catalogue offered, before any gate has looked at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub artist: String,
    pub album: String,
    /// Image URLs to try in order, largest first.
    pub art_urls: Vec<String>,
    pub source: ArtworkSource,
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogueError {
    /// Unreachable, refused, timed out, TLS failed. The normal state of a
    /// device on flaky Wi-Fi, and never logged above debug.
    #[error("network: {0}")]
    Network(String),
    #[error("rate limited; retry after {0:?}")]
    RateLimited(Duration),
    #[error("nothing found")]
    NotFound,
    #[error("HTTP {0}")]
    Status(u16),
    #[error("unreadable response: {0}")]
    Malformed(String),
}

impl CatalogueError {
    /// Whether retrying the identical request could plausibly work.
    pub fn is_transient(&self) -> bool {
        match self {
            CatalogueError::Network(_) | CatalogueError::RateLimited(_) => true,
            // 5xx is the server's problem and may pass; 4xx is ours and will not.
            CatalogueError::Status(code) => *code >= 500,
            CatalogueError::NotFound | CatalogueError::Malformed(_) => false,
        }
    }
}

pub struct Catalogue {
    http: reqwest::Client,
    endpoints: Endpoints,
    country: String,
    max_dimension: u32,
}

impl Catalogue {
    pub fn new(endpoints: Endpoints, country: &str, max_dimension: u32, contact: &str) -> Self {
        // MusicBrainz requires a contactable User-Agent and blocks clients
        // without one. Config validation already refuses to enable
        // MusicBrainz with an empty `contact`, so by the time we are here it
        // is either set or MusicBrainz is off.
        let ua = if contact.trim().is_empty() {
            format!("lpframe/{}", env!("CARGO_PKG_VERSION"))
        } else {
            format!(
                "lpframe/{} ( {} )",
                env!("CARGO_PKG_VERSION"),
                contact.trim()
            )
        };

        let http = reqwest::Client::builder()
            .user_agent(ua)
            // Bounded on purpose. A hung connection must not hold a worker
            // slot for minutes while the next track goes un-enriched.
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_default();

        Catalogue {
            http,
            endpoints,
            country: country.to_string(),
            max_dimension,
        }
    }

    /// iTunes Search, `entity=album` (DESIGN §5.4 ①).
    pub async fn itunes(
        &self,
        artist: &str,
        album: &str,
    ) -> Result<Vec<Candidate>, CatalogueError> {
        let url = format!(
            "{}?term={}&entity=album&limit=10&country={}",
            self.endpoints.itunes_search,
            urlencode(&format!("{artist} {album}")),
            urlencode(&self.country),
        );
        let body: ITunesResponse = self.get_json(&url).await?;

        let candidates: Vec<Candidate> = body
            .results
            .into_iter()
            .filter_map(|r| {
                let art = r.artwork_url100?;
                Some(Candidate {
                    artist: r.artist_name,
                    album: r.collection_name,
                    art_urls: apple_art_urls(&art, self.max_dimension),
                    source: ArtworkSource::Itunes,
                })
            })
            .collect();
        if candidates.is_empty() {
            return Err(CatalogueError::NotFound);
        }
        Ok(candidates)
    }

    /// MusicBrainz release-group search, resolved to Cover Art Archive URLs.
    pub async fn musicbrainz(
        &self,
        artist: &str,
        album: &str,
    ) -> Result<Vec<Candidate>, CatalogueError> {
        let query = format!("artist:\"{}\" AND releasegroup:\"{}\"", artist, album);
        let url = format!(
            "{}/release-group/?query={}&fmt=json&limit=5",
            self.endpoints.musicbrainz,
            urlencode(&query)
        );
        let body: MusicBrainzResponse = self.get_json(&url).await?;

        let candidates: Vec<Candidate> = body
            .release_groups
            .into_iter()
            .map(|g| Candidate {
                artist: g
                    .artist_credit
                    .first()
                    .map(|c| c.name.clone())
                    .unwrap_or_default(),
                album: g.title,
                art_urls: vec![format!(
                    "{}/release-group/{}/{CAA_FRONT}",
                    self.endpoints.cover_art_archive, g.id
                )],
                source: ArtworkSource::CoverArtArchive,
            })
            .collect();
        if candidates.is_empty() {
            return Err(CatalogueError::NotFound);
        }
        Ok(candidates)
    }

    /// Fetch the first URL that exists, walking down the size ladder.
    pub async fn fetch_image(&self, urls: &[String]) -> Result<Vec<u8>, CatalogueError> {
        for url in urls {
            match self.get_bytes(url).await {
                Ok(bytes) => return Ok(bytes),
                // A 404 means this size does not exist for this record; try a
                // smaller one. Anything else is about the connection or our
                // standing with the server, and the next size would hit it
                // too — walking the whole ladder would just be four timeouts.
                Err(CatalogueError::NotFound) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(CatalogueError::NotFound)
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T, CatalogueError> {
        let response = self.send(url).await?;
        response
            .json::<T>()
            .await
            .map_err(|e| CatalogueError::Malformed(e.to_string()))
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, CatalogueError> {
        let response = self.send(url).await?;
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| CatalogueError::Network(e.to_string()))
    }

    async fn send(&self, url: &str) -> Result<reqwest::Response, CatalogueError> {
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| CatalogueError::Network(e.to_string()))?;

        let status = response.status();
        if status.as_u16() == 429 {
            return Err(CatalogueError::RateLimited(retry_after(&response)));
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(CatalogueError::NotFound);
        }
        if !status.is_success() {
            return Err(CatalogueError::Status(status.as_u16()));
        }
        Ok(response)
    }
}

/// `Retry-After`, or a minute if the server did not say.
fn retry_after(response: &reqwest::Response) -> Duration {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        // A date-formatted Retry-After is legal and rare; treating it as
        // absent costs one conservative minute rather than a parser.
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(60))
        .min(Duration::from_secs(3600))
}

/// Rewrite Apple's 100×100 thumbnail URL into the size ladder to try.
///
/// `artworkUrl100` ends in a `<w>x<h>bb.<ext>` segment which is simply
/// substituted; there is no separate API for the full-size image.
pub fn apple_art_urls(url100: &str, max_dimension: u32) -> Vec<String> {
    let Some(slash) = url100.rfind('/') else {
        return vec![url100.to_string()];
    };
    let (prefix, segment) = url100.split_at(slash + 1);
    let Some((dims, ext)) = segment.rsplit_once('.') else {
        return vec![url100.to_string()];
    };
    // Only rewrite a segment that really is a size; Apple has changed this
    // shape before, and mangling an unrecognised one guarantees a 404.
    if !dims
        .strip_suffix("bb")
        .is_some_and(|d| d.split_once('x').is_some_and(|(w, h)| numeric(w, h)))
    {
        return vec![url100.to_string()];
    }

    let mut sizes = vec![max_dimension.clamp(1, 3000)];
    sizes.extend(APPLE_FALLBACK_SIZES.iter().copied());
    sizes.dedup();
    sizes
        .iter()
        .filter(|s| **s <= max_dimension.clamp(1, 3000))
        .map(|s| format!("{prefix}{s}x{s}bb.{ext}"))
        .collect()
}

fn numeric(w: &str, h: &str) -> bool {
    !w.is_empty()
        && !h.is_empty()
        && w.chars().all(|c| c.is_ascii_digit())
        && h.chars().all(|c| c.is_ascii_digit())
}

/// Percent-encode a query parameter value.
///
/// Hand-rolled rather than pulled in as a dependency: the input is an artist
/// and an album name, the output is one query parameter, and the whole rule
/// is "keep unreserved characters, escape the rest".
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(Debug, Deserialize)]
struct ITunesResponse {
    #[serde(default)]
    results: Vec<ITunesResult>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ITunesResult {
    #[serde(default)]
    artist_name: String,
    #[serde(default)]
    collection_name: String,
    #[serde(default)]
    artwork_url100: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MusicBrainzResponse {
    #[serde(default, rename = "release-groups")]
    release_groups: Vec<ReleaseGroup>,
}

#[derive(Debug, Deserialize)]
struct ReleaseGroup {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default, rename = "artist-credit")]
    artist_credit: Vec<ArtistCredit>,
}

#[derive(Debug, Deserialize)]
struct ArtistCredit {
    #[serde(default)]
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPLE: &str = "https://is1-ssl.mzstatic.com/image/thumb/Music/v4/a/b/c/\
                         file.jpg/100x100bb.jpg";

    #[test]
    fn apple_urls_walk_down_the_documented_size_ladder() {
        let urls = apple_art_urls(APPLE, 3000);
        let sizes: Vec<&str> = urls.iter().map(|u| u.rsplit('/').next().unwrap()).collect();
        assert_eq!(
            sizes,
            [
                "3000x3000bb.jpg",
                "1500x1500bb.jpg",
                "1000x1000bb.jpg",
                "600x600bb.jpg"
            ]
        );
        assert!(urls[0].starts_with("https://is1-ssl.mzstatic.com/image/thumb/Music/v4/a/b/c/"));
    }

    #[test]
    fn a_lower_max_dimension_drops_the_sizes_above_it() {
        let sizes: Vec<String> = apple_art_urls(APPLE, 1200)
            .iter()
            .map(|u| u.rsplit('/').next().unwrap().to_string())
            .collect();
        assert_eq!(
            sizes,
            ["1200x1200bb.jpg", "1000x1000bb.jpg", "600x600bb.jpg"]
        );
    }

    #[test]
    fn a_url_apple_has_reshaped_is_left_alone_rather_than_mangled() {
        // Better a 100px candidate that fails the size gate than a fabricated
        // URL that 404s and looks like the network is down.
        for odd in [
            "https://example.test/image/artwork.jpg",
            "https://example.test/image/100x100.jpg",
            "https://example.test/image/widthxheightbb.jpg",
            "no-slashes-at-all",
        ] {
            assert_eq!(apple_art_urls(odd, 3000), vec![odd.to_string()], "{odd}");
        }
    }

    #[test]
    fn the_ceiling_is_three_thousand_whatever_the_config_says() {
        let sizes: Vec<String> = apple_art_urls(APPLE, 99_999)
            .iter()
            .map(|u| u.rsplit('/').next().unwrap().to_string())
            .collect();
        assert_eq!(sizes[0], "3000x3000bb.jpg");
    }

    #[test]
    fn query_parameters_are_encoded() {
        assert_eq!(urlencode("Massive Attack"), "Massive+Attack");
        assert_eq!(urlencode("Sigur Rós"), "Sigur+R%C3%B3s");
        assert_eq!(urlencode("AC/DC"), "AC%2FDC");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencode("~-_."), "~-_.");
    }

    #[test]
    fn only_errors_worth_retrying_are_transient() {
        assert!(CatalogueError::Network("refused".into()).is_transient());
        assert!(CatalogueError::RateLimited(Duration::from_secs(1)).is_transient());
        assert!(CatalogueError::Status(503).is_transient());
        assert!(!CatalogueError::Status(400).is_transient());
        assert!(!CatalogueError::NotFound.is_transient());
        assert!(!CatalogueError::Malformed("junk".into()).is_transient());
    }
}
